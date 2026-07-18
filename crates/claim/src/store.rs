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

use crate::apperror::{app, ErrorKind};

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
    /// A malformed or unreadable claim, and a duplicate id shared by two files,
    /// do **not** fail the whole load: each becomes a reported [`LoadError`] in the
    /// returned [`StoreLoad`] while the well-formed, unique claims still come back.
    /// This is deliberate — one teammate's typo must nag about *that* file, not
    /// deny the whole store's nag (a claim that rots unseen behind a broken sibling
    /// is exactly the false-green this tool exists to prevent, and so is a store
    /// that goes silent because a neighbor is broken). A caller reports the errors,
    /// lists the good claims, and exits non-zero because errors are present.
    ///
    /// # Errors
    ///
    /// Fails only for a fault that makes the *corpus itself* unreadable — the
    /// `.claims/` directory cannot be listed. A single bad claim file is a
    /// [`LoadError`], not an `Err`.
    pub fn load_all(&self) -> Result<StoreLoad> {
        let claims_dir = self.claims_dir();
        let log_dir = self.log_dir();
        let mut files = Vec::new();
        collect_claim_files(&claims_dir, &log_dir, &mut files)?;

        let mut claims: Vec<LoadedClaim> = Vec::with_capacity(files.len());
        let mut errors: Vec<LoadError> = Vec::new();
        for path in files {
            let rel = path
                .strip_prefix(self.root())
                .unwrap_or(&path)
                .display()
                .to_string();
            match load_one(&path, &rel) {
                Ok(claim) => claims.push(LoadedClaim { claim, path: rel }),
                Err(message) => errors.push(LoadError { file: rel, message }),
            }
        }

        // Sort by id so the order is stable across platforms and a JSON array or a
        // human table reads the same every run. A stable sort keeps two same-id
        // entries in filename order, which `reject_duplicate_ids` relies on for a
        // deterministic "kept vs conflicting" message.
        claims.sort_by(|a, b| a.claim.id.cmp(&b.claim.id));
        reject_duplicate_ids(&mut claims, &mut errors);
        errors.sort_by(|a, b| a.file.cmp(&b.file));

        Ok(StoreLoad { claims, errors })
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
/// Fails when no `.claims/` is found within the boundary. The error carries
/// [`ErrorKind::NoStore`], so every verb that calls `discover` reports a missing
/// store with the same machine-readable `kind` an item-7 agent branches on — the
/// mapping lives here, at the single discovery point, not re-derived per verb. The
/// message points at `claim init`.
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
    Err(app(
        ErrorKind::NoStore,
        format!(
            "no claim store found in {} or any parent directory up to the git repository root; \
             run `claim init` to create one",
            start.display()
        ),
    ))
}

/// The result of loading a store's claims: the ones that parsed cleanly and are
/// unique, plus a per-file error for each that did not.
///
/// Separating the two lets every read verb honor one rule: report the broken files
/// loudly (and exit non-zero), yet still act on the good ones. A store is never
/// silenced by a single bad file. Callers treat a non-empty `errors` as an exit-2
/// condition — loud — while listing/checking `claims` as usual — useful.
#[derive(Debug, Clone)]
pub struct StoreLoad {
    /// The well-formed, unique claims, sorted by id.
    pub claims: Vec<LoadedClaim>,
    /// One entry per file that could not be read, did not parse, or shared an id
    /// with another file. Sorted by file path for a stable report.
    pub errors: Vec<LoadError>,
}

/// A single claim file that failed to load, named for a human to fix.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LoadError {
    /// The offending file's path relative to the store root.
    pub file: String,
    /// Why it failed, phrased so the author can fix it — a parse reason, an I/O
    /// error, or a duplicate-id conflict naming the other file.
    pub message: String,
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

/// Read and parse one claim file, returning a bare reason on failure.
///
/// The reason is bare (not an `Err`/`anyhow`) so the caller folds it into a
/// [`LoadError`] alongside the file path, keeping the per-file degradation in one
/// place.
fn load_one(path: &Path, rel: &str) -> std::result::Result<Claim, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("could not be read: {e}"))?;
    parse_claim_file(rel, &text).map_err(|e| e.to_string())
}

/// Detect two or more claims sharing one id and turn every such conflict into a
/// [`LoadError`] on each file, removing the ambiguous claims from `claims`.
///
/// A shared id is a false-green waiting to happen: both files' verdicts land under
/// `.claims/log/<id>/` and interleave, so `compute_status` reads a mixed history —
/// a genuinely drifted fact can read `verified` and vanish from the drift queue.
/// The tool must not pick a winner (which claim's history is "the" history is
/// unknowable), so both are dropped and both are reported, naming each other. This
/// runs on the id-sorted list, so equal ids are adjacent.
fn reject_duplicate_ids(claims: &mut Vec<LoadedClaim>, errors: &mut Vec<LoadError>) {
    let mut kept: Vec<LoadedClaim> = Vec::with_capacity(claims.len());
    let mut i = 0;
    while i < claims.len() {
        let id = claims[i].claim.id.clone();
        let group_end = claims[i + 1..]
            .iter()
            .position(|c| c.claim.id != id)
            .map_or(claims.len(), |offset| i + 1 + offset);

        if group_end - i == 1 {
            kept.push(claims[i].clone());
        } else {
            let files: Vec<&str> = claims[i..group_end]
                .iter()
                .map(|c| c.path.as_str())
                .collect();
            for c in &claims[i..group_end] {
                let others: Vec<&str> = files
                    .iter()
                    .copied()
                    .filter(|f| *f != c.path.as_str())
                    .collect();
                errors.push(LoadError {
                    file: c.path.clone(),
                    message: format!(
                        "duplicate claim id '{}': also declared in {}. Two files sharing an id \
                         share one verdict log, so their histories interleave and a drifted fact \
                         can read as verified. Give each claim a unique id.",
                        id,
                        others.join(", ")
                    ),
                });
            }
        }
        i = group_end;
    }
    *claims = kept;
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
        // treated as empty rather than fatal — there is nothing there to collect.
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A claim file body for id `id`, minimal and valid.
    fn claim_text(id: &str) -> String {
        format!(
            "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: on-change\nmax-age: 30d\n---\nS.\n"
        )
    }

    /// A store scaffolded in a fresh temp dir, ready for `write` + `load_all`.
    fn store() -> (TempDir, Store) {
        let dir = TempDir::new().unwrap();
        let (store, _) = Store::init(dir.path()).unwrap();
        (dir, store)
    }

    fn write(store: &Store, rel: &str, contents: &str) {
        let path = store.root().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn load_all_returns_well_formed_claims_sorted_by_id() {
        let (_dir, store) = store();
        write(&store, ".claims/b.md", &claim_text("b"));
        write(&store, ".claims/a.md", &claim_text("a"));
        let load = store.load_all().unwrap();
        let ids: Vec<&str> = load.claims.iter().map(|c| c.claim.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"], "sorted by id");
        assert!(load.errors.is_empty());
    }

    #[test]
    fn load_all_reports_a_malformed_file_without_dropping_the_good_ones() {
        // M1: one bad file is its own error; the good claim still loads.
        let (_dir, store) = store();
        write(&store, ".claims/good.md", &claim_text("good"));
        write(&store, ".claims/bad.md", "not a claim, no frontmatter\n");
        let load = store.load_all().unwrap();
        assert_eq!(load.claims.len(), 1);
        assert_eq!(load.claims[0].claim.id.as_str(), "good");
        assert_eq!(load.errors.len(), 1);
        assert_eq!(load.errors[0].file, ".claims/bad.md");
    }

    #[test]
    fn load_all_rejects_a_duplicate_id_across_two_files_naming_both() {
        // C1: two files declaring the same id are both dropped and both reported,
        // each naming the other, because their verdict logs would interleave.
        let (_dir, store) = store();
        write(&store, ".claims/one.md", &claim_text("shared"));
        write(&store, ".claims/two.md", &claim_text("shared"));
        let load = store.load_all().unwrap();
        assert!(
            load.claims.is_empty(),
            "an ambiguous claim is not returned as usable"
        );
        assert_eq!(load.errors.len(), 2);
        let files: Vec<&str> = load.errors.iter().map(|e| e.file.as_str()).collect();
        assert!(files.contains(&".claims/one.md"));
        assert!(files.contains(&".claims/two.md"));
        // Each error names the *other* file and the shared id.
        let one = load
            .errors
            .iter()
            .find(|e| e.file == ".claims/one.md")
            .unwrap();
        assert!(one.message.contains(".claims/two.md"));
        assert!(one.message.contains("duplicate claim id 'shared'"));
    }

    #[test]
    fn load_all_keeps_a_third_unique_claim_alongside_a_duplicate_pair() {
        // The duplicate pair is dropped, but an unrelated well-formed claim stays.
        let (_dir, store) = store();
        write(&store, ".claims/one.md", &claim_text("shared"));
        write(&store, ".claims/two.md", &claim_text("shared"));
        write(&store, ".claims/unique.md", &claim_text("unique"));
        let load = store.load_all().unwrap();
        assert_eq!(load.claims.len(), 1);
        assert_eq!(load.claims[0].claim.id.as_str(), "unique");
        assert_eq!(load.errors.len(), 2);
    }

    #[test]
    fn load_all_skips_the_verdict_log_tree() {
        let (_dir, store) = store();
        write(&store, ".claims/a.md", &claim_text("a"));
        // A stray .md inside the log dir must not be parsed as a claim.
        write(&store, ".claims/log/a/note.md", "not a claim\n");
        let load = store.load_all().unwrap();
        assert_eq!(load.claims.len(), 1);
        assert!(
            load.errors.is_empty(),
            "the log tree is excluded, not parsed"
        );
    }
}
