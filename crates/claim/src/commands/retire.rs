//! `claim retire <id> --note`: close a claim on purpose by removing its file.
//!
//! Retirement is a deliberate lifecycle decision — the world changed and the
//! decision the claim rested on was re-reviewed, or the fact became a real test and
//! the note says where it now lives. It removes the claim's definition file from the
//! working tree (a `git rm`, once the user commits): the claim ceases to exist, and
//! there is nothing left to check. There is no stored retirement event — a verdict
//! and its lifecycle are telemetry a hub tracks, not source (see
//! `docs/design/CLI-HUB-BOUNDARY.md`) — and the changelog *is* git history: `git log
//! .claims/` shows exactly when a claim was added and when it was retired, and the
//! reason rides in the removal commit's message.
//!
//! Retiring runs no check — a retired claim is closed regardless of whether its fact
//! still holds — so it is allowed on any claim, drifted or not.
//!
//! Like every write in this tool (invariant #4), `retire` changes the working tree
//! and stops; the user commits. The output names exactly what to `git rm`/commit,
//! with the note as the commit message so the changelog records *why*.

use anyhow::{Context, Result};
use serde::Serialize;

use crate::apperror::{app, ErrorKind};
use crate::cli::RetireArgs;
use crate::output::{emit, relative_to, Format};
use claim_store::{discover, Store};

/// The machine form of `claim retire`.
#[derive(Debug, Serialize)]
struct RetireReport {
    /// Always `"ok"` on success.
    status: &'static str,
    /// The retired claim's id.
    id: String,
    /// The store root (repository root), so an agent invoked from a subdirectory can
    /// resolve `file` and the commit command, which are root-relative.
    root: String,
    /// The removed claim file, relative to `root`. The caller stages the removal and
    /// commits it (invariant #4: the tool does not commit).
    file: String,
    /// The closing note, echoed back — it becomes the commit message so the
    /// changelog (`git log .claims/`) records why.
    note: String,
}

/// Run `claim retire`.
///
/// # Errors
///
/// Fails, with a message naming the fix, on: no store found; a trimmed-empty
/// `--note` (a reasonless retirement is the silent closure the note exists to
/// prevent — clap requires the flag present but not that it carries text); an
/// unknown id ([`ErrorKind::InvalidInput`], never a silent success — retiring a typo
/// must not look like closing a real claim); or an I/O failure removing the file.
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

    // Remove the definition file from the working tree. The claim ceases to exist;
    // there is no verdict or event to write. The user's commit is the tracked
    // removal (a `git rm`), and its message carries the note.
    let claim_file = store.claim_file(&claim.id);
    std::fs::remove_file(&claim_file)
        .with_context(|| format!("failed to remove the claim file {}", claim_file.display()))?;

    let report = RetireReport {
        status: "ok",
        id: claim.id.to_string(),
        root: store.root().display().to_string(),
        file: relative_to(store.root(), &claim_file),
        note: note.to_owned(),
    };

    emit(format, &report, || human(&report))
}

/// Resolve the requested id to a claim that actually exists in the store.
///
/// An unknown id is a loud error naming the id, never a silent no-op: retiring a
/// claim that does not exist would do nothing and look like success. A file that
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
/// relative to `.claims/`, equals the id. So an unparseable file named after the
/// requested id reports *that* file's error rather than "not found".
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
        "Removed {} from the working tree; the changelog is git history (`git log .claims/`).",
        report.file
    );
    println!("\nNothing is committed yet. Review, then commit:");
    println!("  git -C {} rm {}", report.root, report.file);
    println!(
        "  git -C {} commit -m \"Retire claim {}: {}\"",
        report.root, report.id, report.note
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_stem_matches_id_maps_a_claim_path_to_its_id() {
        assert!(file_stem_matches_id(
            ".claims/payments/pin.md",
            "payments/pin"
        ));
        assert!(!file_stem_matches_id(".claims/payments/pin.md", "other"));
        assert!(!file_stem_matches_id("elsewhere/pin.md", "pin"));
    }
}
