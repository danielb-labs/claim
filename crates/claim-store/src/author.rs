//! The claim-authoring core: establish a claim's check against reality, then write
//! the claim file — the non-interactive engine behind the CLI's `claim add`.
//!
//! The core lives here, once, over [`crate::Store`] and [`crate::git`], so authoring
//! is one gate rather than logic a caller could reimplement and get subtly wrong —
//! exactly the drift this tool exists to prevent. Given a validated [`Claim`] and the
//! exact bytes to write, it resolves git provenance, refuses a duplicate id, runs the
//! establishing check requiring [`Verdict::Held`], and — only then — writes the claim
//! file to the working tree. It never commits (invariant #4, the truth is the claim
//! and a write to it is a commit the caller makes) and never touches the tree before
//! the check passes.
//!
//! # No establishing verdict is written
//!
//! `add` remains a birth *gate*, not a birth *certificate*. The check must hold
//! now, but nothing is persisted about that pass: a verdict is telemetry, not
//! source (see `docs/design/CLI-HUB-BOUNDARY.md`). A false claim is caught by the
//! next check the hub or a CI lane runs, so a stored receipt is unnecessary — and
//! committing one would put telemetry in git, the mistake v2 removes. Provenance is
//! still resolved up front so a claim authored with no git identity fails loudly
//! before the tree is touched.
//!
//! What stays with the caller is the surface, not the substance: the CLI keeps its
//! interactive prompting, its optional `--witness-cmd` confidence dance, its `--json`
//! shape, and its unresolved-`supports` warning. It hands this function the same three
//! things — a parsed claim, its file text, and a [`CheckContext`] — and gets back an
//! [`Authored`] outcome or a typed [`AuthorError`].
//!
//! # The honesty gate is here, not in a caller
//!
//! The establishing run is the whole of verification (invariant #5): a passing check
//! against the current tree records the fact. [`Verdict::Drifted`] (the fact is
//! already false) and [`Verdict::Broken`] (the check cannot run) are refused with the
//! observed evidence, writing nothing — [`AuthorError::NotHeld`]. An agent check with
//! no runner in the context is [`Verdict::Unverifiable`], which is not `Held`, so it
//! is refused too: a claim cannot be established by a check that could not be run.
//! Placing the gate here means no caller can accidentally record a claim whose check
//! did not hold.

use claim_core::{run_check, Check, CheckContext, CheckOutcome, Claim, Timestamp, Verdict};

use crate::git::{resolve_actor, resolve_commit};
use crate::{GitError, Store, StoreLoad};

/// The git-derived provenance the authoring gate resolves: the commit the check
/// was observed against and the actor who observed it. Resolved once, before
/// anything is written, so a missing identity or absent repository fails while
/// nothing has been written (invariant #3, provenance from git, not the file).
///
/// No verdict is persisted, so this provenance is not written to any log; it is
/// resolved to *fail early* on a missing identity and returned so a caller can
/// display who authored the claim it must now commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// The full 40-char `HEAD` sha (or the unborn-HEAD sentinel), never abbreviated,
    /// so the reported provenance does not vary with `core.abbrev`.
    pub commit: String,
    /// The actor the claim is attributed to, `Name <email>` from git config.
    pub actor: String,
}

/// The successful result of authoring a claim: what was written, and the provenance
/// the gate resolved.
///
/// The path is absolute so a caller can render it however it needs (relative to the
/// store root for a commit hint, or as-is). The establishing outcome is always
/// [`Verdict::Held`] — that is the gate this function enforces — but it is echoed so
/// a caller can narrate its evidence without re-running the check.
#[derive(Debug, Clone)]
pub struct Authored {
    /// The absolute path of the written claim file.
    pub claim_file: std::path::PathBuf,
    /// The provenance the gate resolved, so a caller can name who authored the
    /// claim it must commit. No verdict is written, so there is no log file.
    pub provenance: Provenance,
    /// The full establishing check outcome (always [`Verdict::Held`]), so a caller
    /// can narrate its evidence without re-running the check. Reported, not stored.
    pub establishing: CheckOutcome,
}

/// A claim that could not be authored, each variant a distinct, loud refusal that
/// writes nothing.
///
/// Every variant degrades toward *not creating a claim* rather than creating a
/// dubious one: a duplicate id, a check that did not hold against reality, an
/// unattributable claim, or an I/O fault — all refuse before or instead of the
/// write. The caller maps each to its own surface (the CLI's `--json` `kind`)
/// without matching on prose.
#[derive(Debug, thiserror::Error)]
pub enum AuthorError {
    /// A claim file already occupies the id's canonical path `.claims/<id>.md`.
    /// Distinct from [`AuthorError::IdAlreadyDeclared`] (a *differently named* file
    /// declaring the same id) so a caller can phrase each case precisely.
    #[error(
        "a claim with id '{id}' already exists at {path}; choose a different id or edit that file"
    )]
    DuplicateId {
        /// The conflicting id.
        id: String,
        /// The canonical path already occupied.
        path: String,
    },

    /// A differently-named file already declares this id. Caught by scanning every
    /// parsed claim's id, not just the canonical path — the id, not the filename, is
    /// what must be unique. Kept distinct from [`AuthorError::DuplicateId`] so the
    /// message can name the *declaring* file rather than the canonical path.
    #[error("a claim with id '{id}' is already declared in {file}; choose a different id or edit that file")]
    IdAlreadyDeclared {
        /// The conflicting id.
        id: String,
        /// The store-relative path of the file that already declares it.
        file: String,
    },

    /// The establishing check did not report [`Verdict::Held`] against the current
    /// tree, so there is no true fact to record. Carries the observed outcome so the
    /// caller can surface the evidence for *why* it was refused.
    #[error("the check did not hold against the current tree ({status}); nothing was written")]
    NotHeld {
        /// The verdict actually observed (`Drifted`, `Broken`, or `Unverifiable`).
        verdict: Verdict,
        /// The human one-liner for how the check ended (`exit 1`, `exit 127`, …).
        status: String,
        /// The check's evidence, if any, for the caller to relay.
        evidence: Option<String>,
    },

    /// Git provenance could not be resolved — no repository, unset identity — so the
    /// claim would be unattributable and is not written.
    #[error("could not attribute the claim: {0}")]
    Provenance(#[from] GitError),

    /// The claim file could not be written (an I/O fault, or a race with a concurrent
    /// author).
    #[error("could not write the claim: {0}")]
    Write(String),

    /// The `on_established` hook aborted the write after the establishing check held.
    /// A sentinel the *caller* returns from its hook to stop authoring while keeping
    /// its own richer error (e.g. the CLI's `--witness-cmd` failure, whose stable
    /// `ErrorKind` must survive) out of band; `author_claim` writes nothing and
    /// propagates it. Never produced by `author_claim` itself.
    #[error("authoring was aborted after the establishing check by the caller's hook")]
    WitnessAborted,
}

impl AuthorError {
    /// Build a [`AuthorError::NotHeld`] from the establishing outcome, capturing the
    /// verdict, its human status, and its evidence for the caller to relay.
    fn not_held(outcome: &CheckOutcome) -> Self {
        AuthorError::NotHeld {
            verdict: outcome.verdict,
            status: outcome.status(),
            evidence: outcome.evidence.clone(),
        }
    }
}

/// Author a claim: refuse a duplicate id, run the establishing check requiring
/// [`Verdict::Held`], then write the claim file to the working tree. Never commits,
/// and never writes a verdict.
///
/// This is the non-interactive core `claim add` calls. The caller supplies a
/// validated `claim` (produced only by
/// [`claim_core::parse_claim_file`], so the schema is already enforced), the exact
/// `file_text` to write (round-tripped through the parser by the caller, so what is
/// validated is what is written), a [`CheckContext`] carrying the working directory
/// and any agent runner, and the loaded corpus `existing` (so the duplicate-id scan
/// and the caller's supports warning share one load). `now` is unused by the write
/// path — no verdict is stamped — but is kept in the signature so a caller threads
/// one clock and a future need for it is not a breaking change.
///
/// The order is deliberate and load-bearing:
///
/// 1. **Duplicate-id refusal**, before anything runs — a second claim under the same
///    id is a false-green hazard, so it is refused loudly.
/// 2. **Provenance resolution**, before the check runs — a missing git identity or
///    absent repository fails while nothing has been written.
/// 3. **The establishing run**, requiring `Held` — the honesty gate (invariant #5).
///    `Drifted`/`Broken`/`Unverifiable` are refused with the observed evidence,
///    writing nothing.
/// 4. **`on_established`**, only after `Held` and only if steps 1–3 passed — a hook
///    for a caller that must do work *between* a confirmed establish and the write and
///    that must NOT run when the add is going to be refused. `claim add --witness-cmd`
///    perturbs an isolated worktree here, so its side-effecting command never runs for
///    an add that a duplicate id or a non-holding check has already doomed. The hook
///    returns an optional evidence note the caller may narrate, or aborts the write
///    with an [`AuthorError`].
/// 5. **The write**, only after the hook succeeds — the claim file with `create_new`
///    (so a file that appeared since the duplicate check is never clobbered). No
///    verdict is written.
///
/// The ordinary path with no hook work passes an `on_established` that returns
/// `Ok(None)`.
///
/// The caller keeps the establishing [`CheckOutcome`] via [`Authored`] for its own
/// narration; the evidence on a refusal rides in [`AuthorError::NotHeld`].
///
/// # Errors
///
/// Returns the matching [`AuthorError`] for each rule it fails, and never writes a
/// claim when it returns `Err`. A [`AuthorError::NotHeld`] carries the refused
/// verdict and its evidence; a [`AuthorError::DuplicateId`] names the conflict; a
/// [`AuthorError::Provenance`] or [`AuthorError::Write`] is an environment fault; and
/// an `on_established` that fails aborts before the write with whatever
/// [`AuthorError`] it returns.
pub fn author_claim(
    store: &Store,
    claim: &Claim,
    file_text: &str,
    existing: &StoreLoad,
    ctx: &CheckContext,
    now: Timestamp,
    on_established: impl FnOnce(&CheckOutcome) -> Result<Option<String>, AuthorError>,
) -> Result<Authored, AuthorError> {
    let _ = now;
    reject_duplicate(store, existing, claim)?;

    // Provenance up front, before the check runs: an unattributable claim fails
    // while nothing has been written, not after. No verdict is persisted, so this is
    // resolved only to fail early and to report who authored the claim.
    let provenance = Provenance {
        commit: resolve_commit(store.root())?,
        actor: resolve_actor(store.root())?,
    };

    let check = establishing_check(claim);
    let outcome = run_check(check, ctx);
    if outcome.verdict != Verdict::Held {
        return Err(AuthorError::not_held(&outcome));
    }

    // The hook runs only now — after the id is confirmed new and the check is confirmed
    // to hold — so a caller's side-effecting work (the `--witness-cmd` dance) never
    // fires for an add that is already going to be refused.
    on_established(&outcome)?;

    write_claim(store, claim, file_text)?;

    Ok(Authored {
        claim_file: store.claim_file(&claim.id),
        provenance,
        establishing: outcome,
    })
}

/// The check the establishing run executes: the claim's first check.
///
/// A claim is guaranteed a non-empty check list by the parser, so `first` is always
/// `Some`; the birth check establishes that first check against reality. v1
/// authoring (both `add` and `create`) writes a single-check claim, so "the first
/// check" is "the check".
fn establishing_check(claim: &Claim) -> &Check {
    claim
        .checks
        .first()
        .expect("a parsed claim always has at least one check")
}

/// Refuse a claim whose id already exists anywhere in the store.
///
/// A duplicate id is a false-green hazard: two files sharing an id conflate the
/// facts they record, so the id, not the filename, is what must be unique. The
/// canonical path `.claims/<id>.md` is checked, *and* every parsed claim's id in
/// `existing` — so a claim declaring the same id from a differently named file is
/// caught too. A canonical-path collision is checked as well, in case a file exists
/// but does not parse (and so is absent from the id scan).
fn reject_duplicate(store: &Store, existing: &StoreLoad, claim: &Claim) -> Result<(), AuthorError> {
    let canonical = store.claim_file(&claim.id);
    if canonical.exists() {
        return Err(AuthorError::DuplicateId {
            id: claim.id.to_string(),
            path: canonical.display().to_string(),
        });
    }
    if let Some(existing) = existing.claims.iter().find(|c| c.claim.id == claim.id) {
        return Err(AuthorError::IdAlreadyDeclared {
            id: claim.id.to_string(),
            file: existing.path.clone(),
        });
    }
    Ok(())
}

/// Write the claim file. The last step, and the only one that touches the store —
/// everything before it is validation. No verdict is written.
fn write_claim(store: &Store, claim: &Claim, file_text: &str) -> Result<(), AuthorError> {
    let claim_file = store.claim_file(&claim.id);
    if let Some(parent) = claim_file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            AuthorError::Write(format!("failed to create {}: {e}", parent.display()))
        })?;
    }
    // create_new so a claim file that appeared between the duplicate check and here
    // (a concurrent author) is never clobbered.
    write_new_file(&claim_file, file_text)
}

/// Create a new file, failing loudly if one already exists (a race with the
/// duplicate check, or a concurrent author).
fn write_new_file(path: &std::path::Path, contents: &str) -> Result<(), AuthorError> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| AuthorError::Write(format!("failed to create {}: {e}", path.display())))?;
    file.write_all(contents.as_bytes())
        .map_err(|e| AuthorError::Write(format!("failed to write {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    /// A temp git repo that is also a claim store, with a deterministic identity so
    /// provenance resolves without the developer's ambient git config.
    struct TestRepo {
        _dir: TempDir,
        store: Store,
    }

    impl TestRepo {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            git(dir.path(), &["init", "-q"]);
            git(dir.path(), &["config", "user.name", "Test Agent"]);
            git(dir.path(), &["config", "user.email", "agent@example.com"]);
            std::fs::write(dir.path().join("requirements.txt"), "libfoo==4.2\n").unwrap();
            git(dir.path(), &["add", "-A"]);
            git(dir.path(), &["commit", "-q", "-m", "init"]);
            let (store, _) = Store::init(dir.path()).unwrap();
            TestRepo { _dir: dir, store }
        }

        fn root(&self) -> &Path {
            self.store.root()
        }

        fn ctx(&self) -> CheckContext {
            CheckContext::new(self.store.root())
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed");
    }

    fn ts() -> Timestamp {
        "2026-07-18T12:00:00Z".parse().unwrap()
    }

    /// A parsed claim plus its file text for the given id, statement, and cmd `run`.
    fn claim(id: &str, statement: &str, run: &str) -> (Claim, String) {
        let text =
            format!("---\nid: {id}\nchecks:\n  - kind: cmd\n    run: {run:?}\n---\n{statement}\n");
        let claim = claim_core::parse_claim_file(&format!(".claims/{id}.md"), &text).unwrap();
        (claim, text)
    }

    #[test]
    fn holds_writes_the_claim_file_and_no_verdict() {
        let repo = TestRepo::new();
        let (c, text) = claim(
            "pin",
            "We pin libfoo at 4.2.",
            "grep -q 'libfoo==4.2' requirements.txt",
        );
        let existing = repo.store.load_all().unwrap();

        let out = author_claim(&repo.store, &c, &text, &existing, &repo.ctx(), ts(), |_| {
            Ok(None)
        })
        .unwrap();

        assert_eq!(out.establishing.verdict, Verdict::Held);
        assert!(out.claim_file.exists(), "the claim file was written");
        // No verdict log is written: a verdict is telemetry, not source.
        assert!(
            !repo.root().join(".claims/log").exists(),
            "no verdict log directory is created"
        );
        // The provenance is the store's git identity, not anything from the caller.
        assert_eq!(out.provenance.actor, "Test Agent <agent@example.com>");
        assert_eq!(out.provenance.commit.len(), 40, "a real HEAD sha");
        // The written file is exactly the validated text.
        assert_eq!(std::fs::read_to_string(&out.claim_file).unwrap(), text);
    }

    #[test]
    fn a_drifted_check_writes_nothing() {
        let repo = TestRepo::new();
        // A grep for a pin that is not present: the fact is already false.
        let (c, text) = claim("x", "S.", "grep -q 'libfoo==9.9' requirements.txt");
        let existing = repo.store.load_all().unwrap();

        let err = author_claim(&repo.store, &c, &text, &existing, &repo.ctx(), ts(), |_| {
            Ok(None)
        })
        .unwrap_err();
        assert!(
            matches!(
                err,
                AuthorError::NotHeld {
                    verdict: Verdict::Drifted,
                    ..
                }
            ),
            "a drifted check is NotHeld, got {err:?}"
        );
        assert!(
            !repo.store.claim_file(&c.id).exists(),
            "nothing is written on a refused establish"
        );
    }

    #[test]
    fn a_broken_check_writes_nothing() {
        let repo = TestRepo::new();
        let (c, text) = claim("x", "S.", "this-binary-does-not-exist-anywhere");
        let existing = repo.store.load_all().unwrap();

        let err = author_claim(&repo.store, &c, &text, &existing, &repo.ctx(), ts(), |_| {
            Ok(None)
        })
        .unwrap_err();
        assert!(
            matches!(
                err,
                AuthorError::NotHeld {
                    verdict: Verdict::Broken,
                    ..
                }
            ),
            "a broken check is NotHeld, got {err:?}"
        );
        assert!(!repo.store.claim_file(&c.id).exists());
    }

    #[test]
    fn an_unverifiable_check_writes_nothing() {
        // An agent check with no runner in the context is Unverifiable, which is not
        // Held: a claim cannot be established by a check that could not be run.
        let repo = TestRepo::new();
        let text = "---\nid: a\nchecks:\n  - kind: agent\n    instruction: investigate\n---\nS.\n";
        let c = claim_core::parse_claim_file(".claims/a.md", text).unwrap();
        let existing = repo.store.load_all().unwrap();

        let err = author_claim(&repo.store, &c, text, &existing, &repo.ctx(), ts(), |_| {
            Ok(None)
        })
        .unwrap_err();
        assert!(
            matches!(
                err,
                AuthorError::NotHeld {
                    verdict: Verdict::Unverifiable,
                    ..
                }
            ),
            "an unrunnable agent check is NotHeld, got {err:?}"
        );
        assert!(!repo.store.claim_file(&c.id).exists());
    }

    #[test]
    fn a_duplicate_id_writes_nothing() {
        let repo = TestRepo::new();
        let (c, text) = claim("dup", "S.", "true");
        let existing = repo.store.load_all().unwrap();
        author_claim(&repo.store, &c, &text, &existing, &repo.ctx(), ts(), |_| {
            Ok(None)
        })
        .unwrap();

        // Re-load so the second author sees the first claim.
        let existing = repo.store.load_all().unwrap();
        let (c2, text2) = claim("dup", "S2.", "true");
        let err = author_claim(
            &repo.store,
            &c2,
            &text2,
            &existing,
            &repo.ctx(),
            ts(),
            |_| Ok(None),
        )
        .unwrap_err();
        assert!(
            matches!(err, AuthorError::DuplicateId { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn never_commits_the_write() {
        let repo = TestRepo::new();
        let (c, text) = claim("pin", "S.", "true");
        let existing = repo.store.load_all().unwrap();
        author_claim(&repo.store, &c, &text, &existing, &repo.ctx(), ts(), |_| {
            Ok(None)
        })
        .unwrap();

        // The claim file is on disk but uncommitted: a write to the truth is a commit
        // the caller makes.
        let status = Command::new("git")
            .arg("-C")
            .arg(repo.root())
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(
            !status.stdout.is_empty(),
            "the authored claim is left uncommitted in the working tree"
        );
    }
}
