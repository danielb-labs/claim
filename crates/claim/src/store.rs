//! Locating and scaffolding a claim store on disk.
//!
//! A store is a `.claims/` directory holding claim files (`.claims/**/*.md`) and
//! the verdict log (`.claims/log/<id>/`). This module owns two questions every
//! verb shares, so they are answered in exactly one place:
//!
//! - **Where is the store?** [`discover`] walks up from a starting directory to
//!   the nearest ancestor containing a `.claims/`, the same way git finds `.git/`.
//!   `claim add` (and later `check`, `list`, `log`, …) call this so they work from
//!   anywhere inside a repository, not only its root.
//! - **How is one created?** [`Store::init`] scaffolds `.claims/` and
//!   `.claims/log/` in a chosen directory, idempotently.
//!
//! The discovery rule is a deliberate contract: the store root is the directory
//! *containing* `.claims/`, and there is exactly one per walk (the nearest),
//! because a claim's `cmd` check paths are written relative to that root
//! ([`claim_core::CheckContext::cwd`]). Anchoring every later command to the same
//! root is what keeps a check running against the tree its author wrote it for.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The store directory name, relative to the store root.
pub const CLAIMS_DIR: &str = ".claims";

/// The verdict-log subdirectory, relative to the store root.
pub const LOG_DIR: &str = "log";

/// A located claim store: the root directory that contains its `.claims/`.
///
/// Holding the *root* (not the `.claims/` path) is deliberate: it is the working
/// directory a claim's `cmd` check runs in and the base its relative paths and
/// `supports` targets resolve against, so every consumer derives those from one
/// place and cannot disagree about where the tree begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// The repository root that contains this store's `.claims/`. A check runs
    /// here and relative paths resolve against it.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The `.claims/` directory itself.
    #[must_use]
    pub fn claims_dir(&self) -> PathBuf {
        self.root.join(CLAIMS_DIR)
    }

    /// The verdict-log root, `.claims/log/`, passed to [`claim_core::append_entry`]
    /// and [`claim_core::read_entries`].
    #[must_use]
    pub fn log_dir(&self) -> PathBuf {
        self.claims_dir().join(LOG_DIR)
    }

    /// The on-disk path of a claim's definition file: `.claims/<id>.md`.
    ///
    /// A namespaced id nests, matching how the verdict log nests a namespaced id
    /// into directories: `payments/libfoo-pin` becomes
    /// `.claims/payments/libfoo-pin.md`. This is safe because a [`claim_core::ClaimId`]
    /// is already validated to contain only `[a-z0-9-/]` with clean, non-empty
    /// segments — no `.`/`..` and nothing that could escape the store.
    #[must_use]
    pub fn claim_file(&self, id: &claim_core::ClaimId) -> PathBuf {
        let mut path = self.claims_dir();
        for segment in id.as_str().split('/') {
            path.push(segment);
        }
        path.set_extension("md");
        path
    }

    /// The claim file path relative to the store root, e.g.
    /// `.claims/payments/libfoo-pin.md`.
    ///
    /// Used as the `path` argument to [`claim_core::parse_claim_file`], so a parse
    /// error names the file the way the user will see it on disk (relative to the
    /// repo root), not an absolute temp path.
    #[must_use]
    pub fn claim_file_relative(&self, id: &claim_core::ClaimId) -> String {
        let mut path = PathBuf::from(CLAIMS_DIR);
        for segment in id.as_str().split('/') {
            path.push(segment);
        }
        path.set_extension("md");
        path.display().to_string()
    }

    /// Scaffold a store under `root`, creating `.claims/` and `.claims/log/`.
    ///
    /// Idempotent: running it against a directory that already has a store is a
    /// no-op that succeeds, so `claim init` can be re-run safely (for example after
    /// a partial first run). Returns the [`Store`] and whether it created anything
    /// new, so the caller can report "created" versus "already present" honestly.
    ///
    /// # Errors
    ///
    /// Fails if the directories cannot be created, or if `.claims` exists but is a
    /// file rather than a directory — a loud error, because silently treating a
    /// stray `.claims` file as a store would hide the real problem.
    pub fn init(root: impl Into<PathBuf>) -> Result<(Self, bool)> {
        let root = root.into();
        let claims = root.join(CLAIMS_DIR);

        if claims.exists() && !claims.is_dir() {
            bail!(
                "{} exists but is not a directory; move it aside before creating a claim store",
                claims.display()
            );
        }

        let existed = claims.is_dir();
        let log = claims.join(LOG_DIR);
        std::fs::create_dir_all(&log)
            .with_context(|| format!("failed to create the store at {}", claims.display()))?;

        let created = !existed;
        Ok((Store { root }, created))
    }
}

/// Find the store by walking up from `start` to the nearest ancestor that contains
/// a `.claims/` directory.
///
/// The nearest match wins, so a nested repository's own store shadows an outer
/// one — the same containment rule git uses for `.git/`. Runs from any directory
/// inside a repository, which is why a verb never assumes it was invoked at the
/// root.
///
/// # Errors
///
/// Fails when no ancestor contains a `.claims/`, with a message pointing at
/// `claim init` — the store has to be scaffolded before a claim can be added to
/// it.
pub fn discover(start: &Path) -> Result<Store> {
    for dir in start.ancestors() {
        let candidate = dir.join(CLAIMS_DIR);
        if candidate.is_dir() {
            return Ok(Store {
                root: dir.to_path_buf(),
            });
        }
    }
    bail!(
        "no claim store found in {} or any parent directory; run `claim init` to create one",
        start.display()
    )
}
