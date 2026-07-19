//! Tests for the hub MCP: parity with the API, pinned schemas, and a stable `tools/list`.
//!
//! The load-bearing property is **parity by construction** (hub-09): each tool must return
//! byte-identical JSON to its API twin over the same store and inputs. Every parity test
//! seeds one temp store, reads the API endpoint through the assembled axum app in-process
//! (no network), calls the matching MCP tool directly on a [`HubMcp`] over the same store,
//! and asserts the tool's `structured_content` equals the API body. The read clock is fixed
//! so both surfaces derive the identical instant.
//!
//! Three further properties get dedicated coverage: the tool input schemas are
//! `insta`-snapshotted (a schema change is deliberate and reviewable); `tools/list` is stable
//! across restarts (two independently built handlers advertise the identical tool list and
//! schemas — deterministic registration); and the honesty invariants (an unknown/retired
//! claim is a tool error mirroring the API's 404, never a fabricated standing; a tool read
//! appends no event).

use super::*;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use claim_core::{parse_claim_file, Timestamp, Verdict};
use claim_hub_core::{check_digest, CheckRef, Event, Producer};
use claim_hub_store::{Ledger, RegisteredClaim, Registry, SqliteStore};
use http_body_util::BodyExt;
use tower::ServiceExt;

const BILLING: &str = "github.com/acme/billing";
const PAYMENTS: &str = "github.com/acme/payments";
/// The fixed read clock, well after the verdict instants and within a 30-day window, so a
/// held claim reads `verified` and every derived field is a constant.
const READ_NOW: &str = "2026-07-20T00:00:00Z";

/// An [`AppState`] over `store` with the fixed read clock, so the API and the MCP tools
/// derive the identical instant. Shared by both surfaces in every parity test.
fn state(store: SqliteStore) -> AppState {
    let read_clock: crate::app::ReadClock =
        Arc::new(|| READ_NOW.parse::<Timestamp>().expect("valid instant"));
    AppState::new(store, None).with_read_clock(read_clock)
}

/// The assembled axum app over `state`, for reading the API side of a parity check.
fn app(state: AppState) -> axum::Router {
    crate::build_app(state)
}

/// Register a claim from frontmatter under `store_id`, returning the parsed claim.
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

/// Register several claims into one store in one snapshot (one registry-version bump).
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
    let mut event = Event::verdict(
        claim.id.as_str().to_owned(),
        CheckRef {
            index: check_index,
            digest: check_digest(&claim.checks[check_index]),
        },
        verdict,
        "abc1234",
        store_id,
        Producer(producer),
        at.parse().unwrap(),
    );
    event.evidence = (verdict == Verdict::Held).then(|| "libfoo==4.2".to_owned());
    event
}

/// A one-`cmd`-check claim's frontmatter with the given id and optional extra lines.
fn frontmatter(id: &str, extra: &str) -> String {
    format!("id: {id}\n{extra}checks:\n  - kind: cmd\n    run: \"true\"")
}

/// GET `uri` through the assembled app and return the parsed JSON body (the API side of a
/// parity check).
async fn api_get(app: &axum::Router, uri: &str) -> serde_json::Value {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// The `structured_content` of a successful tool result — the value that must match the
/// API body. Panics if the result is an error (a parity test expects success).
fn structured(result: CallToolResult) -> serde_json::Value {
    assert_ne!(
        result.is_error,
        Some(true),
        "expected a successful tool result, got an error: {:?}",
        result.content
    );
    result
        .structured_content
        .expect("a successful read tool returns structured content")
}

/// The concatenated text content of a tool result — the reason on an error result.
fn text_of(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
        .collect()
}

/// Seed a fixed corpus across two stores with a mix of standings — the parity/set tests
/// read it through both surfaces:
/// - billing/a: held (verified)
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

// ---- Parity: each tool returns the exact JSON its API twin serves ----

#[tokio::test]
async fn context_matches_api_claims_filtered_by_path() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let st = state(store);
    let mcp = HubMcp::new(st.clone());

    let api = api_get(&app(st), "/api/claims?path=payments/").await;
    let tool = structured(
        mcp.context(Parameters(ContextArgs {
            path: Some("payments/".into()),
            store: None,
        }))
        .await
        .unwrap(),
    );
    assert_eq!(tool, api, "context parity with GET /api/claims?path=");
    // Sanity: the shared body really is the payments namespace, not the whole set.
    assert_eq!(tool["claims"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn context_with_no_arguments_matches_the_whole_set() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let st = state(store);
    let mcp = HubMcp::new(st.clone());

    let api = api_get(&app(st), "/api/claims").await;
    let tool = structured(
        mcp.context(Parameters(ContextArgs::default()))
            .await
            .unwrap(),
    );
    assert_eq!(tool, api, "context with no args is the whole live set");
    assert_eq!(tool["claims"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn search_matches_api_claims_with_the_full_filter_set() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let st = state(store);
    let mcp = HubMcp::new(st.clone());

    let api = api_get(
        &app(st),
        "/api/claims?store=github.com/acme/payments&standing=verified",
    )
    .await;
    let tool = structured(
        mcp.search(Parameters(SearchArgs {
            path: None,
            store: Some(PAYMENTS.into()),
            standing: Some("verified".into()),
            supports: None,
        }))
        .await
        .unwrap(),
    );
    assert_eq!(tool, api, "search parity with the full filter set");
    assert_eq!(tool["claims"][0]["id"], "payments/pin");
}

#[tokio::test]
async fn search_by_supports_matches_the_api() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let st = state(store);
    let mcp = HubMcp::new(st.clone());

    let api = api_get(&app(st), "/api/claims?supports=decision:pin").await;
    let tool = structured(
        mcp.search(Parameters(SearchArgs {
            supports: Some("decision:pin".into()),
            ..Default::default()
        }))
        .await
        .unwrap(),
    );
    assert_eq!(tool, api, "search by supports parity");
    assert_eq!(tool["claims"][0]["id"], "payments/pin");
}

#[tokio::test]
async fn drifts_matches_api_drifted() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let st = state(store);
    let mcp = HubMcp::new(st.clone());

    let api = api_get(&app(st), "/api/drifted").await;
    let tool = structured(mcp.drifts(Parameters(NoArgs {})).await.unwrap());
    assert_eq!(tool, api, "drifts parity with GET /api/drifted");
    assert_eq!(tool["claims"][0]["id"], "payments/drift");
}

#[tokio::test]
async fn due_matches_api_due() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let st = state(store);
    let mcp = HubMcp::new(st.clone());

    let api = api_get(&app(st), "/api/due").await;
    let tool = structured(mcp.due(Parameters(NoArgs {})).await.unwrap());
    assert_eq!(tool, api, "due parity with GET /api/due");
    // The queue is the deriver's union (stale + drifted here), not a standing filter.
    let ids: Vec<&str> = tool["claims"]
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
}

#[tokio::test]
async fn dossier_matches_api_dossier() {
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
    let st = state(store);
    let mcp = HubMcp::new(st.clone());

    let api = api_get(&app(st), "/api/claims/payments/libfoo-pin/dossier").await;
    let tool = structured(
        mcp.dossier(Parameters(DossierArgs {
            id: "payments/libfoo-pin".into(),
        }))
        .await
        .unwrap(),
    );
    assert_eq!(
        tool, api,
        "dossier parity with GET /api/claims/{{id}}/dossier"
    );
    assert_eq!(tool["statement"], "The statement.");
    assert_eq!(tool["history"][0]["verdict"], "held");
}

// ---- Honesty: an unknown/retired claim is a tool error, never a fabricated standing ----

#[tokio::test]
async fn an_unknown_claim_dossier_is_a_tool_error_not_a_fabricated_standing() {
    // Invariant #6: the MCP surface mirrors the API's 404 as a caller-visible tool error the
    // agent reads, never a manufactured `verified`. The reason names the missing claim.
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let mcp = HubMcp::new(state(store));

    let result = mcp
        .dossier(Parameters(DossierArgs {
            id: "payments/ghost".into(),
        }))
        .await
        .unwrap();
    assert_eq!(
        result.is_error,
        Some(true),
        "an unknown claim is a tool error"
    );
    assert!(
        result.structured_content.is_none(),
        "no fabricated standing in structured content"
    );
    let text = text_of(&result);
    assert!(
        text.contains("payments/ghost"),
        "the reason names it: {text}"
    );
}

#[tokio::test]
async fn a_malformed_id_dossier_mirrors_the_api_as_no_claim_not_a_bad_id() {
    // Parity on a bad id: a malformed id through the `dossier` tool answers the same "no
    // claim" the API gives, not a divergent "not a valid claim id" 400. The API's dossier is
    // only reached after the id already matched a claim, so a malformed id there falls to the
    // same not-found; the tool gates on the model lookup first to agree. Never a fabricated
    // standing either way (invariant #6).
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let st = state(store);
    let mcp = HubMcp::new(st.clone());

    // An uppercase segment is not a valid `ClaimId` (ids are lowercase) yet is a valid URI
    // path, so it reaches the handler on both surfaces rather than being rejected as a bad URI.
    let bad_id = "payments/NotAValidId";
    let api_status = app(st)
        .oneshot(
            Request::builder()
                .uri(format!("/api/claims/{bad_id}/dossier"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    assert_eq!(
        api_status,
        StatusCode::NOT_FOUND,
        "the API treats a malformed id as no claim, not a 400"
    );

    let result = mcp
        .dossier(Parameters(DossierArgs { id: bad_id.into() }))
        .await
        .unwrap();
    assert_eq!(
        result.is_error,
        Some(true),
        "a malformed id is a tool error, not a fabricated standing"
    );
    assert!(
        result.structured_content.is_none(),
        "no fabricated standing in structured content"
    );
    let text = text_of(&result);
    assert!(
        text.contains("no claim"),
        "the reason mirrors the API's no-claim message, not a bad-id 400: {text}"
    );
}

#[tokio::test]
async fn a_retired_claim_dossier_is_a_tool_error_mirroring_the_api() {
    // A claim the ledger knows but the registry has dropped is retired: its dossier needs a
    // live statement, so both surfaces answer an honest error, never a green.
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
    store.replace_store(PAYMENTS, &[]).await.unwrap(); // retire it
    let st = state(store);
    let mcp = HubMcp::new(st.clone());

    // The API answers 404 with a "retired" reason; the tool mirrors it as an error.
    let api_status = app(st)
        .oneshot(
            Request::builder()
                .uri("/api/claims/payments/libfoo-pin/dossier")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    assert_eq!(api_status, StatusCode::NOT_FOUND);

    let result = mcp
        .dossier(Parameters(DossierArgs {
            id: "payments/libfoo-pin".into(),
        }))
        .await
        .unwrap();
    assert_eq!(
        result.is_error,
        Some(true),
        "retired dossier is a tool error"
    );
    let text = text_of(&result);
    assert!(text.contains("retired"), "the reason says retired: {text}");
}

#[tokio::test]
async fn a_retired_claim_still_reads_retired_via_search_never_a_green() {
    // The retired claim is absent from the dossier but present in the derived set: `search`
    // by `standing=retired` returns it, mirroring the API, so it is surfaced, not vanished.
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
    store.replace_store(PAYMENTS, &[]).await.unwrap();
    let st = state(store);
    let mcp = HubMcp::new(st.clone());

    let api = api_get(&app(st), "/api/claims?standing=retired").await;
    let tool = structured(
        mcp.search(Parameters(SearchArgs {
            standing: Some("retired".into()),
            ..Default::default()
        }))
        .await
        .unwrap(),
    );
    assert_eq!(tool, api);
    assert_eq!(tool["claims"][0]["standing"], "retired");
}

#[tokio::test]
async fn an_unknown_standing_filter_is_a_tool_error_naming_the_accepted_set() {
    // Mirrors the API's 400: a mistyped standing is a loud error naming the accepted set,
    // never a silently ignored filter returning the wrong set (invariant #6).
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let mcp = HubMcp::new(state(store));

    let result = mcp
        .search(Parameters(SearchArgs {
            standing: Some("green".into()),
            ..Default::default()
        }))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    let text = text_of(&result);
    assert!(text.contains("green"), "names the bad value: {text}");
    assert!(text.contains("verified"), "names the accepted set: {text}");
}

#[tokio::test]
async fn a_tool_read_appends_no_event() {
    // Invariant #3: a tool read derives, it does not store. Calling every tool leaves the
    // ledger head unmoved.
    let store = SqliteStore::open_in_memory().await.unwrap();
    seed_corpus(&store).await;
    let head_before = store.head().await.unwrap();
    let mcp = HubMcp::new(state(store.clone()));

    let _ = mcp
        .context(Parameters(ContextArgs::default()))
        .await
        .unwrap();
    let _ = mcp.search(Parameters(SearchArgs::default())).await.unwrap();
    let _ = mcp.drifts(Parameters(NoArgs {})).await.unwrap();
    let _ = mcp.due(Parameters(NoArgs {})).await.unwrap();
    let _ = mcp
        .dossier(Parameters(DossierArgs {
            id: "payments/pin".into(),
        }))
        .await
        .unwrap();

    assert_eq!(
        store.head().await.unwrap(),
        head_before,
        "tool reads stored nothing"
    );
}

// ---- tools/list is stable across restarts (deterministic registration) ----

#[tokio::test]
async fn tools_list_is_the_five_read_tools() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let mcp = HubMcp::new(state(store));
    let names: Vec<String> = mcp
        .tool_router
        .list_all()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    // list_all sorts by name, so this is the exact advertised order.
    assert_eq!(names, vec!["context", "dossier", "drifts", "due", "search"]);
}

#[tokio::test]
async fn tools_list_is_stable_across_restarts() {
    // Build the server twice (two independent process "restarts"): the tool list and every
    // tool's schema must be byte-identical. Registration is deterministic — rmcp keys tools
    // by name and lists them sorted — so a client sees a stable capability set across
    // restarts and load-balanced replicas.
    let store_a = SqliteStore::open_in_memory().await.unwrap();
    let store_b = SqliteStore::open_in_memory().await.unwrap();
    let one = HubMcp::new(state(store_a)).tool_router.list_all();
    let two = HubMcp::new(state(store_b)).tool_router.list_all();
    assert_eq!(
        one, two,
        "tools/list (names, descriptions, schemas) is stable"
    );
}

// ---- Schema snapshots (insta) ----

/// The tool list rendered as a stable, snapshot-friendly value: each tool's name,
/// description, and input schema (the client-visible contract).
fn tools_snapshot(mcp: &HubMcp) -> serde_json::Value {
    let tools = mcp.tool_router.list_all();
    serde_json::json!(tools
        .into_iter()
        .map(|t| serde_json::json!({
            "name": t.name,
            "description": t.description,
            "input_schema": t.input_schema,
        }))
        .collect::<Vec<_>>())
}

#[tokio::test]
async fn snapshot_tool_schemas() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let mcp = HubMcp::new(state(store));
    insta::assert_json_snapshot!(tools_snapshot(&mcp));
}
