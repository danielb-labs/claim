//! Locating and scaffolding a claim store on disk.
//!
//! A store is a `.claims/` directory holding claim files (`.claims/**/*.md`).
//! There is no committed verdict log: a verdict is telemetry the hub stores, not
//! source (see `docs/design/CLI-HUB-BOUNDARY.md`), so the store holds only the
//! claims themselves. This module owns the questions the CLI's read/verify verbs
//! share — where the store is, what its claims are — so they are answered in exactly
//! one place and no verb can drift from another:
//!
//! - **Where is the store?** [`discover`] walks up from a starting directory to
//!   the nearest ancestor containing a `.claims/`, the same way git finds `.git/`.
//!   Every consumer calls this so it works from anywhere inside a repository, not
//!   only its root.
//! - **How is one created?** [`Store::init`] scaffolds `.claims/` in a chosen
//!   directory, idempotently.
//! - **What claims does it hold?** [`Store::load_all`] walks `.claims/**/*.md`,
//!   parses each file that opens with a frontmatter fence (a plain `README.md` is
//!   a document, not a claim, and is skipped), and returns the whole corpus, so
//!   every consumer shares one definition of "every claim in the store" rather
//!   than each re-deriving the walk.
//!
//! The discovery rule is a deliberate contract: the store root is the directory
//! *containing* `.claims/`, and there is exactly one per walk (the nearest),
//! because a claim's `cmd` check paths are written relative to that root
//! ([`claim_core::CheckContext::cwd`]). Anchoring every consumer to the same root
//! is what keeps a check running against the tree its author wrote it for.

use std::path::{Path, PathBuf};

use claim_core::{parse_claim_file, Claim};

use crate::error::StoreError;

/// The store directory name, relative to the store root.
pub const CLAIMS_DIR: &str = ".claims";

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

    /// The on-disk path of a claim's definition file: `.claims/<id>.md`.
    ///
    /// A namespaced id nests into directories: `payments/libfoo-pin` becomes
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

    /// Parse every claim in the store: `.claims/**/*.md`.
    ///
    /// The one place every consumer agrees on what "the store's claims" are, so
    /// the CLI's `check`, `list`, and `drift` cannot disagree about which files
    /// count. Returns them sorted by id for a stable,
    /// deterministic order (a directory listing is order-unspecified across
    /// platforms), so a JSON array or a human table reads the same on every run.
    ///
    /// Two things under `.claims/` are deliberately skipped: any file that is not a
    /// `.md`; and any `.md` that does not open with a `---` frontmatter fence — a
    /// plain document (a `README.md`) is not a claim and must not break the store. A
    /// `.md` that *does* open with a fence is parsed with
    /// [`claim_core::parse_claim_file`] and named — in a parse error — the way it
    /// appears on disk relative to the store root, so a fenced-but-malformed claim
    /// stays a loud error rather than being silently dropped. See
    /// `collect_claim_files` for the exact rule and its trade-off.
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
    pub fn load_all(&self) -> Result<StoreLoad, StoreError> {
        let claims_dir = self.claims_dir();
        let mut files = Vec::new();
        collect_claim_files(&claims_dir, &mut files)?;

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
                Err(message) => errors.push(LoadError {
                    file: rel,
                    message,
                    // A parse or read failure produced no claim, so there is no id
                    // to key on; this file is matched by its path stem instead.
                    id: None,
                }),
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

    /// Scaffold a store under `root`, creating `.claims/`.
    ///
    /// Idempotent: running it against a directory that already has a store is a
    /// no-op that succeeds, so `claim init` can be re-run safely (for example after
    /// a partial first run). Returns the [`Store`] and whether it created anything
    /// new, so the caller can report "created" versus "already present" honestly.
    ///
    /// # Errors
    ///
    /// Fails if the directory cannot be created, or if `.claims` exists but is a
    /// file rather than a directory — a loud error, because silently treating a
    /// stray `.claims` file as a store would hide the real problem.
    pub fn init(root: impl Into<PathBuf>) -> Result<(Self, bool), StoreError> {
        let root = root.into();
        let claims = root.join(CLAIMS_DIR);

        if claims.exists() && !claims.is_dir() {
            return Err(StoreError::NotADirectory { path: claims });
        }

        let existed = claims.is_dir();
        std::fs::create_dir_all(&claims).map_err(|source| StoreError::Io {
            context: format!("failed to create the store at {}", claims.display()),
            source,
        })?;

        let created = !existed;
        Ok((Store { root }, created))
    }
}

/// Find the store by walking up from `start` to the nearest ancestor that contains
/// a `.claims/` directory, stopping at the git-repository boundary.
///
/// The nearest match wins, so a nested repository's own store shadows an outer
/// one — the same containment rule git uses for `.git/`. Runs from any directory
/// inside a repository, which is why a consumer never assumes it was invoked at
/// the root.
///
/// The walk stops at the first ancestor that *is* a git repository (contains a
/// `.git`): a `.claims/` found strictly above that boundary belongs to a different
/// repository (or a stray `$HOME/.claims`), and adopting it would stamp provenance
/// from the wrong repo onto claims in this one. A `.claims/` at the repo root or
/// below is inside the boundary and accepted. When `start` is not inside any git
/// repository, no boundary is hit and the walk proceeds to the filesystem root — a
/// bare `.claims/` with no repo is still a usable store for reads, and a write
/// verb will separately refuse for lack of a commit to attribute.
///
/// # Errors
///
/// Returns [`StoreError::NoStore`] when no `.claims/` is found within the
/// boundary. That variant is the single, machine-recognizable "run `claim init`"
/// signal the CLI branches on — the classification lives here, at the one
/// discovery point, not re-derived per consumer.
pub fn discover(start: &Path) -> Result<Store, StoreError> {
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
    Err(StoreError::NoStore {
        start: start.to_path_buf(),
    })
}

/// The result of loading a store's claims: the ones that parsed cleanly and are
/// unique, plus a per-file error for each that did not.
///
/// Separating the two lets every consumer honor one rule: report the broken files
/// loudly (and exit non-zero, or surface them as error entries), yet still act on
/// the good ones. A store is never silenced by a single bad file. The CLI treats a
/// non-empty `errors` as an exit-2 condition — loud — while listing/checking
/// `claims` as usual — useful.
#[derive(Debug, Clone)]
pub struct StoreLoad {
    /// The well-formed, unique claims, sorted by id.
    pub claims: Vec<LoadedClaim>,
    /// One entry per file that could not be read, did not parse, or shared an id
    /// with another file. Sorted by file path for a stable report.
    pub errors: Vec<LoadError>,
}

/// The outcome of resolving a single claim id against a loaded store, for the
/// verbs that act on one named claim (`show`, `retire`).
///
/// Distinct from a plain `Option<&LoadedClaim>` because "not shown" has three
/// causes that demand three different messages, and conflating them is a real bug:
/// a *duplicate* id that was dropped is **not** "no such claim" (it exists twice),
/// and a *broken* file named for the id reports *why* it broke. Only
/// [`NotFound`](Resolved::NotFound) is a true unknown id. Every non-[`Found`](Resolved::Found)
/// outcome is still exit 2 — no false green — but the diagnosis a caller prints is
/// now the honest one.
#[derive(Debug)]
pub enum Resolved<'a> {
    /// A single, well-formed claim with this id. The one printable/actionable case.
    Found(&'a LoadedClaim),
    /// Two or more files declared this id, so all were dropped ([`load_all`] refuses
    /// to pick a winner). Carries one of the conflict errors, whose `message` names
    /// the colliding files, so the caller can say "declared twice" rather than "no
    /// such claim".
    ///
    /// [`load_all`]: Store::load_all
    Duplicate(&'a LoadError),
    /// A file whose path is this id failed to parse or could not be read, so no claim
    /// was produced. Carries that file's error so the caller can surface *why* it
    /// could not be shown, distinguishing a broken file from a typo.
    LoadFailed(&'a LoadError),
    /// No claim, no same-id conflict, and no same-id broken file: a genuine unknown
    /// id (a typo, or a claim that never existed).
    NotFound,
}

impl StoreLoad {
    /// Resolve one claim id to a precise outcome, so a single-claim verb prints the
    /// honest diagnosis rather than a blanket "not found".
    ///
    /// The order is deliberate: a clean match wins; else a *duplicate* conflict for
    /// this id (structurally keyed on [`LoadError::id`], never by scanning prose);
    /// else a *broken file* whose path stem is this id; else a true [`Resolved::NotFound`].
    /// A duplicate is checked before a stem match because a duplicated claim's files
    /// are named for *their own* paths (which need not equal the id), so the id-keyed
    /// check is the only one that catches it — and reporting "declared twice" for a
    /// claim that exists twice, not "no such claim", is the whole point of this
    /// method (a bug both `show` and `retire` shared before it existed).
    ///
    /// An unrelated broken *sibling* (a different id) is never returned: this answers
    /// only about the requested id. Store-wide health is `list`/`check`'s concern.
    #[must_use]
    pub fn resolve(&self, id: &str) -> Resolved<'_> {
        if let Some(loaded) = self.claims.iter().find(|c| c.claim.id.as_str() == id) {
            return Resolved::Found(loaded);
        }
        if let Some(err) = self
            .errors
            .iter()
            .find(|e| e.id.as_ref().is_some_and(|eid| eid.as_str() == id))
        {
            return Resolved::Duplicate(err);
        }
        if let Some(err) = self
            .errors
            .iter()
            .find(|e| file_stem_matches_id(&e.file, id))
        {
            return Resolved::LoadFailed(err);
        }
        Resolved::NotFound
    }
}

/// Whether a load-errored file's path could be the file for `id`: its `.md` stem,
/// relative to `.claims/`, equals the id. So an unparseable file named after the
/// requested id is matched to *that* id, and its error reported, rather than the
/// lookup falling through to "not found".
///
/// This is the inverse of [`Store::claim_file_relative`]'s id→path mapping, for the
/// broken-file case where the file did not parse so its id is unknown but its path
/// is not. Lives here, beside `resolve`, so the two single-claim verbs share one
/// definition and cannot disagree.
fn file_stem_matches_id(file: &str, id: &str) -> bool {
    file.strip_prefix(&format!("{CLAIMS_DIR}/"))
        .and_then(|rest| rest.strip_suffix(".md"))
        .is_some_and(|stem| stem == id)
}

/// A single claim file that failed to load, named for a human to fix.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LoadError {
    /// The offending file's path relative to the store root.
    pub file: String,
    /// Why it failed, phrased so the author can fix it — a parse reason, an I/O
    /// error, or a duplicate-id conflict naming the other file.
    pub message: String,
    /// The claim id this error pertains to, when it is known structurally.
    ///
    /// A *duplicate-id* conflict knows its id (both files parsed and declared it),
    /// so this is `Some(id)` — which is what lets [`StoreLoad::resolve`] answer a
    /// single-id lookup ("show me `X`") with the true "declared twice" diagnosis
    /// instead of a false "no such claim", without scanning the human `message`.
    /// A *parse* or I/O failure has no id to record (the file did not parse), so
    /// this is `None`; such a file is matched by its path stem instead. Skipped in
    /// the serialized form: the `--json` `errors` shape the read verbs emit is
    /// unchanged, since a consumer already has `file` and `message`.
    #[serde(skip)]
    pub id: Option<claim_core::ClaimId>,
}

/// A claim parsed from the store, paired with the store-relative path it lives at.
///
/// Bundling the parsed [`Claim`] with its store-relative path (for display and
/// error messages, the way a user sees it on disk) means a consumer never
/// re-derives a path from the id and risks disagreeing with where the file
/// actually is. A caller needing the absolute path joins it onto [`Store::root`].
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
/// The reason is bare (not an `Err`) so the caller folds it into a [`LoadError`]
/// alongside the file path, keeping the per-file degradation in one place.
fn load_one(path: &Path, rel: &str) -> std::result::Result<Claim, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("could not be read: {e}"))?;
    parse_claim_file(rel, &text).map_err(|e| e.to_string())
}

/// Detect two or more claims sharing one id and turn every such conflict into a
/// [`LoadError`] on each file, removing the ambiguous claims from `claims`.
///
/// A shared id is a false-green waiting to happen: two files claiming to be one
/// fact conflate what a hub records against that id, so a genuinely drifted fact
/// can be masked by its namesake. The tool must not pick a winner (which file's
/// fact is "the" fact is unknowable), so both are dropped and both are reported,
/// naming each other. This runs on the id-sorted list, so equal ids are adjacent.
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
                         conflate the fact recorded against it, so a drifted fact can be masked \
                         by its namesake. Give each claim a unique id.",
                        id,
                        others.join(", ")
                    ),
                    // Both files parsed and declared this id, so it is known: a
                    // single-id lookup keys on this to answer "declared twice"
                    // rather than a false "no such claim".
                    id: Some(id.clone()),
                });
            }
        }
        i = group_end;
    }
    *claims = kept;
}

/// Recursively collect standalone claim files under `dir`, skipping any non-`.md`
/// file and any `.md` file that does not *intend* to be a claim.
///
/// A `.claims/**/*.md` file counts as a claim only when its content — after an
/// optional UTF-8 BOM — opens with a `---` frontmatter fence
/// ([`claim_core::has_frontmatter_fence`]). A `.md` that does not open with the
/// fence (a `README.md` documenting the store, say) is a plain document and is
/// skipped silently: it is not parsed, and it is not an error. Without this rule
/// a single non-claim doc dropped into `.claims/` fails to parse and forces
/// `check`/`list`/`drift` to exit 2, so documenting a store breaks it.
///
/// The trade-off is deliberate and narrow: a file that *meant* to be a claim but
/// whose author forgot the opening `---` fence is silently skipped rather than
/// flagged. That is acceptable — a claim file without frontmatter is not a valid
/// claim file (it has no id, no check), and the alternative (one stray README
/// taking the whole store's nag offline) is the false-silence this tool exists to
/// prevent, at store scale. A file that *does* open with the fence but has
/// malformed YAML stays a loud [`load_one`] error: it declared its intent to be a
/// claim, so a real-but-broken claim is never dropped (invariant #6).
///
/// A `.md` that cannot be read to inspect its first line is *kept*, not skipped,
/// so [`load_one`] surfaces the read failure loudly rather than this walk deciding
/// an unreadable file is a plain doc.
///
/// Pushes absolute paths into `out`; the caller parses them.
fn collect_claim_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), StoreError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // A store whose `.claims/` was removed out from under us is not a claim
        // corpus at all; but a missing directory during recursion (a race) is
        // treated as empty rather than fatal — there is nothing there to collect.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(StoreError::Io {
                context: format!("failed to list {}", dir.display()),
                source,
            });
        }
    };

    for entry in entries {
        let entry = entry.map_err(|source| StoreError::Io {
            context: format!("failed to read an entry in {}", dir.display()),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| StoreError::Io {
            context: format!("failed to stat {}", path.display()),
            source,
        })?;

        if file_type.is_dir() {
            collect_claim_files(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("md")
            && intends_to_be_a_claim(&path)
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Whether a `.md` file under `.claims/` should be parsed as a claim: its first
/// line opens with a `---` frontmatter fence, or it could not be read to tell.
///
/// A readable file with no opening fence is a plain document and returns `false`
/// (skip it). A file that opens with the fence returns `true` (parse it, so
/// malformed YAML stays loud). An *unreadable* file also returns `true`: we cannot
/// prove it is a plain doc, so it is kept for [`load_one`] to report the read
/// failure rather than silently dropped here. See [`collect_claim_files`] for the
/// rule and its trade-off.
///
/// Only the first line is read — the fence lives there and
/// [`claim_core::has_frontmatter_fence`] looks no further — so this stays cheap on
/// a store of many `.md` files and never re-reads the whole file that [`load_one`]
/// is about to parse.
fn intends_to_be_a_claim(path: &Path) -> bool {
    use std::io::BufRead;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return true,
    };
    let mut first_line = String::new();
    // A read failure mid-file is treated the same as an unreadable file: keep it, so
    // `load_one` surfaces the fault loudly rather than this walk deciding it is a doc.
    match std::io::BufReader::new(file).read_line(&mut first_line) {
        Ok(_) => claim_core::has_frontmatter_fence(&first_line),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A claim file body for id `id`, minimal and valid.
    fn claim_text(id: &str) -> String {
        format!("---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n---\nS.\n")
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
    fn discover_finds_the_nearest_store_and_reports_none_loudly() {
        // A store at the root is found from a nested subdirectory; a directory with
        // no store above it (to the git boundary) reports NoStore.
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        Store::init(dir.path()).unwrap();
        let nested = dir.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        let store = discover(&nested).unwrap();
        assert_eq!(store.root(), dir.path());

        let bare = TempDir::new().unwrap();
        std::fs::create_dir_all(bare.path().join(".git")).unwrap();
        let err = discover(bare.path()).unwrap_err();
        assert!(matches!(err, StoreError::NoStore { .. }));
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
    fn load_all_reports_a_frontmatter_fenced_file_with_bad_yaml_loudly() {
        // M1: a file that *opens with a fence* declares itself a claim, so malformed
        // YAML under it is a loud per-file error, never silently skipped. The good
        // claim still loads (one broken sibling does not silence the store).
        let (_dir, store) = store();
        write(&store, ".claims/good.md", &claim_text("good"));
        write(
            &store,
            ".claims/broken.md",
            "---\nchecks: [unclosed\n---\nS.\n",
        );
        let load = store.load_all().unwrap();
        assert_eq!(load.claims.len(), 1);
        assert_eq!(load.claims[0].claim.id.as_str(), "good");
        assert_eq!(load.errors.len(), 1, "the fenced-but-broken file is loud");
        assert_eq!(load.errors[0].file, ".claims/broken.md");
    }

    #[test]
    fn load_all_skips_a_non_claim_doc_with_no_frontmatter_fence() {
        // The store scanner's core fix: a plain document dropped into `.claims/`
        // (a README) has no opening `---` fence, so it is not a claim — it is
        // skipped, not parsed and not reported as an error. Without this, one stray
        // doc forces `check`/`list`/`drift` to exit 2 and takes the whole store's
        // nag offline.
        let (_dir, store) = store();
        write(&store, ".claims/good.md", &claim_text("good"));
        write(
            &store,
            ".claims/README.md",
            "# My claim store\n\nDocuments the claims here.\n",
        );
        let load = store.load_all().unwrap();
        assert_eq!(load.claims.len(), 1);
        assert_eq!(load.claims[0].claim.id.as_str(), "good");
        assert!(
            load.errors.is_empty(),
            "a non-claim doc is skipped, not an error"
        );
    }

    #[test]
    fn load_all_treats_a_bom_prefixed_fenced_file_as_a_claim() {
        // The fence test tolerates a leading UTF-8 BOM exactly as `split_frontmatter`
        // does, so a BOM-prefixed claim file is neither skipped as a plain doc nor
        // mis-parsed. This guards the two rules staying in lockstep.
        let (_dir, store) = store();
        write(
            &store,
            ".claims/bom.md",
            &format!("\u{feff}{}", claim_text("bom")),
        );
        let load = store.load_all().unwrap();
        assert_eq!(load.claims.len(), 1);
        assert_eq!(load.claims[0].claim.id.as_str(), "bom");
        assert!(load.errors.is_empty());
    }

    #[test]
    fn load_all_rejects_a_duplicate_id_across_two_files_naming_both() {
        // C1: two files declaring the same id are both dropped and both reported,
        // each naming the other, because a shared id conflates the recorded fact.
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
    fn init_creates_only_the_claims_dir_no_log_tree() {
        // v2 has no committed verdict log, so init scaffolds `.claims/` and nothing
        // under it — a verdict is telemetry the hub stores, not source.
        let (dir, _store) = store();
        assert!(dir.path().join(".claims").is_dir());
        assert!(
            !dir.path().join(".claims/log").exists(),
            "no verdict-log tree is scaffolded"
        );
    }

    #[test]
    fn init_rejects_a_claims_path_that_is_a_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(CLAIMS_DIR), b"not a dir").unwrap();
        let err = Store::init(dir.path()).unwrap_err();
        assert!(matches!(err, StoreError::NotADirectory { .. }));
    }

    #[test]
    fn resolve_finds_a_clean_claim() {
        let (_dir, store) = store();
        write(&store, ".claims/pin.md", &claim_text("pin"));
        let load = store.load_all().unwrap();
        match load.resolve("pin") {
            Resolved::Found(loaded) => assert_eq!(loaded.claim.id.as_str(), "pin"),
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[test]
    fn resolve_reports_a_duplicate_id_as_duplicate_not_not_found() {
        // The bug this method exists to fix: two files declare id `shared`, so both
        // are dropped and keyed by *their own* filenames (`one`, `two`). A stem scan
        // for `shared` misses, so a naive lookup falls through to "no such claim" —
        // a false diagnosis for a claim that exists twice. The id-keyed check catches
        // it and reports Duplicate.
        let (_dir, store) = store();
        write(&store, ".claims/one.md", &claim_text("shared"));
        write(&store, ".claims/two.md", &claim_text("shared"));
        let load = store.load_all().unwrap();
        match load.resolve("shared") {
            Resolved::Duplicate(err) => {
                assert!(err.message.contains("duplicate claim id 'shared'"));
                assert_eq!(err.id.as_ref().map(|i| i.as_str()), Some("shared"));
            }
            other => panic!("expected Duplicate, got {other:?}"),
        }
    }

    #[test]
    fn resolve_surfaces_a_broken_files_error_by_its_path_stem() {
        // A file whose path is the requested id failed to parse: no claim was
        // produced (so `LoadError::id` is None), but its stem names the id, so
        // resolve returns LoadFailed carrying that file's parse error.
        let (_dir, store) = store();
        write(
            &store,
            ".claims/broken.md",
            "---\nchecks: [unterminated\n---\nS.\n",
        );
        let load = store.load_all().unwrap();
        match load.resolve("broken") {
            Resolved::LoadFailed(err) => {
                assert_eq!(err.file, ".claims/broken.md");
                assert!(err.id.is_none(), "a parse failure records no id");
            }
            other => panic!("expected LoadFailed, got {other:?}"),
        }
    }

    #[test]
    fn resolve_returns_not_found_for_a_genuine_unknown_id() {
        let (_dir, store) = store();
        write(&store, ".claims/pin.md", &claim_text("pin"));
        let load = store.load_all().unwrap();
        assert!(matches!(load.resolve("nope"), Resolved::NotFound));
    }

    #[test]
    fn resolve_ignores_an_unrelated_broken_sibling() {
        // A different id's broken file is not the requested id's concern: resolving a
        // clean claim returns Found even when the store also holds a broken sibling.
        let (_dir, store) = store();
        write(&store, ".claims/good.md", &claim_text("good"));
        write(
            &store,
            ".claims/bad.md",
            "---\nchecks: [unterminated\n---\nS.\n",
        );
        let load = store.load_all().unwrap();
        assert!(matches!(load.resolve("good"), Resolved::Found(_)));
    }

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
