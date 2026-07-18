//! The shared claim-authoring core: establish a claim's check against reality,
//! then write the claim file and its birth verdict — used identically by the CLI's
//! `claim add` and the MCP `create` tool.
//!
//! Both front doors must author a claim the same way, or the two would disagree
//! about what it takes to record a fact — exactly the drift this tool exists to
//! prevent. So the non-interactive core lives here, once, over [`crate::Store`] and
//! [`crate::git`]: given a validated [`Claim`] and the exact bytes to write, it
//! resolves git provenance, refuses a duplicate id, runs the establishing check
//! requiring [`Verdict::Held`], and — only then — writes the claim file and its
//! establishing verdict to the working tree. It never commits (invariant #4, a
//! write to the truth is a commit the caller makes) and never touches the tree
//! before the check passes.
//!
//! What stays with each caller is the surface, not the substance: the CLI keeps its
//! interactive prompting, its optional `--witness-cmd` confidence dance, its `--json`
//! shape, and its unresolved-`supports` warning; the MCP tool keeps its request
//! parsing and its response shape. Both hand this function the same three things — a
//! parsed claim, its file text, and a [`CheckContext`] — and get back the same
//! [`Authored`] outcome or the same typed [`AuthorError`].
//!
//! # The honesty gate is here, not in a caller
//!
//! The establishing run is the whole of verification (invariant #5): a passing check
//! against the current tree records the fact. [`Verdict::Drifted`] (the fact is
//! already false) and [`Verdict::Broken`] (the check cannot run) are refused with the
//! observed evidence, writing nothing — [`AuthorError::NotHeld`]. An agent check with
//! no runner in the context is [`Verdict::Unverifiable`], which is not `Held`, so it
//! is refused too: a claim cannot be established by a check that could not be run.
//! Placing the gate here means neither front door can accidentally record a claim
//! whose check did not hold.

use claim_core::{
    append_entry, run_check, Check, CheckContext, CheckOutcome, Claim, Event, LogEntry, Timestamp,
    Verdict,
};

use crate::git::{resolve_actor, resolve_commit};
use crate::{GitError, Store, StoreLoad};

/// The git-derived provenance stamped on the establishing verdict: the commit the
/// check was observed against and the actor who observed it. Resolved once, before
/// anything is written, so a missing identity or absent repository fails while
/// nothing has been written (invariant #3, provenance from git, not the file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// The full 40-char `HEAD` sha (or the unborn-HEAD sentinel), never abbreviated,
    /// so the recorded provenance does not vary with `core.abbrev`.
    pub commit: String,
    /// The actor the verdict is attributed to, `Name <email>` from git config.
    pub actor: String,
}

/// The successful result of authoring a claim: what was written, and the provenance
/// it carries.
///
/// The paths are absolute so a caller can render them however it needs (relative to
/// the store root for a commit hint, or as-is). The establishing verdict is always
/// [`Verdict::Held`] — that is the gate this function enforces — but it is echoed so
/// a caller need not re-derive it.
#[derive(Debug, Clone)]
pub struct Authored {
    /// The absolute path of the written claim file.
    pub claim_file: std::path::PathBuf,
    /// The absolute path of the appended establishing verdict-log entry.
    pub log_file: std::path::PathBuf,
    /// The provenance stamped on the establishing verdict.
    pub provenance: Provenance,
    /// The full establishing check outcome (always [`Verdict::Held`]), so a caller
    /// can narrate its evidence without re-running the check.
    pub establishing: CheckOutcome,
}

/// A claim that could not be authored, each variant a distinct, loud refusal that
/// writes nothing.
///
/// Every variant degrades toward *not creating a claim* rather than creating a
/// dubious one: a duplicate id, a check that did not hold against reality, an
/// unattributable verdict, or an I/O fault — all refuse before or instead of the
/// write. A caller maps each to its own surface (the CLI's `--json` `kind`, the MCP
/// server's `invalid_params` vs `internal_error`) without matching on prose.
#[derive(Debug, thiserror::Error)]
pub enum AuthorError {
    /// A claim file already occupies the id's canonical path `.claims/<id>.md` — a
    /// false-green hazard, since two files sharing an id share one verdict log.
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
    /// verdict would be unattributable and is not written.
    #[error("could not attribute the establishing verdict: {0}")]
    Provenance(#[from] GitError),

    /// The claim file could not be written, or the establishing verdict could not be
    /// appended (an I/O fault, a race with a concurrent author, or core rejecting the
    /// entry).
    #[error("could not write the claim: {0}")]
    Write(String),
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
/// [`Verdict::Held`], then write the claim file and its birth verdict to the working
/// tree with git provenance. Never commits.
///
/// This is the non-interactive core both `claim add` and the MCP `create` tool call.
/// The caller supplies a validated `claim` (produced only by
/// [`claim_core::parse_claim_file`], so the schema is already enforced), the exact
/// `file_text` to write (round-tripped through the parser by the caller, so what is
/// validated is what is written), a [`CheckContext`] carrying the working directory
/// and any agent runner, and the loaded corpus `existing` (so the duplicate-id scan
/// and the caller's supports warning share one load). `now` is a parameter so the
/// recorded instant is deterministic under test.
///
/// `extra_evidence` is folded ahead of the check's own evidence on the birth entry,
/// for a caller that has an additional note to record — `claim add --witness-cmd`
/// records the red it observed in isolation here. It is `None` for the ordinary
/// path.
///
/// The order is deliberate and load-bearing:
///
/// 1. **Duplicate-id refusal**, before anything runs — a second claim under the same
///    id would interleave verdict logs, so it is refused loudly.
/// 2. **Provenance resolution**, before the check runs — a missing git identity or
///    absent repository fails while nothing has been written.
/// 3. **The establishing run**, requiring `Held` — the honesty gate (invariant #5).
///    `Drifted`/`Broken`/`Unverifiable` are refused with the observed evidence,
///    writing nothing.
/// 4. **The write**, only on `Held` — the claim file with `create_new` (so a file
///    that appeared since the duplicate check is never clobbered) and one birth
///    verdict.
///
/// The caller keeps the establishing [`CheckOutcome`] via [`Authored`] for its own
/// narration; the evidence on a refusal rides in [`AuthorError::NotHeld`].
///
/// # Errors
///
/// Returns the matching [`AuthorError`] for each rule it fails, and never writes a
/// claim when it returns `Err`. A [`AuthorError::NotHeld`] carries the refused
/// verdict and its evidence; a [`AuthorError::DuplicateId`] names the conflict; a
/// [`AuthorError::Provenance`] or [`AuthorError::Write`] is an environment fault.
pub fn author_claim(
    store: &Store,
    claim: &Claim,
    file_text: &str,
    existing: &StoreLoad,
    ctx: &CheckContext,
    now: Timestamp,
    extra_evidence: Option<String>,
) -> Result<Authored, AuthorError> {
    reject_duplicate(store, existing, claim)?;

    // Provenance up front, before the check runs: an unattributable verdict fails
    // while nothing has been written, not after.
    let provenance = Provenance {
        commit: resolve_commit(store.root())?,
        actor: resolve_actor(store.root())?,
    };

    let check = establishing_check(claim);
    let outcome = run_check(check, ctx);
    if outcome.verdict != Verdict::Held {
        return Err(AuthorError::not_held(&outcome));
    }

    write_claim_and_log(
        store,
        claim,
        file_text,
        &outcome,
        extra_evidence,
        &provenance,
        now,
    )
}

/// The check the establishing run executes: the claim's first check.
///
/// A claim is guaranteed a non-empty check list by the parser, so `first` is always
/// `Some`; the birth verdict establishes that first check against reality. v1
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
/// A duplicate id is a false-green hazard: two files sharing an id share one verdict
/// log (`.claims/log/<id>/`), so their histories interleave and a drifted fact can
/// read as verified. The canonical path `.claims/<id>.md` is checked, *and* every
/// parsed claim's id in `existing` — the id, not the filename, is what must be
/// unique, so a claim declaring the same id from a differently named file is caught
/// too. A canonical-path collision is checked as well, in case a file exists but does
/// not parse (and so is absent from the id scan).
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

/// Write the claim file and append the establishing verdict. The last step, and the
/// only one that touches the store — everything before it is validation.
fn write_claim_and_log(
    store: &Store,
    claim: &Claim,
    file_text: &str,
    establishing: &CheckOutcome,
    extra_evidence: Option<String>,
    provenance: &Provenance,
    now: Timestamp,
) -> Result<Authored, AuthorError> {
    let claim_file = store.claim_file(&claim.id);
    if let Some(parent) = claim_file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            AuthorError::Write(format!("failed to create {}: {e}", parent.display()))
        })?;
    }
    // create_new so a claim file that appeared between the duplicate check and here
    // (a concurrent author) is never clobbered.
    write_new_file(&claim_file, file_text)?;

    let entry = LogEntry {
        at: now,
        commit: provenance.commit.clone(),
        actor: provenance.actor.clone(),
        event: Event::Verification {
            verdict: establishing.verdict,
            evidence: fold_evidence(extra_evidence, establishing.evidence.as_deref()),
        },
    };
    let log_file = append_entry(&store.log_dir(), &claim.id, &entry)
        .map_err(|e| AuthorError::Write(e.to_string()))?;

    Ok(Authored {
        claim_file,
        log_file,
        provenance: provenance.clone(),
        establishing: establishing.clone(),
    })
}

/// Fold an optional extra note ahead of the check's own evidence, so the birth
/// entry carries both — the note first, then the check's output.
fn fold_evidence(extra: Option<String>, check_evidence: Option<&str>) -> Option<String> {
    match (extra, check_evidence) {
        (Some(note), Some(ev)) => Some(format!("{note}\n{ev}")),
        (Some(note), None) => Some(note),
        (None, ev) => ev.map(ToOwned::to_owned),
    }
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
        let text = format!(
            "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: {run:?}\n    when: on-change\nmax-age: 30d\n---\n{statement}\n"
        );
        let claim = claim_core::parse_claim_file(&format!(".claims/{id}.md"), &text).unwrap();
        (claim, text)
    }

    #[test]
    fn holds_writes_the_claim_and_a_single_held_verdict() {
        let repo = TestRepo::new();
        let (c, text) = claim(
            "pin",
            "We pin libfoo at 4.2.",
            "grep -q 'libfoo==4.2' requirements.txt",
        );
        let existing = repo.store.load_all().unwrap();

        let out = author_claim(&repo.store, &c, &text, &existing, &repo.ctx(), ts(), None).unwrap();

        assert_eq!(out.establishing.verdict, Verdict::Held);
        assert!(out.claim_file.exists(), "the claim file was written");
        assert!(
            out.log_file.exists(),
            "the establishing verdict was appended"
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

        let err =
            author_claim(&repo.store, &c, &text, &existing, &repo.ctx(), ts(), None).unwrap_err();
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
        assert!(
            repo.root().join(".claims/log/x").read_dir().is_err(),
            "no log entry on a refused establish"
        );
    }

    #[test]
    fn a_broken_check_writes_nothing() {
        let repo = TestRepo::new();
        let (c, text) = claim("x", "S.", "this-binary-does-not-exist-anywhere");
        let existing = repo.store.load_all().unwrap();

        let err =
            author_claim(&repo.store, &c, &text, &existing, &repo.ctx(), ts(), None).unwrap_err();
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
        let text = "---\nid: a\nchecks:\n  - kind: agent\n    instruction: investigate\n    when: on-change\nmax-age: 30d\n---\nS.\n";
        let c = claim_core::parse_claim_file(".claims/a.md", text).unwrap();
        let existing = repo.store.load_all().unwrap();

        let err =
            author_claim(&repo.store, &c, text, &existing, &repo.ctx(), ts(), None).unwrap_err();
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
        author_claim(&repo.store, &c, &text, &existing, &repo.ctx(), ts(), None).unwrap();

        // Re-load so the second author sees the first claim.
        let existing = repo.store.load_all().unwrap();
        let (c2, text2) = claim("dup", "S2.", "true");
        let err =
            author_claim(&repo.store, &c2, &text2, &existing, &repo.ctx(), ts(), None).unwrap_err();
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
        author_claim(&repo.store, &c, &text, &existing, &repo.ctx(), ts(), None).unwrap();

        // The claim file and verdict are on disk but uncommitted: a write to the truth
        // is a commit the caller makes.
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
