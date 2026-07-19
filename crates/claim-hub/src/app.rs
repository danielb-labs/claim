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

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use claim_core::Timestamp;
use claim_hub_core::{DeriverConfig, Memo};
use claim_hub_store::SqliteStore;
use tower_http::trace::TraceLayer;

use crate::oidc::SharedVerifier;
use crate::{api, ingest, mcp, status, ui};

/// A source of the current instant, injectable so ingest is deterministic under test.
///
/// The ingest gate stamps each event's `reported_at` with the moment the observation
/// reached the ledger — a wall-clock instant in production, but a fixed value a test
/// supplies so the appended event is exact (CLAUDE.md's determinism rule: time is a
/// parameter, never a hidden `now()` inside logic under test). Boxed behind an `Arc`
/// so [`AppState`] stays `Clone` and cheap to share.
pub type Clock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

/// A source of the read clock, injectable so the read API's derivation is deterministic
/// under test.
///
/// The read surfaces derive a claim's standing *as of a clock instant* — freshness is
/// arithmetic against it (a claim aging into stale is measured from `now`), and the
/// instant is part of every answer's as-of (HUB.md §4). In production this is wall-clock
/// `now`; a test injects a fixed or advancing value so the aging path is exercised without
/// sleeping (CLAUDE.md's determinism rule). Boxed behind an `Arc` so [`AppState`] stays
/// `Clone`. Kept distinct from the ingest [`Clock`] because the two are conceptually
/// different — one stamps *when a verdict was recorded*, the other decides *how stale it is
/// now* — even though production points both at wall-clock time.
pub type ReadClock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

/// State shared across the hub's handlers: the store, the OIDC verifier, the clocks, and
/// the deriver's cache and config.
///
/// The [`SqliteStore`] is a reference-counted pool, so cloning is cheap and every
/// handler shares one database (HUB-IMPLEMENTATION.md §1.4). The `verifier` is the
/// ingest gate's OIDC trust anchor, `None` when the hub has no OIDC config (the ingest
/// route is then not mounted). The `clock` produces the ingest instant; the `read_clock`
/// the read-time derivation instant. The `memo` caches the last derivation (a cache,
/// never a store — invariant #3), and `deriver_config` is the freshness config the read
/// API derives under.
#[derive(Clone)]
pub struct AppState {
    /// The ledger-and-registry store `/status` reads, ingest appends to, and the read
    /// API derives over.
    pub store: SqliteStore,
    /// The OIDC verifier the ingest gate authenticates producers with. `None` when the
    /// hub has no `[oidc]` config — the ingest route is then not mounted, so the
    /// handler's own `None` guard is a defensive backstop, not a live path.
    pub verifier: Option<SharedVerifier>,
    /// The clock the ingest gate stamps each event's `reported_at` from. Defaults to
    /// wall-clock `now`; a test injects a fixed instant.
    pub clock: Clock,
    /// The clock the read API derives standing as-of. Defaults to wall-clock `now`; a
    /// test injects a fixed or advancing instant to exercise clock-driven staleness.
    pub read_clock: ReadClock,
    /// The deriver's memo, shared across read requests. A cache the read API derives
    /// through; discardable by construction (invariant #3). Behind an `Arc` so cloning
    /// the state shares one cache.
    pub memo: Arc<Memo>,
    /// The freshness config the read API derives under, mapped from the hub's `[deriver]`
    /// section. Its hash keys the memo, so a config change invalidates cached answers.
    pub deriver_config: DeriverConfig,
}

impl AppState {
    /// State for a hub with an ingest gate: the store, its verifier, wall-clock time, an
    /// empty memo, and a default (no-window) deriver config.
    ///
    /// The common production shape. [`with_clock`](AppState::with_clock) swaps the ingest
    /// clock, [`with_read_clock`](AppState::with_read_clock) the read clock, and
    /// [`with_deriver_config`](AppState::with_deriver_config) the freshness config, for
    /// deterministic tests and for boot to install the config it parsed.
    #[must_use]
    pub fn new(store: SqliteStore, verifier: Option<SharedVerifier>) -> Self {
        Self {
            store,
            verifier,
            clock: Arc::new(Timestamp::now),
            read_clock: Arc::new(Timestamp::now),
            memo: Arc::new(Memo::new()),
            deriver_config: DeriverConfig::default(),
        }
    }

    /// This state with its ingest clock replaced, for deterministic ingest tests.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// This state with its read clock replaced, for deterministic read/aging tests.
    #[must_use]
    pub fn with_read_clock(mut self, read_clock: ReadClock) -> Self {
        self.read_clock = read_clock;
        self
    }

    /// This state with its deriver config replaced — boot installs the config it parsed.
    #[must_use]
    pub fn with_deriver_config(mut self, config: DeriverConfig) -> Self {
        self.deriver_config = config;
        self
    }
}

/// Assemble the hub's axum [`Router`] over `state`, with the shared middleware stack.
///
/// The router is the mount board for every hub surface. It mounts `/status` always, the
/// read API ([`api::router`] — the claims queries, the derived sets, the dossier, and the
/// cursor feed, all under `/api`), the UI ([`ui::router`] — the server-rendered queue,
/// dossier, and status pages, each with a `.md` markdown twin, plus `/llms.txt`), the hub
/// MCP ([`mcp`] — five read-only tools over the same read model, nested at
/// [`mcp::MCP_MOUNT_PATH`] so the JSON API and the MCP are one process on one port and one
/// middleware stack, HUB.md §5's "one substrate"), and the ingest route `POST /ingest`
/// **only when the state carries a verifier** — a hub with no OIDC config has no way to
/// authenticate a producer, so it exposes no write path rather than a route that rejects
/// everything. The remaining mount point is named for the later item:
///
/// - **hub-13 read auth** — the OAuth 2.1 bearer layer over `/api`, the UI, and MCP; the
///   read routes are unauthenticated until it lands.
///
/// The [`TraceLayer`] wraps every route so a request carries a tracing span through
/// the stack (HUB-IMPLEMENTATION.md §1.12); it is applied last so it observes the
/// whole router, and it is where a later item stacks auth and timeout layers.
pub fn build_app(state: AppState) -> Router {
    // The MCP service holds its own copy of the state (it builds a fresh handler per
    // request), so it is constructed before `with_state` consumes `state` for the
    // state-carrying routes.
    let mcp = mcp::mcp_service(state.clone());
    let mut router = Router::new()
        .route("/status", get(status::status))
        // The read API (hub-08): claims queries, the drifted/due/suspect sets, the
        // dossier, and the cursor feed, all over the deriver under `/api`.
        .merge(api::router())
        // The UI (hub-10): server-rendered pages over the same read model, each with a
        // markdown twin at its path + `.md`, plus `/llms.txt` indexing every surface.
        .merge(ui::router());
    if state.verifier.is_some() {
        // The single telemetry write path (HUB.md §3). Mounted only with a verifier so a
        // hub that cannot authenticate producers exposes no ingest at all.
        router = router.route("/ingest", post(ingest::ingest));
    }
    router
        // hub-09: the hub MCP — rmcp's streamable-HTTP transport as a tower service, so the
        // five read tools share the hub's one port and middleware stack.
        .nest_service(mcp::MCP_MOUNT_PATH, mcp)
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
    ///
    /// No verifier: `/status` needs none, and a state with no verifier does not mount
    /// the ingest route (asserted in `ingest_route_is_absent_without_a_verifier`).
    async fn app_over_empty_store() -> (Router, SqliteStore) {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let app = build_app(AppState::new(store.clone(), None));
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

    #[tokio::test]
    async fn status_rejection_count_is_sourced_from_the_store() {
        // The rejection count is no longer a hardcoded 0: recording a rejection through
        // the store's `Rejections` trait moves it, and /status reflects the new value —
        // the hub-03 placeholder is gone.
        use claim_hub_store::Rejections;
        let (app, store) = app_over_empty_store().await;
        store.record_rejection().await.unwrap();
        store.record_rejection().await.unwrap();
        let (status, body) = get_json(app, "/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["rejection_count"], 2,
            "rejection_count reflects the store, not a constant: {body}"
        );
    }

    #[tokio::test]
    async fn ingest_route_is_absent_without_a_verifier() {
        // A hub with no OIDC verifier exposes no write path: `POST /ingest` is not
        // mounted, so it 404s rather than a route that rejects everything. `/status`
        // still serves.
        let (app, _store) = app_over_empty_store().await;
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/ingest")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "ingest is unmounted without a verifier"
        );
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
