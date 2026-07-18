//! `claim retire <id> --note`: close a claim on purpose.
//!
//! Retirement is a deliberate lifecycle decision — the world changed and the
//! decision the claim rested on was re-reviewed, or the fact became a real test and
//! the note says where it now lives. It appends a single [`Adjudication::Retire`]
//! entry to the claim's verdict log; [`claim_core::compute_status`] already treats
//! the latest past-or-present adjudication as terminal, so the claim's *computed*
//! status becomes [`claim_core::Status::Retired`] with no field ever written into
//! the file (invariant #3, status is derived).
//!
//! The claim file itself stays on disk untouched: history is preserved, and `claim
//! log <id>` still shows the whole story ending in the retirement. Retiring runs no
//! check — a retired claim is closed regardless of whether its fact still holds —
//! so it is allowed on any claim, drifted or not, and needs no witnessed state.
//!
//! Like every write in this tool (invariant #4), `retire` writes the log entry to
//! the working tree and stops; the user commits. The output names exactly what to
//! `git add`.

use anyhow::{Context, Result};
use claim_core::{append_entry, Adjudication, Event, LogEntry};
use serde::Serialize;

use crate::apperror::{app, ErrorKind};
use crate::cli::RetireArgs;
use crate::git;
use crate::output::{emit, relative_to, Format};
use crate::store::{discover, Store};

/// The machine form of `claim retire`.
#[derive(Debug, Serialize)]
struct RetireReport {
    /// Always `"ok"` on success.
    status: &'static str,
    /// The retired claim's id.
    id: String,
    /// The store root (repository root), so an agent invoked from a subdirectory can
    /// resolve `to_commit`, which is root-relative.
    root: String,
    /// The full 40-char commit sha the retirement was recorded against (the
    /// unborn-HEAD sentinel when the repo has no commit yet). Full, not abbreviated,
    /// so the recorded provenance does not vary with `core.abbrev`.
    commit: String,
    /// The actor the retirement was attributed to.
    actor: String,
    /// The closing note, echoed back.
    note: String,
    /// The single log file the caller must `git add` and commit, relative to `root`
    /// (invariant #4: the tool does not commit).
    to_commit: Vec<String>,
}

/// Run `claim retire`.
///
/// # Errors
///
/// Fails, with a message naming the fix, on: no store found; a trimmed-empty
/// `--note` (a reasonless retirement is the silent closure the note exists to
/// prevent — clap requires the flag present but not that it carries text); an
/// unknown id ([`ErrorKind::InvalidInput`], never a silent success — retiring a typo
/// must not look like closing a real claim); a git provenance failure (no
/// repository, or no configured identity); or an I/O failure appending the log
/// entry.
pub fn run(args: &RetireArgs, format: Format) -> Result<()> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let store = discover(&cwd)?;

    // A present-but-blank note defeats the invariant the note enforces, so reject it
    // as loudly as a missing one — mirroring the parser's empty-statement rejection.
    let note = args.note.trim();
    if note.is_empty() {
        return Err(app(
            ErrorKind::InvalidInput,
            "the retirement note is empty; --note must say why the claim is being closed \
             (the world changed and the decision was re-reviewed, or where the fact now lives).",
        ));
    }

    let claim = resolve_claim(&store, &args.id)?;

    // Provenance is git-derived (invariant #3), resolved before the write so a
    // missing repository or identity fails loudly rather than producing an
    // unattributable entry (`append_entry` rejects an empty commit/actor).
    let commit = git::resolve_commit(store.root())?;
    let actor = git::resolve_actor(store.root())?;

    // Stamp the entry through the clock seam (as `check` does), so the recorded
    // instant is governed by the same `now` a read verb uses — deterministic under
    // test, wall clock in a shipped binary.
    let entry = LogEntry {
        at: crate::clock::now()?,
        commit: commit.clone(),
        actor: actor.clone(),
        event: Event::Adjudication {
            action: Adjudication::Retire {
                note: note.to_owned(),
            },
        },
    };
    let path = append_entry(&store.log_dir(), &claim.id, &entry)
        .context("failed to record the retirement in the verdict log")?;

    let to_commit = vec![relative_to(store.root(), &path)];
    let report = RetireReport {
        status: "ok",
        id: claim.id.to_string(),
        root: store.root().display().to_string(),
        commit,
        actor,
        note: note.to_owned(),
        to_commit,
    };

    emit(format, &report, || human(&report))
}

/// Resolve the requested id to a claim that actually exists in the store.
///
/// An unknown id is a loud error naming the id, never a silent no-op: retiring a
/// claim that does not exist would either do nothing (and look like success) or
/// write a stray log directory for a phantom claim. As in `claim log`, a file that
/// *is* the requested id but failed to parse reports *that* file's error rather than
/// "not found", so a typo and a broken file are distinguishable.
fn resolve_claim(store: &Store, id: &str) -> Result<claim_core::Claim> {
    let load = store.load_all()?;
    if let Some(loaded) = load.claims.iter().find(|c| c.claim.id.as_str() == id) {
        return Ok(loaded.claim.clone());
    }
    if let Some(err) = load
        .errors
        .iter()
        .find(|e| file_stem_matches_id(&e.file, id))
    {
        return Err(app(
            ErrorKind::InvalidInput,
            format!("claim '{id}' could not be loaded: {}", err.message),
        ));
    }
    Err(app(
        ErrorKind::InvalidInput,
        format!(
            "no claim with id '{id}' in this store; run `claim list` to see the ids that exist"
        ),
    ))
}

/// Whether a load-errored file's path could be the file for `id`: its `.md` stem,
/// relative to `.claims/`, equals the id. Mirrors `claim log`'s best-effort match so
/// an unparseable file named after the requested id reports *that* file's error.
fn file_stem_matches_id(file: &str, id: &str) -> bool {
    file.strip_prefix(".claims/")
        .and_then(|rest| rest.strip_suffix(".md"))
        .is_some_and(|stem| stem == id)
}

/// Confirm the retirement and tell the user exactly what to commit.
fn human(report: &RetireReport) {
    println!("Retired claim '{}'.", report.id);
    println!("  note: {}", report.note);
    println!(
        "Its status is now `retired` (computed from this log entry); the claim file and its \
         history are kept."
    );
    println!(
        "Recorded the retirement at commit {}.",
        git::short_commit(&report.commit)
    );
    println!("\nNothing is committed yet. Review, then commit:");
    println!(
        "  git -C {} add {}",
        report.root,
        report.to_commit.join(" ")
    );
    println!(
        "  git -C {} commit -m \"Retire claim {}\"",
        report.root, report.id
    );
}
