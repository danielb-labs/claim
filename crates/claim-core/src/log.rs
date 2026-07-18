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
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::claim::{ClaimId, Days};
use crate::error::{Error, Result};
use crate::verdict::{Status, Verdict};

pub use jiff::{SignedDuration, Timestamp};

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
    /// full hex sha supplied by the caller from git. [`append_entry`] rejects an
    /// empty or whitespace-only value: an untraceable verdict has no provenance,
    /// which the trust model forbids.
    pub commit: String,
    /// Who or what made the observation: a human id or an agent id. Cached for
    /// display; the authoritative author is the commit that adds this file.
    /// [`append_entry`] rejects an empty or whitespace-only value.
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
/// and the drift queue need more than the label: "stale in N days" (from
/// `stale_at`) for the due list, the last-verified date for a claim's page,
/// whether it is due *now* for `claim check --due`. Keeping the derivation in one
/// place means those consumers never re-derive it inconsistently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    /// The computed status. The single answer to "is this claim OK right now".
    pub status: Status,
    /// The instant of the most recent *past* [`Verdict::Held`] — the newest one
    /// at or before `now`. `None` means the claim has never passed a check as of
    /// `now` and is stale by definition. A `Held` timestamped in the future (clock
    /// skew, or forgery) is deliberately excluded: a verification that has not yet
    /// happened cannot certify present freshness. This is "when was this last
    /// verified" — always a log timestamp, never a field typed into a file.
    pub last_verified: Option<Timestamp>,
    /// How long ago the claim was last verified, measured from `now`. `None` when
    /// `last_verified` is `None`. Never negative: it mirrors `last_verified`,
    /// which only ever holds a past `Held`.
    pub age: Option<SignedDuration>,
    /// The instant at which the claim becomes (or became) [`Status::Stale`] — the
    /// end of its fresh window. Answers "stale in N days" as `stale_at - now`, and
    /// "how overdue" when it is in the past. `None` when there is no finite
    /// deadline to report: a [`Status::Retired`] claim (terminal), a claim never
    /// verified (already stale, no window to expire), or the rare case where the
    /// window would extend past the representable end of time (see
    /// [`compute_status`]). Reflects the grace-extended window when a
    /// broken/unverifiable streak is active.
    pub stale_at: Option<Timestamp>,
    /// Whether the claim needs a check now: `true` for anything wanting
    /// attention ([`Status::Stale`] or [`Status::Drifted`]), `false` when
    /// [`Status::Verified`] or [`Status::Retired`]. A retired claim is terminal,
    /// not due; a verified claim's window has not lapsed, so it is not due either.
    pub due: bool,
}

/// A grace window: how long a broken or unverifiable streak may keep a claim
/// fresh past its `max_age` before it goes stale anyway.
///
/// A distinct newtype from [`Days`], and the last positional argument of
/// [`compute_status`], so the two day counts that function takes — `max_age` and
/// the grace window — cannot be transposed at a call site without a type error.
/// Transposing them would silently change a claim's staleness, exactly the quiet
/// wrong answer this tool exists to prevent.
///
/// A check that breaks (`Broken`) or keeps coming back inconclusive
/// (`Unverifiable`) does not immediately flip a still-fresh claim to stale:
/// transient breakage — a runner outage, a flaky probe — should not nag on the
/// first failure. But the streak cannot mask indefinitely, or a check that breaks
/// and stays broken becomes a permanent false-fresh. Once the streak has run
/// `grace` past the last real `Held`, the claim goes [`Status::Stale`] regardless.
/// Per PRODUCT.md section 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Grace(pub Days);

impl Grace {
    /// The default grace window, 90 days, per PRODUCT.md section 3.
    ///
    /// A usable compile-time constant: `const` all the way down, so a caller can
    /// pass `Grace::DEFAULT` without parsing a string at runtime.
    pub const DEFAULT: Grace = {
        // 90 is a nonzero literal; the `expect` can never fire, and being const it
        // is checked at compile time rather than trusted at runtime.
        let Some(days) = NonZeroU32::new(90) else {
            panic!("90 is nonzero")
        };
        Grace(Days::from_nonzero(days))
    };

    /// The window as a plain [`Days`].
    #[must_use]
    pub fn days(self) -> Days {
        self.0
    }
}

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
/// - [`Error::Parse`] if `entry.commit` or `entry.actor` is empty or
///   whitespace-only: a verdict with no traceable commit or actor has no
///   provenance, which the trust model forbids.
/// - [`Error::Io`] naming the path if the directory cannot be created or the file
///   cannot be written.
/// - [`Error::Io`] naming the path if the target filename already exists **with
///   different bytes**. The filename is a timestamp plus a 64-bit hash of the
///   entry, so a hash collision between two genuinely different observations is
///   improbable but not impossible — and silently keeping the older file could
///   drop a `Drifted` and leave a stale `Held` as the record, a false-green this
///   tool must never produce. A byte-identical file at the same path is instead a
///   no-op (the observation is already recorded): its path is returned. This also
///   makes concurrent writers racing for the same content-addressed name safe —
///   the loser confirms the bytes match and treats the record as written.
pub fn append_entry(log_root: &Path, id: &ClaimId, entry: &LogEntry) -> Result<PathBuf> {
    if entry.commit.trim().is_empty() {
        return Err(Error::parse(
            "commit",
            "a verdict log entry needs a non-empty commit sha; an untraceable \
             verdict has no provenance",
        ));
    }
    if entry.actor.trim().is_empty() {
        return Err(Error::parse(
            "actor",
            "a verdict log entry needs a non-empty actor; an unattributed verdict \
             has no provenance",
        ));
    }

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

    // Create-new so an existing file is never clobbered, and so the write is
    // atomic against a concurrent writer racing for the same name.
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
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // The name matched; that alone does not prove the *content* matches,
            // because the name is only a timestamp plus a 64-bit hash. Read the
            // existing bytes and decide: identical → the observation is already
            // recorded, a no-op; different → a hash collision that would silently
            // drop this observation, so refuse loudly.
            let existing = fs::read(&path).map_err(|source| Error::Io {
                path: path.display().to_string(),
                source,
            })?;
            if existing == json {
                Ok(path)
            } else {
                Err(Error::Io {
                    path: path.display().to_string(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "a different log entry already occupies this filename \
                         (timestamp + content-hash collision); refusing to drop \
                         the new observation",
                    ),
                })
            }
        }
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

    // Sort by the entry's own timestamp, tie-broken by filename for a total,
    // deterministic order. Sorting by the parsed `at` (not the filename bytes) is
    // what makes the order *data-derived*: a filename whose timestamp portion is
    // not perfectly fixed-width, or was written by an older tool, cannot reorder
    // history behind the reader's back and swallow a later verdict. The filename
    // tiebreak keeps two entries sharing an instant in a stable order.
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

    named.sort_by(|(a_name, a), (b_name, b)| a.at.cmp(&b.at).then_with(|| a_name.cmp(b_name)));
    Ok(named.into_iter().map(|(_, entry)| entry).collect())
}

/// Compute a claim's [`StatusReport`] from its history — the concrete form of
/// invariant #3, status derived and never stored.
///
/// A pure function of its inputs — `max_age`, the `history`, `now`, and the
/// `grace` window for broken/unverifiable streaks — with no hidden clock, so a
/// test can pin every timestamp and get a deterministic answer. The result does
/// **not** depend on the order `history` is passed in: a local copy is sorted by
/// each entry's own `at` (ties broken by preserving input order) before any rule
/// applies, so a caller that mis-sorts cannot flip the verdict.
///
/// The rules, applied in this order over the time-sorted history:
///
/// 1. If the most recent *past-or-present* adjudication is a
///    [`Adjudication::Retire`], the claim is [`Status::Retired`]. Terminal for
///    v1: a later `Held` does not revive it (retiring is deliberate; a stray
///    check re-running must not undo it). To reopen, author a new claim. A
///    *future-dated* retirement is ignored here — it must not calm a claim that
///    is presently alarming (see the honesty note below).
/// 2. Otherwise, consider the latest *conclusive* verdict: a [`Verdict::Held`]
///    at or before `now`, or a [`Verdict::Drifted`] at any time. (`Broken` and
///    `Unverifiable` are inconclusive — the check ran but could not answer — and
///    never win this step; they only affect the grace window in rule 3.) If that
///    latest conclusive verdict is `Drifted`, the claim is [`Status::Drifted`]:
///    its own check says the fact is no longer true, regardless of `max_age`.
/// 3. Otherwise, if the latest conclusive verdict is a `Held` (necessarily at or
///    before `now`) and `now` is within that `Held`'s *fresh window*, the claim
///    is [`Status::Verified`]. The boundary is inclusive: a claim verified
///    *exactly* at the window's end is still `Verified`; staleness begins the
///    instant after.
///
///    The window is normally `max_age`. But when the entries *after* that `Held`
///    are a `Broken`/`Unverifiable` streak — the check ran but could not confirm
///    — the window extends to `grace` from the `Held`. This is the one place
///    `grace` is load-bearing: transient breakage (a runner outage, a flaky
///    probe) should not nag on the first failure past `max_age`, but the streak
///    cannot mask indefinitely or a stuck-broken check becomes a permanent
///    false-fresh. `grace` (default 90 days) is meant to exceed a typical
///    `max_age`, so the window can only extend, never shrink; a fresh `Held`
///    collapses it back to `max_age`.
/// 4. Otherwise the claim is [`Status::Stale`]: never verified, aged past
///    `max_age` with no streak, or a streak run past `grace`. The end state is a
///    human being nagged, never a stale green light (invariant #6).
///
/// **Honesty under a bad clock (invariant #6).** A future-dated entry — clock
/// skew or forgery — can never make a claim *safer* than the same history
/// without it, nor turn `due` off. A future `Held` is excluded from freshness
/// (`last_verified`, `age`) entirely: a verification that has not yet happened
/// cannot certify present freshness. A future `Retire` is excluded from rule 1:
/// it must not calm a presently-alarming claim. Only a future `Drifted` is
/// honored, because it can only *raise* alarm.
///
/// Interactions worth noting: `Held → Drifted → Broken` is `Drifted` (the latest
/// *conclusive* verdict is the `Drifted`; the trailing `Broken` cannot mask it).
/// `Held → Broken` stays `Verified` within `max_age`, extends to `grace` while
/// the streak continues, then goes `Stale`. `Drifted → Held` is `Verified` — the
/// claim was re-verified and the drift is history.
///
/// `stale_at` reports the instant the fresh window ends (grace-extended when a
/// streak is active), or `None` when there is no finite deadline: retired, never
/// verified, or a window that would overflow the representable end of time.
#[must_use]
pub fn compute_status(
    max_age: Days,
    history: &[LogEntry],
    now: Timestamp,
    grace: Grace,
) -> StatusReport {
    // Order-independence (C1a): derive the verdict from the entries' own
    // timestamps, not the order the caller happened to pass. A stable sort keeps
    // input order as the tiebreak for a shared instant, matching `read_entries`.
    let mut sorted: Vec<&LogEntry> = history.iter().collect();
    sorted.sort_by_key(|e| e.at);

    // Freshness may only rest on a Held that has actually happened: a future Held
    // is a pending observation, not present certification.
    let last_verified = sorted.iter().rev().find_map(|e| match &e.event {
        Event::Verification {
            verdict: Verdict::Held,
            ..
        } if e.at <= now => Some(e.at),
        _ => None,
    });
    let age = last_verified.map(|at| now.duration_since(at));

    let (status, stale_at) = classify(max_age, &sorted, now, grace, last_verified);
    let due = !matches!(status, Status::Verified | Status::Retired);

    StatusReport {
        status,
        last_verified,
        age,
        stale_at,
        due,
    }
}

/// The status and its stale-at deadline, over an already time-sorted history.
///
/// Returns `stale_at` alongside the label so the two are computed from one pass
/// of reasoning and can never disagree.
fn classify(
    max_age: Days,
    sorted: &[&LogEntry],
    now: Timestamp,
    grace: Grace,
    last_verified: Option<Timestamp>,
) -> (Status, Option<Timestamp>) {
    // Rule 1: a past-or-present retirement is terminal. A future-dated retire is
    // ignored — honoring it could calm a presently-alarming claim, which
    // invariant #6 forbids.
    if let Some(latest_adjudication) = sorted.iter().rev().find_map(|e| match &e.event {
        Event::Adjudication { action } if e.at <= now => Some(action),
        _ => None,
    }) {
        match latest_adjudication {
            Adjudication::Retire { .. } => return (Status::Retired, None),
        }
    }

    // Rule 2: the latest *conclusive* verdict, by timestamp. A Held counts only
    // if it has happened (future Held excluded from certifying); a Drifted counts
    // at any time (it can only raise alarm). Broken/Unverifiable are inconclusive
    // and never win here, so a trailing broken streak cannot mask an earlier
    // Drifted (C3).
    let latest_conclusive = sorted.iter().rev().find_map(|e| match &e.event {
        Event::Verification {
            verdict: Verdict::Drifted,
            ..
        } => Some(Verdict::Drifted),
        Event::Verification {
            verdict: Verdict::Held,
            ..
        } if e.at <= now => Some(Verdict::Held),
        _ => None,
    });

    if latest_conclusive == Some(Verdict::Drifted) {
        return (Status::Drifted, None);
    }

    // Rule 3: the conclusive verdict is a Held (== last_verified). It is Verified
    // while `now` is within the fresh window; the window extends to grace when the
    // entries after that Held are an inconclusive streak.
    if let Some(held_at) = last_verified {
        debug_assert_eq!(latest_conclusive, Some(Verdict::Held));
        let streak_active = entries_after_are_inconclusive_streak(sorted, held_at);
        let window = if streak_active {
            days_duration(max_age).max(days_duration(grace.days()))
        } else {
            days_duration(max_age)
        };
        // Overflow (C2): a huge max_age/grace, or a late Held, can push the
        // deadline past Timestamp::MAX. An unrepresentable deadline is unreachably
        // far in the future, so the claim is within its window — Verified — and
        // there is no finite `stale_at` to report.
        match held_at.checked_add(window) {
            Ok(stale_at) => {
                if now <= stale_at {
                    return (Status::Verified, Some(stale_at));
                }
            }
            Err(_) => return (Status::Verified, None),
        }
    }

    // Rule 4: stale — never verified, or aged past the window. No finite deadline
    // to report when never verified; otherwise the deadline is already past and a
    // caller wanting "how overdue" recomputes it, so we leave `stale_at` None here
    // to mean "already stale" uniformly.
    (Status::Stale, None)
}

/// Whether every entry strictly after `held_at` is an inconclusive
/// (`Broken`/`Unverifiable`) verification — the condition that extends the fresh
/// window to `grace`.
///
/// `held_at` is the latest conclusive `Held`, so the entries after it contain no
/// `Held` and no `Drifted` (a later `Drifted` would have won rule 2). This
/// confirms there *is* such a trailing streak (at least one inconclusive entry),
/// and that nothing else — an adjudication that was not a retire, say — sits
/// among them. An empty tail means the `Held` is the last word: no extension.
fn entries_after_are_inconclusive_streak(sorted: &[&LogEntry], held_at: Timestamp) -> bool {
    let mut tail = sorted.iter().skip_while(|e| e.at <= held_at).peekable();
    if tail.peek().is_none() {
        return false;
    }
    tail.all(|e| {
        matches!(
            &e.event,
            Event::Verification {
                verdict: Verdict::Broken | Verdict::Unverifiable,
                ..
            }
        )
    })
}

/// The freshness window as a fixed duration.
///
/// `max_age`/`grace` are whole-day counts and a [`Timestamp`] is a UTC instant
/// with no zone or DST, so a day here is an unambiguous 24 hours. A fixed
/// duration (rather than a calendar span) is both correct for instants and what
/// keeps the boundary arithmetic in [`compute_status`] exact. The multiplication
/// is widened to `i64`, which `u32::MAX` days in hours cannot overflow; the
/// subsequent [`Timestamp::checked_add`] is where an out-of-range deadline is
/// caught.
fn days_duration(days: Days) -> SignedDuration {
    SignedDuration::from_hours(i64::from(days.get()) * 24)
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
///
/// The `debug_assert!`s are belt-and-suspenders for a trust product: if a future
/// change to [`ClaimId`] validation ever let a `..` or absolute segment through,
/// a debug build would trip here rather than silently write outside `log_root`.
fn claim_log_dir(log_root: &Path, id: &ClaimId) -> PathBuf {
    let mut dir = log_root.to_path_buf();
    for segment in id.as_str().split('/') {
        debug_assert!(
            !segment.is_empty() && segment != "." && segment != "..",
            "ClaimId must never yield an empty, '.', or '..' path segment: {segment:?}"
        );
        dir.push(segment);
    }
    debug_assert!(
        dir.starts_with(log_root),
        "a claim's log dir must stay under log_root: {}",
        dir.display()
    );
    dir
}

/// The filename for a log entry: a fixed-width time-sortable stamp, a content
/// hash, and `.json`.
///
/// The name must satisfy two constraints at once. It must sort chronologically as
/// a plain string, so a plain directory listing is already in order — met by a
/// **fixed-width** UTC stamp: `YYYY-MM-DDTHH-MM-SS.nnnnnnnnnZ`, with `:` rendered
/// as `-` (`:` is unsafe on some filesystems) and the subsecond part always nine
/// zero-padded digits. Fixed width is essential: `Timestamp::to_string()` omits
/// trailing zeros (`...00Z` vs `...00.5Z`), and because `.` (0x2E) sorts before
/// `Z` (0x5A) a whole-second entry would otherwise sort *after* a fractional
/// entry a moment later, scrambling the listing. And the name must be
/// collision-resistant without randomness (randomness would make tests
/// non-deterministic) — met by a hash *of the entry's serialized bytes*, so two
/// genuinely distinct entries at the same instant get distinct names while a
/// byte-identical re-record maps to the same name.
///
/// Read order does not *depend* on this being perfect — [`read_entries`] and
/// [`compute_status`] sort by the parsed `at`, not the filename — but a listing
/// that already reads chronologically is worth the fixed width.
///
/// Example: `2026-07-17T12-00-00.000000000Z-a1b2c3d4e5f60718.json`.
fn entry_filename(entry: &LogEntry, json: &[u8]) -> String {
    format!(
        "{}-{:016x}.json",
        fixed_width_stamp(entry.at),
        fnv1a64(json)
    )
}

/// A fixed-width, filesystem-safe, chronologically-sortable rendering of an
/// instant: `YYYY-MM-DDTHH-MM-SS.nnnnnnnnnZ`.
///
/// Built from the RFC 3339 string rather than reformatting from components, so it
/// stays anchored to jiff's canonical UTC output; only the variable-width
/// fractional tail is normalized to a constant nine digits and `:` is swapped for
/// `-`.
fn fixed_width_stamp(at: Timestamp) -> String {
    let rfc = at.to_string();
    // `to_string()` is `<date>T<time>[.frac]Z`. Split off the trailing `Z` and any
    // fractional part, keep the whole-second head, then re-attach a fixed 9-digit
    // nanosecond field so every stamp is the same length.
    let head = rfc
        .strip_suffix('Z')
        .unwrap_or(&rfc)
        .split('.')
        .next()
        .unwrap_or(&rfc);
    let nanos = at.subsec_nanosecond();
    format!("{}.{:09}Z", head.replace(':', "-"), nanos)
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

    fn grace(n: u32) -> Grace {
        Grace(days(n))
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
        let tmp = TempDir::new();
        let claim = id("libfoo-pin");
        let entry = verify("2026-07-17T12:00:00Z", Verdict::Held);
        append_entry(tmp.path(), &claim, &entry).unwrap();

        let read = read_entries(tmp.path(), &claim).unwrap();
        assert_eq!(read, vec![entry]);
    }

    #[test]
    fn read_returns_entries_in_chronological_order() {
        let tmp = TempDir::new();
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
    fn read_orders_whole_and_fractional_second_entries_chronologically() {
        // C1: `Timestamp::to_string()` drops trailing zeros, so a whole-second
        // entry and a fractional entry a moment later have unequal-width stamps;
        // sorting on the parsed `at` (not filename bytes) keeps them in true
        // order. Without the fix, the whole-second Held would sort after the
        // fractional Drifted and `compute_status` would read Verified, swallowing
        // the drift.
        let tmp = TempDir::new();
        let claim = id("c");
        let held = verify("2026-07-17T12:00:00Z", Verdict::Held);
        let drifted = verify("2026-07-17T12:00:00.5Z", Verdict::Drifted);
        append_entry(tmp.path(), &claim, &drifted).unwrap();
        append_entry(tmp.path(), &claim, &held).unwrap();

        let read = read_entries(tmp.path(), &claim).unwrap();
        assert_eq!(read, vec![held, drifted]);
        let report = compute_status(days(30), &read, ts("2026-07-17T13:00:00Z"), grace(90));
        assert_eq!(
            report.status,
            Status::Drifted,
            "the later fractional Drifted must win, not be reordered away"
        );
    }

    #[test]
    fn compute_status_is_independent_of_input_order() {
        // C1a: the verdict derives from the entries' own timestamps, so passing
        // the same history in any order yields the same status. A caller that
        // mis-sorts cannot flip Drifted to Verified.
        let held = verify("2026-07-01T12:00:00Z", Verdict::Held);
        let drifted = verify("2026-07-10T12:00:00Z", Verdict::Drifted);
        let in_order = [held.clone(), drifted.clone()];
        let reversed = [drifted, held];
        let now = ts("2026-07-17T12:00:00Z");
        assert_eq!(
            compute_status(days(30), &in_order, now, grace(90)),
            compute_status(days(30), &reversed, now, grace(90)),
        );
    }

    #[test]
    fn two_entries_at_the_same_timestamp_coexist() {
        // One-file-per-entry means a shared instant is not a collision: distinct
        // content yields distinct filenames, and both survive.
        let tmp = TempDir::new();
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
        let tmp = TempDir::new();
        let claim = id("c");
        let entry = verify("2026-07-17T12:00:00Z", Verdict::Held);
        let first = append_entry(tmp.path(), &claim, &entry).unwrap();
        let second = append_entry(tmp.path(), &claim, &entry).unwrap();
        assert_eq!(first, second);
        assert_eq!(read_entries(tmp.path(), &claim).unwrap().len(), 1);
    }

    #[test]
    fn same_name_different_content_is_a_loud_error() {
        // M6: an AlreadyExists whose bytes differ from the new entry is a hash
        // collision that would silently drop this observation — possibly a
        // Drifted, leaving an older Held as the record. It must be loud, not a
        // no-op. Simulate the collision by pre-writing a different payload at the
        // exact filename the new entry will target.
        let tmp = TempDir::new();
        let claim = id("c");
        let entry = verify("2026-07-17T12:00:00Z", Verdict::Held);
        let json = serde_json::to_vec_pretty(&entry).unwrap();
        let dir = tmp.path().join("c");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(entry_filename(&entry, &json));
        fs::write(&path, b"{\"different\":\"content\"}").unwrap();

        let err = append_entry(tmp.path(), &claim, &entry).unwrap_err();
        match &err {
            Error::Io { path: p, .. } => assert!(p.contains(".json"), "path: {p}"),
            other => panic!("expected a loud I/O error on collision, got {other:?}"),
        }
    }

    #[test]
    fn append_rejects_empty_commit_or_actor() {
        // m4: a verdict with no traceable commit or actor has no provenance.
        let tmp = TempDir::new();
        let claim = id("c");
        let mut no_commit = verify("2026-07-17T12:00:00Z", Verdict::Held);
        no_commit.commit = "   ".to_owned();
        let err = append_entry(tmp.path(), &claim, &no_commit).unwrap_err();
        assert!(matches!(&err, Error::Parse { reason, .. } if reason.contains("commit")));

        let mut no_actor = verify("2026-07-17T12:00:00Z", Verdict::Held);
        no_actor.actor = String::new();
        let err = append_entry(tmp.path(), &claim, &no_actor).unwrap_err();
        assert!(matches!(&err, Error::Parse { reason, .. } if reason.contains("actor")));

        // Nothing was written for either rejected entry.
        assert!(read_entries(tmp.path(), &claim).unwrap().is_empty());
    }

    #[test]
    fn namespaced_id_maps_to_nested_path_and_round_trips() {
        let tmp = TempDir::new();
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
        let tmp = TempDir::new();
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
    fn read_does_not_pick_up_a_nested_namespace() {
        // m6: reading `a` must not sweep in entries under `a/b`. The `.json`
        // extension filter plus per-claim directories keep namespaces isolated;
        // this pins that so a future change to the listing can't merge histories.
        let tmp = TempDir::new();
        let parent = id("a");
        let child = id("a/b");
        append_entry(
            tmp.path(),
            &parent,
            &verify("2026-07-17T12:00:00Z", Verdict::Held),
        )
        .unwrap();
        append_entry(
            tmp.path(),
            &child,
            &verify("2026-07-17T13:00:00Z", Verdict::Held),
        )
        .unwrap();

        assert_eq!(read_entries(tmp.path(), &parent).unwrap().len(), 1);
        assert_eq!(read_entries(tmp.path(), &child).unwrap().len(), 1);
        assert_eq!(
            read_entries(tmp.path(), &parent).unwrap()[0].at,
            ts("2026-07-17T12:00:00Z")
        );
    }

    #[test]
    fn hostile_ids_are_rejected_before_they_reach_the_path() {
        // m3: path-traversal ids must never validate. `claim_log_dir`'s
        // debug_assert is the second line of defense; `ClaimId` is the first.
        for bad in ["../x", "a/../b", "/abs", "..", "a/..", "./x"] {
            assert!(
                ClaimId::from_str(bad).is_err(),
                "id {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn missing_log_dir_reads_as_empty() {
        let tmp = TempDir::new();
        let read = read_entries(tmp.path(), &id("never-logged")).unwrap();
        assert!(read.is_empty());
    }

    #[test]
    fn malformed_entry_file_errors_and_names_the_file() {
        // Invariant #6: a bad entry is loud, never silently dropped. Dropping it
        // could hide a Drifted verdict and read the claim as fresh.
        let tmp = TempDir::new();
        let claim = id("c");
        let dir = tmp.path().join("c");
        fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("2026-07-17T12-00-00.000000000Z-deadbeefdeadbeef.json");
        fs::write(&bad, b"{ this is not valid json").unwrap();

        let err = read_entries(tmp.path(), &claim).unwrap_err();
        match &err {
            Error::Parse { path, reason } => {
                assert!(path.contains("2026-07-17T12-00-00"), "path: {path}");
                assert!(reason.contains("malformed"), "reason: {reason}");
            }
            other => panic!("expected a parse error naming the file, got {other:?}"),
        }
    }

    #[test]
    fn non_json_files_in_log_dir_are_ignored() {
        let tmp = TempDir::new();
        let claim = id("c");
        let entry = verify("2026-07-17T12:00:00Z", Verdict::Held);
        append_entry(tmp.path(), &claim, &entry).unwrap();
        // A stray file a git directory might accumulate must not break reads.
        fs::write(tmp.path().join("c").join(".gitkeep"), b"").unwrap();

        assert_eq!(read_entries(tmp.path(), &claim).unwrap(), vec![entry]);
    }

    // --- Filenames and the hash. ---

    #[test]
    fn filename_is_fixed_width_sortable_and_colon_free() {
        let whole = verify("2026-07-17T12:00:00Z", Verdict::Held);
        let frac = verify("2026-07-17T12:00:00.5Z", Verdict::Held);
        let name_whole = fixed_width_stamp(whole.at);
        let name_frac = fixed_width_stamp(frac.at);
        assert!(!name_whole.contains(':'), "colon-free: {name_whole}");
        assert_eq!(
            name_whole.len(),
            name_frac.len(),
            "stamps must be fixed width: {name_whole} vs {name_frac}"
        );
        assert!(
            name_whole < name_frac,
            "the earlier instant must sort first: {name_whole} vs {name_frac}"
        );
        assert_eq!(name_whole, "2026-07-17T12-00-00.000000000Z");
        assert_eq!(name_frac, "2026-07-17T12-00-00.500000000Z");
    }

    #[test]
    fn entry_filename_is_pinned() {
        // M5: pin a full filename so a change to the stamp format or the hash is a
        // visible test failure, not a silent rename of every log file.
        let entry = LogEntry {
            at: ts("2026-07-17T12:00:00Z"),
            commit: "deadbeef".to_owned(),
            actor: "ci".to_owned(),
            event: Event::Verification {
                verdict: Verdict::Held,
                evidence: None,
            },
        };
        let json = serde_json::to_vec_pretty(&entry).unwrap();
        let name = entry_filename(&entry, &json);
        assert_eq!(name, "2026-07-17T12-00-00.000000000Z-fe9ee47967c600f3.json");
    }

    #[test]
    fn fnv1a64_matches_known_answers() {
        // M5: pin the hand-rolled FNV-1a against the canonical test vectors, so a
        // future edit (e.g. to FNV-1) is caught before it silently renames files.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    // --- Status computation. ---
    //
    // Status tests assert the full `StatusReport` on the branches that matter, so
    // `due`/`age`/`last_verified`/`stale_at` cannot silently regress (m1).

    #[test]
    fn empty_history_is_stale_and_due() {
        let report = compute_status(days(30), &[], ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(
            report,
            StatusReport {
                status: Status::Stale,
                last_verified: None,
                age: None,
                stale_at: None,
                due: true,
            }
        );
    }

    #[test]
    fn single_held_within_max_age_is_verified() {
        let history = [verify("2026-07-01T12:00:00Z", Verdict::Held)];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(
            report,
            StatusReport {
                status: Status::Verified,
                last_verified: Some(ts("2026-07-01T12:00:00Z")),
                age: Some(SignedDuration::from_hours(16 * 24)),
                // No streak, so the window is max_age: Jul 1 + 30d = Jul 31.
                stale_at: Some(ts("2026-07-31T12:00:00Z")),
                due: false,
            }
        );
    }

    #[test]
    fn held_exactly_at_max_age_boundary_is_verified() {
        // Documented decision: the boundary is inclusive. A claim verified exactly
        // max_age ago is still within the window; staleness begins the instant
        // after.
        let history = [verify("2026-06-17T12:00:00Z", Verdict::Held)];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(report.status, Status::Verified);
        assert_eq!(report.stale_at, Some(ts("2026-07-17T12:00:00Z")));
        assert!(!report.due);
    }

    #[test]
    fn held_one_second_past_max_age_is_stale() {
        let history = [verify("2026-06-17T12:00:00Z", Verdict::Held)];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:01Z"), grace(90));
        assert_eq!(
            report,
            StatusReport {
                status: Status::Stale,
                // last_verified is still reported when stale — the CLI shows "last
                // verified <date>, now overdue".
                last_verified: Some(ts("2026-06-17T12:00:00Z")),
                age: Some(SignedDuration::from_hours(30 * 24) + SignedDuration::from_secs(1)),
                stale_at: None,
                due: true,
            }
        );
    }

    #[test]
    fn held_well_past_max_age_is_stale() {
        let history = [verify("2026-01-01T12:00:00Z", Verdict::Held)];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(report.status, Status::Stale);
        assert!(report.due);
        assert_eq!(report.last_verified, Some(ts("2026-01-01T12:00:00Z")));
    }

    #[test]
    fn latest_drifted_is_drifted() {
        let history = [
            verify("2026-07-01T12:00:00Z", Verdict::Held),
            verify("2026-07-10T12:00:00Z", Verdict::Drifted),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(
            report,
            StatusReport {
                status: Status::Drifted,
                // The last Held is still recorded even though the claim drifted.
                last_verified: Some(ts("2026-07-01T12:00:00Z")),
                age: Some(SignedDuration::from_hours(16 * 24)),
                stale_at: None,
                due: true,
            }
        );
    }

    #[test]
    fn drifted_then_later_held_is_verified() {
        // Re-verification clears drift: the fact was fixed, the drift is history.
        let history = [
            verify("2026-07-01T12:00:00Z", Verdict::Drifted),
            verify("2026-07-10T12:00:00Z", Verdict::Held),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(report.status, Status::Verified);
        assert_eq!(report.last_verified, Some(ts("2026-07-10T12:00:00Z")));
        assert_eq!(report.stale_at, Some(ts("2026-08-09T12:00:00Z")));
        assert!(!report.due);
    }

    #[test]
    fn held_then_drifted_then_broken_is_drifted() {
        // C3: a trailing Broken must not mask an earlier Drifted. The latest
        // *conclusive* verdict is the Drifted, so the claim is Drifted, not
        // Verified via a grace window measured from the pre-drift Held.
        let history = [
            verify("2026-07-01T12:00:00Z", Verdict::Held),
            verify("2026-07-10T12:00:00Z", Verdict::Drifted),
            verify("2026-07-12T12:00:00Z", Verdict::Broken),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(report.status, Status::Drifted);
        assert!(report.due);
    }

    #[test]
    fn held_then_drifted_then_unverifiable_is_drifted() {
        // C3, with Unverifiable in the trailing position.
        let history = [
            verify("2026-07-01T12:00:00Z", Verdict::Held),
            verify("2026-07-10T12:00:00Z", Verdict::Drifted),
            verify("2026-07-12T12:00:00Z", Verdict::Unverifiable),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(report.status, Status::Drifted);
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
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(report.status, Status::Verified);
        // A streak is active, so the window is the grace-extended one (90d from
        // the Held), not max_age.
        assert_eq!(report.stale_at, Some(ts("2026-09-29T12:00:00Z")));
        assert!(!report.due);
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
        let report = compute_status(days(30), &history, ts("2026-03-15T12:00:00Z"), grace(90));
        assert_eq!(report.status, Status::Verified);
        assert_eq!(report.stale_at, Some(ts("2026-04-01T12:00:00Z")));
        assert!(!report.due);
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
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(report.status, Status::Stale);
        assert!(report.due);
    }

    #[test]
    fn broken_streak_at_grace_boundary_is_verified() {
        // Grace, like max_age, is inclusive of its final instant. Held Jan 1,
        // grace 90d → Apr 1 12:00:00 is still fresh; one second later is stale.
        let history = [
            verify("2026-01-01T12:00:00Z", Verdict::Held),
            verify("2026-02-01T12:00:00Z", Verdict::Broken),
        ];
        let at_boundary = compute_status(days(30), &history, ts("2026-04-01T12:00:00Z"), grace(90));
        assert_eq!(at_boundary.status, Status::Verified);
        assert_eq!(at_boundary.stale_at, Some(ts("2026-04-01T12:00:00Z")));
        let past = compute_status(days(30), &history, ts("2026-04-01T12:00:01Z"), grace(90));
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
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(report.status, Status::Stale);
        assert!(report.due);
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
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(report.status, Status::Stale);
        assert_eq!(report.last_verified, None);
        assert_eq!(report.age, None);
        assert!(report.due);
    }

    #[test]
    fn broken_only_history_is_stale() {
        let history = [verify("2026-07-16T12:00:00Z", Verdict::Broken)];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(report.status, Status::Stale);
        assert_eq!(report.last_verified, None);
        assert!(report.due);
    }

    #[test]
    fn retirement_is_terminal_even_with_a_later_held() {
        // Documented decision: retirement is terminal for v1. A later Held does
        // not revive a retired claim. m2: last_verified may still reflect the
        // pre-retirement Held (history is history); due is false because terminal.
        let history = [
            verify("2026-07-01T12:00:00Z", Verdict::Held),
            retire("2026-07-05T12:00:00Z", "superseded by CI gate"),
            verify("2026-07-10T12:00:00Z", Verdict::Held),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(
            report,
            StatusReport {
                status: Status::Retired,
                last_verified: Some(ts("2026-07-10T12:00:00Z")),
                age: Some(SignedDuration::from_hours(7 * 24)),
                stale_at: None,
                due: false,
            }
        );
    }

    #[test]
    fn retirement_after_drift_is_retired() {
        let history = [
            verify("2026-07-01T12:00:00Z", Verdict::Drifted),
            retire("2026-07-05T12:00:00Z", "decision reversed"),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(report.status, Status::Retired);
        assert!(!report.due);
    }

    #[test]
    fn grace_does_not_apply_without_a_broken_streak() {
        // A large grace must not extend a plain Held past its max_age: grace only
        // buys time for a Broken/Unverifiable streak, never for an ordinary
        // aged-out verification. Held Jan 1, max_age 30d → stale by Feb 1
        // regardless of a 90d grace, because the Held is the last word.
        let history = [verify("2026-01-01T12:00:00Z", Verdict::Held)];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
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
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(report.status, Status::Verified);
        assert_eq!(report.last_verified, Some(ts("2026-07-16T12:00:00Z")));
        // Window is max_age from the fresh Held (no streak after it), not grace.
        assert_eq!(report.stale_at, Some(ts("2026-08-15T12:00:00Z")));
    }

    #[test]
    fn most_recent_held_wins_when_several_exist() {
        // last_verified must be the newest Held, not the first.
        let history = [
            verify("2026-07-01T12:00:00Z", Verdict::Held),
            verify("2026-07-15T12:00:00Z", Verdict::Held),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(report.last_verified, Some(ts("2026-07-15T12:00:00Z")));
        assert_eq!(report.status, Status::Verified);
    }

    // --- Future-dated entries (C4): never safer than reality. ---

    #[test]
    fn future_held_alone_is_stale_and_due() {
        // C4: a Held timestamped after `now` cannot certify present freshness. It
        // does not set last_verified/age, and the claim is stale and due.
        let history = [verify("2026-08-01T12:00:00Z", Verdict::Held)];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(
            report,
            StatusReport {
                status: Status::Stale,
                last_verified: None,
                age: None,
                stale_at: None,
                due: true,
            }
        );
    }

    #[test]
    fn future_held_does_not_supersede_a_past_held() {
        // Freshness rests on the most recent *past* Held; a future Held is ignored
        // for last_verified/age, so it cannot make the claim look fresher.
        let history = [
            verify("2026-07-10T12:00:00Z", Verdict::Held),
            verify("2026-08-01T12:00:00Z", Verdict::Held),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(report.status, Status::Verified);
        assert_eq!(report.last_verified, Some(ts("2026-07-10T12:00:00Z")));
        assert_eq!(report.age, Some(SignedDuration::from_hours(7 * 24)));
    }

    #[test]
    fn future_held_never_makes_a_claim_safer_than_reality() {
        // The guarantee behind C4: adding a future-dated entry can never yield a
        // less-alarming status or turn `due` off. A stale claim with a future Held
        // appended stays stale and due.
        let base = [verify("2026-01-01T12:00:00Z", Verdict::Held)];
        let with_future = [
            verify("2026-01-01T12:00:00Z", Verdict::Held),
            verify("2027-01-01T12:00:00Z", Verdict::Held),
        ];
        let now = ts("2026-07-17T12:00:00Z");
        let base_report = compute_status(days(30), &base, now, grace(90));
        let future_report = compute_status(days(30), &with_future, now, grace(90));
        assert_eq!(base_report.status, Status::Stale);
        assert_eq!(future_report.status, Status::Stale);
        assert!(future_report.due);
    }

    #[test]
    fn future_retire_does_not_calm_a_present_claim() {
        // A future-dated retirement must not turn a due claim into a terminal,
        // not-due Retired — that would be strictly safer than reality, which
        // invariant #6 forbids. It stays stale and due until the retirement
        // actually happens.
        let history = [
            verify("2026-01-01T12:00:00Z", Verdict::Held),
            retire("2027-01-01T12:00:00Z", "future close"),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(report.status, Status::Stale);
        assert!(report.due);
    }

    #[test]
    fn future_drifted_is_honored_because_it_only_raises_alarm() {
        // A future Drifted may drive the claim to Drifted: honoring it can only
        // increase alarm, never decrease it, so it is safe under invariant #6.
        let history = [
            verify("2026-07-01T12:00:00Z", Verdict::Held),
            verify("2027-01-01T12:00:00Z", Verdict::Drifted),
        ];
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(report.status, Status::Drifted);
        assert!(report.due);
    }

    // --- Overflow safety (C2): never panic. ---

    #[test]
    fn huge_max_age_does_not_panic_and_is_verified() {
        // C2: Days accepts up to u32::MAX; the resulting window overflows
        // Timestamp::MAX. That is an unreachably distant deadline, so the claim is
        // within its window (Verified) with no finite stale_at — and, above all,
        // no panic.
        let history = [verify("2026-07-01T12:00:00Z", Verdict::Held)];
        let max_days = Days::from_nonzero(NonZeroU32::new(u32::MAX).unwrap());
        let report = compute_status(max_days, &history, ts("2026-07-17T12:00:00Z"), grace(90));
        assert_eq!(report.status, Status::Verified);
        assert_eq!(report.stale_at, None);
        assert!(!report.due);
    }

    #[test]
    fn huge_grace_on_a_streak_does_not_panic() {
        // C2 via the grace path: a broken streak with a u32::MAX grace overflows
        // the window; still Verified, no finite stale_at, no panic.
        let history = [
            verify("2026-07-01T12:00:00Z", Verdict::Held),
            verify("2026-07-10T12:00:00Z", Verdict::Broken),
        ];
        let huge = Grace(Days::from_nonzero(NonZeroU32::new(u32::MAX).unwrap()));
        let report = compute_status(days(30), &history, ts("2026-07-17T12:00:00Z"), huge);
        assert_eq!(report.status, Status::Verified);
        assert_eq!(report.stale_at, None);
    }

    #[test]
    fn held_near_timestamp_max_does_not_panic() {
        // C2: a valid modest max_age but a Held so late that Held + window exceeds
        // Timestamp::MAX. checked_add catches it; no panic, Verified, no finite
        // deadline.
        let history = [LogEntry {
            at: Timestamp::MAX,
            commit: "c".to_owned(),
            actor: "ci".to_owned(),
            event: Event::Verification {
                verdict: Verdict::Held,
                evidence: None,
            },
        }];
        let report = compute_status(days(30), &history, Timestamp::MAX, grace(90));
        assert_eq!(report.status, Status::Verified);
        assert_eq!(report.stale_at, None);
    }

    // --- Grace as a usable constant, and the newtype guard (M8). ---

    #[test]
    fn grace_default_is_ninety_days_and_usable_as_a_constant() {
        assert_eq!(Grace::DEFAULT.days().get(), 90);
        // The whole point of M8: this compiles and runs without parsing a string.
        let history = [
            verify("2026-01-01T12:00:00Z", Verdict::Held),
            verify("2026-02-01T12:00:00Z", Verdict::Broken),
        ];
        let report = compute_status(
            days(30),
            &history,
            ts("2026-03-15T12:00:00Z"),
            Grace::DEFAULT,
        );
        assert_eq!(report.status, Status::Verified);
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
