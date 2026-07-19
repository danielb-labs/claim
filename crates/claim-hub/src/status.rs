//! The `/status` endpoint: the hub's machine-readable health and position.
//!
//! `/status` reports where the hub *is* — the ledger head, the registry version,
//! the last sync time, and the count of rejected ingests (HUB.md §5, §3). It is the
//! one route this shell serves, and it must report **truthfully against a real,
//! possibly empty store** (invariant #6): an empty database reports head 0, version
//! 0, and no sync — not an error, and never a fabricated "healthy" — because a hub
//! that lies about its own position is the first thing a monitor would trust and the
//! last thing it should.
//!
//! Two fields — last sync and rejection count — have no producer in this shell:
//! registry sync (hub-05) records the sync time, and the ingest gate (hub-04)
//! records rejections. They are wired here reading zero/`None`, so the shape is
//! stable and the later items fill the source rather than reshape the endpoint.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use claim_hub_store::{Ledger, Registry};
use serde::Serialize;

use crate::app::AppState;

/// The body of a `/status` response: the hub's derived position, all four fields.
///
/// Serialized as JSON (HUB.md §5's machine-readable status endpoint). Every field is
/// sourced from the store or the derived read model, never a stored "health" flag —
/// the hub's position is *derived* at read time, so it can never disagree with the
/// evidence (invariant #3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    /// The ledger head — the position of the most recent event, `0` on an empty
    /// ledger. Sourced from [`Ledger::head`]. Advances as events are appended, so a
    /// monitor can watch it move.
    pub ledger_head: i64,

    /// The registry version — the number of store syncs applied, `0` before the first
    /// sync. Sourced from [`Registry::version`].
    pub registry_version: i64,

    /// When the registry was last synced (RFC 3339), or `None` if it never has been.
    /// `None` in this shell: registry sync (hub-05) is what records a sync time; the
    /// field is wired so that item fills the source, not the shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<String>,

    /// How many ingests the hub has rejected — a quiet source of staleness a monitor
    /// must be able to see (invariant #6). `0` in this shell: the ingest gate (hub-04)
    /// is what counts rejections; the field is wired so that item fills the source.
    pub rejection_count: i64,
}

/// The `/status` handler: read the hub's position from the store and report it.
///
/// Reads the ledger head and registry version through the trait seam (never SQL),
/// so an empty store reports truthful zeros rather than erroring — the birth state
/// of a freshly-booted hub is a valid, reportable position. A store read that
/// genuinely fails (a disk fault, a closed pool) is a `500`: the hub cannot state
/// its position, so it says so loudly rather than reporting a fabricated one.
///
/// `last_sync` and `rejection_count` read `None`/`0` here; hub-05 and hub-04 wire
/// their sources without changing this handler's shape.
pub async fn status(State(state): State<AppState>) -> Result<Json<Status>, StatusCode> {
    let ledger_head = state
        .store
        .head()
        .await
        .map_err(|error| {
            // The hub cannot state its own position; a stale green here would be a lie.
            tracing::error!(%error, "failed to read ledger head for /status");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .0;
    let registry_version = state
        .store
        .version()
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to read registry version for /status");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .0;

    Ok(Json(Status {
        ledger_head,
        registry_version,
        // Sourced by hub-05 (registry sync) and hub-04 (ingest gate) respectively;
        // truthfully empty until then.
        last_sync: None,
        rejection_count: 0,
    }))
}
