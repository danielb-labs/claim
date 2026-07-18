//! The verdict log: a claim's append-only history, and the status derived from
//! it.
//!
//! A claim's definition file says nothing about whether the claim currently
//! holds — that would be a stored status, forgeable and prone to rot. Instead
//! the truth of a claim over time lives here, as a sequence of [`LogEntry`]
//! events under `.claims/log/<claim-id>/`, and its [`Status`] is *computed* from
//! that history against the claim's `max_age` at the moment it is read. This is
//! golden invariant #3 (status is derived, never stored) made concrete.
//!
//! Two properties of this module are load-bearing for the product's honesty:
//!
//! - **Append-only, one file per entry.** [`append_entry`] only ever creates new
//!   files; it never mutates or deletes an existing one. Each entry is its own
//!   JSON file with a time-sortable, content-addressed name, so two concurrent
//!   runs — two commits, in git terms — never write the same path and never
//!   conflict. History is a pile of immutable facts, not a mutable record.
//! - **Every path degrades toward stale, never toward a false pass** (invariant
//!   #6). A single malformed entry file is a loud error naming the file
//!   ([`read_entries`] refuses to silently skip history); a claim with no passing
//!   verdict is [`Status::Stale`] and due immediately, not `Verified`; a broken
//!   or unverifiable streak ages past a grace window into `Stale` rather than
//!   holding a stale green light.
//!
//! Time is always a parameter here. Nothing in this module reads the wall clock;
//! [`compute_status`] takes `now` so the logic is deterministic and testable.
//! Git is also kept out: [`append_entry`] writes the working-tree file and stops
//! there — committing it is the caller's job, per invariant #4.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::claim::{ClaimId, Days};
use crate::error::{Error, Result};
use crate::verdict::{Status, Verdict};

pub use jiff::Timestamp;

/// One event in a claim's history: who observed what, when, and at which commit.
///
/// An entry is immutable once written. It records the *observation*, not a
/// derived conclusion — [`Status`] is computed from a whole sequence of these by
/// [`compute_status`], never stored in an entry. The fields present here are the
/// ones that cannot be recovered from anywhere else at read time: the instant of
/// observation, the commit it was observed against, and the actor who made it.
///
/// Provenance note (invariant #3): the `actor` is cached here as a convenience
/// for display, but the *authoritative* record of who did what is the git commit
/// that adds this file. A forged `actor` string is caught by the commit not
/// matching it; the log is evidence, not the source of identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    /// When the observation was made, as an instant. Serialized as an RFC 3339
    /// string in UTC (e.g. `2026-07-17T12:00:00Z`), which sorts chronologically
    /// and round-trips losslessly.
    pub at: Timestamp,
    /// The git commit sha the claim's checks were observed against, so a verdict
    /// can always be traced back to the exact tree that produced it. A short or
    /// full hex sha; validated only as non-empty here (the caller supplies it
    /// from git).
    pub commit: String,
    /// Who or what made the observation: a human id or an agent id. Cached for
    /// display; the authoritative author is the commit that adds this file.
    pub actor: String,
    /// What happened: a verification produced a verdict, or an adjudication
    /// closed the claim.
    pub event: Event,
}

/// The two kinds of thing that can happen to a claim: a check ran and produced a
/// verdict, or a human/agent adjudicated it.
///
/// Serialized as an internally tagged JSON object (`"type": "verification"` or
/// `"type": "adjudication"`) so the on-disk form is self-describing and an
/// unknown future variant fails to deserialize loudly rather than being
/// misread. v1 has exactly one adjudication, [`Adjudication::Retire`]; the
/// enum shape leaves room for `amend` and others without a format break.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Event {
    /// A check ran to completion and produced a [`Verdict`].
    Verification {
        /// The verdict the check reported. Only [`Verdict::Held`] keeps the
        /// claim fresh; see [`Verdict`] for the honesty contract.
        verdict: Verdict,
        /// Free-form evidence the check or agent recorded — a changelog line, a
        /// command's output, a link. Optional: a plain `cmd` check often has
        /// nothing to add beyond its exit code.
        evidence: Option<String>,
    },
    /// A human or agent deliberately closed or altered the claim's lifecycle.
    Adjudication {
        /// The adjudication performed. v1 supports only retirement.
        action: Adjudication,
    },
}

/// A deliberate lifecycle decision recorded against a claim.
///
/// Not `#[non_exhaustive]`: the workspace crates version together, so an
/// exhaustive `match` here forces every consumer (above all [`compute_status`])
/// to handle a new adjudication the moment one is added, rather than silently
/// ignoring it. That compile error is the point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Adjudication {
    /// The claim was closed on purpose: the decision it rested on was
    /// re-reviewed, or the fact became a real test. Terminal for v1 — a retired
    /// claim stays [`Status::Retired`] regardless of any later verdict.
    Retire {
        /// The closing note explaining why, and where the fact now lives if it
        /// became a test. Required: a retirement with no reason is exactly the
        /// silent closure this tool exists to prevent.
        note: String,
    },
}

/// A claim's status plus the facts a caller needs to act on it.
///
/// [`compute_status`] returns this rather than a bare [`Status`] because the CLI
/// and the drift queue need more than the label: "stale in N days" for the due
/// list, the last-verified date for a claim's page, whether it is due *now* for
/// `claim check --due`. Keeping the derivation in one place means those consumers
/// never re-derive it inconsistently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    /// The computed status. The single answer to "is this claim OK right now".
    pub status: Status,
    /// The instant of the most recent [`Verdict::Held`] in the history, if any.
    /// `None` means the claim has never passed a check and is stale by
    /// definition. This is "when was this last verified" — always a log
    /// timestamp, never a field typed into a file.
    pub last_verified: Option<Timestamp>,
    /// How long ago the claim was last verified, measured from `now`. `None`
    /// when `last_verified` is `None`. May be negative if a `Held` entry is
    /// timestamped in the future relative to `now`; callers treating a future
    /// verification as valid get the natural answer.
    pub age: Option<jiff::SignedDuration>,
    /// Whether the claim needs a check now: `true` for anything wanting
    /// attention ([`Status::Stale`] or [`Status::Drifted`]), `false` when
    /// [`Status::Verified`] or [`Status::Retired`]. A retired claim is terminal,
    /// not due; a verified claim's window has not lapsed, so it is not due either.
    pub due: bool,
}

/// The default grace window for a broken or unverifiable streak, in days.
///
/// A check that breaks (`Broken`) or keeps coming back inconclusive
/// (`Unverifiable`) does not immediately flip a still-fresh claim to stale:
/// transient breakage — a runner outage, a flaky probe — should not nag on the
/// first failure. But the streak cannot mask indefinitely, or a check that
/// breaks and stays broken becomes a permanent false-fresh. After this many days
/// past the last real `Held`, the claim goes [`Status::Stale`] regardless. Per
/// PRODUCT.md section 3; configurable by passing a different value to
/// [`compute_status`].
pub const DEFAULT_GRACE_DAYS: u32 = 90;

/// Append one entry to a claim's log, creating the log directory as needed.
///
/// Writes exactly one new JSON file under `<log_root>/<claim-id>/` and returns
/// its path. This function *only ever creates* files: it never opens an existing
/// entry for writing, never deletes, never truncates. The append-only guarantee
/// is what lets two concurrent runs write history without a lock and without a
/// merge conflict — each entry lands at its own content-addressed path: a
/// time-sortable UTC stamp plus a hash of the entry's bytes.
///
/// Git is deliberately out of scope: this writes the working-tree file and
/// returns. Committing it is the caller's responsibility (invariant #4, "a write
/// to the truth is a commit"). A caller with no write access — a fork PR's CI —
/// simply does not call this and reports the verdict in its output instead.
///
/// # Errors
///
/// Returns [`Error::Io`] naming the path if the directory cannot be created or
/// the file cannot be written. In the astronomically unlikely event that an
/// entry with byte-identical content already exists at the same timestamp (same
/// instant, same commit, same actor, same event), the existing file is left
/// untouched and its path returned — re-recording an identical observation is a
/// no-op, not an error, and never an overwrite.
pub fn append_entry(log_root: &Path, id: &ClaimId, entry: &LogEntry) -> Result<PathBuf> {
    let dir = claim_log_dir(log_root, id);
    fs::create_dir_all(&dir).map_err(|source| Error::Io {
        path: dir.display().to_string(),
        source,
    })?;

    let json = serde_json::to_vec_pretty(entry).map_err(|source| Error::Io {
        path: dir.display().to_string(),
        // A serialization failure on our own well-formed type is an environment
        // fault (e.g. OOM), not malformed input; surface it as I/O against the
        // directory we were about to write into rather than inventing a variant.
        source: std::io::Error::other(source),
    })?;
    let path = dir.join(entry_filename(entry, &json));

    // Create-new so a byte-identical re-record cannot clobber, and so the write
    // is atomic against a concurrent writer racing for the same content-addressed
    // name: the loser sees AlreadyExists and treats the observation as recorded.
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(&json).map_err(|source| Error::Io {
                path: path.display().to_string(),
                source,
            })?;
            Ok(path)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(path),
        Err(source) => Err(Error::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// Read every entry in a claim's log, parsed and returned in chronological
/// order.
///
/// Returns an empty vector if the claim has no log directory or an empty one —
/// a claim can exist with no verdicts (it is simply stale and due immediately),
/// so "no history" is a normal state, not an error.
///
/// # Errors
///
/// Returns [`Error::Io`] if the directory cannot be listed, or [`Error::Parse`]
/// *naming the offending file* if any entry file contains malformed JSON. A
/// single bad file fails the whole read: silently skipping a history entry could
/// hide a `Drifted` verdict and let a claim read as fresh when it is not, which
/// is precisely the false-green failure invariant #6 forbids. Non-`.json` files
/// (a stray `.gitkeep`, an editor's swap file) are ignored so the log directory
/// tolerates the incidental clutter every git directory accumulates.
pub fn read_entries(log_root: &Path, id: &ClaimId) -> Result<Vec<LogEntry>> {
    let dir = claim_log_dir(log_root, id);
    let read_dir = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::Io {
                path: dir.display().to_string(),
                source,
            })
        }
    };

    // Collect (filename, entry) so the sort key is the on-disk name, which is
    // time-sortable by construction and breaks ties deterministically by content
    // hash. Sorting by the parsed `at` alone would be non-deterministic for two
    // entries sharing an instant; the filename is a total order.
    let mut named: Vec<(std::ffi::OsString, LogEntry)> = Vec::new();
    for dirent in read_dir {
        let dirent = dirent.map_err(|source| Error::Io {
            path: dir.display().to_string(),
            source,
        })?;
        let path = dirent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let bytes = fs::read(&path).map_err(|source| Error::Io {
            path: path.display().to_string(),
            source,
        })?;
        let entry: LogEntry = serde_json::from_slice(&bytes).map_err(|e| {
            Error::parse(
                path.display().to_string(),
                format!("malformed verdict log entry: {e}"),
            )
        })?;
        named.push((dirent.file_name(), entry));
    }

    named.sort_by(|(a, _), (b, _)| a.cmp(b));
    Ok(named.into_iter().map(|(_, entry)| entry).collect())
}

/// Compute a claim's [`Status`] and a [`StatusReport`] from its history.
///
/// This is the heart of the item and the concrete form of invariant #3. It is a
/// pure function of its inputs — `max_age`, the ordered `history`, `now`, and the
/// `grace` window for broken/unverifiable streaks — with no hidden clock, so a
/// test can pin every timestamp and get a deterministic answer.
///
/// `history` must be in chronological order, as [`read_entries`] returns it. The
/// rules, applied in this order:
///
/// 1. If the most recent adjudication anywhere in the history is a
///    [`Adjudication::Retire`], the claim is [`Status::Retired`]. Retirement is
///    terminal for v1: a later `Held` does *not* revive it (retiring is a
///    deliberate human act; a stray check re-running should not undo it). To
///    reopen, author a new claim.
/// 2. Otherwise, if the most recent *verdict* is [`Verdict::Drifted`], the claim
///    is [`Status::Drifted`] — its own check says the fact is no longer true, and
///    that is true regardless of `max_age`.
/// 3. Otherwise, if any [`Verdict::Held`] exists and the latest one is within the
///    claim's *fresh window* of `now`, the claim is [`Status::Verified`]. The
///    boundary is inclusive: a claim verified *exactly* at the window's end is
///    still `Verified`, because the window is the length of validity and its last
///    instant is still inside it; staleness begins the instant after.
///
///    The fresh window is normally `max_age`. But when the most recent verdict
///    since that `Held` is a `Broken` or `Unverifiable` streak — the check ran
///    but could not confirm — a claim already past `max_age` is given until
///    `grace` (from the last `Held`) before it goes stale. This is the one place
///    `grace` is load-bearing: transient breakage (a runner outage, a flaky
///    probe) should not nag on the first failure past `max_age`, but the streak
///    cannot mask indefinitely, or a check that breaks and stays broken becomes a
///    permanent false-fresh. Since `grace` (default 90 days) is meant to exceed a
///    typical `max_age`, the window can only ever *extend*, never shrink; a fresh
///    `Held` collapses it back to `max_age`.
/// 4. Otherwise the claim is [`Status::Stale`]. This one branch covers every way
///    a claim ages out: never verified (no `Held` ever), last `Held` older than
///    `max_age` with no lingering streak, or a `Broken`/`Unverifiable` streak
///    that has run past the last `Held` for longer than `grace`. In every case
///    the end state is a human being nagged, never a stale green light
///    (invariant #6).
///
/// Note the interaction of rules 2 and 3: a `Held` followed later by a `Broken`
/// is *not* drifted (rule 2 only fires on `Drifted`); it stays `Verified` while
/// within `max_age`, extends to `grace` while the streak continues, then goes
/// `Stale`. A `Drifted` followed later by a `Held` is `Verified` — the claim was
/// re-verified and the drift is history.
#[must_use]
pub fn compute_status(
    max_age: Days,
    history: &[LogEntry],
    now: Timestamp,
    grace: Days,
) -> StatusReport {
    let last_held = history.iter().rev().find_map(|e| match &e.event {
        Event::Verification {
            verdict: Verdict::Held,
            ..
        } => Some(e.at),
        _ => None,
    });

    let age = last_held.map(|at| now.duration_since(at));

    let status = compute_status_kind(max_age, history, now, grace, last_held);
    let due = !matches!(status, Status::Verified | Status::Retired);

    StatusReport {
        status,
        last_verified: last_held,
        age,
        due,
    }
}

/// The status label alone, factored out so [`compute_status`] can assemble the
/// full report around it. Takes the pre-computed `last_held` to avoid scanning
/// the history twice.
fn compute_status_kind(
    max_age: Days,
    history: &[LogEntry],
    now: Timestamp,
    grace: Days,
    last_held: Option<Timestamp>,
) -> Status {
    // Rule 1: a retirement is terminal, and only the *latest* adjudication
    // counts, so an amend-then-retire (or a hypothetical future reopen) reads
    // correctly the moment those exist.
    if let Some(latest_adjudication) = history.iter().rev().find_map(|e| match &e.event {
        Event::Adjudication { action } => Some(action),
        Event::Verification { .. } => None,
    }) {
        match latest_adjudication {
            Adjudication::Retire { .. } => return Status::Retired,
        }
    }

    // The most recent verdict, if the claim has ever been checked. Adjudications
    // are skipped: they were handled by rule 1, and a retirement that a later
    // amend reopened must not masquerade as a verdict here.
    let latest_verdict = history.iter().rev().find_map(|e| match &e.event {
        Event::Verification { verdict, .. } => Some(*verdict),
        Event::Adjudication { .. } => None,
    });

    // Rule 2: the most recent verdict wins if it is Drifted. Broken and
    // Unverifiable deliberately fall through — they are freshness failures, not a
    // statement that the fact changed, so they age via the grace window in rule 3
    // rather than flipping straight to a drift report.
    if latest_verdict == Some(Verdict::Drifted) {
        return Status::Drifted;
    }

    // Rule 3: a Held within the fresh window is Verified, inclusive of the
    // window's final instant. The window is `max_age` normally, but extends to
    // `grace` when a Broken/Unverifiable streak is in progress since the last
    // Held — a check that ran and could not confirm buys the claim time to
    // recover, bounded so it can never mask a permanently broken check.
    if let Some(held_at) = last_held {
        let streak_active = matches!(
            latest_verdict,
            Some(Verdict::Broken | Verdict::Unverifiable)
        );
        let window = if streak_active {
            days_duration(max_age).max(days_duration(grace))
        } else {
            days_duration(max_age)
        };
        if now <= held_at + window {
            return Status::Verified;
        }
    }

    // Rule 4: everything else is stale — never verified, aged past max_age with
    // no streak, or a streak run past grace. The end state is always a nag, never
    // a stale green light (invariant #6).
    Status::Stale
}

/// The freshness window as a fixed duration.
///
/// `max_age` is a whole-day count and a [`Timestamp`] is a UTC instant with no
/// zone or DST, so a day here is an unambiguous 24 hours. Using a fixed duration
/// (rather than a calendar span) is both correct for instants and what keeps the
/// boundary arithmetic in [`compute_status`] exact and total.
fn days_duration(days: Days) -> jiff::SignedDuration {
    jiff::SignedDuration::from_hours(i64::from(days.get()) * 24)
}

/// The directory holding a claim's log entries: `<log_root>/<claim-id>/`.
///
/// A claim id may be namespaced with `/` (e.g. `payments/libfoo-pin`); it maps
/// to nested directories directly and safely. This is sound *because* [`ClaimId`]
/// is already validated to contain only `[a-z0-9-/]` with clean, non-empty
/// segments and no `.` — so no segment can be `.` or `..`, none can escape
/// `log_root`, and the mapping is an unambiguous bijection (a namespaced id and a
/// flat id can never collide). No escaping is needed or wanted: an escaped path
/// would be less legible in `git log` and on the forge, where these files are
/// meant to be read by humans.
fn claim_log_dir(log_root: &Path, id: &ClaimId) -> PathBuf {
    let mut dir = log_root.to_path_buf();
    for segment in id.as_str().split('/') {
        dir.push(segment);
    }
    dir
}

/// The filename for a log entry: a time-sortable stamp, a short content hash, and
/// `.json`.
///
/// The name must satisfy two constraints at once. It must sort chronologically
/// as a plain string, so listing a directory yields history in order without
/// parsing every file — met by a fixed-width UTC RFC 3339 stamp with `:`
/// rendered as `-` (`:` is unsafe on some filesystems; the substitution is
/// uniform and positional, so lexicographic order is preserved). And it must be
/// collision-resistant without randomness, because randomness would make tests
/// non-deterministic — met by a hash *of the entry's serialized bytes*, so two
/// genuinely distinct entries at the same instant get distinct names while a
/// byte-identical re-record maps to the same name (and [`append_entry`] treats
/// that as the no-op it is).
///
/// Example: `2026-07-17T12-00-00Z-a1b2c3d4e5f60718.json`.
fn entry_filename(entry: &LogEntry, json: &[u8]) -> String {
    let stamp = entry.at.to_string().replace(':', "-");
    format!("{stamp}-{:016x}.json", fnv1a64(json))
}

/// FNV-1a over the entry bytes, for the filename's collision-resistant suffix.
///
/// A tiny, fully-specified hash rather than [`std::hash::DefaultHasher`]: the
/// latter's output is explicitly not stable across Rust versions or platforms,
/// which would make an entry's filename — and therefore a snapshot or golden
/// test — depend on the toolchain. This is a filename disambiguator, not a
/// security primitive; 64 bits of FNV is ample to separate distinct entries that
/// happen to share a timestamp.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn id(s: &str) -> ClaimId {
        ClaimId::from_str(s).unwrap()
    }

    fn days(n: u32) -> Days {
        Days::from_str(&format!("{n}d")).unwrap()
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    /// A verification entry at a given instant with a given verdict.
    fn verify(at: &str, verdict: Verdict) -> LogEntry {
        LogEntry {
            at: ts(at),
            commit: "abc123".to_owned(),
            actor: "ci".to_owned(),
            event: Event::Verification {
                verdict,
                evidence: None,
            },
        }
    }

    /// A retirement entry at a given instant.
    fn retire(at: &str, note: &str) -> LogEntry {
        LogEntry {
            at: ts(at),
            commit: "abc123".to_owned(),
            actor: "human:dana".to_owned(),
            event: Event::Adjudication {
                action: Adjudication::Retire {
                    note: note.to_owned(),
                },
            },
        }
    }

    // --- Serialization and disk round-trips. ---

    #[test]
    fn entry_round_trips_through_json() {
        let entry = LogEntry {
            at: ts("2026-07-17T12:00:00Z"),
            commit: "deadbeef".to_owned(),
            actor: "agent:claude".to_owned(),
            event: Event::Verification {
                verdict: Verdict::Held,
                evidence: Some("grep matched libfoo==4.2".to_owned()),
            },
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: LogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn timestamp_serializes_as_rfc3339() {
        let entry = verify("2026-07-17T12:00:00Z", Verdict::Held);
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            json.contains("\"2026-07-17T12:00:00Z\""),
            "timestamp should serialize as an RFC 3339 string: {json}"
        );
    }

    #[test]
    fn event_is_tagged_in_json() {
        // The on-disk form must be self-describing: a reader (or a future
        // variant) distinguishes a verification from an adjudication by tag.
        let v = serde_json::to_string(&verify("2026-07-17T12:00:00Z", Verdict::Held)).unwrap();
        assert!(v.contains("\"type\":\"verification\""), "{v}");
        let r = serde_json::to_string(&retire("2026-07-17T12:00:00Z", "done")).unwrap();
        assert!(r.contains("\"type\":\"adjudication\""), "{r}");
        assert!(r.contains("\"action\":\"retire\""), "{r}");
    }

    #[test]
    fn append_then_read_round_trips_one_entry() {
        let tmp = tempdir();
        let claim = id("libfoo-pin");
        let entry = verify("2026-07-17T12:00:00Z", Verdict::Held);
        append_entry(tmp.path(), &claim, &entry).unwrap();

        let read = read_entries(tmp.path(), &claim).unwrap();
        assert_eq!(read, vec![entry]);
    }

    #[test]
    fn read_returns_entries_in_chronological_order() {
        let tmp = tempdir();
        let claim = id("c");
        // Append out of chronological order to prove the read sorts, not the
        // write.
        let later = verify("2026-07-17T15:00:00Z", Verdict::Drifted);
        let earlier = verify("2026-07-17T09:00:00Z", Verdict::Held);
        let middle = verify("2026-07-17T12:00:00Z", Verdict::Broken);
        append_entry(tmp.path(), &claim, &later).unwrap();
        append_entry(tmp.path(), &claim, &earlier).unwrap();
        append_entry(tmp.path(), &claim, &middle).unwrap();

        let read = read_entries(tmp.path(), &claim).unwrap();
        assert_eq!(read, vec![earlier, middle, later]);
    }

    #[test]
    fn two_entries_at_the_same_timestamp_coexist() {
        // One-file-per-entry means a shared instant is not a collision: distinct
        // content yields distinct filenames, and both survive.
        let tmp = tempdir();
        let claim = id("c");
        let a = LogEntry {
            at: ts("2026-07-17T12:00:00Z"),
            commit: "sha-a".to_owned(),
            actor: "ci".to_owned(),
            event: Event::Verification {
                verdict: Verdict::Held,
                evidence: None,
            },
        };
        let b = LogEntry {
            at: ts("2026-07-17T12:00:00Z"),
            commit: "sha-b".to_owned(),
            actor: "ci".to_owned(),
            event: Event::Verification {
                verdict: Verdict::Held,
                evidence: None,
            },
        };
        let pa = append_entry(tmp.path(), &claim, &a).unwrap();
        let pb = append_entry(tmp.path(), &claim, &b).unwrap();
        assert_ne!(pa, pb, "distinct entries must get distinct files");

        let read = read_entries(tmp.path(), &claim).unwrap();
        assert_eq!(read.len(), 2);
        assert!(read.contains(&a) && read.contains(&b));
    }

    #[test]
    fn appending_a_byte_identical_entry_is_a_noop() {
        // A re-recorded identical observation must not overwrite or duplicate:
        // append-only means new files only, and an identical entry is the same
        // file.
        let tmp = tempdir();
        let claim = id("c");
        let entry = verify("2026-07-17T12:00:00Z", Verdict::Held);
        let first = append_entry(tmp.path(), &claim, &entry).unwrap();
        let second = append_entry(tmp.path(), &claim, &entry).unwrap();
        assert_eq!(first, second);
        assert_eq!(read_entries(tmp.path(), &claim).unwrap().len(), 1);
    }

    #[test]
    fn namespaced_id_maps_to_nested_path_and_round_trips() {
        let tmp = tempdir();
        let claim = id("payments/libfoo-pin");
        let entry = verify("2026-07-17T12:00:00Z", Verdict::Held);
        let path = append_entry(tmp.path(), &claim, &entry).unwrap();

        // The id's `/` became a real directory boundary under the log root.
        assert!(
            path.starts_with(tmp.path().join("payments").join("libfoo-pin")),
            "namespaced id must nest under log_root: {}",
            path.display()
        );
        assert_eq!(read_entries(tmp.path(), &claim).unwrap(), vec![entry]);
    }

    #[test]
    fn distinct_namespaced_ids_do_not_collide() {
        let tmp = tempdir();
        let a = id("payments/pin");
        let b = id("payments/pin-two");
        append_entry(
            tmp.path(),
            &a,
            &verify("2026-07-17T12:00:00Z", Verdict::Held),
        )
        .unwrap();
        append_entry(
            tmp.path(),
            &b,
            &verify("2026-07-17T12:00:00Z", Verdict::Held),
        )
        .unwrap();
        assert_eq!(read_entries(tmp.path(), &a).unwrap().len(), 1);
        assert_eq!(read_entries(tmp.path(), &b).unwrap().len(), 1);
    }

    #[test]
    fn missing_log_dir_reads_as_empty() {
        let tmp = tempdir();
        let read = read_entries(tmp.path(), &id("never-logged")).unwrap();
        assert!(read.is_empty());
    }

    #[test]
    fn malformed_entry_file_errors_and_names_the_file() {
        // Invariant #6: a bad entry is loud, never silently dropped. Dropping it
        // could hide a Drifted verdict and read the claim as fresh.
        let tmp = tempdir();
        let claim = id("c");
        let dir = tmp.path().join("c");
        fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("2026-07-17T12-00-00Z-deadbeefdeadbeef.json");
        fs::write(&bad, b"{ this is not valid json").unwrap();

        let err = read_entries(tmp.path(), &claim).unwrap_err();
        match &err {
            Error::Parse { path, reason } => {
                assert!(path.contains("2026-07-17T12-00-00Z"), "path: {path}");
                assert!(reason.contains("malformed"), "reason: {reason}");
            }
            other => panic!("expected a parse error naming the file, got {other:?}"),
        }
    }

    #[test]
    fn non_json_files_in_log_dir_are_ignored() {
        let tmp = tempdir();
        let claim = id("c");
        let entry = verify("2026-07-17T12:00:00Z", Verdict::Held);
        append_entry(tmp.path(), &claim, &entry).unwrap();
        // A stray file a git directory might accumulate must not break reads.
        fs::write(tmp.path().join("c").join(".gitkeep"), b"").unwrap();

        assert_eq!(read_entries(tmp.path(), &claim).unwrap(), vec![entry]);
    }

    #[test]
    fn filename_is_time_sortable_and_colon_free() {
        let entry = verify("2026-07-17T12:00:00Z", Verdict::Held);
        let json = serde_json::to_vec_pretty(&entry).unwrap();
        let name = entry_filename(&entry, &json);
        assert!(!name.contains(':'), "filename must be colon-free: {name}");
        assert!(name.starts_with("2026-07-17T12-00-00Z-"), "{name}");
        assert!(name.ends_with(".json"), "{name}");
    }

    // --- Status computation. ---

    #[test]
    fn empty_history_is_stale_and_due() {
        let report = compute_status(days(30), &[], ts("2026-07-17T12:00:00Z"), days(90));
        assert_eq!(report.status, Status::Stale);
        assert!(report.due);
        assert_eq!(report.last_verified, None);
        assert_eq!(report.age, None);
    }

    #[test]
    fn single_held_within_max_age_is_verified() {
        let history = [verify("2026-07-01T12:00:00Z", Verdict::Held)];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), days(90));
        assert_eq!(report.status, Status::Verified);
        assert!(!report.due);
        assert_eq!(report.last_verified, Some(ts("2026-07-01T12:00:00Z")));
        assert!(report.age.unwrap().as_hours() == 16 * 24);
    }

    #[test]
    fn held_exactly_at_max_age_boundary_is_verified() {
        // Documented decision: the boundary is inclusive. A claim verified
        // exactly max_age ago is still within the window; staleness begins the
        // instant after.
        let history = [verify("2026-06-17T12:00:00Z", Verdict::Held)];
        // Exactly 30 days later.
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), days(90));
        assert_eq!(report.status, Status::Verified);
    }

    #[test]
    fn held_one_second_past_max_age_is_stale() {
        let history = [verify("2026-06-17T12:00:00Z", Verdict::Held)];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:01Z"), days(90));
        assert_eq!(report.status, Status::Stale);
        assert!(report.due);
        // last_verified is still reported even when stale — the CLI shows "last
        // verified <date>, now overdue".
        assert_eq!(report.last_verified, Some(ts("2026-06-17T12:00:00Z")));
    }

    #[test]
    fn held_well_past_max_age_is_stale() {
        let history = [verify("2026-01-01T12:00:00Z", Verdict::Held)];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), days(90));
        assert_eq!(report.status, Status::Stale);
    }

    #[test]
    fn latest_drifted_is_drifted() {
        let history = [
            verify("2026-07-01T12:00:00Z", Verdict::Held),
            verify("2026-07-10T12:00:00Z", Verdict::Drifted),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), days(90));
        assert_eq!(report.status, Status::Drifted);
        assert!(report.due);
        // The last Held is still recorded even though the claim has since drifted.
        assert_eq!(report.last_verified, Some(ts("2026-07-01T12:00:00Z")));
    }

    #[test]
    fn drifted_then_later_held_is_verified() {
        // Re-verification clears drift: the fact was fixed, the drift is history.
        let history = [
            verify("2026-07-01T12:00:00Z", Verdict::Drifted),
            verify("2026-07-10T12:00:00Z", Verdict::Held),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), days(90));
        assert_eq!(report.status, Status::Verified);
        assert_eq!(report.last_verified, Some(ts("2026-07-10T12:00:00Z")));
    }

    #[test]
    fn held_then_later_broken_within_max_age_is_verified() {
        // A broken check is not a drift and does not immediately flip a fresh
        // claim: while the last Held is within max_age, the claim stays Verified,
        // giving transient breakage room to recover.
        let history = [
            verify("2026-07-01T12:00:00Z", Verdict::Held),
            verify("2026-07-10T12:00:00Z", Verdict::Broken),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), days(90));
        assert_eq!(report.status, Status::Verified);
    }

    #[test]
    fn held_then_broken_past_max_age_but_within_grace_is_verified() {
        // A broken streak extends the fresh window from max_age (30d) to grace
        // (90d), measured from the last Held. Held Jan 1, so grace runs to Apr 1;
        // Mar 15 is past max_age but inside grace, so the claim rides out the
        // transient breakage as Verified.
        let history = [
            verify("2026-01-01T12:00:00Z", Verdict::Held),
            verify("2026-02-01T12:00:00Z", Verdict::Broken),
        ];
        let report = compute_status(days(30), &history, ts("2026-03-15T12:00:00Z"), days(90));
        assert_eq!(report.status, Status::Verified);
    }

    #[test]
    fn held_then_broken_past_grace_is_stale() {
        // Once the broken streak runs past grace (90d from the last Held), the
        // claim goes stale and nags. Invariant #6: a broken check never holds a
        // green light indefinitely.
        let history = [
            verify("2026-01-01T12:00:00Z", Verdict::Held),
            verify("2026-02-01T12:00:00Z", Verdict::Broken),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), days(90));
        assert_eq!(report.status, Status::Stale);
    }

    #[test]
    fn broken_streak_at_grace_boundary_is_verified() {
        // Grace, like max_age, is inclusive of its final instant. Held Jan 1,
        // grace 90d → Apr 1 12:00:00 is still fresh; one second later is stale
        // (asserted by the past-grace test above at a coarser distance).
        let history = [
            verify("2026-01-01T12:00:00Z", Verdict::Held),
            verify("2026-02-01T12:00:00Z", Verdict::Broken),
        ];
        let at_boundary = compute_status(days(30), &history, ts("2026-04-01T12:00:00Z"), days(90));
        assert_eq!(at_boundary.status, Status::Verified);
        let past = compute_status(days(30), &history, ts("2026-04-01T12:00:01Z"), days(90));
        assert_eq!(past.status, Status::Stale);
    }

    #[test]
    fn unverifiable_streak_after_held_past_grace_is_stale() {
        // Unverifiable behaves like Broken for the grace window: it buys time,
        // then goes stale. Held Jan 1, grace 90d → Apr 1; Jul 17 is well past.
        let history = [
            verify("2026-01-01T12:00:00Z", Verdict::Held),
            verify("2026-02-01T12:00:00Z", Verdict::Unverifiable),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), days(90));
        assert_eq!(report.status, Status::Stale);
        // The last Held is still reported for the CLI's "last verified" line.
        assert_eq!(report.last_verified, Some(ts("2026-01-01T12:00:00Z")));
    }

    #[test]
    fn unverifiable_only_history_is_stale() {
        // An unverifiable streak with no Held ever is stale: it never earned
        // freshness in the first place.
        let history = [
            verify("2026-07-01T12:00:00Z", Verdict::Unverifiable),
            verify("2026-07-10T12:00:00Z", Verdict::Unverifiable),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), days(90));
        assert_eq!(report.status, Status::Stale);
        assert_eq!(report.last_verified, None);
    }

    #[test]
    fn broken_only_history_is_stale() {
        let history = [verify("2026-07-16T12:00:00Z", Verdict::Broken)];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), days(90));
        assert_eq!(report.status, Status::Stale);
        assert_eq!(report.last_verified, None);
    }

    #[test]
    fn retirement_is_terminal_even_with_a_later_held() {
        // Documented decision: retirement is terminal for v1. A later Held does
        // not revive a retired claim — retiring is a deliberate act, and a check
        // re-running must not silently undo it. Only the *latest* adjudication
        // decides, which is what makes this future-proof against amend/reopen.
        let history = [
            verify("2026-07-01T12:00:00Z", Verdict::Held),
            retire("2026-07-05T12:00:00Z", "superseded by CI gate"),
            verify("2026-07-10T12:00:00Z", Verdict::Held),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), days(90));
        assert_eq!(report.status, Status::Retired);
        assert!(!report.due, "a retired claim is terminal, not due");
    }

    #[test]
    fn retirement_after_drift_is_retired() {
        let history = [
            verify("2026-07-01T12:00:00Z", Verdict::Drifted),
            retire("2026-07-05T12:00:00Z", "decision reversed"),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), days(90));
        assert_eq!(report.status, Status::Retired);
    }

    #[test]
    fn grace_does_not_apply_without_a_broken_streak() {
        // A large grace must not extend a plain Held past its max_age: grace only
        // buys time for a Broken/Unverifiable streak, never for an ordinary
        // aged-out verification. Held Jan 1, max_age 30d → stale by Feb 1
        // regardless of a 90d grace, because the latest verdict is the Held
        // itself, not a streak.
        let history = [verify("2026-01-01T12:00:00Z", Verdict::Held)];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), days(90));
        assert_eq!(report.status, Status::Stale);
    }

    #[test]
    fn a_fresh_held_collapses_an_extended_window() {
        // A Held after a broken streak resets the clock to max_age: the streak's
        // grace extension does not linger once the check recovers.
        let history = [
            verify("2026-01-01T12:00:00Z", Verdict::Held),
            verify("2026-02-01T12:00:00Z", Verdict::Broken),
            verify("2026-07-16T12:00:00Z", Verdict::Held),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), days(90));
        assert_eq!(report.status, Status::Verified);
        assert_eq!(report.last_verified, Some(ts("2026-07-16T12:00:00Z")));
    }

    #[test]
    fn most_recent_held_wins_when_several_exist() {
        // last_verified must be the newest Held, not the first.
        let history = [
            verify("2026-07-01T12:00:00Z", Verdict::Held),
            verify("2026-07-15T12:00:00Z", Verdict::Held),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), days(90));
        assert_eq!(report.last_verified, Some(ts("2026-07-15T12:00:00Z")));
        assert_eq!(report.status, Status::Verified);
    }

    fn tempdir() -> TempDir {
        TempDir::new()
    }

    /// A minimal self-cleaning temp directory, so the log tests touch a real
    /// filesystem without depending on the `tempfile` crate (a test-only dep the
    /// workspace reserves for the CLI integration tests). Uses the process id and
    /// a monotonic counter for a unique path; removed on drop.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("claim-log-test-{}-{}", std::process::id(), n));
            fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
