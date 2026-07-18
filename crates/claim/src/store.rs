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
//! - **What claims does it hold?** [`Store::load_all`] walks `.claims/**/*.md`,
//!   parses each, and returns the whole corpus, so the read/verify verbs
//!   (`check`, `list`, `log`, `drift`) share one definition of "every claim in
//!   the store" rather than each re-deriving the walk.
//!
//! The discovery rule is a deliberate contract: the store root is the directory
//! *containing* `.claims/`, and there is exactly one per walk (the nearest),
//! because a claim's `cmd` check paths are written relative to that root
//! ([`claim_core::CheckContext::cwd`]). Anchoring every later command to the same
//! root is what keeps a check running against the tree its author wrote it for.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use claim_core::{parse_claim_file, Claim};

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

    /// Parse every claim in the store: `.claims/**/*.md`, excluding the verdict
    /// log.
    ///
    /// The one place the read/verify verbs agree on what "the store's claims" are,
    /// so `check`, `list`, `log`, and `drift` cannot disagree about which files
    /// count. Returns them sorted by id for a stable, deterministic order (a
    /// directory listing is order-unspecified across platforms), so a JSON array or
    /// a human table reads the same on every run.
    ///
    /// Two directories under `.claims/` are deliberately skipped: `log/`, which
    /// holds verdict-log JSON (`read_entries` owns those), and any file that is not
    /// a `.md`. Everything else under `.claims/` is a standalone claim file, parsed
    /// with [`claim_core::parse_claim_file`] and named — in a parse error — the way
    /// it appears on disk relative to the store root.
    ///
    /// Embedded claims (`<!-- claim ... -->` blocks in host files like CLAUDE.md)
    /// are *not* collected here: v1 authoring writes standalone `.claims/*.md`
    /// files, and a store walk that also scraped every text file in the repo would
    /// be both slow and surprising. Harvesting embedded claims is its own later
    /// concern.
    ///
    /// # Errors
    ///
    /// Fails if the `.claims/` directory cannot be listed, or if any claim file
    /// cannot be read or does not parse — a malformed claim is a loud error naming
    /// the file, never a silently dropped one. One bad file fails the whole load:
    /// a verb that quietly skipped an unparseable claim could report a store as
    /// clean while a claim in it rots unseen, exactly the false-green this tool
    /// exists to prevent.
    pub fn load_all(&self) -> Result<Vec<LoadedClaim>> {
        let claims_dir = self.claims_dir();
        let log_dir = self.log_dir();
        let mut loaded = Vec::new();
        collect_claim_files(&claims_dir, &log_dir, &mut loaded)?;

        let mut parsed: Vec<LoadedClaim> = Vec::with_capacity(loaded.len());
        for path in loaded {
            let rel = path
                .strip_prefix(self.root())
                .unwrap_or(&path)
                .display()
                .to_string();
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read claim file {rel}"))?;
            let claim = parse_claim_file(&rel, &text)
                .with_context(|| format!("failed to parse claim file {rel}"))?;
            parsed.push(LoadedClaim { claim, path: rel });
        }

        parsed.sort_by(|a, b| a.claim.id.cmp(&b.claim.id));
        Ok(parsed)
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
/// a `.claims/` directory, stopping at the git-repository boundary.
///
/// The nearest match wins, so a nested repository's own store shadows an outer
/// one — the same containment rule git uses for `.git/`. Runs from any directory
/// inside a repository, which is why a verb never assumes it was invoked at the
/// root.
///
/// The walk stops at the first ancestor that *is* a git repository (contains a
/// `.git`): a `.claims/` found strictly above that boundary belongs to a different
/// repository (or a stray `$HOME/.claims`), and adopting it would stamp provenance
/// from the wrong repo onto claims in this one. A `.claims/` at the repo root or
/// below is inside the boundary and accepted. When `start` is not inside any git
/// repository, no boundary is hit and the walk proceeds to the filesystem root — a
/// bare `.claims/` with no repo is still a usable store for the CLI, and `add` will
/// separately refuse for lack of a commit to attribute.
///
/// # Errors
///
/// Fails when no `.claims/` is found within the boundary, with a message pointing
/// at `claim init` — the store has to be scaffolded before a claim can be added to
/// it.
pub fn discover(start: &Path) -> Result<Store> {
    for dir in start.ancestors() {
        let candidate = dir.join(CLAIMS_DIR);
        if candidate.is_dir() {
            return Ok(Store {
                root: dir.to_path_buf(),
            });
        }
        // The git boundary: a `.claims/` above this belongs to another repo, so do
        // not adopt it. Checked *after* this level's `.claims/` so a store at the
        // repo root (alongside `.git`) is still found.
        if dir.join(".git").exists() {
            break;
        }
    }
    bail!(
        "no claim store found in {} or any parent directory up to the git repository root; \
         run `claim init` to create one",
        start.display()
    )
}

/// A claim parsed from the store, paired with the store-relative path it lives at.
///
/// Bundling the parsed [`Claim`] with its store-relative path (for display and
/// error messages, the way a user sees it on disk) means a caller never re-derives
/// a path from the id and risks disagreeing with where the file actually is. A
/// caller needing the absolute path joins it onto [`Store::root`].
#[derive(Debug, Clone)]
pub struct LoadedClaim {
    /// The parsed claim.
    pub claim: Claim,
    /// The claim file's path relative to the store root, e.g.
    /// `.claims/payments/libfoo-pin.md`. What a human sees and what a parse error
    /// names.
    pub path: String,
}

/// Recursively collect standalone claim files under `dir`, skipping the verdict
/// log directory and any non-`.md` file.
///
/// Pushes absolute paths into `out`; the caller parses them. The `log_dir` guard
/// is by path equality (not name), so a claim legitimately named `log.md` at the
/// store root is still collected — only the actual `.claims/log/` tree is
/// excluded.
fn collect_claim_files(dir: &Path, log_dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // A store whose `.claims/` was removed out from under us is not a claim
        // corpus at all; but a missing directory during recursion (a race) is
        // treated as empty rather than fatal — there is simply nothing there.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e).with_context(|| format!("failed to list {}", dir.display()));
        }
    };

    for entry in entries {
        let entry =
            entry.with_context(|| format!("failed to read an entry in {}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to stat {}", path.display()))?;

        if file_type.is_dir() {
            // The verdict log is not a claim corpus; its JSON entries are owned by
            // `read_entries`, not this walk.
            if path == log_dir {
                continue;
            }
            collect_claim_files(&path, log_dir, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
    Ok(())
}
