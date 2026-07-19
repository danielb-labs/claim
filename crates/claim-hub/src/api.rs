//! The read API: the hub's derived read model, served over the deriver.
//!
//! This is the hub-07 walking skeleton's one read endpoint, `GET /api/claims/{id}`,
//! wired to the real deriver over the real store. It is the whole spine in one route:
//! it loads the registry snapshot and scans the ledger from `claim-hub-store`, reads the
//! injectable clock and the freshness config, calls [`claim_hub_core::derive`], and
//! returns the claim's [`Standing`](claim_hub_core::Standing) with its **as-of** — the
//! ledger head, registry version, and clock instant the answer derives from (HUB.md §4).
//!
//! It is a **read** (invariant #3): it computes at read time and stores nothing. There is
//! no status column to update; a claim's standing is derived from the ledger and the
//! clock every time, so it can never disagree with the evidence. The derivation runs
//! through the shared [`Memo`](claim_hub_core::Memo) — a cache keyed on (ledger head,
//! registry version, config hash) plus a clock-horizon check — so successive reads at the
//! same inputs do not repeat the work, and a cache miss fails safe to a fresh derivation.
//!
//! hub-08 grows the full read surface (claims by path/repo/standing, the drifted/due
//! sets, the dossier, the cursor feed) into this same `/api` nest; hub-07 seeds it with
//! the one endpoint that proves the loop.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use claim_hub_core::{AsOf, ClaimStanding};
use claim_hub_store::{ledger_events, registry_snapshot};
use serde::Serialize;

use crate::app::AppState;
use crate::http::problem;

/// The read API's router: `GET /api/claims/{id}` in the hub-07 skeleton.
///
/// The id is a **catch-all** segment (`{*id}`), because a claim id is namespaced with
/// `/` (e.g. `payments/libfoo-pin`) — a single-segment capture would 404 on any
/// namespaced id, since axum would read the `/` as a path boundary. The catch-all
/// captures the whole tail as the id. Nested under `/api` by [`crate::build_app`];
/// returned as a `Router<AppState>` so it composes with the app's state, and hub-08 adds
/// the remaining read routes here.
pub fn router() -> Router<AppState> {
    Router::new().route("/api/claims/{*id}", get(claim_standing))
}

/// `GET /api/claims/{id}`: the derived standing of one claim, with its as-of.
///
/// The full spine of a read: build the registry snapshot and the ledger events from the
/// live store, derive the whole read model at the read clock under the hub's freshness
/// config (through the memo), then return the requested claim's standing. A claim the
/// registry does not know — never synced, or retired and with no ledger history — is a
/// `404`. A store read fault is a `500`: the hub cannot state the standing, so it says so
/// loudly rather than fabricating one (invariant #6).
///
/// The `id` locates the claim across every connected store; if two stores happened to
/// share a claim id (each store's ids are unique only within it), the lexicographically
/// first `(store, id)` match is returned — a deliberate, documented tie-break for the M0
/// single-store shape, refined when the full query API (hub-08) adds a `store` selector.
async fn claim_standing(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let registry = match registry_snapshot(&state.store).await {
        Ok(registry) => registry,
        Err(error) => {
            tracing::error!(%error, "failed to build the registry snapshot for a read");
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "cannot read the registry right now",
            );
        }
    };
    let events = match ledger_events(&state.store).await {
        Ok(events) => events,
        Err(error) => {
            tracing::error!(%error, "failed to scan the ledger for a read");
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "cannot read the ledger right now",
            );
        }
    };

    let now = (state.read_clock)();
    // Derive through the memo: a cache, never a store (invariant #3). The result is
    // identical to a direct `derive`; the memo only changes how often the work runs.
    let model = state
        .memo
        .read(&registry, &events, now, &state.deriver_config);

    // The read model is keyed by (store, id); the URL carries only the id, so find the
    // first entry whose id matches. `BTreeMap` iteration is in (store, id) order, so the
    // tie-break is deterministic.
    match model.claims.iter().find(|((_, cid), _)| cid == &id) {
        Some((_, standing)) => (
            StatusCode::OK,
            Json(ClaimStandingResponse::new(standing, model.as_of)),
        )
            .into_response(),
        None => problem(
            StatusCode::NOT_FOUND,
            &format!(
                "no claim `{id}` in the registry — it may not be synced yet, or it was retired \
                 with no verdict history"
            ),
        ),
    }
}

/// The body of a `GET /api/claims/{id}` response: the derived standing and its as-of.
///
/// Every read answer carries its as-of (HUB.md §4) so the hub can never show a green
/// older than its evidence and an agent can cache, diff, and resume: the same (ledger
/// head, registry version, clock) always yields the same standing. The standing is
/// flattened in, so the response is the standing's own fields plus an `as_of` object —
/// the shape hub-08's fuller responses extend.
#[derive(Debug, Serialize)]
struct ClaimStandingResponse<'a> {
    /// The claim's full derived standing: its conservative verdict over all checks, its
    /// freshness, due-ness, and skips.
    #[serde(flatten)]
    standing: &'a ClaimStanding,
    /// The exact inputs this standing was derived from.
    as_of: AsOf,
}

impl<'a> ClaimStandingResponse<'a> {
    fn new(standing: &'a ClaimStanding, as_of: AsOf) -> Self {
        Self { standing, as_of }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::Request;
    use claim_core::{parse_claim_file, Timestamp, Verdict};
    use claim_hub_core::{check_digest, CheckRef, Event, EventKind, Producer};
    use claim_hub_store::{Ledger, RegisteredClaim, Registry, SqliteStore};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const STORE: &str = "github.com/acme/payments";
    const READ_NOW: &str = "2026-07-20T00:00:00Z";

    /// An app over `store` whose read clock is a fixed `READ_NOW`, so a read's freshness is
    /// deterministic. No verifier: these tests exercise only the read path.
    fn app(store: SqliteStore) -> Router {
        let read_clock: crate::app::ReadClock =
            Arc::new(|| READ_NOW.parse::<Timestamp>().expect("valid instant"));
        crate::build_app(AppState::new(store, None).with_read_clock(read_clock))
    }

    /// Seed one claim into the registry and return the parsed claim (for its digest).
    async fn seed(store: &SqliteStore, frontmatter: &str) -> claim_core::Claim {
        let text = format!("---\n{frontmatter}\n---\nStatement.\n");
        let claim = parse_claim_file(".claims/t.md", &text).expect("valid claim");
        store
            .replace_store(STORE, &[RegisteredClaim::from_claim(&claim, "seedcommit")])
            .await
            .unwrap();
        claim
    }

    /// A `held` verdict event for the claim's first check, at `at`.
    fn held_event(claim: &claim_core::Claim, at: &str) -> Event {
        let mut producer = serde_json::Map::new();
        producer.insert("run".into(), serde_json::json!("run-1"));
        Event {
            kind: EventKind::Verdict,
            claim: claim.id.as_str().to_owned(),
            check: CheckRef {
                index: 0,
                digest: check_digest(&claim.checks[0]),
            },
            verdict: Verdict::Held,
            evidence: None,
            commit: "abc".into(),
            store: STORE.into(),
            producer: Producer(producer),
            reported_at: at.parse().unwrap(),
        }
    }

    async fn get(app: &Router, id: &str) -> (StatusCode, serde_json::Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/claims/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
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

    #[tokio::test]
    async fn an_unknown_claim_is_a_404_naming_it() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let (status, json) = get(&app(store), "payments/not-there").await;
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
    async fn a_namespaced_id_with_a_slash_routes_to_the_handler() {
        // The catch-all route must capture a `/`-namespaced id whole; a single-segment
        // route would 404 in the router before the handler ever ran.
        let store = SqliteStore::open_in_memory().await.unwrap();
        let claim = seed(
            &store,
            "id: payments/libfoo-pin\nchecks:\n  - kind: cmd\n    run: \"true\"",
        )
        .await;
        store
            .append(&held_event(&claim, "2026-07-19T00:00:00Z"))
            .await
            .unwrap();

        let (status, json) = get(&app(store), "payments/libfoo-pin").await;
        assert_eq!(status, StatusCode::OK, "the namespaced id routes: {json}");
        assert_eq!(json["id"], "payments/libfoo-pin");
        // No window: a held latest reads verified, and the answer carries its as-of.
        assert_eq!(json["standing"], "verified");
        assert_eq!(json["as_of"]["ledger_head"], 1);
        assert_eq!(json["as_of"]["registry_version"], 1);
        assert_eq!(json["as_of"]["clock"], READ_NOW);
    }

    #[tokio::test]
    async fn a_read_appends_no_event() {
        // Invariant #3: a read derives, it does not store. Reading twice leaves the ledger
        // head unmoved.
        let store = SqliteStore::open_in_memory().await.unwrap();
        let claim = seed(
            &store,
            "id: payments/libfoo-pin\nchecks:\n  - kind: cmd\n    run: \"true\"",
        )
        .await;
        store
            .append(&held_event(&claim, "2026-07-19T00:00:00Z"))
            .await
            .unwrap();
        let head_before = store.head().await.unwrap();

        let app = app(store.clone());
        let _ = get(&app, "payments/libfoo-pin").await;
        let _ = get(&app, "payments/libfoo-pin").await;

        assert_eq!(
            store.head().await.unwrap(),
            head_before,
            "a read stored nothing"
        );
    }

    #[tokio::test]
    async fn a_shared_id_across_two_stores_returns_the_lexicographically_first_store() {
        // The documented tie-break for the M0 single-id-per-URL shape: two stores each hold
        // a claim with the same id (ids are unique only *within* a store). The read model is
        // keyed by (store, id) in a BTreeMap, so `find` returns the ascending-first store's
        // standing. Pinned so the behavior is a deliberate contract, not an accident — the
        // full query API (hub-08) will add a `store` selector to address a claim exactly.
        let store = SqliteStore::open_in_memory().await.unwrap();
        let text = "---\nid: payments/libfoo-pin\nchecks:\n  - kind: cmd\n    run: \"true\"\n---\nStatement.\n";
        let claim = parse_claim_file(".claims/t.md", text).unwrap();
        // Two stores, ascending: "...billing" sorts before "...payments".
        let first = "github.com/acme/billing";
        let second = "github.com/acme/payments";
        for store_id in [first, second] {
            store
                .replace_store(
                    store_id,
                    &[RegisteredClaim::from_claim(&claim, "seedcommit")],
                )
                .await
                .unwrap();
        }
        // A held verdict only in the FIRST store; the second store's claim has no verdict, so
        // it would derive `stale`. The returned standing must be the first store's.
        let mut ev = held_event(&claim, "2026-07-19T00:00:00Z");
        ev.store = first.into();
        store.append(&ev).await.unwrap();

        let (status, json) = get(&app(store), "payments/libfoo-pin").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json["store"], first,
            "the lexicographically-first store wins the tie-break: {json}"
        );
        assert_eq!(
            json["standing"], "verified",
            "and its standing (held) is what is returned, not the other store's stale: {json}"
        );
    }
}
