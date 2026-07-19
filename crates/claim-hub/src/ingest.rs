//! The ingest gate: the hub's single telemetry write path (HUB.md §3).
//!
//! One route, `POST /ingest`. It authenticates the producer's OIDC id-token, validates
//! the pushed `claim check --json` report against the wire types, turns each check
//! result into a content-keyed [`Event`], and appends every event verbatim — or rejects
//! the whole push loudly, with a counted record and no event written. There is no other
//! way in: no backfill endpoint, no manual verdict entry (invariant #4). A feature that
//! seems to need one is a new event kind with its own producer, not a side door.
//!
//! The honesty rules this route encodes:
//!
//! - **A rejected push writes nothing and is counted.** A forged signature, an expired
//!   token, a wrong audience, an unconnected repository, or a malformed envelope returns
//!   a 4xx naming the reason and appends no event — but increments the persisted
//!   rejection counter, so a hub turning telemetry away is visible at `/status` rather
//!   than silently aging claims into stale (invariant #6, HUB.md §3).
//! - **A redelivery is idempotent.** The storage layer dedups on
//!   (store, producer run, claim, check identity), so a retried CI push returns the
//!   original success and adds no row (HUB.md §2).
//! - **Evidence is capped at the door.** Each event's evidence is bounded via
//!   [`cap_evidence`] before it reaches the ledger — truncated with a marker, never
//!   dropped (invariant #6).
//! - **A check's identity is content, not position.** The wire report carries a
//!   positional check index; the digest — the ledger's key — comes from the registry's
//!   parsed check definition. A claim or check the registry does not know is rejected,
//!   never filed under a fabricated identity (issue #18, invariant #6).
//!
//! The token is not authenticated by this handler directly: a token that cannot even be
//! verified is caught here, but the *identity* is verified up front and passed in as a
//! [`VerifiedProducer`] — see [`ingest`].

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use claim_core::{ClaimId, Timestamp};
use claim_hub_core::wire::{CheckReport, CheckResult};
use claim_hub_core::{cap_evidence, CheckRef, Event, EventKind};
use claim_hub_store::{Appended, Ledger, Registry, Rejections};
use serde::Serialize;
use std::str::FromStr;

use crate::app::AppState;
use crate::oidc::{AuthReject, VerifiedProducer, VerifyError};

/// The `Authorization: Bearer <token>` scheme prefix the OIDC id-token arrives under.
const BEARER_PREFIX: &str = "Bearer ";

/// The `POST /ingest` handler: verify the producer, validate the envelope, append or
/// reject.
///
/// The flow, each step loud on failure:
///
/// 1. Extract the bearer OIDC token from `Authorization`; a missing or malformed header
///    is a 401 (no identity to trust).
/// 2. Verify the token through the configured [`OidcVerifier`](crate::oidc::OidcVerifier):
///    a rejection is a 401 with the reason and a counted rejection; an inability to
///    verify (JWKS unreachable) is a 503, *not* counted — the hub could not judge the
///    token, so it does not call a possibly-valid push forged.
/// 3. Parse the body as a `claim check --json` report; a malformed envelope is a 400
///    naming the field, and a counted rejection.
/// 4. Build one [`Event`] per check result — verdict, capped evidence, the verified
///    store/commit/producer, and the check digest read from the registry. A claim or
///    check the registry does not know is a 400, and a counted rejection.
/// 5. Append every event; return the ledger positions. A redelivery dedups to the
///    original success.
///
/// Verification failures and envelope failures both count as rejections (invariant #6),
/// but ingest is only ever reached with a *guarded* store: [`AppState`] carries the
/// verifier, so if the hub has no OIDC config the route is not mounted and this handler
/// is unreachable.
pub async fn ingest(State(state): State<AppState>, request: axum::extract::Request) -> Response {
    let verifier = match &state.verifier {
        Some(v) => v,
        None => {
            // Unreachable in practice — the route is only mounted when the verifier is
            // configured — but a defensive 503 rather than a panic keeps a misassembly
            // loud, not a crash.
            tracing::error!("ingest reached with no OIDC verifier configured");
            return problem(StatusCode::SERVICE_UNAVAILABLE, "ingest is not configured");
        }
    };

    let (parts, body) = request.into_parts();

    // 1. The bearer token.
    let token = match bearer_token(&parts.headers) {
        Some(token) => token,
        None => {
            // No identity at all: a 401, not a counted *rejection* — a rejection is a
            // push the hub judged and refused, and there is nothing here to judge. It is
            // still loud (a 401 with a reason), just not part of the turned-away-telemetry
            // count that tracks judged-and-refused pushes.
            return problem(
                StatusCode::UNAUTHORIZED,
                "missing `Authorization: Bearer <oidc-token>` header",
            );
        }
    };

    // 2. Verify identity.
    let producer = match verifier.verify(token).await {
        Ok(producer) => producer,
        Err(VerifyError::Reject(reject)) => {
            return reject_ingest(&state, reject).await;
        }
        Err(VerifyError::Unavailable(detail)) => {
            // The hub could not verify (JWKS unreachable): a 503, not a rejection. It did
            // not judge the token invalid — it could not judge it — so it must not count
            // a rejection or call the push forged (invariant #1: a broken verifier never
            // manufactures a verdict). The producer retries.
            tracing::warn!(%detail, "ingest could not verify a token (JWKS unavailable)");
            return problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "cannot verify the token right now (identity provider unreachable); retry",
            );
        }
    };

    // 3. Read and parse the envelope. A body past the size limit (or unreadable) is an
    // envelope problem, not an identity one: a counted 400, like any malformed report.
    let bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return reject_envelope(
                &state,
                &format!(
                    "request body exceeds the {MAX_BODY_BYTES}-byte ingest limit or could not be read"
                ),
            )
            .await;
        }
    };
    let report = match CheckReport::from_json(&bytes) {
        Ok(report) => report,
        Err(error) => {
            // A malformed envelope is a *rejection*: the producer authenticated but sent
            // something the hub cannot read. Counted, and the serde error names the field.
            return reject_envelope(&state, &format!("malformed check report: {error}")).await;
        }
    };

    // 4. Build the events. The ingest instant (a UTC `now` from the state's clock,
    // injectable so tests are deterministic) stamps every event's `reported_at` — the
    // moment the observation reached the ledger.
    let reported_at = (state.clock)();
    let events = match build_events(&state, &producer, &report, reported_at).await {
        Ok(events) => events,
        Err(BuildError::Rejected(reason)) => {
            return reject_envelope(&state, &reason).await;
        }
        Err(BuildError::Store(detail)) => {
            tracing::error!(%detail, "ingest failed reading the registry for a check digest");
            return problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "cannot resolve check identities right now; retry",
            );
        }
    };

    // 5. Append.
    let mut positions = Vec::with_capacity(events.len());
    for event in &events {
        match state.store.append(event).await {
            Ok(appended) => positions.push(append_record(appended)),
            Err(error) => {
                // An append fault after some events landed is an infrastructure error, not
                // a rejection: the appended events are already durable (append-only), and
                // a retry dedups them. Report loudly.
                tracing::error!(%error, "ingest failed appending an event");
                return problem(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to record the verdict",
                );
            }
        }
    }

    let accepted = IngestAccepted {
        status: "accepted",
        accepted: positions.len(),
        positions,
    };
    (StatusCode::OK, Json(accepted)).into_response()
}

/// The maximum ingest body size, in bytes.
///
/// A `claim check --json` report scales with the number of claims and their evidence;
/// each check's evidence is separately capped downstream, but the *whole* body is bounded
/// here so a hostile or runaway producer cannot force the hub to buffer an unbounded
/// request. Generous enough for a large repo's full report (thousands of claims), small
/// enough to bound memory: a body past it is rejected before it is read.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Extract the bearer token from an `Authorization` header, if present and well-formed.
///
/// Returns the token with the `Bearer ` scheme stripped, or `None` when the header is
/// absent, not UTF-8, or not a bearer credential. The token itself is not validated here
/// — that is the verifier's job.
fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    let value = headers.get(axum::http::header::AUTHORIZATION)?;
    let value = value.to_str().ok()?;
    value.strip_prefix(BEARER_PREFIX).map(str::trim)
}

/// The internal failure of [`build_events`]: a producer-caused rejection, or a store
/// fault reading the registry.
enum BuildError {
    /// The envelope named a claim or check the registry does not know, or a value that
    /// could not be turned into an event — a rejection, with a producer-facing reason.
    Rejected(String),
    /// The registry could not be read (a store fault) — an infrastructure error, not a
    /// rejection.
    Store(String),
}

/// Turn a validated report and a verified producer into the events to append.
///
/// One event per check result across every claim. For each, the check's *identity* — the
/// digest the ledger keys on — is read from the registry by (store, claim, index), since
/// the report carries only the positional index (issue #18). A claim the registry has not
/// synced, or a check index past what the registry holds, is a [`BuildError::Rejected`]:
/// the hub refuses to fabricate a check identity, because a wrong digest would file a
/// verdict under the wrong check (invariant #6). Evidence is capped here, and the
/// verified producer/store/commit are stamped onto every event.
///
/// Skipped checks in the report carry no verdict (a skip is not a pass), so they produce
/// no event — they are surfaced by the CLI, not ingested as telemetry.
async fn build_events(
    state: &AppState,
    producer: &VerifiedProducer,
    report: &CheckReport,
    reported_at: Timestamp,
) -> Result<Vec<Event>, BuildError> {
    let commit = producer.commit().ok_or_else(|| {
        BuildError::Rejected(
            "the verified OIDC token carried no commit sha (`sha` claim); a verdict must \
             record the commit it was checked against"
                .to_owned(),
        )
    })?;
    let store = producer.store();

    let mut events = Vec::new();
    for claim in &report.claims {
        let claim_id = ClaimId::from_str(&claim.id).map_err(|_| {
            BuildError::Rejected(format!(
                "claim id `{}` in the report is not a valid claim id",
                claim.id
            ))
        })?;

        for (index, check) in claim.checks.iter().enumerate() {
            let digest = match state.store.check_digest(store, &claim_id, index).await {
                Ok(Some(digest)) => digest,
                Ok(None) => {
                    // The registry does not know this claim/check: reject loudly rather
                    // than fabricate a digest. Most often the store has not synced yet
                    // (the claim exists in git but the hub has not mirrored it), or the
                    // report and the tip disagree on the check count. Either way the hub
                    // cannot honestly identify the check, so it refuses the whole push.
                    return Err(BuildError::Rejected(format!(
                        "the registry has no check at index {index} of claim `{}` in store \
                         `{store}` — the store may not be synced yet, or the report and the \
                         registered tip disagree; the push is refused rather than filed under \
                         a fabricated check identity",
                        claim.id
                    )));
                }
                Err(error) => return Err(BuildError::Store(error.to_string())),
            };

            events.push(build_event(
                store,
                commit,
                producer,
                &claim.id,
                index,
                &digest,
                check,
                reported_at,
            ));
        }
    }
    Ok(events)
}

/// Build one event from a check result, capping evidence and stamping the verified
/// identity.
///
/// `reported_at` is the ingest instant — the moment the observation reached the ledger —
/// injected by the caller so tests are deterministic. The report carries no per-check
/// timestamp the hub relies on, so the hub's own clock is authoritative for when it saw
/// the verdict. The verdict is the shared [`claim_core::Verdict`], so `held`/`drifted`/
/// `broken` mean what the CLI meant; the evidence is capped
/// ([`cap_evidence`]); the producer block is the verbatim verified identity.
#[allow(clippy::too_many_arguments)]
fn build_event(
    store: &str,
    commit: &str,
    producer: &VerifiedProducer,
    claim_id: &str,
    index: usize,
    digest: &str,
    check: &CheckResult,
    reported_at: Timestamp,
) -> Event {
    Event {
        kind: EventKind::Verdict,
        claim: claim_id.to_owned(),
        check: CheckRef {
            index,
            digest: digest.to_owned(),
        },
        verdict: check.verdict,
        evidence: check.evidence.as_deref().map(cap_evidence),
        commit: commit.to_owned(),
        store: store.to_owned(),
        producer: producer.producer().clone(),
        reported_at,
    }
}

/// A rejection caused by the envelope (a malformed report, an unknown claim/check).
///
/// Counted like every rejection (invariant #6) and answered 400 — the producer
/// authenticated but the payload was unusable, so the fault is the request's, not the
/// identity's. Names the reason for the producer to fix.
async fn reject_envelope(state: &AppState, reason: &str) -> Response {
    count_rejection(state).await;
    tracing::warn!(%reason, "ingest rejected an envelope");
    problem(StatusCode::BAD_REQUEST, reason)
}

/// A rejection caused by the token's identity, answered with the right 4xx and counted.
///
/// An [`AuthReject`] is a token the hub *judged* and refused; it counts toward the
/// turned-away-telemetry total and returns 401 (an authentication failure) — except an
/// unconnected repository, which is a 403 (the token is authentic, but not authorized for
/// a store this hub tracks). The reason is named for the producer.
async fn reject_ingest(state: &AppState, reject: AuthReject) -> Response {
    count_rejection(state).await;
    let reason = reject.reason();
    tracing::warn!(%reason, "ingest rejected a token");
    let status = match reject {
        AuthReject::UnconnectedRepository(_) => StatusCode::FORBIDDEN,
        _ => StatusCode::UNAUTHORIZED,
    };
    problem(status, &reason)
}

/// Increment the persisted rejection counter, logging (not failing) if it cannot be
/// recorded.
///
/// The counter is best-effort in the sense that a store fault must not turn a rejection
/// into a different response — the push is still rejected. But a failure to *count* is
/// itself loud in the log, because an uncounted rejection is exactly the invisible
/// staleness invariant #6 forbids; the log line is the backstop when the counter's own
/// store is faulting.
async fn count_rejection(state: &AppState) {
    if let Err(error) = state.store.record_rejection().await {
        tracing::error!(%error, "failed to record an ingest rejection in the counter");
    }
}

/// Build a `{ "error": <reason> }` JSON problem response with `status`.
///
/// One shape for every non-2xx, so a producer (human or agent) reads the reason the same
/// way regardless of which check failed. The body is machine-readable and the reason is
/// the loud, actionable message.
fn problem(status: StatusCode, reason: &str) -> Response {
    (
        status,
        Json(ProblemBody {
            error: reason.to_owned(),
        }),
    )
        .into_response()
}

/// The body of a rejection: a single machine-readable reason.
#[derive(Debug, Serialize)]
struct ProblemBody {
    /// The producer-facing reason the push was refused, naming what to fix.
    error: String,
}

/// The body of an accepted ingest: how many events landed and where.
#[derive(Debug, Serialize)]
struct IngestAccepted {
    /// Always `"accepted"` for a 200 — the push was recorded (some or all events may
    /// have been redeliveries that deduped, still a success).
    status: &'static str,
    /// How many events the push carried (new plus deduped) — the length of `positions`.
    accepted: usize,
    /// One record per event: its ledger position and whether it was newly appended or a
    /// deduped redelivery.
    positions: Vec<AppendRecord>,
}

/// One event's append outcome: its ledger position and whether it was new or a duplicate.
#[derive(Debug, Serialize)]
struct AppendRecord {
    /// The ledger position the event occupies (its own, or the original a duplicate
    /// collapsed onto).
    position: i64,
    /// `true` when this call newly appended the event; `false` when it deduped onto an
    /// existing one (an idempotent redelivery, HUB.md §2).
    new: bool,
}

/// Turn an [`Appended`] into its serializable record.
fn append_record(appended: Appended) -> AppendRecord {
    match appended {
        Appended::New(position) => AppendRecord {
            position: position.0,
            new: true,
        },
        Appended::Duplicate(position) => AppendRecord {
            position: position.0,
            new: false,
        },
    }
}
