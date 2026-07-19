//! Boot-and-serve integration tests: the shell stands up from a minimal config
//! against an empty directory and serves `/status` truthfully.
//!
//! These drive the assembled app in-process via [`tower::ServiceExt::oneshot`] — no
//! bound port, no network (HUB-IMPLEMENTATION.md §1.14) — over a *real, file-backed*
//! store the boot path creates, so the test exercises first-boot database creation
//! and migration, not just an in-memory shortcut. The unit tests in `app.rs` cover
//! the head/version advance; these cover the boot seam an operator hits.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use claim_hub::{build_app, AppState, Config};
use claim_hub_store::{Ledger, SqliteStore};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// Open the store the way boot does — from a config's database path — over a fresh
/// temp directory, and build the app on it. Returns the app plus the store (kept so
/// a test can append) and the tempdir guard (kept so the file outlives the test).
async fn boot_app_from_minimal_config() -> (axum::Router, SqliteStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("hub.db");
    // A minimal config: only the database path, everything else defaulted. This is
    // the smallest file a self-hoster writes.
    let toml = format!("database = {:?}\n", db_path.to_str().unwrap());
    let config = Config::from_toml(&toml).unwrap();
    // The boot path: open (creating + migrating) the SQLite file from the config's
    // database path. Empty directory in, a stood-up schema out.
    let store = SqliteStore::open(&config.database).await.unwrap();
    // No verifier: the boot test exercises `/status` and first-boot schema creation, not
    // ingest (which has its own test file with an injected JWKS).
    let app = build_app(AppState::new(store.clone(), None));
    (app, store, dir)
}

/// Send a GET through the assembled app in-process and return its status and JSON.
async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let response = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    (status, body)
}

#[tokio::test]
async fn boots_from_minimal_config_and_status_reports_truthful_zeros() {
    let (app, _store, _dir) = boot_app_from_minimal_config().await;
    let (status, body) = get_json(app, "/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ledger_head"], 0, "empty ledger reports head 0");
    assert_eq!(
        body["registry_version"], 0,
        "empty registry reports version 0"
    );
    assert_eq!(body["rejection_count"], 0);
    assert!(
        body.get("last_sync").is_none(),
        "never synced: no fabricated timestamp: {body}"
    );
}

#[tokio::test]
async fn status_reflects_a_non_empty_store_after_boot() {
    // Append one event through the Ledger trait and confirm /status, served over the
    // same file-backed store the boot path opened, advances its head.
    let (app, store, _dir) = boot_app_from_minimal_config().await;
    let mut producer = serde_json::Map::new();
    producer.insert("run".into(), serde_json::json!("run-1"));
    let event = claim_hub_core::Event {
        kind: claim_hub_core::EventKind::Verdict,
        claim: "payments/libfoo-pin".into(),
        check: claim_hub_core::CheckRef {
            index: 0,
            digest: "b".repeat(64),
        },
        verdict: claim_core::Verdict::Held,
        evidence: None,
        commit: "8f2c0a1".into(),
        store: "github.com/acme/payments".into(),
        producer: claim_hub_core::Producer(producer),
        reported_at: "2026-07-18T06:00:00Z".parse().unwrap(),
    };
    store.append(&event).await.unwrap();

    let (status, body) = get_json(app, "/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ledger_head"], 1, "head advances after the append");
}

#[tokio::test]
async fn serve_fails_loudly_when_the_database_cannot_be_opened() {
    // Point `database` at a directory: SQLite cannot open a directory as a database
    // file, so `serve` errors before it binds or serves. The message names the path
    // so an operator sees *which* database refused (the `with_context` in `serve`).
    let dir = tempfile::tempdir().unwrap();
    let config = Config::from_toml(&format!(
        "database = {:?}\nlisten = \"127.0.0.1:0\"\n",
        dir.path().to_str().unwrap()
    ))
    .unwrap();
    let err = claim_hub::serve(config)
        .await
        .expect_err("opening a directory as a database file must fail");
    let message = format!("{err:#}");
    assert!(
        message.contains(dir.path().to_str().unwrap()),
        "names the database path: {message}"
    );
    assert!(
        message.contains("opening the hub database"),
        "names the failing operation: {message}"
    );
}

#[tokio::test]
async fn serve_fails_loudly_when_the_listen_address_cannot_be_bound() {
    // 192.0.2.1 is RFC 5737 TEST-NET-1: reserved for documentation and not assigned
    // to this host, so binding it fails. The database opens fine (a real temp file),
    // so the failure is the bind — `serve` errors before it serves, naming the
    // address (the `with_context` on the listener). A network-free negative: no
    // socket to any real peer is attempted, only a local bind that the kernel refuses.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("hub.db");
    // `open_reads = true` so read-auth resolution (which runs before the bind, enforcing
    // secure-by-default) succeeds and the failure is genuinely the bind — this test is
    // about the listener, not the auth policy.
    let config = Config::from_toml(&format!(
        "database = {:?}\nlisten = \"192.0.2.1:8080\"\n[read_auth]\nopen_reads = true\n",
        db_path.to_str().unwrap()
    ))
    .unwrap();
    let err = claim_hub::serve(config)
        .await
        .expect_err("binding an unassigned address must fail");
    let message = format!("{err:#}");
    assert!(
        message.contains("192.0.2.1:8080"),
        "names the listen address: {message}"
    );
    assert!(
        message.contains("binding the hub listener"),
        "names the failing operation: {message}"
    );
}
