//! The backup-restore self-host test (hub-15): the data-ownership invariant as an
//! executable exercise, not an assertion.
//!
//! HUB.md §1/§4: the whole hub is one SQLite file the customer owns — back up by taking a
//! consistent snapshot of it, leave by taking that file. This test proves the promise holds:
//! seed a real file-backed hub through the real ingest gate, read a claim's standing, **back
//! up the database online** (`VACUUM INTO`, safe against a live writer) into one
//! self-contained file, open a second hub over it, and derive the same claim's standing — the
//! two derivations must be byte-identical. If a restored hub ever derived a different answer,
//! "you own your data" would be a lie; this is the test that would catch it.
//!
//! It is deterministic and network-free (the harness's injected JWKS and fixed clocks), and
//! it exercises the **real** store: a real `SqliteStore::open` on a real file, a real ingest
//! append, a real online backup of the live DB into one sidecar-free file, and the real
//! deriver over the restored copy. Why online backup rather than a file copy: a `cp` against
//! a live WAL-mode hub can race a checkpoint and silently drop the ledger tail (invariants #4
//! and #6); `crates/claim-hub-store/tests/backup.rs` pins that data-loss and its fix directly.
//! The shell script `scripts/hub-backup-restore.sh` is the same exercise one layer out — over
//! the real HTTP *server* binary — run in CI; this test is the gate's deterministic,
//! in-process guarantee of the identical-answers property.

mod common;

use std::sync::Arc;

use claim_core::Timestamp;
use claim_hub::app::{AppState, Clock, ReadClock};
use claim_hub::oidc::OidcVerifier;
use claim_hub_core::{derive, DeriverConfig};
use claim_hub_store::{ledger_events, registry_snapshot, SqliteStore};
use common::*;

/// A one-cmd-check claim with a 30-day window, so the derived standing carries a concrete
/// `stale_at` that must survive the copy unchanged.
const PIN_CLAIM: &str = "id: payments/libfoo-pin\nhub:\n  max-age: 30d\nchecks:\n  - kind: cmd\n    run: \"grep -q 'libfoo==4.2' requirements.txt\"";

/// A fixed read instant within the window, so both derivations run at the same clock and the
/// standing is `verified` on each side — the identical-answers comparison is over the derived
/// facts, not over two different clocks.
const READ_NOW: &str = "2026-07-28T12:00:00Z";

/// Open a file-backed store and build an app whose ingest clock is the harness's fixed
/// instant and whose read clock is `READ_NOW`, verifying tokens against `source`.
async fn file_backed_app(
    db_path: &std::path::Path,
    source: TestJwksSource,
) -> (axum::Router, SqliteStore) {
    let store = SqliteStore::open(db_path).await.expect("open file store");
    let verifier = OidcVerifier::new(
        TEST_ISSUER,
        TEST_AUDIENCE,
        [TEST_REPOSITORY.to_owned()],
        source,
    );
    let ingest_clock: Clock = Arc::new(ingest_instant);
    let read_clock: ReadClock = Arc::new(|| READ_NOW.parse::<Timestamp>().expect("instant"));
    let state = AppState::new(store.clone(), Some(Arc::new(verifier)))
        .with_clock(ingest_clock)
        .with_read_clock(read_clock);
    (claim_hub::build_app(state), store)
}

/// Derive the read model over a store at `READ_NOW`, then render the one claim's standing as
/// canonical JSON — the value the backup must reproduce. Serialized deterministically
/// (`ClaimStanding` is a `serde` struct over a `BTreeMap`), so two byte-identical strings
/// mean two identical derivations.
async fn derived_standing_json(store: &SqliteStore, id: &str) -> String {
    let registry = registry_snapshot(store).await.expect("registry snapshot");
    let events = ledger_events(store).await.expect("ledger events");
    let now: Timestamp = READ_NOW.parse().expect("instant");
    let model = derive(&registry, &events, now, &DeriverConfig::default());
    let standing = model
        .claims
        .iter()
        .find(|((_, cid), _)| cid == id)
        .map(|(_, s)| s)
        .unwrap_or_else(|| panic!("claim `{id}` not in the derived model"));
    serde_json::to_string(standing).expect("serialize standing")
}

#[tokio::test]
async fn a_restored_hub_derives_an_identical_standing() {
    let dir = tempfile::tempdir().unwrap();
    let db_a = dir.path().join("a").join("hub.db");
    std::fs::create_dir_all(db_a.parent().unwrap()).unwrap();

    // Seed the original hub through the REAL ingest gate: sync-equivalent registry seed plus
    // one attested `held` verdict, appended to a real file-backed store.
    let (app, store_a) = file_backed_app(&db_a, TestJwksSource::with_signing_key()).await;
    seed_claim(&store_a, ".claims/pin.md", PIN_CLAIM).await;
    let token = sign_token(&TokenClaims::valid());
    let body = one_check_report("payments/libfoo-pin", "held", Some("libfoo==4.2"));
    let (status, json) = post_ingest(&app, Some(&token), &body).await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "the seed verdict is ingested: {json}"
    );

    // The original hub's derived standing.
    let original = derived_standing_json(&store_a, "payments/libfoo-pin").await;
    assert!(
        original.contains("\"verified\""),
        "the seeded claim derives verified: {original}"
    );
    assert!(
        original.contains("2026-08-17T12:00:00Z"),
        "stale_at is the claim's own 30d window from the verdict instant: {original}"
    );

    // Back up: an online `VACUUM INTO` snapshot of the live store into one self-contained
    // file — no sidecars, safe against the still-open writer, so no committed event can be
    // lost to a racing checkpoint. This is "leave by taking the file", taken correctly
    // against a running hub (a live hub is not stopped to be backed up).
    let db_b = dir.path().join("b").join("hub.db");
    std::fs::create_dir_all(db_b.parent().unwrap()).unwrap();
    backup_database(&store_a, &db_b).await;

    // Restore: open a FRESH hub over the copy — no re-seed, no re-ingest — and derive again.
    let store_b = SqliteStore::open(&db_b)
        .await
        .expect("open the restored store");
    let restored = derived_standing_json(&store_b, "payments/libfoo-pin").await;

    assert_eq!(
        original, restored,
        "the restored hub must derive a byte-identical standing:\n original: {original}\n restored: {restored}"
    );
}

#[tokio::test]
async fn a_restored_hub_reads_the_same_answer_over_the_http_read_api() {
    // The same guarantee one layer out: the restored hub's `GET /api/claims/{id}` response
    // matches the original's on every field but the wall-clock read instant (`as_of.clock`),
    // which the two reads legitimately differ on. Proves the copy carries the whole read
    // surface, not just the raw rows.
    let dir = tempfile::tempdir().unwrap();
    let db_a = dir.path().join("a").join("hub.db");
    std::fs::create_dir_all(db_a.parent().unwrap()).unwrap();

    let (app_a, store_a) = file_backed_app(&db_a, TestJwksSource::with_signing_key()).await;
    seed_claim(&store_a, ".claims/pin.md", PIN_CLAIM).await;
    let token = sign_token(&TokenClaims::valid());
    let body = one_check_report("payments/libfoo-pin", "held", Some("libfoo==4.2"));
    let (status, _json) = post_ingest(&app_a, Some(&token), &body).await;
    assert_eq!(status, axum::http::StatusCode::OK);

    let mut answer_a = get_claim(&app_a, "payments/libfoo-pin").await;

    // The store stays open behind `app_a` (a live hub); the online backup snapshots it
    // consistently into one sidecar-free file, no need to stop the writer.
    let db_b = dir.path().join("b").join("hub.db");
    std::fs::create_dir_all(db_b.parent().unwrap()).unwrap();
    backup_database(&store_a, &db_b).await;

    let (app_b, _store_b) = file_backed_app(&db_b, TestJwksSource::with_signing_key()).await;
    let mut answer_b = get_claim(&app_b, "payments/libfoo-pin").await;

    // Drop the wall-clock read instant from each as-of: it is the only field a second read
    // seconds later is allowed to differ on. Everything else is a function of the copied
    // ledger and registry and must match exactly.
    strip_read_clock(&mut answer_a);
    strip_read_clock(&mut answer_b);
    assert_eq!(
        answer_a, answer_b,
        "the restored hub serves an identical read answer (modulo the wall-clock read instant)"
    );
}

/// Remove the volatile `as_of.clock` (wall-clock read instant) so two reads taken seconds
/// apart are comparable on the fields that must survive the backup unchanged.
fn strip_read_clock(answer: &mut serde_json::Value) {
    if let Some(as_of) = answer.get_mut("as_of").and_then(|v| v.as_object_mut()) {
        as_of.remove("clock");
    }
}
