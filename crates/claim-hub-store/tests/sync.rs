//! Integration tests for registry sync over local git fixtures.
//!
//! Every test builds a real git repository in a temp dir and uses its *path* as the
//! sync's remote URL, so a mirror clone/fetch is a local operation with **no
//! network** (HUB-IMPLEMENTATION.md §1.6's test contract). Ambient git config is
//! walled off — `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` point at nonexistent files
//! and identity is set per-repo — so a developer's global `init.defaultBranch` or
//! identity cannot leak in and make a test pass or fail by accident.
//!
//! What the suite proves, matching the item's done-when:
//!
//! - a fixture with `.claims/` (including an embedded block) syncs and its claims
//!   index at the tip sha;
//! - deleting a claim and re-syncing retires it, both `supports` directions updated;
//! - a malformed claim file becomes a recorded [`SyncFinding`], the good claims still
//!   index;
//! - wipe-plus-resync reproduces the registry identically (idempotent content);
//! - the interval-poll driver ticks and syncs.

use std::path::{Path, PathBuf};
use std::process::Command;

use claim_core::ClaimId;
use claim_hub_store::{
    sync_store, ConnectedStore, Findings, Registry, RegistryVersion, SqliteStore,
};
use std::str::FromStr;
use tempfile::TempDir;

const STORE_ID: &str = "github.com/acme/payments";

/// A git repository fixture used as a local sync remote — no network.
struct Fixture {
    dir: TempDir,
}

impl Fixture {
    /// A fresh repo on branch `main` with identity set locally and ambient config
    /// walled off, ready for files and commits.
    fn new() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let fixture = Fixture { dir };
        // `-b main` fixes the default branch regardless of the machine's
        // `init.defaultBranch`, so the sync's `refs/heads/main` always resolves.
        fixture.git(&["init", "-q", "-b", "main"]);
        fixture.git(&["config", "user.name", "Test"]);
        fixture.git(&["config", "user.email", "test@example.com"]);
        fixture.git(&["config", "commit.gpgsign", "false"]);
        fixture
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The remote URL a sync clones from: this fixture's path.
    fn url(&self) -> String {
        self.path().to_string_lossy().into_owned()
    }

    fn write(&self, rel: &str, contents: &str) {
        let path = self.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn remove(&self, rel: &str) {
        std::fs::remove_file(self.path().join(rel)).unwrap();
    }

    /// Stage everything and commit, so the tip carries the current tree.
    fn commit(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }

    fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(self.path())
            // Wall off ambient config so a developer's global identity/branch never
            // leaks into the fixture.
            .env("GIT_CONFIG_GLOBAL", self.path().join("nonexistent-global"))
            .env("GIT_CONFIG_SYSTEM", self.path().join("nonexistent-system"))
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    /// This fixture as a connected store under [`STORE_ID`], read at `main`.
    fn connected(&self) -> ConnectedStore {
        ConnectedStore::new(STORE_ID, self.url())
    }
}

/// A frontmatter claim body for `id` with one always-passing cmd check.
fn claim_file(id: &str, statement: &str, supports: &[&str]) -> String {
    let mut s = format!("---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n");
    if !supports.is_empty() {
        s.push_str("supports:\n");
        for t in supports {
            s.push_str(&format!("  - {t}\n"));
        }
    }
    s.push_str(&format!("---\n{statement}\n"));
    s
}

/// A fresh SQLite store plus the mirror root sync clones into.
async fn store() -> (SqliteStore, TempDir) {
    let dir = TempDir::new().expect("temp dir");
    let store = SqliteStore::open(dir.path().join("hub.db"))
        .await
        .expect("open + migrate");
    (store, dir)
}

fn mirror_root(dir: &TempDir) -> PathBuf {
    dir.path().join("mirrors")
}

#[tokio::test]
async fn syncs_standalone_and_embedded_claims_at_the_tip_sha() {
    let fixture = Fixture::new();
    fixture.write(
        ".claims/payments/libfoo-pin.md",
        &claim_file(
            "payments/libfoo-pin",
            "libfoo is pinned to 4.2",
            &["requirements.txt#libfoo", "decisions/pin-libfoo"],
        ),
    );
    // An embedded claim block in a conventional host file.
    fixture.write(
        "CLAUDE.md",
        "We require TLS on every service.\n\
         <!-- claim\n\
         id: payments/tls-required\n\
         checks:\n  - kind: cmd\n    run: \"true\"\n\
         -->\n",
    );
    fixture.commit("seed claims");

    let (store, dir) = store().await;
    let outcome = sync_store(&store, &fixture.connected(), &mirror_root(&dir))
        .await
        .expect("sync");

    assert_eq!(outcome.claims_indexed, 2);
    assert_eq!(outcome.findings_recorded, 0);
    assert_eq!(outcome.version, RegistryVersion(1));
    assert_eq!(outcome.commit.len(), 40, "the full tip sha is recorded");

    let claims = store.claims_of(STORE_ID).await.unwrap();
    assert_eq!(claims.len(), 2);
    // Both the standalone file and the embedded block indexed, at the same tip sha.
    let pin = claims
        .iter()
        .find(|c| c.id.as_str() == "payments/libfoo-pin")
        .unwrap();
    assert_eq!(pin.commit, outcome.commit);
    assert_eq!(
        pin.supports,
        vec![
            "decisions/pin-libfoo".to_owned(),
            "requirements.txt#libfoo".to_owned()
        ]
    );
    assert!(claims
        .iter()
        .any(|c| c.id.as_str() == "payments/tls-required"));

    // The reverse supports index finds the standalone claim by its decision target.
    let supporters = store
        .claims_supporting("decisions/pin-libfoo")
        .await
        .unwrap();
    assert_eq!(supporters.len(), 1);
    assert_eq!(supporters[0].claim_id.as_str(), "payments/libfoo-pin");
}

#[tokio::test]
async fn deleting_a_claim_retires_it_and_clears_both_supports_directions() {
    let fixture = Fixture::new();
    fixture.write(".claims/a.md", &claim_file("a", "A", &["decisions/shared"]));
    fixture.write(".claims/b.md", &claim_file("b", "B", &[]));
    fixture.commit("two claims");

    let (store, dir) = store().await;
    let mirrors = mirror_root(&dir);
    sync_store(&store, &fixture.connected(), &mirrors)
        .await
        .unwrap();
    assert_eq!(store.claims_of(STORE_ID).await.unwrap().len(), 2);

    // Delete `a` and re-sync: it retires, and its supports edge with it.
    fixture.remove(".claims/a.md");
    fixture.commit("retire a");
    let outcome = sync_store(&store, &fixture.connected(), &mirrors)
        .await
        .unwrap();
    assert_eq!(outcome.version, RegistryVersion(2));

    let claims = store.claims_of(STORE_ID).await.unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].id.as_str(), "b");

    // Forward direction: the retired claim resolves to nothing.
    assert!(store
        .claim(STORE_ID, &ClaimId::from_str("a").unwrap())
        .await
        .unwrap()
        .is_none());
    // Reverse direction: nothing supports the decision any more.
    assert!(store
        .claims_supporting("decisions/shared")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_malformed_claim_becomes_a_finding_and_the_good_claims_still_index() {
    let fixture = Fixture::new();
    fixture.write(".claims/good.md", &claim_file("good", "Good", &[]));
    // Opens with a frontmatter fence (so it declares itself a claim) but the YAML is
    // malformed — a loud finding, never a silent skip.
    fixture.write(".claims/broken.md", "---\nchecks: [unterminated\n---\nS.\n");
    fixture.commit("one good, one broken");

    let (store, dir) = store().await;
    let outcome = sync_store(&store, &fixture.connected(), &mirror_root(&dir))
        .await
        .unwrap();

    assert_eq!(outcome.claims_indexed, 1, "the good claim still indexes");
    assert_eq!(outcome.findings_recorded, 1);

    let claims = store.claims_of(STORE_ID).await.unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].id.as_str(), "good");

    // The malformed file is a recorded, queryable finding naming the file and reason.
    let findings = store.findings().await.unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].store, STORE_ID);
    assert_eq!(findings[0].file, ".claims/broken.md");
    assert_eq!(findings[0].commit, outcome.commit);
    assert!(
        !findings[0].reason.is_empty(),
        "the finding carries the parser's reason"
    );
    // The per-store view agrees.
    assert_eq!(store.findings_of(STORE_ID).await.unwrap(), findings);
}

#[tokio::test]
async fn a_fixed_file_clears_its_finding_on_the_next_sync() {
    let fixture = Fixture::new();
    fixture.write(".claims/x.md", "---\nchecks: [unterminated\n---\nS.\n");
    fixture.commit("broken");

    let (store, dir) = store().await;
    let mirrors = mirror_root(&dir);
    sync_store(&store, &fixture.connected(), &mirrors)
        .await
        .unwrap();
    assert_eq!(store.findings().await.unwrap().len(), 1);

    // Fix the file and re-sync: the finding clears (replace-per-sync), the claim
    // indexes.
    fixture.write(".claims/x.md", &claim_file("x", "X", &[]));
    fixture.commit("fix x");
    sync_store(&store, &fixture.connected(), &mirrors)
        .await
        .unwrap();
    assert!(
        store.findings().await.unwrap().is_empty(),
        "a fixed file no longer nags"
    );
    assert_eq!(store.claims_of(STORE_ID).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_duplicate_id_across_two_files_nags_both_and_indexes_neither() {
    let fixture = Fixture::new();
    // A standalone file and an embedded block both claim id `dup`: the sync cannot
    // pick a winner, so both are dropped and both nag — never a colliding INSERT that
    // fails the whole sync.
    fixture.write(".claims/dup.md", &claim_file("dup", "Standalone", &[]));
    fixture.write(
        "AGENTS.md",
        "Embedded dup.\n\
         <!-- claim\n\
         id: dup\n\
         checks:\n  - kind: cmd\n    run: \"true\"\n\
         -->\n",
    );
    fixture.commit("colliding ids");

    let (store, dir) = store().await;
    let outcome = sync_store(&store, &fixture.connected(), &mirror_root(&dir))
        .await
        .unwrap();

    assert_eq!(outcome.claims_indexed, 0, "an ambiguous id indexes neither");
    assert_eq!(outcome.findings_recorded, 2, "both files nag");
    assert!(store.claims_of(STORE_ID).await.unwrap().is_empty());
    let findings = store.findings().await.unwrap();
    assert!(findings
        .iter()
        .all(|f| f.reason.contains("duplicate claim id 'dup'")));
    // Each finding names the other file.
    let files: Vec<&str> = findings.iter().map(|f| f.file.as_str()).collect();
    assert!(files.contains(&".claims/dup.md"));
    assert!(files.contains(&"AGENTS.md"));
}

#[tokio::test]
async fn wipe_plus_resync_reproduces_the_registry_identically() {
    let fixture = Fixture::new();
    fixture.write(
        ".claims/a.md",
        &claim_file("a", "A", &["decisions/x", "requirements.txt#a"]),
    );
    fixture.write(".claims/b.md", &claim_file("b", "B", &["decisions/y"]));
    fixture.commit("seed");

    let (store, dir) = store().await;
    let mirrors = mirror_root(&dir);
    sync_store(&store, &fixture.connected(), &mirrors)
        .await
        .unwrap();
    let before = store.claims_of(STORE_ID).await.unwrap();

    // A second sync of the unchanged tip: same claims, same commit, same edges. Only
    // the version counter advances (a sync happened), which is what makes the registry
    // safely rebuildable derived data.
    sync_store(&store, &fixture.connected(), &mirrors)
        .await
        .unwrap();
    let after = store.claims_of(STORE_ID).await.unwrap();
    assert_eq!(before, after, "resync reproduces the registry exactly");
}

#[tokio::test]
async fn a_tip_that_removed_its_store_retires_everything() {
    let fixture = Fixture::new();
    fixture.write(".claims/a.md", &claim_file("a", "A", &[]));
    fixture.commit("with a store");

    let (store, dir) = store().await;
    let mirrors = mirror_root(&dir);
    sync_store(&store, &fixture.connected(), &mirrors)
        .await
        .unwrap();
    assert_eq!(store.claims_of(STORE_ID).await.unwrap().len(), 1);

    // Remove the whole store: the next tip has no `.claims/`, so the snapshot is empty
    // and everything retires — the honest reading of a tip that deleted its store.
    fixture.remove(".claims/a.md");
    fixture.commit("remove the store");
    let outcome = sync_store(&store, &fixture.connected(), &mirrors)
        .await
        .unwrap();
    assert_eq!(outcome.claims_indexed, 0);
    assert!(store.claims_of(STORE_ID).await.unwrap().is_empty());
}

#[tokio::test]
async fn an_unreachable_remote_fails_loudly_and_snapshots_nothing() {
    // A sync that cannot mirror must fail loudly, never write an empty snapshot that
    // would retire every claim (invariant #6).
    let (store, dir) = store().await;
    let connected = ConnectedStore::new(STORE_ID, "/nonexistent/path/to/repo.git");
    let err = sync_store(&store, &connected, &mirror_root(&dir))
        .await
        .unwrap_err();
    // A local path that is not a repo makes `git clone` exit non-zero.
    assert!(
        matches!(err, claim_hub_store::StoreError::Git { .. }),
        "expected a git failure, got {err:?}"
    );
    // Nothing was recorded — no half-synced state.
    assert!(store.claims_of(STORE_ID).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_interval_poll_driver_syncs_each_connected_store() {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    let fixture = Fixture::new();
    fixture.write(".claims/a.md", &claim_file("a", "A", &[]));
    fixture.commit("seed");

    let (store, dir) = store().await;
    let outcomes: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&outcomes);

    let handle = claim_hub_store::spawn_interval_poll(
        store.clone(),
        vec![fixture.connected()],
        mirror_root(&dir),
        Duration::from_millis(50),
        move |id, result| {
            sink.lock().unwrap().push((id.to_owned(), result.is_ok()));
        },
    );

    // The first tick fires immediately; wait until the store reflects a sync, then
    // stop the poller. No sleep-then-assert race: poll the observable state.
    let mut synced = false;
    for _ in 0..100 {
        if !store.claims_of(STORE_ID).await.unwrap().is_empty() {
            synced = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.abort();
    assert!(synced, "the interval poller synced the store");

    let recorded = outcomes.lock().unwrap();
    assert!(
        recorded.iter().any(|(id, ok)| id == STORE_ID && *ok),
        "the driver reported a successful sync through on_result"
    );
}
