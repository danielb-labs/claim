//! Integration tests for the [`Ledger`] over the SQLite store.
//!
//! Each test migrates a fresh temp database file and drives the trait — no shared
//! state, no network. The properties pinned here are the ones the ledger's honesty
//! rests on: the cursor round-trip and its monotonicity, idempotent redelivery on
//! the dedup key, and immutability enforced below the trait by the triggers.

use claim_core::Verdict;
use claim_hub_core::{CheckRef, Event, EventKind, Producer};
use claim_hub_store::{Appended, Ledger, Position, SqliteStore, StoreError};
use tempfile::TempDir;

/// A store over a fresh migrated database file in a returned temp dir (kept alive
/// by the caller so the file outlives the store).
async fn fresh_store() -> (SqliteStore, TempDir) {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("hub.db");
    let store = SqliteStore::open(&path).await.expect("open + migrate");
    (store, dir)
}

/// An event with a caller-chosen run id, claim, and digest in the default store, so
/// most tests control the dedup key's (run, claim, digest) directly.
fn event(run: &str, claim: &str, digest: &str, verdict: Verdict) -> Event {
    event_in_store("github.com/acme/payments", run, claim, digest, verdict)
}

/// An event with a caller-chosen store *and* (run, claim, digest), so the cross-store
/// dedup test can hold (run, claim, digest) fixed while varying the store.
fn event_in_store(store: &str, run: &str, claim: &str, digest: &str, verdict: Verdict) -> Event {
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
        store: store.into(),
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
async fn a_raw_insert_or_replace_against_events_is_rejected_by_the_trigger() {
    use sqlx::sqlite::SqliteConnectOptions;
    use std::str::FromStr;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hub.db");
    let store = SqliteStore::open(&path).await.unwrap();
    let e = event("run-1", "payments/pin", &"a".repeat(64), Verdict::Held);
    store.append(&e).await.unwrap();

    // INSERT OR REPLACE runs as an implicit DELETE-then-INSERT on a conflict, and a
    // BEFORE DELETE trigger fires for that implicit delete *only* when
    // recursive_triggers is ON — which `SqliteStore` sets on every connection. Open a
    // raw connection with the same hardening the app applies, and prove REPLACE on the
    // live dedup key is rejected, so it cannot silently rewrite the row or its seq.
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .unwrap()
        .foreign_keys(true)
        .pragma("recursive_triggers", "ON");
    let raw = sqlx::SqlitePool::connect_with(opts).await.unwrap();

    // A REPLACE that conflicts on the UNIQUE dedup key (store, run, claim, digest)
    // would delete the Held row and insert a Drifted one with a new seq.
    let err = sqlx::query(
        r#"INSERT OR REPLACE INTO events
               (kind, claim_id, check_index, check_digest, verdict,
                "commit", store, producer, reported_at, dedup_run)
           VALUES ('verdict', 'payments/pin', 0, ?, 'drifted',
                   'deadbeef', 'github.com/acme/payments', '{}', '2026-07-18T07:00:00Z', 'run-1')"#,
    )
    .bind("a".repeat(64))
    .execute(&raw)
    .await
    .expect_err("INSERT OR REPLACE against a live events row must fail");
    assert!(
        err.to_string().contains("append-only"),
        "the append-only trigger fires for REPLACE's implicit delete: {err}"
    );

    // The original row is untouched — same verdict, same seq.
    let all = store.scan_from(Position(0)).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].event.verdict, Verdict::Held, "history not rewritten");
    assert_eq!(all[0].position, Position(1), "seq unchanged");
}

#[tokio::test]
async fn recursive_triggers_is_off_by_default_but_on_when_hardened() {
    use sqlx::sqlite::SqliteConnectOptions;
    use std::str::FromStr;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hub.db");
    let _store = SqliteStore::open(&path).await.unwrap();
    let url = format!("sqlite://{}", path.display());

    // A bare connection defaults recursive_triggers OFF — which is exactly why the
    // REPLACE bypass exists and why the app must set the pragma itself.
    let bare = sqlx::SqlitePool::connect(&url).await.unwrap();
    let (off,): (i64,) = sqlx::query_as("PRAGMA recursive_triggers")
        .fetch_one(&bare)
        .await
        .unwrap();
    assert_eq!(off, 0, "a bare connection defaults the pragma off");

    // A connection hardened the way `SqliteStore` hardens its own reads it back ON, so
    // the pragma string the app uses genuinely enables the trigger-firing REPLACE
    // guards. The REPLACE test above proves the guard fires under exactly this pragma.
    let opts = SqliteConnectOptions::from_str(&url)
        .unwrap()
        .foreign_keys(true)
        .pragma("recursive_triggers", "ON");
    let hardened = sqlx::SqlitePool::connect_with(opts).await.unwrap();
    let (on,): (i64,) = sqlx::query_as("PRAGMA recursive_triggers")
        .fetch_one(&hardened)
        .await
        .unwrap();
    assert_eq!(on, 1, "the app's pragma turns recursive_triggers on");
}

#[tokio::test]
async fn the_same_observation_from_two_stores_is_two_rows_not_a_dedup() {
    // A run id is unique per repository, not globally, and the check digest is
    // content-based and stable across repos. Two stores sharing (run, claim, digest)
    // are genuinely distinct observations; `store` in the dedup key keeps both.
    let (store, _dir) = fresh_store().await;
    let digest = "e".repeat(64);
    let a = event_in_store(
        "github.com/acme/payments",
        "run-1",
        "shared/pin",
        &digest,
        Verdict::Held,
    );
    let b = event_in_store(
        "github.com/acme/web",
        "run-1",
        "shared/pin",
        &digest,
        Verdict::Held,
    );

    assert!(matches!(store.append(&a).await.unwrap(), Appended::New(_)));
    assert!(
        matches!(store.append(&b).await.unwrap(), Appended::New(_)),
        "a different store is not a duplicate, even with the same run/claim/digest"
    );
    assert_eq!(store.scan_from(Position(0)).await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_verdict_with_no_producer_run_is_rejected_loudly() {
    let (store, _dir) = fresh_store().await;

    // A producer with no `run` at all.
    let mut no_run = event("run-1", "payments/pin", &"a".repeat(64), Verdict::Held);
    no_run.producer.0.remove("run");
    let err = store
        .append(&no_run)
        .await
        .expect_err("run-less is rejected");
    assert!(matches!(err, StoreError::MissingProducerRun), "{err}");

    // An empty-string `run` is as unattributable as an absent one.
    let mut empty_run = event("", "payments/pin", &"b".repeat(64), Verdict::Held);
    empty_run
        .producer
        .0
        .insert("run".into(), serde_json::json!(""));
    let err = store
        .append(&empty_run)
        .await
        .expect_err("empty run is rejected");
    assert!(matches!(err, StoreError::MissingProducerRun), "{err}");

    // Neither was stored: a rejected verdict adds nothing.
    assert!(store.scan_from(Position(0)).await.unwrap().is_empty());
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
