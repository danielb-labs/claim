//! Integration tests for the [`Rejections`] counter over the SQLite store.
//!
//! The ingest gate increments this on every refused push, and `/status` reads it, so a
//! hub turning telemetry away is visible rather than silently aging claims into stale
//! (invariant #6). The counter is monotonic and starts at zero.

use claim_hub_store::{Rejections, SqliteStore};
use tempfile::TempDir;

async fn fresh_store() -> (SqliteStore, TempDir) {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("hub.db");
    let store = SqliteStore::open(&path).await.expect("open + migrate");
    (store, dir)
}

#[tokio::test]
async fn a_fresh_store_has_no_rejections() {
    let (store, _dir) = fresh_store().await;
    assert_eq!(store.rejection_count().await.unwrap(), 0);
}

#[tokio::test]
async fn recording_a_rejection_increments_and_returns_the_new_count() {
    let (store, _dir) = fresh_store().await;
    assert_eq!(store.record_rejection().await.unwrap(), 1);
    assert_eq!(store.record_rejection().await.unwrap(), 2);
    assert_eq!(store.record_rejection().await.unwrap(), 3);
    assert_eq!(
        store.rejection_count().await.unwrap(),
        3,
        "the read reflects every recorded rejection"
    );
}

#[tokio::test]
async fn the_count_survives_reopening_the_database() {
    // A rejection is durable: the counter persists across a reopen, so a monitor's view
    // of turned-away telemetry does not reset when the hub restarts.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hub.db");
    {
        let store = SqliteStore::open(&path).await.unwrap();
        store.record_rejection().await.unwrap();
        store.record_rejection().await.unwrap();
    }
    let reopened = SqliteStore::open(&path).await.unwrap();
    assert_eq!(reopened.rejection_count().await.unwrap(), 2);
}
