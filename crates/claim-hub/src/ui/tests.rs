//! Integration tests for the UI, the markdown twins, and `llms.txt` — in-process via
//! [`tower::ServiceExt::oneshot`], no bound port, no network.
//!
//! The load-bearing properties: **twin-parity** (the HTML page and its `.md` twin render
//! from one view model, so they carry the same facts — asserted by cross-checking both
//! against the same seeded standing, and pinned by `insta` snapshots of both); **`llms.txt`
//! names every surface** (a new page or endpoint that forgets to register there fails the
//! test); and **a read stores nothing** (invariant #3). The read clock is fixed so freshness
//! and the snapshots are deterministic.

use super::*;
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use claim_core::{parse_claim_file, Timestamp, Verdict};
use claim_hub_core::{check_digest, CheckRef, Event, EventKind, Producer};
use claim_hub_store::{Ledger, RegisteredClaim, Registry, SqliteStore};
use http_body_util::BodyExt;
use tower::ServiceExt;

const PAYMENTS: &str = "github.com/acme/payments";
const BILLING: &str = "github.com/acme/billing";
/// The fixed read clock: within a 30-day window of the seeded verdicts, so a held claim
/// reads `verified` and its `stale_at`/`as_of` are constants the snapshots pin.
const READ_NOW: &str = "2026-07-20T00:00:00Z";

/// An app over `store` with a fixed read clock and no verifier (the UI is a read).
fn app(store: SqliteStore) -> Router {
    let read_clock: crate::app::ReadClock =
        Arc::new(|| READ_NOW.parse::<Timestamp>().expect("valid instant"));
    crate::build_app(AppState::new(store, None).with_read_clock(read_clock))
}

/// Parse a claim from frontmatter and register it under `store_id` at `seedcommit`.
async fn seed(
    store: &SqliteStore,
    store_id: &str,
    file: &str,
    frontmatter: &str,
    statement: &str,
) -> claim_core::Claim {
    let text = format!("---\n{frontmatter}\n---\n{statement}\n");
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

/// A verdict event for the nth check of a claim, at `at`, with a fixed producer.
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
        check: CheckRef {
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

/// A one-`cmd`-check claim frontmatter with the given id and optional extra lines.
fn frontmatter(id: &str, extra: &str) -> String {
    format!("id: {id}\n{extra}checks:\n  - kind: cmd\n    run: \"true\"")
}

/// GET `uri` and return the status, the `content-type`, and the body as a string.
async fn get(app: &Router, uri: &str) -> (StatusCode, String, String) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8(bytes.to_vec()).expect("utf-8 body");
    (status, content_type, body)
}

/// Seed a mixed corpus across two stores: a verified claim with a verdict and supports edge,
/// a stale (never-verified) claim, and a drifted claim — so the queue holds two and the
/// dossier renders a verified claim's full history.
async fn seed_corpus(store: &SqliteStore) {
    let pin = seed(
        store,
        PAYMENTS,
        ".claims/pin.md",
        &format!(
            "{}\nsupports:\n  - decision:pin",
            frontmatter("payments/pin", "hub:\n  max-age: 30d\n")
        ),
        "libfoo is pinned to 4.2.",
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

    // A drifted claim in billing: its latest verdict is drifted, so it is in the queue.
    let drift = seed(
        store,
        BILLING,
        ".claims/drift.md",
        &frontmatter("billing/drift", ""),
        "The rate cache is warmed on boot.",
    )
    .await;
    store
        .append(&verdict_event(
            BILLING,
            &drift,
            0,
            Verdict::Drifted,
            "2026-07-19T00:00:00Z",
        ))
        .await
        .unwrap();
}

// ---- twin path convention ----

#[tokio::test]
async fn every_page_serves_html_and_a_markdown_twin_at_dot_md() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let app = app(store);

    for (html_path, md_path) in [
        ("/ui/queue", "/ui/queue.md"),
        ("/ui/status", "/ui/status.md"),
        ("/ui/claims/payments/pin", "/ui/claims/payments/pin.md"),
    ] {
        let (hs, hct, hbody) = get(&app, html_path).await;
        assert_eq!(hs, StatusCode::OK, "{html_path} serves");
        assert!(hct.starts_with("text/html"), "{html_path} is html: {hct}");
        assert!(hbody.contains("<!DOCTYPE html>"), "{html_path} is a page");

        let (ms, mct, mbody) = get(&app, md_path).await;
        assert_eq!(ms, StatusCode::OK, "{md_path} serves");
        assert!(
            mct.starts_with("text/markdown"),
            "{md_path} is markdown: {mct}"
        );
        assert!(!mbody.contains("<!DOCTYPE html>"), "{md_path} is not html");
    }
}

// ---- twin parity: the two lenses agree on the facts ----

#[tokio::test]
async fn the_queue_html_and_twin_hold_the_same_claims() {
    // Twin-parity by construction: both render from one QueueView, so both must name the
    // same queued claims and the same as-of.
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let app = app(store);

    let (_s, _ct, html) = get(&app, "/ui/queue").await;
    let (_s2, _ct2, md) = get(&app, "/ui/queue.md").await;

    // The drifted claim is queued in both lenses.
    assert!(
        html.contains("billing/drift"),
        "html names the drifted claim"
    );
    assert!(md.contains("billing/drift"), "twin names the drifted claim");
    // The as-of clock is identical across the two lenses.
    assert!(html.contains(READ_NOW), "html carries the as-of clock");
    assert!(md.contains(READ_NOW), "twin carries the as-of clock");
    // A fresh, not-yet-due verified claim is in neither.
    assert!(
        !html.contains("payments/pin"),
        "a fresh not-due claim is not queued (html)"
    );
    assert!(
        !md.contains("payments/pin"),
        "a fresh not-due claim is not queued (twin)"
    );
}

#[tokio::test]
async fn the_dossier_html_and_twin_hold_the_same_statement_and_history() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let app = app(store);

    let (_s, _ct, html) = get(&app, "/ui/claims/payments/pin").await;
    let (_s2, _ct2, md) = get(&app, "/ui/claims/payments/pin.md").await;

    for body in [&html, &md] {
        assert!(
            body.contains("libfoo is pinned to 4.2."),
            "statement present"
        );
        assert!(body.contains("decision:pin"), "supports edge present");
        assert!(body.contains("held"), "the held verdict is in the history");
        assert!(body.contains("run=run-1"), "producer origin is rendered");
        assert!(
            body.contains("verified"),
            "the derived standing is rendered"
        );
        assert!(body.contains(READ_NOW), "the as-of clock is rendered");
    }
}

// ---- the producer is rendered as evidence, never an instruction ----

#[tokio::test]
async fn a_producer_string_is_rendered_as_a_flat_origin_line() {
    // A hub UI an agent reads must not be an injection channel (PRODUCT.md §6): the producer
    // is a flat `key=value` origin line, sorted, not free prose that could carry a command.
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let app = app(store);
    let (_s, _ct, md) = get(&app, "/ui/claims/payments/pin.md").await;
    // Sorted keys: `repository` sorts before `run`.
    assert!(
        md.contains("repository=acme/payments run=run-1"),
        "producer is a sorted flat origin line: {md}"
    );
}

// ---- llms.txt indexes every surface ----

#[tokio::test]
async fn llms_txt_indexes_every_surface() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let app = app(store);
    let (status, ct, body) = get(&app, "/llms.txt").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.starts_with("text/plain"), "llms.txt is plain text: {ct}");

    // Every JSON API endpoint.
    for surface in [
        "/api/claims/{id}",
        "/api/claims",
        "/api/claims/{id}/dossier",
        "/api/drifted",
        "/api/due",
        "/api/suspect",
        "/api/feed",
        "/status",
        "POST /ingest",
    ] {
        assert!(body.contains(surface), "llms.txt names `{surface}`");
    }
    // Every UI page and its twin.
    for surface in [
        "/ui/queue",
        "/ui/queue.md",
        "/ui/claims/{id}",
        "/ui/claims/{id}.md",
        "/ui/status",
        "/ui/status.md",
    ] {
        assert!(
            body.contains(surface),
            "llms.txt names UI surface `{surface}`"
        );
    }
}

// ---- reads are deterministic (same inputs → byte-identical render) ----

#[tokio::test]
async fn a_page_renders_byte_identically_on_repeated_reads() {
    // The same (ledger head, registry version, clock) must render the same bytes, so an
    // agent can cache and diff. The fixed read clock makes this hold across two reads.
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let app = app(store);

    for uri in ["/ui/queue", "/ui/queue.md", "/ui/claims/payments/pin.md"] {
        let (_s1, _c1, first) = get(&app, uri).await;
        let (_s2, _c2, second) = get(&app, uri).await;
        assert_eq!(first, second, "{uri} renders byte-identically on repeat");
    }
}

// ---- a claim aging into stale by the clock alone enters the queue ----

#[tokio::test]
async fn a_claim_stale_by_the_clock_alone_appears_in_the_queue() {
    // No new event: a held claim past its max-age window reads stale and is queued, purely by
    // the clock advancing. The queue is a derived projection, so the transition needs no write.
    let store = SqliteStore::open_in_memory().await.unwrap();
    let pin = seed(
        &store,
        PAYMENTS,
        ".claims/pin.md",
        &frontmatter("payments/pin", "hub:\n  max-age: 30d\n"),
        "A fact with a 30-day window.",
    )
    .await;
    // Held long ago: at READ_NOW (2026-07-20) the 30-day window from 2026-01-01 has lapsed.
    store
        .append(&verdict_event(
            PAYMENTS,
            &pin,
            0,
            Verdict::Held,
            "2026-01-01T00:00:00Z",
        ))
        .await
        .unwrap();
    let app = app(store);

    let (_s, _ct, html) = get(&app, "/ui/queue").await;
    let (_s2, _ct2, md) = get(&app, "/ui/queue.md").await;
    assert!(
        html.contains("payments/pin"),
        "the stale claim is queued (html)"
    );
    assert!(
        md.contains("payments/pin"),
        "the stale claim is queued (twin)"
    );
    assert!(html.contains("stale"), "its standing is stale (html)");
    assert!(md.contains("stale"), "its standing is stale (twin)");
}

// ---- honest emptiness and honest 404s ----

#[tokio::test]
async fn an_empty_queue_says_so_rather_than_faking_a_green() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    // A single verified, not-yet-due claim: nothing is queued.
    let pin = seed(
        &store,
        PAYMENTS,
        ".claims/pin.md",
        &frontmatter("payments/pin", "hub:\n  max-age: 30d\n"),
        "A held fact.",
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
    let app = app(store);

    let (_s, _ct, html) = get(&app, "/ui/queue").await;
    let (_s2, _ct2, md) = get(&app, "/ui/queue.md").await;
    assert!(
        html.contains("queue is empty"),
        "html says the queue is empty"
    );
    assert!(
        md.contains("queue is empty"),
        "twin says the queue is empty"
    );
}

#[tokio::test]
async fn an_unknown_claim_dossier_is_a_404_naming_it() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let app = app(store);
    let (status, _ct, body) = get(&app, "/ui/claims/payments/not-there").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body.contains("payments/not-there"),
        "the 404 names the claim"
    );

    // The twin of an unknown id also 404s, and is not silently shadowed as a claim id ending
    // in `.md`.
    let (md_status, _ct2, _b) = get(&app, "/ui/claims/payments/not-there.md").await;
    assert_eq!(md_status, StatusCode::NOT_FOUND);
}

// ---- a read stores nothing (invariant #3) ----

#[tokio::test]
async fn rendering_pages_appends_no_event() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let head_before = store.head().await.unwrap();
    let app = app(store.clone());

    for uri in [
        "/ui/queue",
        "/ui/queue.md",
        "/ui/status",
        "/ui/status.md",
        "/ui/claims/payments/pin",
        "/ui/claims/payments/pin.md",
        "/llms.txt",
    ] {
        let _ = get(&app, uri).await;
    }

    assert_eq!(
        store.head().await.unwrap(),
        head_before,
        "rendering pages stored nothing"
    );
}

// ---- snapshots: both lenses of every page are pinned ----

/// Seed a fixed corpus for the snapshots: constant ids, timestamps, and producer, so the
/// rendered HTML and markdown are byte-stable across runs.
async fn seed_snapshot_corpus(store: &SqliteStore) {
    let pin = seed(
        store,
        PAYMENTS,
        ".claims/pin.md",
        &format!(
            "{}\nsupports:\n  - decision:pin",
            frontmatter("payments/pin", "hub:\n  max-age: 30d\n")
        ),
        "libfoo is pinned to 4.2.",
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
    let drift = seed(
        store,
        BILLING,
        ".claims/drift.md",
        &frontmatter("billing/drift", ""),
        "The rate cache is warmed on boot.",
    )
    .await;
    store
        .append(&verdict_event(
            BILLING,
            &drift,
            0,
            Verdict::Drifted,
            "2026-07-19T00:00:00Z",
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn snapshot_queue_html() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_snapshot_corpus(&store).await;
    let (_s, _ct, body) = get(&app(store), "/ui/queue").await;
    insta::assert_snapshot!(body);
}

#[tokio::test]
async fn snapshot_queue_md() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_snapshot_corpus(&store).await;
    let (_s, _ct, body) = get(&app(store), "/ui/queue.md").await;
    insta::assert_snapshot!(body);
}

#[tokio::test]
async fn snapshot_dossier_html() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_snapshot_corpus(&store).await;
    let (_s, _ct, body) = get(&app(store), "/ui/claims/payments/pin").await;
    insta::assert_snapshot!(body);
}

#[tokio::test]
async fn snapshot_dossier_md() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_snapshot_corpus(&store).await;
    let (_s, _ct, body) = get(&app(store), "/ui/claims/payments/pin.md").await;
    insta::assert_snapshot!(body);
}

#[tokio::test]
async fn snapshot_status_html() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_snapshot_corpus(&store).await;
    let (_s, _ct, body) = get(&app(store), "/ui/status").await;
    insta::assert_snapshot!(body);
}

#[tokio::test]
async fn snapshot_status_md() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_snapshot_corpus(&store).await;
    let (_s, _ct, body) = get(&app(store), "/ui/status.md").await;
    insta::assert_snapshot!(body);
}

#[tokio::test]
async fn snapshot_llms_txt() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let (_s, _ct, body) = get(&app(store), "/llms.txt").await;
    insta::assert_snapshot!(body);
}
