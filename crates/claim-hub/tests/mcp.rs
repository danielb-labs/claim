//! End-to-end test that the hub MCP is mounted on the axum router and speaks the protocol.
//!
//! The unit tests (`src/mcp/tests.rs`) prove parity and schema stability by calling the tool
//! methods directly. This binary proves the *transport*: the streamable-HTTP MCP service is
//! nested at `/mcp` on the same app that serves `/api`, and a real MCP request over that
//! mount lists the tools and calls one, returning the same body its API twin serves — all
//! in-process via `tower::ServiceExt::oneshot`, no bound port and no network
//! (HUB-IMPLEMENTATION.md §1.14). The transport runs in stateless JSON mode, so a
//! `tools/list` or `tools/call` is one POST with a plain-JSON response, no session handshake.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use claim_core::{parse_claim_file, Timestamp, Verdict};
use claim_hub::app::{AppState, ReadClock};
use claim_hub_core::{check_digest, CheckRef, Event, EventKind, Producer};
use claim_hub_store::{Ledger, RegisteredClaim, Registry, SqliteStore};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

const STORE: &str = "github.com/acme/payments";
const READ_NOW: &str = "2026-07-20T00:00:00Z";

/// The assembled app over a seeded store, with a fixed read clock so both the MCP and the
/// API derive the identical instant. No verifier: reads need none.
async fn app_with_seed() -> axum::Router {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let text = "---\nid: payments/pin\nhub:\n  max-age: 30d\nsupports:\n  - decision:pin\nchecks:\n  - kind: cmd\n    run: \"true\"\n---\nThe pin holds.\n";
    let claim = parse_claim_file(".claims/pin.md", text).unwrap();
    store
        .replace_store(STORE, &[RegisteredClaim::from_claim(&claim, "seedcommit")])
        .await
        .unwrap();
    let mut producer = serde_json::Map::new();
    producer.insert("run".into(), json!("run-1"));
    store
        .append(&Event {
            kind: EventKind::Verdict,
            claim: "payments/pin".into(),
            check: CheckRef {
                index: 0,
                digest: check_digest(&claim.checks[0]),
            },
            verdict: Verdict::Held,
            evidence: Some("libfoo==4.2".into()),
            commit: "abc1234".into(),
            store: STORE.into(),
            producer: Producer(producer),
            reported_at: "2026-07-18T00:00:00Z".parse().unwrap(),
        })
        .await
        .unwrap();

    let read_clock: ReadClock = Arc::new(|| READ_NOW.parse::<Timestamp>().expect("valid instant"));
    claim_hub::build_app(AppState::new(store, None).with_read_clock(read_clock))
}

/// POST an MCP JSON-RPC request body to the mounted `/mcp` and return the parsed response.
///
/// The `Accept` header offers both JSON and SSE per the spec; the stateless-JSON transport
/// answers with `application/json`, so the body parses directly. The `Host: localhost` header
/// satisfies rmcp's DNS-rebinding protection (which rejects a request with no `Host`); a real
/// HTTP client always sends one, so this is a test-shim, not a behavior of the mount.
async fn mcp_post(app: &axum::Router, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("Host", "localhost")
                .header("Content-Type", "application/json")
                .header("Accept", "application/json, text/event-stream")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// GET an API endpoint on the same app, for the transport-level parity check.
async fn api_get(app: &axum::Router, uri: &str) -> serde_json::Value {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn tools_list_over_the_mount_names_the_five_read_tools() {
    let app = app_with_seed().await;
    let (status, body) = mcp_post(
        &app,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tools/list over /mcp: {body}");
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["context", "dossier", "drifts", "due", "search"],
        "the mount advertises the five read tools: {body}"
    );
}

#[tokio::test]
async fn calling_dossier_over_the_mount_matches_the_api_body() {
    // The transport-level parity proof: a `tools/call` for `dossier` over `/mcp` returns the
    // same structured content the API's dossier endpoint serves, on the same app.
    let app = app_with_seed().await;
    let (status, body) = mcp_post(
        &app,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "dossier", "arguments": { "id": "payments/pin" } }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tools/call dossier: {body}");
    let structured = &body["result"]["structuredContent"];
    let api = api_get(&app, "/api/claims/payments/pin/dossier").await;
    assert_eq!(
        structured, &api,
        "the tool's structured content equals the API dossier body: {body}"
    );
    assert!(
        body["result"]["isError"].as_bool() != Some(true),
        "a known claim's dossier is not an error: {body}"
    );
}

#[tokio::test]
async fn calling_context_over_the_mount_matches_the_api_claims_body() {
    let app = app_with_seed().await;
    let (status, body) = mcp_post(
        &app,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "context", "arguments": { "path": "payments/" } }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tools/call context: {body}");
    let structured = &body["result"]["structuredContent"];
    let api = api_get(&app, "/api/claims?path=payments/").await;
    assert_eq!(structured, &api, "context parity over the mount: {body}");
}

#[tokio::test]
async fn an_unknown_claim_over_the_mount_is_a_tool_error_not_a_fabricated_standing() {
    // Invariant #6 over the wire: a `tools/call` for an unknown claim comes back as a
    // tool-level error the agent reads, never a manufactured standing.
    let app = app_with_seed().await;
    let (status, body) = mcp_post(
        &app,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "dossier", "arguments": { "id": "payments/ghost" } }
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a tool error is still HTTP 200: {body}"
    );
    assert_eq!(
        body["result"]["isError"], true,
        "an unknown claim is a tool error: {body}"
    );
    assert!(
        body["result"]["structuredContent"].is_null(),
        "no fabricated standing in structured content: {body}"
    );
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("payments/ghost"),
        "the reason names it: {body}"
    );
}
