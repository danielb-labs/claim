//! Cross-surface consistency for the skip-ranking projection (hub-14, issue #9).
//!
//! The load-bearing property of hub-14 is that **all four read surfaces show the same ranked
//! skip set**: the JSON API (`GET /api/skips`), the hub MCP (`skips` tool over the mounted
//! `/mcp`), the markdown twin (`/ui/queue.md`), and the HTML UI (`/ui/queue`). They share one
//! pure ranking (`claim_hub_core::rank_skips`) through one derived read model, so they *cannot*
//! disagree — this test asserts that end to end, over the same seeded store and the same fixed
//! read clock, in-process via `tower::ServiceExt::oneshot` (no bound port, no network).
//!
//! The seeded store has three skips across two stores whose ranked order is unambiguous —
//! lapsed first, then the nearer un-lapsed expiry, then the indefinite skip — so a surface that
//! ranked differently (or dropped a skip) would fail here even though its own body still parsed.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use claim_core::{parse_claim_file, Timestamp};
use claim_hub::app::{AppState, ReadClock};
use claim_hub_store::{RegisteredClaim, Registry, SqliteStore};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

const PAYMENTS: &str = "github.com/acme/payments";
const BILLING: &str = "github.com/acme/billing";
/// Read at 2026-07-20: the 2026-01-01 skip has lapsed, the 2027-06-01 one has not.
const READ_NOW: &str = "2026-07-20T00:00:00Z";

/// The ranked reasons every surface must show, in this exact order.
const EXPECTED_RANKED_REASONS: &[&str] = &["lapsed one", "not-yet-lapsed one", "indefinite one"];

/// Register a claim from frontmatter under `store_id`.
async fn seed(store: &SqliteStore, store_id: &str, file: &str, frontmatter: &str) {
    let text = format!("---\n{frontmatter}\n---\nStatement body.\n");
    let claim = parse_claim_file(file, &text).expect("valid claim");
    store
        .replace_store(
            store_id,
            &[RegisteredClaim::from_claim(&claim, "seedcommit")],
        )
        .await
        .expect("seed the registry");
}

/// An app over a store seeded with three skips across two stores, at a fixed read clock so
/// every surface derives the identical instant.
async fn app_with_skips() -> axum::Router {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed(
        &store,
        PAYMENTS,
        ".claims/parked.md",
        "id: payments/parked\nchecks:\n  \
         - kind: cmd\n    run: \"a\"\n    \
         skip:\n      reason: lapsed one\n      until: 2026-01-01\n  \
         - kind: cmd\n    run: \"b\"\n    \
         skip:\n      reason: not-yet-lapsed one\n      until: 2027-06-01",
    )
    .await;
    seed(
        &store,
        BILLING,
        ".claims/muted.md",
        "id: billing/muted\nchecks:\n  \
         - kind: cmd\n    run: \"c\"\n    skip:\n      reason: indefinite one",
    )
    .await;
    let read_clock: ReadClock = Arc::new(|| READ_NOW.parse::<Timestamp>().expect("valid instant"));
    claim_hub::build_app(AppState::new(store, None).with_read_clock(read_clock))
}

/// GET `uri` and return the body as a string.
async fn get_text(app: &axum::Router, uri: &str) -> String {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).expect("utf-8 body")
}

/// GET `uri` and parse the JSON body.
async fn get_json(app: &axum::Router, uri: &str) -> serde_json::Value {
    serde_json::from_str(&get_text(app, uri).await).expect("json body")
}

/// POST an MCP `tools/call` for `skips` over the mounted `/mcp`, returning the structured body.
async fn mcp_skips(app: &axum::Router) -> serde_json::Value {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("Host", "localhost")
                .header("Content-Type", "application/json")
                .header("Accept", "application/json, text/event-stream")
                .body(Body::from(
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "tools/call",
                        "params": { "name": "skips", "arguments": {} }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "tools/call skips");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    body["result"]["structuredContent"].clone()
}

/// The ranked reasons, in order, as a JSON skips body reports them.
fn reasons_of(body: &serde_json::Value) -> Vec<String> {
    body["skips"]
        .as_array()
        .expect("skips array")
        .iter()
        .map(|s| s["reason"].as_str().unwrap().to_owned())
        .collect()
}

/// The order the three reasons appear in a rendered page, by first-occurrence index.
fn rendered_order(body: &str) -> Vec<&'static str> {
    let mut with_pos: Vec<(usize, &'static str)> = EXPECTED_RANKED_REASONS
        .iter()
        .map(|&reason| {
            (
                body.find(reason)
                    .unwrap_or_else(|| panic!("missing `{reason}` in page: {body}")),
                reason,
            )
        })
        .collect();
    with_pos.sort_by_key(|(pos, _)| *pos);
    with_pos.into_iter().map(|(_, r)| r).collect()
}

#[tokio::test]
async fn all_four_surfaces_show_the_same_ranked_skip_set() {
    let app = app_with_skips().await;

    // 1. JSON API.
    let api = get_json(&app, "/api/skips").await;
    assert_eq!(
        reasons_of(&api),
        EXPECTED_RANKED_REASONS,
        "the API ranks lapsed → aging → indefinite: {api}"
    );

    // 2. MCP tool over the mount — byte-identical structured content to the API.
    let mcp = mcp_skips(&app).await;
    assert_eq!(
        mcp, api,
        "the MCP `skips` tool returns the API's exact body"
    );

    // 3. Markdown twin — the same skips in the same ranked order.
    let twin = get_text(&app, "/ui/queue.md").await;
    assert_eq!(
        rendered_order(&twin),
        EXPECTED_RANKED_REASONS,
        "the twin renders the same ranked order: {twin}"
    );

    // 4. HTML UI — the same skips in the same ranked order.
    let html = get_text(&app, "/ui/queue").await;
    assert_eq!(
        rendered_order(&html),
        EXPECTED_RANKED_REASONS,
        "the HTML queue renders the same ranked order: {html}"
    );

    // The four surfaces agree on the exact ranked reason sequence — one ranking, four lenses.
    let api_reasons = reasons_of(&api);
    assert_eq!(api_reasons, reasons_of(&mcp));
    assert_eq!(
        api_reasons,
        rendered_order(&twin)
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        api_reasons,
        rendered_order(&html)
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_skip_does_not_change_a_claims_standing_on_any_surface() {
    // Invariant #4: a skip is queue data, never a verdict. The all-skipped claim derives
    // `stale` (never verified), and no surface folds the skip into a green — the standing at
    // `/api/claims/{id}` is `stale`, and the queue shows the skip as a debt, not a pass.
    let app = app_with_skips().await;

    let standing = get_json(&app, "/api/claims/payments/parked").await;
    assert_eq!(
        standing["standing"], "stale",
        "an all-skipped claim is stale, never verified: {standing}"
    );

    // The skip surface lists the debt without asserting any verdict for it.
    let api = get_json(&app, "/api/skips").await;
    let first = &api["skips"][0];
    assert!(
        first.get("standing").is_none() && first.get("verdict").is_none(),
        "a ranked skip carries no standing and no verdict: {api}"
    );
}
