//! Git provenance resolution: the `commit` and `actor` a verdict-log entry needs.
//!
//! The trust model derives provenance from git rather than from fields a claim
//! file asserts about itself (invariant #3). When `claim add` writes the birth
//! verdict, that entry must carry the commit the check was observed against and
//! the identity of whoever observed it — both looked up here from the repository,
//! not typed by the author.
//!
//! Git is treated as the database (invariant #4), so this shells out to the `git`
//! binary through [`std::process`] rather than linking a library: the repository
//! on disk is the source of truth, and the same `git` a human runs is the one the
//! tool consults. The helpers are kept small and reusable — later verbs (`check`,
//! `drift`) resolve the same `commit`/`actor` when they append their own entries.
//!
//! Two edges are handled deliberately, because [`claim_core::append_entry`] rejects
//! an empty `commit` or `actor` and an untraceable verdict has no provenance:
//!
//! - **No git repository.** Resolving a commit fails with an error naming the fix
//!   (run inside a git repo). A claim store lives in a git repo by design — a
//!   write to the truth is a commit — so this is a real misconfiguration, not a
//!   state to paper over.
//! - **An unborn HEAD** (a freshly `git init`-ed repo with no commits yet). There
//!   is no sha to report, so [`resolve_commit`] returns the sentinel
//!   [`UNBORN_HEAD_SENTINEL`]: git's own all-zero object name, which reads as "no
//!   commit yet" to anyone who knows git and keeps the log entry's `commit`
//!   non-empty so it is still a valid, appendable entry. Only a *genuinely* unborn
//!   HEAD gets the sentinel; a corrupt HEAD stays a loud error rather than being
//!   masked as "no commit yet".
//!
//! Recorded shas are always the full 40-char form, never `--short`: the abbreviated
//! width is `core.abbrev`-dependent, and the trust substrate must not vary with a
//! user's config. Abbreviation is display-only ([`short_commit`]).

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

/// The `commit` recorded when the store's repository has no commit yet (an unborn
/// HEAD, immediately after `git init`).
///
/// Forty zeros is git's own null object name — the value `HEAD` conceptually points
/// at before the first commit, and what appears in a reflog's "from" field for an
/// initial commit. Reusing it means a reader who knows git reads the sentinel
/// correctly with no new convention to learn, and it satisfies
/// [`claim_core::append_entry`]'s non-empty-commit requirement so the birth verdict
/// is a valid entry. Once the author commits the new claim file, every subsequent
/// verdict resolves a real sha; only the very first entry in a brand-new repo can
/// carry this.
pub const UNBORN_HEAD_SENTINEL: &str = "0000000000000000000000000000000000000000";

/// Resolve the *full* sha of the repository's current `HEAD`, for a verdict entry's
/// `commit`.
///
/// The full 40-char sha, not `--short`: the abbreviated form respects the user's
/// `core.abbrev` config, so recording it would make the trust substrate (the
/// verdict log) vary in width and content with local configuration, and be
/// inconsistent with the 40-char unborn sentinel. The full sha is
/// configuration-independent; callers abbreviate only for human display
/// ([`short_commit`]).
///
/// `dir` is any path inside the repository (the store root); git discovers the
/// repository from it. On an unborn HEAD this returns [`UNBORN_HEAD_SENTINEL`]
/// rather than failing, because a fresh repo is a legitimate place to author the
/// first claim.
///
/// # Errors
///
/// Fails when `dir` is not inside a git repository, when `HEAD` is present but
/// corrupt (distinct from unborn), or when the `git` binary cannot be run, with a
/// message naming the fix. Those are real misconfigurations for a git-native tool,
/// not states to silently continue past.
pub fn resolve_commit(dir: &Path) -> Result<String> {
    let head = run_git(dir, &["rev-parse", "HEAD"])?;
    if let Some(sha) = head.success_stdout() {
        return Ok(sha);
    }

    // HEAD did not resolve. Classify precisely: an *unborn* HEAD is a symbolic ref
    // to a branch that has no commit yet, which `symbolic-ref -q HEAD` still
    // resolves to (the ref name), while a corrupt HEAD or a bare "not a repo" does
    // not. Only the genuine unborn case gets the sentinel; every other failure
    // stays loud, so a corrupt HEAD is never masked as "no commit yet".
    let symref = run_git(dir, &["symbolic-ref", "-q", "HEAD"])?;
    if symref.ok && !symref.stdout.is_empty() {
        // A symbolic HEAD exists but rev-parse found no commit: an unborn branch.
        // Confirm we are inside a work tree so a detached-but-unborn oddity or a
        // stray symref outside a repo cannot slip through.
        let inside = run_git(dir, &["rev-parse", "--is-inside-work-tree"])?;
        if inside.success_stdout().as_deref() == Some("true") {
            return Ok(UNBORN_HEAD_SENTINEL.to_owned());
        }
    }

    bail!(
        "could not resolve HEAD in {} (not a git repository, or HEAD is corrupt); \
         a claim store lives in a git repo — run `git init` or `claim` from inside \
         your repository. If HEAD is corrupt, repair it before recording a verdict.",
        dir.display()
    )
}

/// Abbreviate a commit sha for human display, without touching the recorded value.
///
/// The log and JSON keep the full sha ([`resolve_commit`]); only the human-readable
/// print shortens it. The unborn sentinel abbreviates to a recognizable `0000000`
/// rather than a misleading truncation.
#[must_use]
pub fn short_commit(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Resolve the author identity for a verdict entry's `actor`, as `Name <email>`.
///
/// Reads `user.name` and `user.email` from git config (which merges repo-local and
/// global config exactly as a commit would), so the actor cached in the log matches
/// the identity a subsequent `git commit` will stamp on the entry. This is a
/// convenience for display; the authoritative author remains the commit that adds
/// the file (invariant #3).
///
/// # Errors
///
/// Fails when either `user.name` or `user.email` is unset, naming the `git config`
/// command to set it. An unattributed verdict has no provenance
/// ([`claim_core::append_entry`] rejects an empty actor), so a missing identity is a
/// loud error, not a silent blank.
pub fn resolve_actor(dir: &Path) -> Result<String> {
    let name = git_config(dir, "user.name")?;
    let email = git_config(dir, "user.email")?;
    Ok(format!("{name} <{email}>"))
}

/// Whether the *tracked* working tree has uncommitted changes — unstaged edits or
/// staged-but-uncommitted ones.
///
/// The default witnessed-red restore reverts every tracked file to the index, so it
/// is only safe when the tracked tree starts clean; a pre-existing edit would be
/// silently destroyed. `claim add` calls this before perturbing on the default path
/// and refuses if it reports `true`. Untracked files are intentionally not counted
/// (`-uno`): the restore never touches them, so their presence is not a hazard.
///
/// Combines `git diff --quiet` (working tree vs index) and `git diff --cached
/// --quiet` (index vs HEAD); either being non-zero means dirty. On an unborn HEAD
/// the `--cached` diff has no HEAD to compare against and reports the staged files
/// as changes, which correctly reads as dirty — the default git restore cannot help
/// there anyway.
///
/// # Errors
///
/// Fails only if git cannot be run at all (not installed); a non-zero *diff* exit is
/// the "dirty" signal, not an error.
pub fn tracked_tree_is_dirty(dir: &Path) -> Result<bool> {
    // `diff --quiet` exits 1 when there is a difference, 0 when clean. Distinguish
    // that from a real spawn failure via GitOutput::ok being about the *exit*, not
    // the spawn (run_git already turns a spawn failure into an Err).
    let unstaged = run_git(dir, &["diff", "--quiet"])?;
    let staged = run_git(dir, &["diff", "--cached", "--quiet"])?;
    Ok(!unstaged.ok || !staged.ok)
}

/// Revert *tracked* working-tree modifications to their staged (index) state, with
/// `git checkout -- .`.
///
/// The default restore for the scripted witnessed-red flow when no `--restore-cmd`
/// is supplied, used only after [`tracked_tree_is_dirty`] confirmed the tracked tree
/// was clean before the perturbation — so it reverts exactly the perturbation and
/// nothing of the author's. Deliberately narrow: it touches only tracked files and
/// restores them from the *index* (not `HEAD`), so it can never delete the untracked
/// `.claims/` store or any other untracked file — unlike a `git clean`, which this
/// intentionally does not run. A perturbation that created *untracked* files is not
/// undone here; the confirm-green run that follows catches an incomplete restore, so
/// nothing is written against a still-perturbed tree.
///
/// # Errors
///
/// Fails if the checkout fails (an unborn HEAD with an empty index has nothing to
/// restore, or `git` missing), so a botched restore is loud. On an unborn HEAD the
/// author must supply `--restore-cmd` instead.
pub fn revert_tracked_changes(dir: &Path) -> Result<()> {
    run_git(dir, &["checkout", "--", "."])?
        .into_result()
        .context(
            "failed to revert tracked changes while restoring the tree; on a repo with no \
             commit yet, pass --restore-cmd to undo the perturbation",
        )
}

/// Whether `dir` is inside a git working tree.
///
/// A best-effort predicate for the `init` warning: a store outside a repo is usable
/// but cannot attribute verdicts. Returns `false` on any failure (git missing, not a
/// repo), because the only caller wants "is this safely a repo?" and treats every
/// non-yes as "warn".
#[must_use]
pub fn is_inside_work_tree(dir: &Path) -> bool {
    run_git(dir, &["rev-parse", "--is-inside-work-tree"])
        .ok()
        .and_then(|o| o.success_stdout())
        .as_deref()
        == Some("true")
}

/// Read one git config value, trimmed. `None` maps to a clear "set it" error.
fn git_config(dir: &Path, key: &str) -> Result<String> {
    let out = run_git(dir, &["config", key])?;
    out.success_stdout()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "git {key} is not set; a verdict needs an attributable author. \
             Set it with `git config {key} \"...\"`"
            )
        })
}

/// The outcome of a git invocation: its exit status and trimmed stdout.
struct GitOutput {
    ok: bool,
    stdout: String,
    stderr: String,
    args: String,
}

impl GitOutput {
    /// The trimmed stdout when the command succeeded, else `None`. A non-zero exit
    /// (unborn HEAD, unset config) is a soft signal callers interpret, not an error.
    fn success_stdout(&self) -> Option<String> {
        self.ok.then(|| self.stdout.clone())
    }

    /// Turn a non-zero exit into an error carrying git's own stderr, for the git
    /// mutations whose failure must be loud.
    fn into_result(self) -> Result<()> {
        if self.ok {
            Ok(())
        } else {
            bail!("`git {}` failed: {}", self.args, self.stderr.trim())
        }
    }
}

/// Run `git` with `args` in `dir`, capturing output.
///
/// A failure to *spawn* git at all (git not installed) is an error; a git command
/// that runs and exits non-zero is not — that is data the caller classifies (an
/// unborn HEAD, an unset config key), so it comes back in [`GitOutput::ok`].
fn run_git(dir: &Path, args: &[&str]) -> Result<GitOutput> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| {
            format!(
                "failed to run `git {}`; is git installed and on PATH?",
                args.join(" ")
            )
        })?;
    Ok(GitOutput {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        args: args.join(" "),
    })
}
