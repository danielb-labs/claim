//! Integration tests for the read API, in-process via [`tower::ServiceExt::oneshot`] —
//! no bound port, no network (HUB-IMPLEMENTATION.md §1.14).
//!
//! Every test seeds a real temp store, drives the assembled app, and asserts both the
//! response *shape* and its *as-of* (HUB.md §4). The read clock is fixed so freshness is
//! deterministic, and every input (check digests, timestamps) is a constant, so the
//! `insta` snapshots that pin the response shapes are stable across runs. Three properties
//! get dedicated tests: determinism (same inputs → byte-identical bytes), seq-pagination
//! (a resumed cursor gets exactly what is new, no gap, no dupe), and honest emptiness (an
//! unknown id is a 404, an empty set is an empty array, never a fabricated `verified`).

use super::*;
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use claim_core::{parse_claim_file, Timestamp, Verdict};
use claim_hub_core::{check_digest, CheckRef as EventCheckRef, Event, EventKind, Producer};
use claim_hub_store::{Ledger, RegisteredClaim, Registry, SqliteStore};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// The two stores the tests attribute claims to; `billing` sorts before `payments`, so a
/// (store, id) ordering is observable.
const BILLING: &str = "github.com/acme/billing";
const PAYMENTS: &str = "github.com/acme/payments";
/// The fixed read clock: well after the verdict instants, within a 30-day window from
/// them, so a held claim reads `verified` and its `stale_at`/`as_of` are constants.
const READ_NOW: &str = "2026-07-20T00:00:00Z";

/// An app over `store` whose read clock is a fixed `READ_NOW`. No verifier: these tests
/// exercise only the read path, so no ingest route is mounted.
fn app(store: SqliteStore) -> Router {
    let read_clock: crate::app::ReadClock =
        Arc::new(|| READ_NOW.parse::<Timestamp>().expect("valid instant"));
    crate::build_app(AppState::new(store, None).with_read_clock(read_clock))
}

/// Parse a claim from frontmatter and register it in `store` under `store_id` at
/// `seedcommit`. Returns the parsed claim so a caller can compute a check's expected
/// digest.
async fn seed(
    store: &SqliteStore,
    store_id: &str,
    file: &str,
    frontmatter: &str,
) -> claim_core::Claim {
    let text = format!("---\n{frontmatter}\n---\nThe statement.\n");
    let claim = parse_claim_file(file, &text).expect("valid claim");
    store
        .replace_store(
            store_id,
            &[RegisteredClaim::from_claim(&claim, "seedcommit")],
        )
        .await
        .expect("seed the registry");
    claim
}

/// Register several claims into one store in one snapshot (so the registry version is a
/// single bump), returning the parsed claims in the order given.
async fn seed_many(
    store: &SqliteStore,
    store_id: &str,
    files: &[(&str, &str)],
) -> Vec<claim_core::Claim> {
    let mut parsed = Vec::new();
    let mut registered = Vec::new();
    for (file, frontmatter) in files {
        let text = format!("---\n{frontmatter}\n---\nThe statement.\n");
        let claim = parse_claim_file(file, &text).expect("valid claim");
        registered.push(RegisteredClaim::from_claim(&claim, "seedcommit"));
        parsed.push(claim);
    }
    store
        .replace_store(store_id, &registered)
        .await
        .expect("seed the registry");
    parsed
}

/// A verdict event for the claim's `check_index`th check, in `store_id`, at `at`.
fn verdict_event(
    store_id: &str,
    claim: &claim_core::Claim,
    check_index: usize,
    verdict: Verdict,
    at: &str,
) -> Event {
    let mut producer = serde_json::Map::new();
    producer.insert("run".into(), serde_json::json!("run-1"));
    producer.insert("repository".into(), serde_json::json!("acme/payments"));
    Event {
        kind: EventKind::Verdict,
        claim: claim.id.as_str().to_owned(),
        check: EventCheckRef {
            index: check_index,
            digest: check_digest(&claim.checks[check_index]),
        },
        verdict,
        evidence: (verdict == Verdict::Held).then(|| "libfoo==4.2".to_owned()),
        commit: "abc1234".into(),
        store: store_id.into(),
        producer: Producer(producer),
        reported_at: at.parse().unwrap(),
    }
}

/// GET `uri` and return the status and parsed JSON body.
async fn get(app: &Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

/// GET `uri` and return the raw response body bytes — for the byte-identity determinism
/// test, which must compare the wire form, not a re-serialized value.
async fn get_bytes(app: &Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

/// A one-`cmd`-check claim with the given id and optional extra frontmatter lines.
fn frontmatter(id: &str, extra: &str) -> String {
    format!("id: {id}\n{extra}checks:\n  - kind: cmd\n    run: \"true\"")
}

// ---- GET /api/claims/{id} (carried over from hub-07, still covered here) ----

#[tokio::test]
async fn an_unknown_claim_is_a_404_naming_it() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let (status, json) = get(&app(store), "/api/claims/payments/not-there").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .contains("payments/not-there"),
        "the reason names the missing claim: {json}"
    );
}

#[tokio::test]
async fn a_namespaced_standing_carries_its_as_of() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let claim = seed(
        &store,
        PAYMENTS,
        ".claims/pin.md",
        &frontmatter("payments/libfoo-pin", "hub:\n  max-age: 30d\n"),
    )
    .await;
    store
        .append(&verdict_event(
            PAYMENTS,
            &claim,
            0,
            Verdict::Held,
            "2026-07-18T00:00:00Z",
        ))
        .await
        .unwrap();

    let (status, json) = get(&app(store), "/api/claims/payments/libfoo-pin").await;
    assert_eq!(status, StatusCode::OK, "the namespaced id routes: {json}");
    assert_eq!(json["id"], "payments/libfoo-pin");
    assert_eq!(json["standing"], "verified");
    assert_eq!(json["as_of"]["ledger_head"], 1);
    assert_eq!(json["as_of"]["registry_version"], 1);
    assert_eq!(json["as_of"]["clock"], READ_NOW);
}

#[tokio::test]
async fn a_read_appends_no_event() {
    // Invariant #3: a read derives, it does not store. Reading leaves the head unmoved.
    let store = SqliteStore::open_in_memory().await.unwrap();
    let claim = seed(
        &store,
        PAYMENTS,
        ".claims/pin.md",
        &frontmatter("payments/libfoo-pin", ""),
    )
    .await;
    store
        .append(&verdict_event(
            PAYMENTS,
            &claim,
            0,
            Verdict::Held,
            "2026-07-18T00:00:00Z",
        ))
        .await
        .unwrap();
    let head_before = store.head().await.unwrap();

    let app = app(store.clone());
    let _ = get(&app, "/api/claims/payments/libfoo-pin").await;
    let _ = get(&app, "/api/claims").await;
    let _ = get(&app, "/api/feed").await;
    let _ = get(&app, "/api/claims/payments/libfoo-pin/dossier").await;

    assert_eq!(
        store.head().await.unwrap(),
        head_before,
        "reads stored nothing"
    );
}

// ---- GET /api/claims (filters) ----

/// Seed a fixed corpus across two stores with a mix of standings, for the filter tests:
/// - billing/a: held in billing (verified)
/// - payments/a: never verified (stale)
/// - payments/pin: held, supports `decision:pin` (verified)
/// - payments/drift: drifted
async fn seed_corpus(store: &SqliteStore) {
    let billing = seed_many(
        store,
        BILLING,
        &[(".claims/a.md", &frontmatter("billing/a", ""))],
    )
    .await;
    store
        .append(&verdict_event(
            BILLING,
            &billing[0],
            0,
            Verdict::Held,
            "2026-07-18T00:00:00Z",
        ))
        .await
        .unwrap();

    let payments = seed_many(
        store,
        PAYMENTS,
        &[
            (".claims/a.md", &frontmatter("payments/a", "")),
            (
                ".claims/pin.md",
                &frontmatter("payments/pin", "supports:\n  - decision:pin\n"),
            ),
            (".claims/drift.md", &frontmatter("payments/drift", "")),
        ],
    )
    .await;
    // payments/a stays never-verified (stale). pin holds, drift drifts.
    store
        .append(&verdict_event(
            PAYMENTS,
            &payments[1],
            0,
            Verdict::Held,
            "2026-07-18T00:00:00Z",
        ))
        .await
        .unwrap();
    store
        .append(&verdict_event(
            PAYMENTS,
            &payments[2],
            0,
            Verdict::Drifted,
            "2026-07-18T00:00:00Z",
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn listing_all_claims_returns_the_whole_set_with_one_as_of() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let (status, json) = get(&app(store), "/api/claims").await;
    assert_eq!(status, StatusCode::OK);
    let claims = json["claims"].as_array().unwrap();
    assert_eq!(claims.len(), 4, "every seeded claim: {json}");
    // (store, id) order: billing/a, then the three payments claims by id.
    assert_eq!(claims[0]["store"], BILLING);
    assert_eq!(claims[0]["id"], "billing/a");
    assert_eq!(claims[1]["id"], "payments/a");
    // One shared as-of at the top level; list members carry none of their own.
    assert_eq!(json["as_of"]["registry_version"], 2, "two store snapshots");
    assert_eq!(json["as_of"]["clock"], READ_NOW);
    assert!(
        claims[0].get("as_of").is_none(),
        "no per-claim as-of in a list"
    );
}

#[tokio::test]
async fn the_store_filter_selects_one_store_exactly() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let (status, json) = get(&app(store), &format!("/api/claims?store={PAYMENTS}")).await;
    assert_eq!(status, StatusCode::OK);
    let claims = json["claims"].as_array().unwrap();
    assert_eq!(claims.len(), 3, "only the payments store: {json}");
    assert!(claims.iter().all(|c| c["store"] == PAYMENTS));
}

#[tokio::test]
async fn the_path_filter_is_an_id_prefix_match() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let (status, json) = get(&app(store), "/api/claims?path=payments/").await;
    assert_eq!(status, StatusCode::OK);
    let claims = json["claims"].as_array().unwrap();
    assert_eq!(claims.len(), 3, "the payments/ namespace: {json}");
    assert!(claims
        .iter()
        .all(|c| c["id"].as_str().unwrap().starts_with("payments/")));
}

#[tokio::test]
async fn the_standing_filter_selects_that_standing() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let (status, json) = get(&app(store), "/api/claims?standing=drifted").await;
    assert_eq!(status, StatusCode::OK);
    let claims = json["claims"].as_array().unwrap();
    assert_eq!(claims.len(), 1, "only the drifted claim: {json}");
    assert_eq!(claims[0]["id"], "payments/drift");
    assert_eq!(claims[0]["standing"], "drifted");
}

#[tokio::test]
async fn an_unknown_standing_filter_is_a_400_naming_the_accepted_set() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let (status, json) = get(&app(store), "/api/claims?standing=green").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let reason = json["error"].as_str().unwrap();
    assert!(reason.contains("green"), "names the bad value: {reason}");
    assert!(
        reason.contains("verified"),
        "names the accepted set: {reason}"
    );
}

#[tokio::test]
async fn an_unknown_query_param_is_a_400_not_silently_ignored() {
    // deny_unknown_fields: a mistyped filter must fail loudly, never silently return the
    // wrong set (invariant #6).
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let (status, _json) = get(&app(store), "/api/claims?stnading=drifted").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a typo'd filter is rejected"
    );
}

#[tokio::test]
async fn the_supports_filter_selects_claims_justifying_a_target() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let (status, json) = get(&app(store), "/api/claims?supports=decision:pin").await;
    assert_eq!(status, StatusCode::OK);
    let claims = json["claims"].as_array().unwrap();
    assert_eq!(
        claims.len(),
        1,
        "only the claim supporting decision:pin: {json}"
    );
    assert_eq!(claims[0]["id"], "payments/pin");
}

#[tokio::test]
async fn filters_combine_with_and() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    // payments store AND verified standing: pin (held) but not a (stale) or drift.
    let (status, json) = get(
        &app(store),
        &format!("/api/claims?store={PAYMENTS}&standing=verified"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let claims = json["claims"].as_array().unwrap();
    assert_eq!(claims.len(), 1, "payments AND verified: {json}");
    assert_eq!(claims[0]["id"], "payments/pin");
}

#[tokio::test]
async fn an_empty_result_is_an_empty_array_never_a_fabricated_verified() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let (status, json) = get(&app(store), "/api/claims?path=nonexistent/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json["claims"].as_array().unwrap().len(),
        0,
        "honest empty set"
    );
    // The as-of is still present and truthful — an empty set is a derivation, not an error.
    assert_eq!(json["as_of"]["clock"], READ_NOW);
}

// ---- The derived sets ----

#[tokio::test]
async fn the_drifted_set_holds_only_drifted_claims() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let (status, json) = get(&app(store), "/api/drifted").await;
    assert_eq!(status, StatusCode::OK);
    let claims = json["claims"].as_array().unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0]["id"], "payments/drift");
    assert_eq!(claims[0]["standing"], "drifted");
    assert_eq!(json["as_of"]["clock"], READ_NOW, "carries its as-of");
}

#[tokio::test]
async fn the_due_set_is_the_review_queue_not_a_standing_filter() {
    // The due set is drifted + stale + due-for-recheck, from the model's computed
    // membership. In the corpus: payments/a (stale) and payments/drift (drifted) — but not
    // the two held claims (verified, not yet due).
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let (status, json) = get(&app(store), "/api/due").await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = json["claims"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"payments/a"), "stale is queued: {ids:?}");
    assert!(
        ids.contains(&"payments/drift"),
        "drifted is queued: {ids:?}"
    );
    assert!(
        !ids.contains(&"payments/pin"),
        "verified not-yet-due is not: {ids:?}"
    );
    assert!(
        !ids.contains(&"billing/a"),
        "verified not-yet-due is not: {ids:?}"
    );
}

#[tokio::test]
async fn the_suspect_set_is_empty_until_the_propagation_rule_lands() {
    // No propagation rule yet: the set is honestly empty, not fabricated.
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let (status, json) = get(&app(store), "/api/suspect").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["claims"].as_array().unwrap().len(), 0);
    assert_eq!(json["as_of"]["clock"], READ_NOW);
}

// ---- The dossier ----

#[tokio::test]
async fn the_dossier_carries_statement_check_standing_history_and_as_of() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let claim = seed(
        &store,
        PAYMENTS,
        ".claims/pin.md",
        &frontmatter(
            "payments/libfoo-pin",
            "hub:\n  max-age: 30d\nsupports:\n  - decision:pin\n",
        ),
    )
    .await;
    store
        .append(&verdict_event(
            PAYMENTS,
            &claim,
            0,
            Verdict::Held,
            "2026-07-18T00:00:00Z",
        ))
        .await
        .unwrap();

    let (status, json) = get(&app(store), "/api/claims/payments/libfoo-pin/dossier").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["id"], "payments/libfoo-pin");
    assert_eq!(json["store"], PAYMENTS);
    assert_eq!(json["statement"], "The statement.");
    assert_eq!(
        json["commit"], "seedcommit",
        "the git reference the claim was read at"
    );
    assert_eq!(json["supports"][0], "decision:pin");
    // The check by git reference: index + content digest matching the ledger's join key.
    assert_eq!(json["checks"][0]["index"], 0);
    assert_eq!(json["checks"][0]["digest"], check_digest(&claim.checks[0]));
    // The standing, with the good news dated at the verdict instant.
    assert_eq!(json["standing"]["standing"], "verified");
    assert_eq!(json["standing"]["verified_as_of"], "2026-07-18T00:00:00Z");
    // The verdict history: one dated observation with its producer provenance.
    let history = json["history"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["seq"], 1);
    assert_eq!(history[0]["verdict"], "held");
    assert_eq!(history[0]["evidence"], "libfoo==4.2");
    assert_eq!(
        history[0]["producer"]["run"], "run-1",
        "derived provenance, verbatim"
    );
    // The as-of pins the derivation.
    assert_eq!(json["as_of"]["ledger_head"], 1);
    assert_eq!(json["as_of"]["clock"], READ_NOW);
}

#[tokio::test]
async fn a_dossier_for_an_unknown_claim_is_a_404() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let (status, json) = get(&app(store), "/api/claims/payments/ghost/dossier").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(json["error"].as_str().unwrap().contains("payments/ghost"));
}

#[tokio::test]
async fn a_dossier_history_holds_every_verdict_for_the_claim_in_order() {
    // Two verdicts for one check: held then drifted. The history carries both in seq order,
    // and the standing reflects the latest (drifted). Distinct producer runs so the two are
    // genuinely distinct observations, not one deduped on (store, run, claim, check).
    let store = SqliteStore::open_in_memory().await.unwrap();
    let claim = seed(
        &store,
        PAYMENTS,
        ".claims/pin.md",
        &frontmatter("payments/pin", ""),
    )
    .await;
    let mut held = verdict_event(PAYMENTS, &claim, 0, Verdict::Held, "2026-07-18T00:00:00Z");
    held.producer
        .0
        .insert("run".into(), serde_json::json!("run-a"));
    store.append(&held).await.unwrap();
    let mut drifted = verdict_event(
        PAYMENTS,
        &claim,
        0,
        Verdict::Drifted,
        "2026-07-19T00:00:00Z",
    );
    drifted
        .producer
        .0
        .insert("run".into(), serde_json::json!("run-b"));
    store.append(&drifted).await.unwrap();

    let (status, json) = get(&app(store), "/api/claims/payments/pin/dossier").await;
    assert_eq!(status, StatusCode::OK);
    let history = json["history"].as_array().unwrap();
    assert_eq!(history.len(), 2, "both verdicts: {json}");
    assert_eq!(history[0]["seq"], 1);
    assert_eq!(history[0]["verdict"], "held");
    assert_eq!(history[1]["seq"], 2);
    assert_eq!(history[1]["verdict"], "drifted");
    assert_eq!(
        json["standing"]["standing"], "drifted",
        "latest verdict wins"
    );
}

// ---- The cursor feed (seq pagination) ----

#[tokio::test]
async fn the_feed_from_the_start_returns_the_whole_ledger_with_its_head() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let claim = seed(
        &store,
        PAYMENTS,
        ".claims/pin.md",
        &frontmatter("payments/pin", ""),
    )
    .await;
    let mut held = verdict_event(PAYMENTS, &claim, 0, Verdict::Held, "2026-07-18T00:00:00Z");
    held.producer
        .0
        .insert("run".into(), serde_json::json!("run-a"));
    store.append(&held).await.unwrap();
    let mut drifted = verdict_event(
        PAYMENTS,
        &claim,
        0,
        Verdict::Drifted,
        "2026-07-19T00:00:00Z",
    );
    drifted
        .producer
        .0
        .insert("run".into(), serde_json::json!("run-b"));
    store.append(&drifted).await.unwrap();

    let (status, json) = get(&app(store), "/api/feed").await;
    assert_eq!(status, StatusCode::OK);
    let events = json["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["seq"], 1);
    assert_eq!(events[1]["seq"], 2);
    // The event is the verbatim envelope, flattened alongside its seq.
    assert_eq!(events[0]["verdict"], "held");
    assert_eq!(events[0]["claim"], "payments/pin");
    assert_eq!(json["next_cursor"], 2, "the last seq to resume after");
    assert_eq!(json["ledger_head"], 2, "the feed's as-of position");
}

#[tokio::test]
async fn a_cursor_resumes_exactly_after_the_last_seen_seq_no_gap_no_dupe() {
    // The seq-pagination contract: page one from cursor 0 returns seqs 1..=2; passing back
    // next_cursor returns seqs 3.., with no overlap and no gap.
    let store = SqliteStore::open_in_memory().await.unwrap();
    let claim = seed(
        &store,
        PAYMENTS,
        ".claims/pin.md",
        &frontmatter("payments/pin", ""),
    )
    .await;
    for (i, at) in [
        "2026-07-18T00:00:00Z",
        "2026-07-19T00:00:00Z",
        "2026-07-20T00:00:00Z",
    ]
    .iter()
    .enumerate()
    {
        // Distinct producer runs so each append is a new event, not a dedup.
        let mut ev = verdict_event(PAYMENTS, &claim, 0, Verdict::Held, at);
        ev.producer
            .0
            .insert("run".into(), serde_json::json!(format!("run-{i}")));
        store.append(&ev).await.unwrap();
    }
    let app = app(store);

    // Page one from the start.
    let (_status, page1) = get(&app, "/api/feed?cursor=0").await;
    let seqs1: Vec<i64> = page1["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_i64().unwrap())
        .collect();
    assert_eq!(seqs1, vec![1, 2, 3]);
    let next = page1["next_cursor"].as_i64().unwrap();
    assert_eq!(next, 3);

    // Resume after the last-seen seq: nothing new, no dupe of 1..=3.
    let (_status, page2) = get(&app, &format!("/api/feed?cursor={next}")).await;
    assert_eq!(
        page2["events"].as_array().unwrap().len(),
        0,
        "caught up: {page2}"
    );
    // A caught-up poller passing back the same cursor stays put (next_cursor unchanged).
    assert_eq!(page2["next_cursor"], 3);
    assert_eq!(page2["ledger_head"], 3);

    // Resuming from a mid-ledger cursor returns strictly what follows, no overlap.
    let (_status, mid) = get(&app, "/api/feed?cursor=1").await;
    let seqs: Vec<i64> = mid["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_i64().unwrap())
        .collect();
    assert_eq!(seqs, vec![2, 3], "strictly after cursor 1, no dupe of 1");
}

#[tokio::test]
async fn a_negative_cursor_reads_from_the_start() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let claim = seed(
        &store,
        PAYMENTS,
        ".claims/pin.md",
        &frontmatter("payments/pin", ""),
    )
    .await;
    store
        .append(&verdict_event(
            PAYMENTS,
            &claim,
            0,
            Verdict::Held,
            "2026-07-18T00:00:00Z",
        ))
        .await
        .unwrap();
    let (status, json) = get(&app(store), "/api/feed?cursor=-5").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json["events"].as_array().unwrap().len(),
        1,
        "clamped to the start"
    );
}

#[tokio::test]
async fn an_empty_ledger_feed_is_an_empty_page_at_head_zero() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let (status, json) = get(&app(store), "/api/feed").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["events"].as_array().unwrap().len(), 0);
    assert_eq!(json["next_cursor"], 0);
    assert_eq!(json["ledger_head"], 0);
}

// ---- Determinism ----

#[tokio::test]
async fn the_same_inputs_yield_byte_identical_responses() {
    // Determinism (HUB.md §5): with the same (cursor, registry version, clock) the same
    // endpoint returns byte-identical bytes. The read clock is fixed, the registry unchanged
    // between reads, and the ledger append-only, so every read of every endpoint is exact.
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let app = app(store);

    for uri in [
        "/api/claims",
        "/api/claims?store=github.com/acme/payments",
        "/api/drifted",
        "/api/due",
        "/api/suspect",
        "/api/feed",
        "/api/claims/payments/pin/dossier",
    ] {
        let (s1, b1) = get_bytes(&app, uri).await;
        let (s2, b2) = get_bytes(&app, uri).await;
        assert_eq!(s1, s2, "{uri} status is stable");
        assert_eq!(b1, b2, "{uri} bytes are identical across reads");
    }
}

// ---- Snapshots (insta) pinning the response shapes ----

/// Build a fixed, fully-populated corpus for the snapshots: one held claim with a window,
/// one drifted claim, both with a verdict history — so every field a snapshot pins is a
/// constant (fixed clock, fixed digests, fixed timestamps).
async fn seed_snapshot_corpus(store: &SqliteStore) {
    let pin = seed(
        store,
        PAYMENTS,
        ".claims/pin.md",
        &frontmatter(
            "payments/pin",
            "hub:\n  max-age: 30d\nsupports:\n  - decision:pin\n",
        ),
    )
    .await;
    store
        .append(&verdict_event(
            PAYMENTS,
            &pin,
            0,
            Verdict::Held,
            "2026-07-18T00:00:00Z",
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn snapshot_claim_standing() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_snapshot_corpus(&store).await;
    let (_status, json) = get(&app(store), "/api/claims/payments/pin").await;
    insta::assert_json_snapshot!(json);
}

#[tokio::test]
async fn snapshot_claims_list() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_snapshot_corpus(&store).await;
    let (_status, json) = get(&app(store), "/api/claims").await;
    insta::assert_json_snapshot!(json);
}

#[tokio::test]
async fn snapshot_dossier() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_snapshot_corpus(&store).await;
    let (_status, json) = get(&app(store), "/api/claims/payments/pin/dossier").await;
    insta::assert_json_snapshot!(json);
}

#[tokio::test]
async fn snapshot_feed() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_snapshot_corpus(&store).await;
    let (_status, json) = get(&app(store), "/api/feed").await;
    insta::assert_json_snapshot!(json);
}

#[tokio::test]
async fn snapshot_drifted_empty_set() {
    // An empty derived set is a shape worth pinning: `claims: []` with a truthful as-of,
    // never a fabricated verified.
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_snapshot_corpus(&store).await;
    let (_status, json) = get(&app(store), "/api/drifted").await;
    insta::assert_json_snapshot!(json);
}
