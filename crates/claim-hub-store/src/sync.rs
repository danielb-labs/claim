//! Registry sync: turn a connected git store into a [`Registry`] snapshot.
//!
//! Sync is the hub's mirror of git made current (HUB.md §3, HUB-IMPLEMENTATION.md
//! §1.6). For one connected store it:
//!
//! 1. **Mirrors the store** into a bare local mirror, by shelling to the system
//!    `git` binary — the same choice `claim-store` makes for provenance
//!    ([`Command::new("git")`]), so the workspace has one way of talking to git and
//!    no `libgit2`/`gix` surface to audit. A first sync clones `--mirror`; a later
//!    sync fetches. The default-branch tip sha is recorded.
//! 2. **Reads the tip** by checking it out into a throwaway worktree and loading
//!    `.claims/` through `claim-store`'s [`Store::load_all`], plus scanning the
//!    conventional agent-context host files for embedded `<!-- claim -->` blocks
//!    with `claim-core`'s [`extract_embedded_claims`]. Both funnel through the one
//!    `claim-core` grammar the CLI uses — one grammar for every front door.
//! 3. **Snapshots** the parsed claims into [`RegisteredClaim`]s and, together with any
//!    findings, calls the atomic [`Registry::replace_store_snapshot`], so a claim
//!    absent at the new tip is *retired* (dropped from the live set; its history stays
//!    derivable from git and the ledger), and the cross-store `supports` index is
//!    maintained in both directions.
//! 4. **Records malformed files as findings** in that same atomic write, never silent
//!    skips (invariant #6): a claim file that fails to parse at the tip becomes a
//!    queryable [`SyncFinding`] naming the file and the reason, while the well-formed
//!    claims still index. Because the claims and findings land in one transaction, a
//!    malformed file can never be indexed-away from the registry with its nag lost.
//!
//! The public entry points are [`sync_store`] — one callable sync of one store,
//! which hub-07 wires to the manual-resync route once routes land — and
//! [`spawn_interval_poll`], the v1 interval-poll trigger that re-syncs each connected
//! store on a cadence. The authenticated manual-resync HTTP endpoint mentioned in the
//! plan (HUB-IMPLEMENTATION.md §1.6) needs the axum shell (hub-03) and auth (hub-13);
//! it is not built here — it will be a thin route calling [`sync_store`].
//!
//! [`Command::new("git")`]: std::process::Command
//! [`Store::load_all`]: claim_store::Store::load_all
//! [`extract_embedded_claims`]: claim_core::extract_embedded_claims

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use claim_core::{extract_embedded_claims, Claim};
use claim_hub_core::check_digest;
use claim_store::{discover, LoadError, StoreError as ClaimStoreError};

use crate::error::{Result, StoreError};
use crate::findings::SyncFinding;
use crate::registry::{RegisteredClaim, Registry, RegistryVersion};

/// The default git branch a store is read at when a sync does not name one.
///
/// Sync reads a store at the tip of its default branch (HUB.md §2). Rather than ask
/// the remote which branch is its head — an extra round trip and a moving target —
/// a [`ConnectedStore`] names the branch explicitly, defaulting to this. A store on
/// a differently-named default branch sets its own.
pub const DEFAULT_BRANCH: &str = "main";

/// The host files a sync scans for embedded `<!-- claim -->` blocks, beyond the
/// standalone `.claims/**/*.md` files.
///
/// Embedded claims travel inside a context file so that file can carry the claims
/// that keep it honest (PRODUCT.md; HUB.md §3 has sync reindex "full claim files and
/// embedded claim blocks"). v1 scans the conventional agent-context file names at
/// any depth in the checkout — the ones the design names — rather than every text
/// file in the repo, which would be both slow and surprising. A later item may widen
/// this behind the same sync entry point; the set is one constant so widening it is a
/// one-line, reviewed change.
///
/// The names are matched case-sensitively against a file's own name (not its path),
/// so `docs/AGENTS.md` and a root `CLAUDE.md` both qualify while a `claude.md`
/// documenting the format does not masquerade as one.
pub const EMBEDDED_HOST_FILES: &[&str] = &["CLAUDE.md", "AGENTS.md"];

/// A connected git store to sync: where to mirror it from and which branch is its
/// tip.
///
/// The `id` is the store's canonical name as it appears on an [`Event`]'s `store`
/// field and as the [`Registry`] keys on it (e.g. `github.com/acme/payments`); the
/// `url` is what `git clone`/`fetch` talks to, which may be an `https://`/`ssh://`
/// remote or — in tests — a local filesystem path used as a git remote, so no
/// network is required to exercise sync. Keeping the two distinct means the stored
/// identity is stable even if the fetch URL changes (a mirror moves hosts).
///
/// The fields are **private and validated at construction**: a `url` or `branch`
/// beginning with `-` is rejected ([`ConnectedStore::try_new`]), because git would
/// parse such a value as an option rather than a positional and a crafted
/// `--upload-pack=…`/`ext::…` URL reaches arbitrary command execution during a clone.
/// Making the fields private closes the struct-literal bypass, so every
/// `ConnectedStore` a sync sees has passed the check — defense in depth alongside the
/// `--end-of-options` guard on the git argv itself. [`ConnectedStore::new`] is the
/// infallible constructor for statically-known-safe inputs (tests, config already
/// validated); it debug-asserts the same rule.
///
/// [`Event`]: claim_hub_core::Event
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectedStore {
    id: String,
    url: String,
    branch: String,
}

impl ConnectedStore {
    /// A connected store read at [`DEFAULT_BRANCH`], for a statically-known-safe
    /// `url`.
    ///
    /// `id` is the canonical store name; `url` is the git remote (or local path) to
    /// mirror from. This constructor does **not** validate the URL — it is for
    /// call sites (tests, a literal, config already checked) that promise a safe
    /// input. For a fetch URL from untrusted configuration, use
    /// [`try_new`](ConnectedStore::try_new), which rejects an option-like URL loudly
    /// with [`StoreError::UnsafeStoreInput`]. A URL that slips through here anyway is
    /// still safe against argument injection: every git call passes the URL after
    /// `--end-of-options`, so git treats it as a positional path, never an option — the
    /// argv guard is the always-on floor, and construction-time validation is the loud
    /// early nag on top of it.
    #[must_use]
    pub fn new(id: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            url: url.into(),
            branch: DEFAULT_BRANCH.to_owned(),
        }
    }

    /// A connected store read at [`DEFAULT_BRANCH`], validating the `url`.
    ///
    /// The one constructor to use for a fetch URL from untrusted configuration. A
    /// `url` beginning with `-` is rejected with [`StoreError::UnsafeStoreInput`], so
    /// an option-like URL never reaches `git`.
    ///
    /// # Errors
    ///
    /// [`StoreError::UnsafeStoreInput`] if `url` begins with `-`.
    pub fn try_new(id: impl Into<String>, url: impl Into<String>) -> Result<Self> {
        let url = url.into();
        reject_option_like("url", &url)?;
        Ok(Self {
            id: id.into(),
            url,
            branch: DEFAULT_BRANCH.to_owned(),
        })
    }

    /// This store read at `branch` instead of [`DEFAULT_BRANCH`], validating it.
    ///
    /// # Errors
    ///
    /// [`StoreError::UnsafeStoreInput`] if `branch` begins with `-`. The branch is
    /// wrapped in `refs/heads/{branch}` before it reaches git — already safe from
    /// being read as an option — but it is validated anyway, so no external input
    /// reaches git argv unchecked.
    pub fn with_branch(mut self, branch: impl Into<String>) -> Result<Self> {
        let branch = branch.into();
        reject_option_like("branch", &branch)?;
        self.branch = branch;
        Ok(self)
    }

    /// The store's canonical id, as events and the registry reference it.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The git URL or local path the mirror is cloned/fetched from.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The branch whose tip is read.
    #[must_use]
    pub fn branch(&self) -> &str {
        &self.branch
    }
}

/// What one sync of one store observed: the tip it read and the counts it recorded.
///
/// Returned by [`sync_store`] so a caller (the interval driver's log, or the future
/// manual-resync route's response) can report the outcome without re-querying: the
/// tip sha the snapshot was taken at, the new registry version the replace advanced
/// to, and how many claims indexed versus how many files were recorded as findings.
/// A non-zero `findings` count is the nag surface — the caller may surface it, and
/// the findings themselves are queryable through [`Findings`](crate::Findings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutcome {
    /// The default-branch tip sha the snapshot was read at.
    pub commit: String,
    /// The registry version [`Registry::replace_store`] advanced to for this sync.
    pub version: RegistryVersion,
    /// How many well-formed claims (standalone files and embedded blocks) indexed.
    pub claims_indexed: usize,
    /// How many claim files failed to parse and were recorded as findings.
    pub findings_recorded: usize,
}

/// Sync one connected store: mirror it, read its tip, snapshot the registry, and
/// record any malformed files as findings.
///
/// This is the single sync entry point every trigger funnels through — the interval
/// poll ([`spawn_interval_poll`]) and, once routes land (hub-07), the authenticated
/// manual-resync endpoint. `mirror_root` is the directory bare per-store mirrors live
/// under; it is created if absent and reused across syncs so a re-sync fetches rather
/// than re-clones. `store` is the storage backend implementing [`Registry`] (the one
/// [`SqliteStore`](crate::SqliteStore) does).
///
/// The claims and the findings are written together through
/// [`Registry::replace_store_snapshot`] — **one atomic write**, load-bearing for
/// invariant #6: a malformed file must never be indexed-away from the registry while
/// its [`SyncFinding`] is lost, a silent coverage gap. A claim absent at the new tip
/// is retired; a malformed file is recorded as a finding and the well-formed claims
/// still index. The snapshot exactly describes the tip — idempotent in content: a
/// second sync of an unchanged tip reproduces the same registry (only the version
/// counter advances, marking that a sync happened).
///
/// # Errors
///
/// Returns a [`StoreError`] when the store cannot be *mirrored or read at all* — the
/// `git` binary is missing ([`StoreError::GitSpawn`]), a clone/fetch/tip-resolve
/// fails ([`StoreError::Git`]), a mirror path or worktree cannot be created
/// ([`StoreError::Io`]), or the `.claims/` corpus cannot be listed
/// ([`StoreError::Corpus`]). These are loud environment faults the caller reports and
/// the interval driver retries next tick — never a silently empty snapshot that would
/// retire every claim. A single malformed *claim file* is **not** an error: it is a
/// recorded finding, and the sync succeeds with the good claims indexed.
pub async fn sync_store<S>(
    store: &S,
    connected: &ConnectedStore,
    mirror_root: &Path,
) -> Result<SyncOutcome>
where
    S: Registry,
{
    // Mirroring and reading the tip are blocking git + filesystem work; run them off
    // the async runtime's threads so a sync of a large store never stalls the reactor
    // the interval driver and (later) the axum server share. The storage write below
    // is the crate's own async trait method.
    let connected = connected.clone();
    let mirror_root = mirror_root.to_path_buf();
    let read = tokio::task::spawn_blocking(move || read_tip(&connected, &mirror_root))
        .await
        .map_err(|join| StoreError::Io {
            context: "the registry-sync worker thread panicked".to_owned(),
            source: std::io::Error::other(join.to_string()),
        })??;

    // Claims and findings in one atomic write, so the registry and its findings can
    // never skew (invariant #6): the malformed-file nag and the claims that indexed
    // around it land together or not at all.
    let version = store
        .replace_store_snapshot(&read.store_id, &read.claims, &read.findings)
        .await?;

    Ok(SyncOutcome {
        commit: read.commit,
        version,
        claims_indexed: read.claims.len(),
        findings_recorded: read.findings.len(),
    })
}

/// The parsed result of reading a store at its tip, before it is written to storage.
///
/// Separating the blocking read (git + parse) from the async storage write keeps the
/// `spawn_blocking` boundary clean: the worker returns this plain-data snapshot and
/// the caller persists it.
struct TipRead {
    store_id: String,
    commit: String,
    claims: Vec<RegisteredClaim>,
    findings: Vec<SyncFinding>,
}

/// Mirror the store, resolve its default-branch tip, check it out, and parse every
/// claim (standalone files and embedded blocks) into a snapshot plus findings.
///
/// All-blocking: git subprocesses and filesystem walks. The tip sha is stamped onto
/// every [`RegisteredClaim`] and every [`SyncFinding`], so the whole snapshot pins one
/// commit.
fn read_tip(connected: &ConnectedStore, mirror_root: &Path) -> Result<TipRead> {
    let mirror = update_mirror(connected, mirror_root)?;
    let commit = resolve_tip(connected, &mirror)?;
    let checkout = TipCheckout::create(connected, &mirror, &commit)?;

    let (claims, findings) = parse_store_at(connected.id(), checkout.path(), &commit)?;
    Ok(TipRead {
        store_id: connected.id().to_owned(),
        commit,
        claims,
        findings,
    })
}

/// Clone or update the bare mirror for `connected` under `mirror_root`, returning its
/// path.
///
/// A first sync `git clone --mirror`s the remote into `mirror_root/<sanitized-id>.git`;
/// a later sync `git remote update --prune`s the existing mirror, so re-syncs are
/// cheap fetches and a branch deleted upstream is pruned (its tip stops resolving,
/// which is what makes an upstream retirement observable). `--mirror` keeps a bare
/// copy of every ref, and reading a specific commit into a worktree needs no working
/// tree in the mirror itself.
fn update_mirror(connected: &ConnectedStore, mirror_root: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(mirror_root).map_err(|source| StoreError::Io {
        context: format!(
            "failed to create the mirror root at {}",
            mirror_root.display()
        ),
        source,
    })?;
    let mirror = mirror_root.join(format!("{}.git", sanitize_store_id(connected.id())));

    if mirror.join("HEAD").exists() {
        // An existing mirror: fetch new refs and prune deleted ones. `git -C <mirror>
        // remote update --prune` refreshes every ref the `--mirror` clone tracks. The
        // URL is not on this argv (git reads it from the mirror's stored config), so
        // no external input reaches git as a positional here.
        run_git(
            connected,
            mirror_root,
            &[
                "-C",
                &mirror.to_string_lossy(),
                "remote",
                "update",
                "--prune",
            ],
        )?;
    } else {
        // First sync: a bare mirror clone of every ref. The URL may be a real remote
        // or a local path (a test fixture used as a remote), so no network is implied.
        // `--end-of-options` immediately before the `<url>` positional forces git to
        // treat the URL as a path, never an option, so a crafted `--upload-pack=…` /
        // `ext::…` URL cannot inject a command (defense in depth over the
        // construction-time rejection of a leading-dash url).
        run_git(
            connected,
            mirror_root,
            &[
                "clone",
                "--mirror",
                "--end-of-options",
                connected.url(),
                &mirror.to_string_lossy(),
            ],
        )?;
    }
    Ok(mirror)
}

/// Resolve the default-branch tip sha in the mirror to a full 40-char sha.
///
/// The full sha, never `--short`: the abbreviated width is `core.abbrev`-dependent
/// and the recorded commit must not vary with config, matching `claim-store`'s
/// provenance rule. A branch that does not resolve (never existed, or pruned upstream)
/// is a loud [`StoreError::Git`], not an empty snapshot — a sync that cannot find its
/// tip must fail rather than retire every claim (invariant #6).
fn resolve_tip(connected: &ConnectedStore, mirror: &Path) -> Result<String> {
    // The branch is wrapped in `refs/heads/{branch}`, so it can never be read as an
    // option, and it was rejected at construction if it began with `-`. `rev-parse`
    // echoes an `--end-of-options` literal into its own output, so that guard is not
    // used here; the two protections above are what keep this call safe.
    let out = run_git(
        connected,
        mirror,
        &[
            "-C",
            &mirror.to_string_lossy(),
            "rev-parse",
            &format!("refs/heads/{}", connected.branch()),
        ],
    )?;
    Ok(out)
}

/// A throwaway worktree checked out at a store's tip, torn down on drop.
///
/// The mirror is bare (no working tree), so reading `.claims/` needs a checkout. A
/// `git worktree add --detach <dir> <sha>` materializes the tip's tree without moving
/// any ref, and [`Drop`] removes it, so a panic mid-parse leaves no leaked worktree.
/// The worktree lives under the system temp root, outside both the mirror and any
/// caller tree.
struct TipCheckout {
    /// The owning mirror the worktree belongs to, retained so [`Drop`] can address it
    /// in the `git worktree remove` call (a worktree is removed via its parent repo).
    mirror: PathBuf,
    path: Option<PathBuf>,
}

impl TipCheckout {
    /// Check `commit` out of `mirror` into a fresh temp worktree.
    fn create(connected: &ConnectedStore, mirror: &Path, commit: &str) -> Result<Self> {
        let dir = unique_temp_dir(connected.id())?;
        // Both positionals are internally generated and already safe: `dir` is our own
        // temp path (never option-like), and `commit` is a resolved 40-char sha from
        // `resolve_tip`. `git worktree add` does not accept `--end-of-options`, so no
        // argv guard is added here; the guard lives where external input reaches git
        // (the clone URL) and where a subcommand accepts it (rev-parse).
        run_git(
            connected,
            mirror,
            &[
                "-C",
                &mirror.to_string_lossy(),
                "worktree",
                "add",
                "--detach",
                "--force",
                &dir.to_string_lossy(),
                commit,
            ],
        )
        .inspect_err(|_| {
            // Clean up the empty dir we created before git failed, so a repeated
            // failure does not litter temp.
            std::fs::remove_dir_all(&dir).ok();
        })?;
        Ok(Self {
            mirror: mirror.to_path_buf(),
            path: Some(dir),
        })
    }

    /// The checkout's root — where `.claims/` and the host files are read.
    fn path(&self) -> &Path {
        self.path.as_deref().expect("checkout used after teardown")
    }
}

impl Drop for TipCheckout {
    fn drop(&mut self) {
        if let Some(dir) = self.path.take() {
            // Best-effort teardown; a leaked temp worktree is not worth a panic in
            // drop. `--force` because the checkout is disposable.
            Command::new("git")
                .arg("-C")
                .arg(&self.mirror)
                .args(["worktree", "remove", "--force"])
                .arg(&dir)
                .output()
                .ok();
            std::fs::remove_dir_all(&dir).ok();
        }
    }
}

/// Parse every claim at a checked-out tip into a snapshot plus findings.
///
/// Two sources funnel through the one `claim-core` grammar:
///
/// - **Standalone `.claims/**/*.md`**, via `claim-store`'s [`Store::load_all`], which
///   already skips non-claim docs and turns a malformed file into a [`LoadError`].
/// - **Embedded `<!-- claim -->` blocks** in the conventional host files
///   ([`EMBEDDED_HOST_FILES`]), via [`extract_embedded_claims`].
///
/// A well-formed claim becomes a [`RegisteredClaim`] stamped with `commit`. A
/// malformed standalone file becomes a [`SyncFinding`] (invariant #6). A host file
/// whose embedded block fails to parse likewise becomes a finding, while its
/// well-formed siblings — and every other file — still index. A checkout whose store
/// is *legitimately absent* (no `.claims/`) yields an empty snapshot (no claims, no
/// findings), which retires everything the store previously had, honestly reflecting a
/// tip that removed its store.
///
/// # Errors
///
/// Returns [`StoreError::Corpus`] when the store cannot be *read* — the `.claims/`
/// directory cannot be listed, or discovery hits a real filesystem fault (as opposed
/// to a store that is simply not there). A genuine read fault is loud, never swallowed
/// into an empty snapshot that would mass-retire every claim (invariant #6). A single
/// malformed file is not this error; it is a recorded finding.
///
/// [`Store::load_all`]: claim_store::Store::load_all
fn parse_store_at(
    store_id: &str,
    checkout: &Path,
    commit: &str,
) -> Result<(Vec<RegisteredClaim>, Vec<SyncFinding>)> {
    // Claims are collected with the file they came from, so a duplicate id across two
    // sources can name both in its finding. Findings accumulate from parse failures
    // here and duplicate-id conflicts below.
    let mut parsed: Vec<ParsedClaim> = Vec::new();
    let mut findings: Vec<SyncFinding> = Vec::new();

    // Standalone files. `discover` from the checkout root finds `.claims/`. Only a
    // *legitimately absent* store (`NoStore`) is an empty snapshot; any other discovery
    // fault (an I/O error, a `.claims` that is a file) is a loud `Corpus` error, never
    // swallowed into an empty snapshot that would mass-retire every claim on a
    // filesystem fault (invariant #6). `load_all` already rejects duplicate ids *within*
    // the standalone corpus as errors, so those arrive as findings; the cross-source
    // dedup below catches an embedded block colliding with a standalone file.
    match discover(checkout) {
        Ok(store) => {
            let load = store.load_all().map_err(|e| StoreError::Corpus {
                store: store_id.to_owned(),
                reason: e.to_string(),
            })?;
            for loaded in load.claims {
                parsed.push(ParsedClaim {
                    claim: registered(&loaded.claim, commit),
                    file: loaded.path,
                });
            }
            for err in load.errors {
                findings.push(finding(store_id, err, commit));
            }
        }
        // No `.claims/` at this tip: the store removed its claim store, so the snapshot
        // is legitimately empty and everything retires — the honest reading of that tip.
        Err(ClaimStoreError::NoStore { .. }) => {}
        // A genuine read fault (not a truly-absent store): loud, so a filesystem
        // hiccup cannot masquerade as "the store deleted all its claims".
        Err(e) => {
            return Err(StoreError::Corpus {
                store: store_id.to_owned(),
                reason: e.to_string(),
            });
        }
    }

    // Embedded blocks in the conventional host files across the checkout. The root
    // is threaded through so a nested host file's finding names its full path
    // (`docs/AGENTS.md`), not just its file name.
    scan_embedded(
        store_id,
        checkout,
        checkout,
        commit,
        &mut parsed,
        &mut findings,
    )?;

    // A claim id is unique within a store; two files (standalone or embedded) sharing
    // one conflate the fact recorded against it, so — like the CLI's store loader —
    // both are dropped and both nag, rather than the sync letting a colliding
    // `INSERT` fail the whole snapshot. This catches a collision the CLI's per-corpus
    // `load_all` cannot see: an embedded block whose id matches a standalone file.
    let mut claims = reject_duplicate_ids(store_id, commit, parsed, &mut findings);

    // A stable order so a wipe-plus-resync reproduces the registry identically and
    // tests read deterministically. Claims key on id; findings on file.
    claims.sort_by(|a, b| a.id.cmp(&b.id));
    findings.sort_by(|a, b| a.file.cmp(&b.file));
    Ok((claims, findings))
}

/// A parsed claim paired with the file it was read from, so a duplicate-id conflict
/// can name both colliding files in its finding.
struct ParsedClaim {
    claim: RegisteredClaim,
    file: String,
}

/// Drop every claim whose id is shared by two or more files, recording each as a
/// finding that names the others, and return the uniquely-identified claims.
///
/// A shared id is a false-green waiting to happen (the CLI's store loader refuses it
/// for the same reason): two files claiming to be one fact conflate what the ledger
/// records against that id, so a genuinely drifted fact could be masked by its
/// namesake. The sync cannot pick a winner, so both are dropped and both nag —
/// mirroring `claim-store`'s `reject_duplicate_ids`, extended across the standalone
/// and embedded sources the registry unifies. Without this, a colliding
/// `(store, claim_id)` would fail the `replace_store` transaction and take the whole
/// sync down.
fn reject_duplicate_ids(
    store_id: &str,
    commit: &str,
    parsed: Vec<ParsedClaim>,
    findings: &mut Vec<SyncFinding>,
) -> Vec<RegisteredClaim> {
    use std::collections::HashMap;
    let mut by_id: HashMap<String, Vec<ParsedClaim>> = HashMap::new();
    for p in parsed {
        by_id
            .entry(p.claim.id.as_str().to_owned())
            .or_default()
            .push(p);
    }
    let mut kept = Vec::new();
    for (id, mut group) in by_id {
        if group.len() == 1 {
            kept.push(group.pop().expect("one element").claim);
            continue;
        }
        let files: Vec<&str> = group.iter().map(|p| p.file.as_str()).collect();
        for p in &group {
            let others: Vec<&str> = files
                .iter()
                .copied()
                .filter(|f| *f != p.file.as_str())
                .collect();
            findings.push(SyncFinding {
                store: store_id.to_owned(),
                file: p.file.clone(),
                commit: commit.to_owned(),
                reason: format!(
                    "duplicate claim id '{id}': also declared in {}. Two files sharing an id \
                     conflate the fact recorded against it, so a drifted fact can be masked by \
                     its namesake. Give each claim a unique id.",
                    others.join(", ")
                ),
            });
        }
    }
    kept
}

/// Walk the checkout for [`EMBEDDED_HOST_FILES`] and extract their embedded claims,
/// appending well-formed ones to `claims` and parse failures to `findings`.
///
/// A host file that reads but whose block fails to parse is a finding, not a silent
/// skip. A host file that cannot be read is itself a finding — we cannot prove it
/// carries no claim, so it nags rather than being dropped. The `.git` metadata
/// directory of the worktree is skipped so mirror internals are never scanned.
///
/// Only a *regular file* named like a host file is read: `entry.file_type()` does not
/// follow symlinks, so a symlink named `CLAUDE.md` reports as a symlink and is skipped
/// rather than followed — a symlink could point outside the checkout or at an
/// arbitrarily large file, and following it while scanning an untrusted checkout is a
/// footgun. (`claim-store`'s `collect_claim_files` follows symlinks for standalone
/// files under `.claims/`; that is a separate, tracked concern, not fixed here.)
fn scan_embedded(
    store_id: &str,
    root: &Path,
    dir: &Path,
    commit: &str,
    claims: &mut Vec<ParsedClaim>,
    findings: &mut Vec<SyncFinding>,
) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
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
        // `file_type()` is the entry's own type and does not follow a symlink, so a
        // symlinked host file reports neither dir nor regular file and is skipped below.
        let file_type = entry.file_type().map_err(|source| StoreError::Io {
            context: format!("failed to stat {}", path.display()),
            source,
        })?;
        if file_type.is_dir() {
            // Never descend into the worktree's own git metadata.
            if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
                continue;
            }
            scan_embedded(store_id, root, &path, commit, claims, findings)?;
        } else if file_type.is_file() && is_embedded_host_file(&path) {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            harvest_embedded(store_id, &path, rel, commit, claims, findings);
        }
    }
    Ok(())
}

/// Extract embedded claims from one host file, recording each outcome.
///
/// Unreadable → a finding (we cannot prove it holds no claim). Readable but a block
/// fails to parse → a finding naming the parser's reason. Readable and every block
/// well-formed → those claims index. `rel` is the file's path relative to the
/// checkout root, so a finding names the file the way an author sees it.
fn harvest_embedded(
    store_id: &str,
    path: &Path,
    rel: &Path,
    commit: &str,
    claims: &mut Vec<ParsedClaim>,
    findings: &mut Vec<SyncFinding>,
) {
    let rel_display = rel.display().to_string();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            findings.push(SyncFinding {
                store: store_id.to_owned(),
                file: rel_display,
                commit: commit.to_owned(),
                reason: format!("host file could not be read: {e}"),
            });
            return;
        }
    };
    match extract_embedded_claims(&rel_display, &text) {
        Ok(embedded) => {
            for claim in embedded {
                claims.push(ParsedClaim {
                    claim: registered(&claim, commit),
                    file: rel_display.clone(),
                });
            }
        }
        Err(e) => findings.push(SyncFinding {
            store: store_id.to_owned(),
            file: rel_display,
            commit: commit.to_owned(),
            reason: e.to_string(),
        }),
    }
}

/// Whether a path is one of the conventional embedded-claim host files, matched by
/// its file name (case-sensitive) against [`EMBEDDED_HOST_FILES`].
fn is_embedded_host_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| EMBEDDED_HOST_FILES.contains(&name))
}

/// Turn a parsed [`Claim`] into the registry's stored shape, stamped with the tip sha.
///
/// The [`Registry`] holds a claim's *stored* shape — id, statement, supports, commit,
/// and each check's content digest — not the full check *definitions*, which live with
/// the verdicts they produce on the ledger. The digests are computed here, at parse
/// time, by the one canonical [`check_digest`] function over the checks in declared
/// order, so `check_digests[i]` is the identity of check `i`; the ingest gate reads
/// them by position to map a positional CLI report onto content-keyed events (issue
/// #18) without re-parsing. Supports targets are carried as their canonical strings,
/// the transparent serialized form the store persists and returns.
fn registered(claim: &Claim, commit: &str) -> RegisteredClaim {
    RegisteredClaim {
        id: claim.id.clone(),
        statement: claim.statement.clone(),
        supports: claim
            .supports
            .iter()
            .map(|t| t.as_str().to_owned())
            .collect(),
        commit: commit.to_owned(),
        check_digests: claim.checks.iter().map(check_digest).collect(),
    }
}

/// Turn a store-load [`LoadError`] into a recorded [`SyncFinding`].
///
/// The file and reason come straight from the store loader, so the finding names the
/// file the way the author sees it and carries the parser's own reason (which already
/// names the field to fix).
fn finding(store_id: &str, err: LoadError, commit: &str) -> SyncFinding {
    SyncFinding {
        store: store_id.to_owned(),
        file: err.file,
        commit: commit.to_owned(),
        reason: err.message,
    }
}

/// Turn a store id into a filesystem-safe mirror directory stem.
///
/// A store id like `github.com/acme/payments` contains `/`, which cannot be a single
/// path component. Every non-`[A-Za-z0-9._-]` byte becomes `_`, so two distinct ids
/// could in principle collide — acceptable because the id is also recorded inside the
/// mirror's config and the registry keys on the full id, not the directory name; the
/// stem is only a stable, readable handle for the mirror directory.
fn sanitize_store_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A fresh, unique temp directory *path* for a tip worktree, named so concurrent
/// syncs never collide.
///
/// Created empty and handed to `git worktree add`. Uniqueness is the process id, a
/// sanitized store id, and a monotonic counter — no dependency just for a temp name,
/// matching `claim-store`'s approach.
fn unique_temp_dir(store_id: &str) -> Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!(
        "claim-sync-{}-{}-{n}",
        std::process::id(),
        sanitize_store_id(store_id)
    );
    let dir = std::env::temp_dir().join(name);
    std::fs::create_dir(&dir).map_err(|source| StoreError::Io {
        context: format!(
            "failed to create a temp checkout directory at {}",
            dir.display()
        ),
        source,
    })?;
    Ok(dir)
}

/// Run `git` with `args`, mapping a spawn failure and a non-zero exit to the store's
/// typed errors, and returning trimmed stdout on success.
///
/// A failure to *spawn* git is [`StoreError::GitSpawn`] (git not installed); a git
/// command that runs and exits non-zero is [`StoreError::Git`] carrying git's stderr —
/// both name the store so the operator knows which sync failed. `cwd` is where git
/// runs when `args` does not already `-C` into a specific repo (clone runs in the
/// mirror root); the explicit `-C` in most calls makes the working directory
/// immaterial, but a valid one is always passed.
///
/// Credential prompting is disabled three ways so a background sync tick can never
/// wedge on an unanswerable prompt: `GIT_TERMINAL_PROMPT=0` (the terminal prompt),
/// and `GIT_ASKPASS`/`SSH_ASKPASS` pointed at `false` (the GUI/askpass and
/// ssh-passphrase prompts a credential helper or ssh-agent would otherwise raise).
/// `GIT_ASKPASS` set to a program that exits non-zero makes git fall through to
/// failure rather than block. A bounded subprocess timeout would be stronger against a
/// helper that ignores these and blocks anyway; it is a noted follow-up, not built
/// here.
fn run_git(connected: &ConnectedStore, cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(cwd)
        // Never prompt for credentials in a background sync: a private remote with no
        // usable credential must fail loudly and fast, not hang the interval task on a
        // prompt no one will answer. `false` exits non-zero, so git gets an empty/failed
        // credential and gives up instead of blocking.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "false")
        .env("SSH_ASKPASS", "false")
        .args(args)
        .output()
        .map_err(|source| StoreError::GitSpawn {
            store: connected.id().to_owned(),
            args: args.join(" "),
            source,
        })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(StoreError::Git {
            store: connected.id().to_owned(),
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

/// Reject a git-bound external input that begins with `-`, which git would parse as an
/// option rather than a positional.
///
/// The construction-time half of the argument-injection defense (the argv-time half is
/// `--end-of-options` on each git call). `field` names the offending input in the
/// error so an operator can fix the misconfiguration.
fn reject_option_like(field: &'static str, value: &str) -> Result<()> {
    if value.starts_with('-') {
        return Err(StoreError::UnsafeStoreInput {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

/// Spawn the interval-poll trigger: a background task that re-syncs every connected
/// store on a fixed cadence.
///
/// This is the v1 interval-poll trigger (HUB-IMPLEMENTATION.md §1.6). It ticks every
/// `period`, and on each tick syncs each store in `stores` in turn through
/// [`sync_store`]. A per-store failure is reported through `on_result` and does **not**
/// stop the loop or the other stores — a store whose remote is briefly unreachable
/// fails this tick and is retried next tick, never taking the whole poller down. The
/// first tick fires immediately (tokio's interval yields at once), so a freshly
/// started hub syncs without waiting a full period.
///
/// `on_result` is invoked with each store's id and the sync's outcome (or error), so
/// the caller wires it to `tracing` or a `/status` counter without this module knowing
/// about either. The returned [`JoinHandle`] lets the caller abort the poller on
/// shutdown; dropping it detaches the task, which keeps running until the runtime
/// stops.
///
/// The manual-resync HTTP route (HUB-IMPLEMENTATION.md §1.6) is a separate trigger
/// over the same [`sync_store`]; it lands with routes in hub-07, not here, so hub-05
/// stays independent of the axum shell.
///
/// [`JoinHandle`]: tokio::task::JoinHandle
pub fn spawn_interval_poll<S, F>(
    store: S,
    stores: Vec<ConnectedStore>,
    mirror_root: PathBuf,
    period: std::time::Duration,
    mut on_result: F,
) -> tokio::task::JoinHandle<()>
where
    S: Registry + Sync + Send + 'static,
    F: FnMut(&str, &Result<SyncOutcome>) + Send + 'static,
{
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        loop {
            interval.tick().await;
            for connected in &stores {
                let result = sync_store(&store, connected, &mirror_root).await;
                on_result(connected.id(), &result);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_store_id_replaces_path_separators() {
        assert_eq!(
            sanitize_store_id("github.com/acme/payments"),
            "github.com_acme_payments"
        );
        assert_eq!(sanitize_store_id("a b:c"), "a_b_c");
        assert_eq!(sanitize_store_id("plain-id.v2"), "plain-id.v2");
    }

    #[test]
    fn is_embedded_host_file_matches_conventional_names_only() {
        assert!(is_embedded_host_file(Path::new("CLAUDE.md")));
        assert!(is_embedded_host_file(Path::new("docs/AGENTS.md")));
        // Case-sensitive: a lowercase doc about the format is not a host file.
        assert!(!is_embedded_host_file(Path::new("claude.md")));
        assert!(!is_embedded_host_file(Path::new("README.md")));
    }

    #[test]
    fn connected_store_new_defaults_to_the_main_branch() {
        let s = ConnectedStore::new("github.com/acme/payments", "/tmp/x");
        assert_eq!(s.branch(), DEFAULT_BRANCH);
        assert_eq!(s.id(), "github.com/acme/payments");
    }

    #[test]
    fn try_new_rejects_an_option_like_url() {
        // A URL git would read as an option (`--upload-pack=…`, `ext::…` after an
        // option flag, etc.) is refused up front, so it never reaches git argv.
        let err = ConnectedStore::try_new("s", "--upload-pack=touch /tmp/pwned").unwrap_err();
        assert!(
            matches!(err, StoreError::UnsafeStoreInput { field: "url", .. }),
            "{err:?}"
        );
        // A short-option form is caught the same way.
        assert!(ConnectedStore::try_new("s", "-u").is_err());
        // A legitimate URL and a local path both pass.
        assert!(ConnectedStore::try_new("s", "https://github.com/acme/x.git").is_ok());
        assert!(ConnectedStore::try_new("s", "/tmp/local/repo.git").is_ok());
    }

    #[test]
    fn with_branch_rejects_an_option_like_branch() {
        let err = ConnectedStore::new("s", "/tmp/x")
            .with_branch("--output=/tmp/pwned")
            .unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::UnsafeStoreInput {
                    field: "branch",
                    ..
                }
            ),
            "{err:?}"
        );
        // An ordinary branch name is accepted.
        assert_eq!(
            ConnectedStore::new("s", "/tmp/x")
                .with_branch("release/2.0")
                .unwrap()
                .branch(),
            "release/2.0"
        );
    }
}
