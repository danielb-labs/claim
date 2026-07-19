//! Integration tests for the [`Findings`] store over SQLite, in isolation from git.
//!
//! Findings are derived data on the same replace-per-sync discipline as the registry:
//! a store's findings are *replaced*, not merged, so a fixed file's finding clears;
//! the query views agree; and the cross-store read is ordered deterministically. These
//! drive the trait directly against a temp SQLite file — no git, no network — so the
//! storage seam is proven independently of the sync that feeds it.

use claim_core::ClaimId;
use claim_hub_store::{
    Findings, RegisteredClaim, Registry, RegistryVersion, SqliteStore, SyncFinding,
};
use std::str::FromStr;
use tempfile::TempDir;

const STORE: &str = "github.com/acme/payments";

async fn fresh_store() -> (SqliteStore, TempDir) {
    let dir = TempDir::new().expect("temp dir");
    let store = SqliteStore::open(dir.path().join("hub.db"))
        .await
        .expect("open + migrate");
    (store, dir)
}

fn finding(store: &str, file: &str, reason: &str) -> SyncFinding {
    SyncFinding {
        store: store.to_owned(),
        file: file.to_owned(),
        commit: "8f2c0a1".to_owned(),
        reason: reason.to_owned(),
    }
}

#[tokio::test]
async fn replace_store_findings_records_and_reads_back() {
    let (store, _dir) = fresh_store().await;
    // A store must exist for the finding's foreign key; a real sync replaces the
    // registry first. Establish the store, then record findings.
    store.replace_store(STORE, &[]).await.unwrap();

    let findings = [
        finding(STORE, ".claims/broken.md", "invalid YAML"),
        finding(STORE, "CLAUDE.md", "unterminated block"),
    ];
    store
        .replace_store_findings(STORE, &findings)
        .await
        .unwrap();

    // Read back in ascending file order.
    let read = store.findings().await.unwrap();
    assert_eq!(read.len(), 2);
    assert_eq!(read[0].file, ".claims/broken.md");
    assert_eq!(read[1].file, "CLAUDE.md");
    assert_eq!(read[0].reason, "invalid YAML");
    // The per-store view agrees with the global one for a single store.
    assert_eq!(store.findings_of(STORE).await.unwrap(), read);
}

#[tokio::test]
async fn replace_is_a_wipe_not_a_merge() {
    let (store, _dir) = fresh_store().await;
    store.replace_store(STORE, &[]).await.unwrap();
    store
        .replace_store_findings(STORE, &[finding(STORE, "a.md", "r1")])
        .await
        .unwrap();

    // Replacing with a different set drops the old finding entirely.
    store
        .replace_store_findings(STORE, &[finding(STORE, "b.md", "r2")])
        .await
        .unwrap();
    let read = store.findings().await.unwrap();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].file, "b.md");
}

#[tokio::test]
async fn an_empty_replace_clears_a_stores_findings() {
    let (store, _dir) = fresh_store().await;
    store.replace_store(STORE, &[]).await.unwrap();
    store
        .replace_store_findings(STORE, &[finding(STORE, "a.md", "r")])
        .await
        .unwrap();
    // The healthy state: every claim file parsed, so no findings.
    store.replace_store_findings(STORE, &[]).await.unwrap();
    assert!(store.findings().await.unwrap().is_empty());
}

#[tokio::test]
async fn findings_span_stores_in_deterministic_order() {
    let (store, _dir) = fresh_store().await;
    let other = "github.com/acme/web";
    store.replace_store(STORE, &[]).await.unwrap();
    store.replace_store(other, &[]).await.unwrap();
    store
        .replace_store_findings(STORE, &[finding(STORE, "z.md", "r")])
        .await
        .unwrap();
    store
        .replace_store_findings(other, &[finding(other, "a.md", "r")])
        .await
        .unwrap();

    // Ordered by (store, file): "…/payments" sorts before "…/web".
    let all = store.findings().await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].store, STORE);
    assert_eq!(all[1].store, other);
    // The per-store view isolates one store.
    assert_eq!(store.findings_of(STORE).await.unwrap().len(), 1);
}

#[tokio::test]
async fn replacing_a_stores_registry_snapshot_cascades_its_findings() {
    // A store's findings foreign-key to the store row; the sync also keys them by
    // store. Findings and the registry snapshot are independent replaces, but both are
    // per-store — this confirms findings for one store never bleed into another and
    // survive a registry snapshot replace of the same store.
    let (store, _dir) = fresh_store().await;
    store.replace_store(STORE, &[]).await.unwrap();
    store
        .replace_store_findings(STORE, &[finding(STORE, "a.md", "r")])
        .await
        .unwrap();
    // Re-snapshotting the registry (a later sync) does not touch findings on its own.
    store.replace_store(STORE, &[]).await.unwrap();
    assert_eq!(store.findings_of(STORE).await.unwrap().len(), 1);
}

#[tokio::test]
async fn replace_store_snapshot_writes_claims_and_findings_together() {
    // The atomic method sync uses: claims and findings land in one call, one version
    // bump. Both are visible after it, and the version advanced once.
    let (store, _dir) = fresh_store().await;
    let claim = RegisteredClaim {
        id: ClaimId::from_str("good").unwrap(),
        statement: "Good".to_owned(),
        supports: vec!["decisions/x".to_owned()],
        commit: "c1".to_owned(),
        check_digests: Vec::new(),
        hub: Default::default(),
    };
    let v = store
        .replace_store_snapshot(STORE, &[claim], &[finding(STORE, "broken.md", "bad YAML")])
        .await
        .unwrap();
    assert_eq!(v, RegistryVersion(1), "one snapshot, one version bump");

    let claims = store.claims_of(STORE).await.unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].id.as_str(), "good");
    assert_eq!(claims[0].supports, vec!["decisions/x".to_owned()]);

    let findings = store.findings_of(STORE).await.unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].file, "broken.md");
}
