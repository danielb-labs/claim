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
//!   non-empty so it is still a valid, appendable entry.

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

/// Resolve the short sha of the repository's current `HEAD`, for a verdict entry's
/// `commit`.
///
/// `dir` is any path inside the repository (the store root); git discovers the
/// repository from it. On an unborn HEAD this returns [`UNBORN_HEAD_SENTINEL`]
/// rather than failing, because a fresh repo is a legitimate place to author the
/// first claim.
///
/// # Errors
///
/// Fails when `dir` is not inside a git repository, or when the `git` binary
/// cannot be run, with a message naming the fix. Those are real misconfigurations
/// for a git-native tool, not states to silently continue past.
pub fn resolve_commit(dir: &Path) -> Result<String> {
    // `rev-parse --short HEAD` is the sha; it fails on an unborn HEAD, which we
    // distinguish from "not a repo" so only the former gets the sentinel.
    let head = run_git(dir, &["rev-parse", "--short", "HEAD"])?;
    if let Some(sha) = head.success_stdout() {
        return Ok(sha);
    }

    // HEAD did not resolve. Either there is no repository, or the repository has
    // no commits yet. `rev-parse --is-inside-work-tree` tells the two apart.
    let inside = run_git(dir, &["rev-parse", "--is-inside-work-tree"])?;
    match inside.success_stdout().as_deref() {
        Some("true") => Ok(UNBORN_HEAD_SENTINEL.to_owned()),
        _ => bail!(
            "not inside a git repository (git could not resolve HEAD in {}); \
             a claim store lives in a git repo — run `git init` or `claim` from \
             inside your repository",
            dir.display()
        ),
    }
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

/// Revert *tracked* working-tree modifications to their state at `HEAD`, with
/// `git checkout -- .`.
///
/// The default restore for the scripted witnessed-red flow when no `--restore-cmd`
/// is supplied. Deliberately narrow: it touches only tracked files, so it can never
/// delete the untracked `.claims/` store or any other untracked file — unlike a
/// `git clean`, which this intentionally does not run. A perturbation that created
/// *untracked* files is not undone here; the confirm-green run that follows catches
/// an incomplete restore, so nothing is written against a still-perturbed tree.
///
/// # Errors
///
/// Fails if the checkout fails (no commit to restore from — an unborn HEAD — or
/// `git` missing), so a botched restore is loud. On an unborn HEAD the author must
/// supply `--restore-cmd` instead, since there is no committed state to revert to.
pub fn revert_tracked_changes(dir: &Path) -> Result<()> {
    run_git(dir, &["checkout", "--", "."])?
        .into_result()
        .context(
            "failed to revert tracked changes while restoring the tree; on a repo with no \
             commit yet, pass --restore-cmd to undo the perturbation",
        )
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
