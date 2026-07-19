//! Integration tests for the [`Ledger`] over the SQLite store.
//!
//! Each test migrates a fresh temp database file and drives the trait — no shared
//! state, no network. The properties pinned here are the ones the ledger's honesty
//! rests on: the cursor round-trip and its monotonicity, idempotent redelivery on
//! the dedup key, and immutability enforced below the trait by the triggers.

use claim_core::Verdict;
use claim_hub_core::{CheckRef, Event, EventKind, Producer};
use claim_hub_store::{Appended, Ledger, Position, SqliteStore};
use tempfile::TempDir;

/// A store over a fresh migrated database file in a returned temp dir (kept alive
/// by the caller so the file outlives the store).
async fn fresh_store() -> (SqliteStore, TempDir) {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("hub.db");
    let store = SqliteStore::open(&path).await.expect("open + migrate");
    (store, dir)
}

/// An event with a caller-chosen run id, claim, and digest, so tests control the
/// dedup key (run, claim, digest) directly.
fn event(run: &str, claim: &str, digest: &str, verdict: Verdict) -> Event {
    let mut producer = serde_json::Map::new();
    producer.insert(
        "iss".into(),
        serde_json::json!("https://token.actions.githubusercontent.com"),
    );
    producer.insert("repository".into(), serde_json::json!("acme/payments"));
    producer.insert("run".into(), serde_json::json!(run));
    Event {
        kind: EventKind::Verdict,
        claim: claim.into(),
        check: CheckRef {
            index: 0,
            digest: digest.into(),
        },
        verdict,
        evidence: Some("libfoo==4.2".into()),
        commit: "8f2c0a1".into(),
        store: "github.com/acme/payments".into(),
        producer: Producer(producer),
        reported_at: "2026-07-18T06:00:00Z".parse().unwrap(),
    }
}

#[tokio::test]
async fn append_scan_head_round_trip_with_monotonic_seq() {
    let (store, _dir) = fresh_store().await;

    // A fresh ledger's head is Position(0) and a scan from 0 is empty.
    assert_eq!(store.head().await.unwrap(), Position(0));
    assert!(store.scan_from(Position(0)).await.unwrap().is_empty());

    let a = event("run-1", "payments/pin", &"a".repeat(64), Verdict::Held);
    let b = event("run-2", "payments/pin", &"b".repeat(64), Verdict::Drifted);

    let pa = store.append(&a).await.unwrap();
    let pb = store.append(&b).await.unwrap();
    let (pa, pb) = (pa.position(), pb.position());

    // seq is strictly increasing and is the cursor.
    assert!(pa < pb, "positions are monotonic: {pa:?} < {pb:?}");
    assert_eq!(store.head().await.unwrap(), pb, "head is the last position");

    // A full scan returns both events, in order, verbatim.
    let all = store.scan_from(Position(0)).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].position, pa);
    assert_eq!(all[0].event, a, "stored event equals the appended one");
    assert_eq!(all[1].position, pb);
    assert_eq!(all[1].event, b);

    // The cursor resumes: scanning from the first position yields only the second.
    let after_a = store.scan_from(pa).await.unwrap();
    assert_eq!(after_a.len(), 1);
    assert_eq!(after_a[0].event, b);

    // Scanning from head yields nothing.
    assert!(store.scan_from(pb).await.unwrap().is_empty());
}

#[tokio::test]
async fn redelivering_the_same_observation_yields_one_row_and_idempotent_success() {
    let (store, _dir) = fresh_store().await;
    let digest = "c".repeat(64);
    let e = event("run-42", "payments/pin", &digest, Verdict::Held);

    let first = store.append(&e).await.unwrap();
    let Appended::New(pos) = first else {
        panic!("first append is New, got {first:?}");
    };

    // The same (producer run, claim, check identity), redelivered, is absorbed: one
    // row, an idempotent success carrying the original position.
    let second = store.append(&e).await.unwrap();
    assert_eq!(
        second,
        Appended::Duplicate(pos),
        "redelivery returns the original position as a Duplicate"
    );

    // Even a redelivery whose *verdict* differs must not slip past the dedup key —
    // the same run re-reporting the same check is the same observation, and a
    // changed verdict getting a second row would be a silent double-count.
    let mut tampered = e.clone();
    tampered.verdict = Verdict::Drifted;
    let third = store.append(&tampered).await.unwrap();
    assert_eq!(third, Appended::Duplicate(pos));

    // Exactly one row on the ledger, still the original.
    let all = store.scan_from(Position(0)).await.unwrap();
    assert_eq!(all.len(), 1, "dedup kept a single row");
    assert_eq!(all[0].event.verdict, Verdict::Held, "the original survived");
}

#[tokio::test]
async fn a_different_run_is_not_a_duplicate() {
    let (store, _dir) = fresh_store().await;
    let digest = "d".repeat(64);
    let a = event("run-1", "payments/pin", &digest, Verdict::Held);
    let b = event("run-2", "payments/pin", &digest, Verdict::Held);

    assert!(matches!(store.append(&a).await.unwrap(), Appended::New(_)));
    // Same claim and check identity, different producer run: a distinct observation.
    assert!(matches!(store.append(&b).await.unwrap(), Appended::New(_)));
    assert_eq!(store.scan_from(Position(0)).await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_different_check_identity_is_not_a_duplicate() {
    let (store, _dir) = fresh_store().await;
    let a = event("run-1", "payments/pin", &"a".repeat(64), Verdict::Held);
    let b = event("run-1", "payments/pin", &"b".repeat(64), Verdict::Held);

    assert!(matches!(store.append(&a).await.unwrap(), Appended::New(_)));
    // Same run and claim, different check digest: a distinct observation.
    assert!(matches!(store.append(&b).await.unwrap(), Appended::New(_)));
    assert_eq!(store.scan_from(Position(0)).await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_raw_update_against_events_is_rejected_by_the_trigger() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hub.db");
    let store = SqliteStore::open(&path).await.unwrap();
    let e = event("run-1", "payments/pin", &"a".repeat(64), Verdict::Held);
    store.append(&e).await.unwrap();

    // Reach around the trait with raw SQL on a separate connection to the same file:
    // the append-only trigger must reject it, so history is immutable even below the
    // seam, not merely by the trait's shape (HUB-IMPLEMENTATION.md §1.4).
    let url = format!("sqlite://{}", path.display());
    let raw = sqlx::SqlitePool::connect(&url).await.unwrap();
    let err = sqlx::query("UPDATE events SET verdict = 'drifted' WHERE seq = 1")
        .execute(&raw)
        .await
        .expect_err("UPDATE against events must fail");
    assert!(
        err.to_string().contains("append-only"),
        "the trigger names the reason: {err}"
    );

    // The row is untouched.
    let all = store.scan_from(Position(0)).await.unwrap();
    assert_eq!(all[0].event.verdict, Verdict::Held);
}

#[tokio::test]
async fn a_raw_delete_against_events_is_rejected_by_the_trigger() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hub.db");
    let store = SqliteStore::open(&path).await.unwrap();
    let e = event("run-1", "payments/pin", &"a".repeat(64), Verdict::Held);
    store.append(&e).await.unwrap();

    let url = format!("sqlite://{}", path.display());
    let raw = sqlx::SqlitePool::connect(&url).await.unwrap();
    let err = sqlx::query("DELETE FROM events WHERE seq = 1")
        .execute(&raw)
        .await
        .expect_err("DELETE against events must fail");
    assert!(
        err.to_string().contains("append-only"),
        "the trigger names the reason: {err}"
    );

    // The row survives.
    assert_eq!(store.scan_from(Position(0)).await.unwrap().len(), 1);
}

#[tokio::test]
async fn migrations_run_from_an_empty_file_on_first_boot() {
    // A path that does not exist yet: open must create the file, migrate it, and
    // return a working store — the self-host first-boot story.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nested").join("fresh.db");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    assert!(!path.exists(), "the database file does not exist yet");

    let store = SqliteStore::open(&path).await.expect("first boot migrates");
    assert!(path.exists(), "open created the database file");
    assert_eq!(store.head().await.unwrap(), Position(0));
    let e = event("run-1", "payments/pin", &"a".repeat(64), Verdict::Held);
    assert!(matches!(store.append(&e).await.unwrap(), Appended::New(_)));
}

#[tokio::test]
async fn evidence_none_round_trips_as_none() {
    let (store, _dir) = fresh_store().await;
    let mut e = event("run-1", "payments/pin", &"a".repeat(64), Verdict::Held);
    e.evidence = None;
    store.append(&e).await.unwrap();
    let back = store.scan_from(Position(0)).await.unwrap();
    assert_eq!(back[0].event.evidence, None, "absent evidence stays absent");
    assert_eq!(back[0].event, e);
}
