//! The axum application: state, the router, and the tower middleware stack.
//!
//! This is the shell every later component mounts into (HUB-IMPLEMENTATION.md §4.3).
//! [`build_app`] assembles the router from tower layers, so auth (hub-13), a request
//! timeout, and richer tracing each become one composable layer a later item adds
//! rather than framework-specific plumbing (HUB-IMPLEMENTATION.md §1.3). The router
//! is deliberately minimal: `/status` is the only real route in this item, and the
//! mount points for the later surfaces are named in one place ([`build_app`]) so the
//! shape a later agent slots into is obvious.
//!
//! The whole app is built without binding a port, so a test drives it in-process via
//! [`tower::ServiceExt::oneshot`] with no network (HUB-IMPLEMENTATION.md §1.14).
//! Binding and serving is [`crate::serve`]'s job, kept apart from assembly so the
//! part under test never touches the network.

use axum::routing::get;
use axum::Router;
use claim_hub_store::SqliteStore;
use tower_http::trace::TraceLayer;

use crate::status;

/// State shared across the hub's handlers: the one store, cloned per handler.
///
/// The [`SqliteStore`] is a reference-counted pool, so cloning is cheap and every
/// handler shares one database (HUB-IMPLEMENTATION.md §1.4). It is held in an
/// [`AppState`] rather than passed as a bare store so a later item adds shared state
/// (the deriver's memo, the JWKS cache, config) as fields without changing every
/// handler's extractor.
#[derive(Clone)]
pub struct AppState {
    /// The ledger-and-registry store `/status` reads and later surfaces derive over.
    pub store: SqliteStore,
}

/// Assemble the hub's axum [`Router`] over `state`, with the shared middleware stack.
///
/// The router is the mount board for every hub surface. This shell mounts one real
/// route, `/status`; the comments mark where the later items attach:
///
/// - **hub-04 ingest** — the one telemetry write path, `POST /ingest`, behind the
///   OIDC verification layer.
/// - **hub-08 JSON API** — the read surface, nested under `/api`.
/// - **hub-09 MCP** — rmcp's streamable-HTTP tower service, mounted with
///   `Router::nest_service`.
/// - **hub-10 UI + twins** — the server-rendered pages, `/llms.txt`, and the `.md`
///   twins.
///
/// The [`TraceLayer`] wraps every route so a request carries a tracing span through
/// the stack (HUB-IMPLEMENTATION.md §1.12); it is applied last so it observes the
/// whole router, and it is where a later item stacks auth and timeout layers.
pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/status", get(status::status))
        // hub-04: `.route("/ingest", post(ingest::ingest))` behind the OIDC layer.
        // hub-08: `.nest("/api", api::router())`.
        // hub-09: `.nest_service("/mcp", mcp_service)`.
        // hub-10: the UI pages, `/llms.txt`, and the markdown twins.
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use claim_hub_store::{Ledger, Registry};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// Build an app over a fresh in-memory store — no file, no port, no network.
    async fn app_over_empty_store() -> (Router, SqliteStore) {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let app = build_app(AppState {
            store: store.clone(),
        });
        (app, store)
    }

    /// Send one request through the assembled app in-process and return its status and
    /// JSON body.
    async fn get_json(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, body)
    }

    #[tokio::test]
    async fn status_reports_truthful_zeros_on_an_empty_store() {
        // The birth state: an empty ledger and registry report head 0 / version 0, not
        // an error and not a fabricated "healthy". This is invariant #6 at the shell.
        let (app, _store) = app_over_empty_store().await;
        let (status, body) = get_json(app, "/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ledger_head"], 0);
        assert_eq!(body["registry_version"], 0);
        assert_eq!(body["rejection_count"], 0);
        // Never synced: the field is omitted, not a fabricated timestamp.
        assert!(
            body.get("last_sync").is_none(),
            "no fabricated sync: {body}"
        );
    }

    #[tokio::test]
    async fn status_head_advances_after_an_append() {
        // A non-empty store: appending one event through the Ledger trait moves the
        // head, and /status reflects it — the field is truly sourced from the store,
        // not a constant.
        let (app, store) = app_over_empty_store().await;
        let event = sample_event();
        let appended = store.append(&event).await.unwrap();
        assert_eq!(appended.position().0, 1, "first event lands at position 1");

        let (status, body) = get_json(app, "/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ledger_head"], 1, "head reflects the appended event");
    }

    #[tokio::test]
    async fn status_registry_version_advances_after_a_sync() {
        // The registry_version field is likewise sourced from the store: a store sync
        // advances it, and /status shows the new value.
        let (app, store) = app_over_empty_store().await;
        store
            .replace_store("github.com/acme/payments", &[])
            .await
            .unwrap();
        let (_status, body) = get_json(app, "/status").await;
        assert_eq!(body["registry_version"], 1);
    }

    /// A minimal valid verdict event for the head-advance test. Mirrors the envelope
    /// crate's sample: a non-empty producer `run` is required for the append to be
    /// accepted (it is the dedup key's run component).
    fn sample_event() -> claim_hub_core::Event {
        let mut producer = serde_json::Map::new();
        producer.insert("run".into(), serde_json::json!("1234567890"));
        claim_hub_core::Event {
            kind: claim_hub_core::EventKind::Verdict,
            claim: "payments/libfoo-pin".into(),
            check: claim_hub_core::CheckRef {
                index: 0,
                digest: "a".repeat(64),
            },
            verdict: claim_core::Verdict::Held,
            evidence: None,
            commit: "8f2c0a1".into(),
            store: "github.com/acme/payments".into(),
            producer: claim_hub_core::Producer(producer),
            reported_at: "2026-07-18T06:00:00Z".parse().unwrap(),
        }
    }
}
