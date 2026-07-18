//! Errors raised while locating a store or resolving git provenance.
//!
//! These are library errors ([`thiserror`]), not binary errors: a store is
//! shared by the CLI and the MCP server, and each maps a failure to its own
//! surface — the CLI to a `--json` error object with a stable `kind`, the MCP
//! server to a protocol error. Raising a typed enum here (rather than an
//! `anyhow` string) is what lets both recover the *reason* a store could not be
//! opened, above all "no store found", without matching on English prose.

use std::path::PathBuf;

/// A failure to locate or read a claim store.
///
/// The variant a consumer branches on most is [`StoreError::NoStore`]: both
/// binaries turn it into a distinct, machine-readable signal (the CLI's
/// `no-store` `kind`, the MCP server's own not-found error) so an agent knows to
/// suggest `claim init` rather than retrying. Every other failure is an
/// environment fault the caller reports verbatim.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// No `.claims/` directory was found from the start directory up to the git
    /// boundary. The message names the start directory and points at `claim
    /// init`; a consumer that needs the machine signal matches this variant.
    #[error(
        "no claim store found in {start} or any parent directory up to the git repository root; \
         run `claim init` to create one"
    )]
    NoStore {
        /// The directory the search started from, named so the message is
        /// actionable.
        start: PathBuf,
    },

    /// A `.claims` path exists but is a file, not a directory, so it cannot be a
    /// store. Loud rather than silently ignored: a stray `.claims` file hides the
    /// real problem.
    #[error("{path} exists but is not a directory; move it aside before creating a claim store")]
    NotADirectory {
        /// The offending `.claims` path.
        path: PathBuf,
    },

    /// An I/O fault made the store itself unreadable — the directory could not be
    /// created, listed, or stat-ed. Distinct from a single malformed claim file,
    /// which is a [`crate::LoadError`], not an error that fails the whole load.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted, naming the path, so the message is
        /// actionable.
        context: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

/// A failure to resolve git provenance for a verdict entry.
///
/// Kept separate from [`StoreError`] because git provenance is a distinct
/// concern from store location: a store can be found while git is misconfigured,
/// and vice versa. Every variant is a real misconfiguration for a git-native
/// tool (invariant #4, a write to the truth is a commit), reported with the fix.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GitError {
    /// `HEAD` could not be resolved and the repository is not merely unborn — it
    /// is not a git repository, or `HEAD` is corrupt. An unborn HEAD is *not* an
    /// error: it resolves to [`crate::git::UNBORN_HEAD_SENTINEL`].
    #[error(
        "could not resolve HEAD in {dir} (not a git repository, or HEAD is corrupt); \
         a claim store lives in a git repo — run `git init` or `claim` from inside \
         your repository. If HEAD is corrupt, repair it before recording a verdict."
    )]
    UnresolvableHead {
        /// The directory git discovery started from.
        dir: PathBuf,
    },

    /// A required git identity (`user.name` or `user.email`) is unset, so a
    /// verdict would be unattributable. The message names the `git config`
    /// command to set it.
    #[error(
        "git {key} is not set; a verdict needs an attributable author. \
         Set it with `git config {key} \"...\"`"
    )]
    MissingIdentity {
        /// The unset config key (`user.name` or `user.email`).
        key: String,
    },

    /// A git command that was expected to succeed failed; the message carries
    /// git's own stderr.
    #[error("`git {args}` failed: {stderr}")]
    CommandFailed {
        /// The git subcommand and arguments that failed.
        args: String,
        /// Git's stderr, trimmed.
        stderr: String,
    },

    /// The `git` binary could not be spawned at all — it is not installed or not
    /// on `PATH`. Distinct from a git command that ran and exited non-zero.
    #[error("failed to run `git {args}`; is git installed and on PATH? ({source})")]
    Spawn {
        /// The git subcommand and arguments that could not be spawned.
        args: String,
        /// The underlying spawn error.
        source: std::io::Error,
    },

    /// A filesystem fault while preparing a temporary worktree for the optional
    /// witnessed-red dance — the temp directory could not be created. Distinct from
    /// a git command failure so the message can name the path.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted, naming the path, so the message is actionable.
        context: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}
