//! Git provenance resolution: the `commit` and `actor` a verdict-log entry needs.
//!
//! The trust model derives provenance from git rather than from fields a claim
//! file asserts about itself (invariant #3). When a verdict is written, that entry
//! must carry the commit the check was observed against and the identity of
//! whoever observed it — both looked up here from the repository, not typed by the
//! author. Shared so the CLI's write verbs and the MCP server's `report` resolve
//! the same `commit`/`actor` and cannot disagree.
//!
//! Git is treated as the database (invariant #4), so this shells out to the `git`
//! binary through [`std::process`] rather than linking a library: the repository
//! on disk is the source of truth, and the same `git` a human runs is the one the
//! tool consults.
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

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::GitError;

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
/// Returns [`GitError::UnresolvableHead`] when `dir` is not inside a git
/// repository or when `HEAD` is present but corrupt (distinct from unborn), and
/// [`GitError::Spawn`] when the `git` binary cannot be run. Those are real
/// misconfigurations for a git-native tool, not states to silently continue past.
pub fn resolve_commit(dir: &Path) -> Result<String, GitError> {
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

    Err(GitError::UnresolvableHead {
        dir: dir.to_path_buf(),
    })
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
/// Returns [`GitError::MissingIdentity`] when either `user.name` or `user.email` is
/// unset, naming the `git config` command to set it. An unattributed verdict has no
/// provenance ([`claim_core::append_entry`] rejects an empty actor), so a missing
/// identity is a loud error, not a silent blank.
pub fn resolve_actor(dir: &Path) -> Result<String, GitError> {
    let name = git_config(dir, "user.name")?;
    let email = git_config(dir, "user.email")?;
    Ok(format!("{name} <{email}>"))
}

/// A throwaway `git worktree` checked out at `HEAD`, for the optional witnessed-red
/// dance without ever touching the caller's working tree.
///
/// `claim add --witness-cmd` proves a check can go red by perturbing a tree and
/// observing `Drifted`. Doing that in the user's own tree would risk their
/// uncommitted work; instead the perturbation is applied to an *isolated* checkout
/// of `HEAD` created here. The witness command and the check both run against
/// [`Worktree::path`], so the user's tree is never mutated, and no clean-tree
/// requirement is needed.
///
/// The worktree is removed on [`Drop`] as a safety net, so an early return or panic
/// mid-witness still tears it down. Prefer the explicit [`Worktree::remove`] where a
/// removal failure should be surfaced; `Drop` only best-effort-cleans and cannot
/// report. Removal uses `git worktree remove --force`, which deletes the checkout
/// even though the perturbation left it dirty — this is a temp tree that exists only
/// to be discarded.
pub struct Worktree {
    /// The repository the worktree belongs to, for the `git worktree remove` call.
    repo: PathBuf,
    /// The temporary checkout's path. `None` after [`Worktree::remove`] consumed it,
    /// so `Drop` does not double-remove.
    path: Option<PathBuf>,
}

impl Worktree {
    /// Create a detached worktree at `HEAD` under a fresh temp directory.
    ///
    /// `repo` is any path inside the repository. The checkout is *detached* at the
    /// current `HEAD` commit, so it does not touch or move any branch. It requires a
    /// born `HEAD`: an unborn repository (no commit yet) has nothing to check out,
    /// and the error says so, so the caller can fall back to the no-witness path.
    ///
    /// The temp directory is created under the system temp root, *outside* the
    /// repository, so the worktree checkout and its perturbation never appear in the
    /// user's tree (not even as an ignored path).
    ///
    /// # Errors
    ///
    /// Returns [`GitError::Io`] if the temp directory cannot be created, and
    /// [`GitError::CommandFailed`] / [`GitError::Spawn`] if `git worktree add` fails
    /// (including the unborn-HEAD case, where there is no commit to detach at).
    pub fn create_at_head(repo: &Path) -> Result<Self, GitError> {
        let dir = unique_temp_dir()?;
        let dir_str = dir.to_string_lossy().into_owned();
        // `--detach` at HEAD: an anonymous checkout of the current commit that moves
        // no branch. `--no-checkout` is deliberately not used — the witness needs the
        // real files present to perturb.
        let out = run_git(repo, &["worktree", "add", "--detach", &dir_str, "HEAD"])?;
        if !out.ok {
            // Clean up the empty temp dir we created before git failed, so a repeated
            // failure does not litter temp. Best-effort: the real error is git's.
            std::fs::remove_dir_all(&dir).ok();
            return Err(GitError::CommandFailed {
                args: out.args,
                stderr: out.stderr.trim().to_owned(),
            });
        }
        Ok(Worktree {
            repo: repo.to_path_buf(),
            path: Some(dir),
        })
    }

    /// The isolated checkout's root — where the witness command and the check run.
    ///
    /// # Panics
    ///
    /// Panics if called after [`Worktree::remove`], which is a use-after-free of the
    /// worktree; the type is not meant to be used past its explicit removal.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("worktree path used after remove()")
    }

    /// Remove the worktree, surfacing any failure. Consumes `self` so `Drop` does not
    /// also try to remove it.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::CommandFailed`] / [`GitError::Spawn`] if `git worktree
    /// remove` fails, so a caller that cares about a leaked checkout hears about it.
    pub fn remove(mut self) -> Result<(), GitError> {
        match self.path.take() {
            Some(dir) => remove_worktree(&self.repo, &dir),
            None => Ok(()),
        }
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        // Best-effort teardown for the panic / early-return path; a caller wanting to
        // observe failure calls `remove` instead. Errors are swallowed because Drop
        // cannot report and a leaked temp worktree is not worth aborting over.
        if let Some(dir) = self.path.take() {
            remove_worktree(&self.repo, &dir).ok();
        }
    }
}

/// Remove a worktree checkout with `git worktree remove --force`, then prune the
/// temp directory if git left anything behind.
///
/// `--force` is required because the witness perturbation left the checkout dirty;
/// the tree is disposable, so its dirtiness is not a reason to keep it. After git's
/// own removal, a stray temp directory is cleared best-effort so no litter remains.
fn remove_worktree(repo: &Path, dir: &Path) -> Result<(), GitError> {
    let dir_str = dir.to_string_lossy().into_owned();
    let result = run_git(repo, &["worktree", "remove", "--force", &dir_str])?.into_result();
    // Whether or not git removed it, make sure the temp directory is gone.
    std::fs::remove_dir_all(dir).ok();
    result
}

/// A fresh, unique temporary directory *path* (not yet a worktree) under the system
/// temp root, named so concurrent `claim add` runs never collide.
///
/// The directory is created empty and handed to `git worktree add`, which populates
/// it. Uniqueness comes from the process id plus a monotonic counter, avoiding a
/// dependency purely for temp-name generation.
fn unique_temp_dir() -> Result<PathBuf, GitError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!("claim-witness-{}-{n}", std::process::id());
    let dir = std::env::temp_dir().join(name);
    std::fs::create_dir(&dir).map_err(|source| GitError::Io {
        context: format!(
            "failed to create a temp worktree directory at {}",
            dir.display()
        ),
        source,
    })?;
    Ok(dir)
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

/// Read one git config value, trimmed. An unset value maps to a clear "set it"
/// error naming the key.
fn git_config(dir: &Path, key: &str) -> Result<String, GitError> {
    let out = run_git(dir, &["config", key])?;
    out.success_stdout()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| GitError::MissingIdentity {
            key: key.to_owned(),
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
    fn into_result(self) -> Result<(), GitError> {
        if self.ok {
            Ok(())
        } else {
            Err(GitError::CommandFailed {
                args: self.args,
                stderr: self.stderr.trim().to_owned(),
            })
        }
    }
}

/// Run `git` with `args` in `dir`, capturing output.
///
/// A failure to *spawn* git at all (git not installed) is an error; a git command
/// that runs and exits non-zero is not — that is data the caller classifies (an
/// unborn HEAD, an unset config key), so it comes back in [`GitOutput::ok`].
fn run_git(dir: &Path, args: &[&str]) -> Result<GitOutput, GitError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|source| GitError::Spawn {
            args: args.join(" "),
            source,
        })?;
    Ok(GitOutput {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        args: args.join(" "),
    })
}
