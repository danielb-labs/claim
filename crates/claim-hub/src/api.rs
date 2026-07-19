//! The read API: the hub's derived read model, served over the deriver.
//!
//! Every route here is a **read** (invariant #3): it loads the registry snapshot and
//! scans the ledger from `claim-hub-store`, reads the injectable clock and the freshness
//! config, calls [`claim_hub_core::derive`] (through the shared [`Memo`](claim_hub_core::Memo)),
//! and renders part of the resulting read model. Nothing is stored; a standing is
//! recomputed from the ledger and the clock every time, so it can never disagree with the
//! evidence. A read never appends an event.
//!
//! **Every response carries its as-of** (HUB.md §4) — the ledger head, the registry
//! version, and the clock instant the answer derives from — so the hub can never show a
//! green older than its evidence, and an agent can cache, diff, and resume. Reads are
//! deterministic: the same (ledger head, registry version, clock) always yields
//! byte-identical bytes.
//!
//! The surface (HUB.md §5), all over the one deriver:
//!
//! - `GET /api/claims/{id}` — one claim's derived standing (the hub-07 endpoint).
//! - `GET /api/claims` — claims filtered by `path` (id prefix), `store`, `standing`, or
//!   `supports` (a target a claim justifies), each with its standing.
//! - `GET /api/drifted`, `/api/due`, `/api/suspect` — the derived sets.
//! - `GET /api/claims/{id}/dossier` — a claim's full dossier: the statement and check by
//!   git reference at a commit, the standing with its as-of, the verdict history from the
//!   ledger, evidence, and the derived provenance the registry already holds.
//! - `GET /api/feed` — the cursor feed: the ledger, pollable from a position
//!   (`?cursor=<seq>`), **paginated by ledger seq, not offset**, so an intermittent agent
//!   catches up deterministically with no gap and no dupe.
//!
//! Auth is deferred to hub-13; the mount seam is [`crate::build_app`]. These routes are
//! unauthenticated for now.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use claim_core::ClaimId;
use claim_hub_core::{AsOf, ClaimStanding, ReadModel, Standing};
use claim_hub_store::{
    ledger_events, registry_snapshot, Ledger, Position, Registry, StoreError, StoredEvent,
};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::http::problem;

/// The read API's router: every hub-08 read route, nested under `/api` by
/// [`crate::build_app`].
///
/// The claim-scoped routes use a **catch-all** segment (`{*id}`) because a claim id is
/// namespaced with `/` (e.g. `payments/libfoo-pin`); a single-segment capture would 404
/// on any namespaced id. The dossier shares that catch-all: `GET /api/claims/{*id}`
/// captures both `payments/libfoo-pin` and `payments/libfoo-pin/dossier`, and the handler
/// splits the trailing `/dossier` off the id. Routing the dossier as its own
/// `/api/claims/{*id}/dossier` route is impossible: axum forbids a catch-all anywhere but
/// the final segment.
pub fn router() -> Router<AppState> {
    Router::new()
        // The claim-scoped catch-all serves both `GET /api/claims/{id}` and
        // `GET /api/claims/{id}/dossier`; the handler branches on a trailing `/dossier`.
        .route("/api/claims/{*id}", get(claim_or_dossier))
        .route("/api/claims", get(list_claims))
        .route("/api/drifted", get(drifted_set))
        .route("/api/due", get(due_set))
        .route("/api/suspect", get(suspect_set))
        .route("/api/feed", get(cursor_feed))
}

/// The whole read model plus the raw ledger, derived once from the live store.
///
/// Every read handler needs the derived [`ReadModel`] (for standings, the due set, the
/// as-of), and the dossier also needs the raw ledger events. Building both in one place
/// keeps the derivation identical across handlers and the as-of consistent: the model's
/// [`AsOf`] is the single source of every response's as-of.
///
/// Visible to the crate so the UI ([`crate::ui`]) renders its pages from **the same
/// derivation** the JSON API serves — the HTML/markdown surface is a lens over this read
/// model, not a second read of the store, so the two can never disagree.
pub(crate) struct ReadState {
    /// The derived read model — every claim's standing, the due set, the horizon, and the
    /// as-of the whole surface reports.
    pub(crate) model: ReadModel,
    /// The ledger's events in ascending seq order, retained so the dossier can render a
    /// claim's verdict history. The same slice the model was derived from, so the two
    /// never disagree about the ledger head.
    pub(crate) events: Vec<(u64, claim_hub_core::Event)>,
}

/// Build the read state — the registry snapshot, the ledger scan, and the derivation —
/// from the live store at the read clock.
///
/// The full spine of a read, shared by every handler: it reads the registry and the
/// ledger through the store's traits (never SQL), derives the whole read model at the
/// read clock under the hub's freshness config through the memo, and returns both the
/// model and the raw events. A store read fault is surfaced as a [`ReadError`] the caller
/// maps to a `500` — the hub cannot state the standing, so it says so loudly rather than
/// fabricating one (invariant #6).
///
/// `pub(crate)` so the UI reuses the identical derivation rather than re-deriving.
pub(crate) async fn read_state(state: &AppState) -> Result<ReadState, ReadError> {
    let registry = registry_snapshot(&state.store)
        .await
        .map_err(|error| ReadError::new("cannot read the registry right now", error))?;
    let events = ledger_events(&state.store)
        .await
        .map_err(|error| ReadError::new("cannot read the ledger right now", error))?;
    let now = (state.read_clock)();
    // Derive through the memo: a cache, never a store (invariant #3). The result is
    // identical to a direct `derive`; the memo only changes how often the work runs.
    let model = state
        .memo
        .read(&registry, &events, now, &state.deriver_config);
    Ok(ReadState { model, events })
}

/// A store read fault, with the operator-facing message the handler answers `500` with.
///
/// The underlying [`StoreError`] is logged (it may name a disk or connection fault the
/// client must not see), and the client gets the terse, safe `message`. `pub(crate)` so the
/// UI answers a store fault the same loud way (invariant #6).
pub(crate) struct ReadError {
    message: &'static str,
    source: StoreError,
}

impl ReadError {
    fn new(message: &'static str, source: StoreError) -> Self {
        Self { message, source }
    }

    /// A read fault from a store error, for the UI and any crate-internal caller reading the
    /// store outside [`read_state`]. `message` is the safe, terse client-facing reason.
    pub(crate) fn from_store(message: &'static str, source: StoreError) -> Self {
        Self::new(message, source)
    }

    /// Log the underlying fault and answer a `500` with the safe message.
    pub(crate) fn into_response(self) -> Response {
        tracing::error!(error = %self.source, "a read failed to reach the store");
        problem(StatusCode::INTERNAL_SERVER_ERROR, self.message)
    }
}

/// `GET /api/claims/{id}` and `GET /api/claims/{id}/dossier`, disambiguated by a trailing
/// `/dossier` in the catch-all capture.
///
/// The catch-all captures the whole tail after `/api/claims/`. Splitting here rather than
/// in two routes is forced by axum: a catch-all must be the final path segment, so
/// `/api/claims/{*id}/dossier` cannot be expressed.
///
/// A trailing `/dossier` is the dossier request **only when the id before it names a claim
/// the model holds**. Otherwise the whole capture is the id — including a claim whose own
/// id legitimately ends in `/dossier` — so that claim's standing stays reachable at
/// `GET /api/claims/{id}` instead of being silently shadowed by the dossier route (a silent
/// wrong answer invariant #6 forbids). In the rare case both `x` and `x/dossier` exist as
/// claims, `GET /api/claims/x/dossier` resolves to x's dossier; the standing of `x/dossier`
/// is then addressed exactly via `GET /api/claims?path=x/dossier`.
async fn claim_or_dossier(State(state): State<AppState>, Path(rest): Path<String>) -> Response {
    let read = match read_state(&state).await {
        Ok(read) => read,
        Err(error) => return error.into_response(),
    };
    if let Some(prefix) = rest.strip_suffix("/dossier") {
        if read.model.claims.keys().any(|(_, cid)| cid == prefix) {
            return dossier(&state, &read, prefix).await;
        }
    }
    claim_standing(&read, &rest)
}

/// `GET /api/claims/{id}`: the derived standing of one claim, with its as-of.
///
/// A claim the registry does not know — never synced, or retired and with no ledger
/// history — is a `404` naming it, never a fabricated `verified`. Where two connected
/// stores share a claim id (ids are unique only within a store), the lexicographically
/// first `(store, id)` match is returned — the documented M0 tie-break; the `store` query
/// param on `GET /api/claims` addresses a claim exactly.
fn claim_standing(read: &ReadState, id: &str) -> Response {
    match read.model.claims.iter().find(|((_, cid), _)| cid == id) {
        Some((_, standing)) => {
            Json(StandingResponse::new(standing, read.model.as_of)).into_response()
        }
        None => problem(
            StatusCode::NOT_FOUND,
            &format!(
                "no claim `{id}` in the registry — it may not be synced yet, or it was retired \
                 with no verdict history"
            ),
        ),
    }
}

/// The query parameters that filter `GET /api/claims`.
///
/// All are optional and combine with AND: a claim is returned only if it matches every
/// supplied filter. With no parameters the whole live set is returned. `deny_unknown_fields`
/// makes a mistyped filter a loud `400` rather than a silently ignored one that would
/// return the wrong set (invariant #6).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimsQuery {
    /// Match claims whose id starts with this prefix. The claim id is the namespaced
    /// handle for "what the org believes about what I am touching" (HUB.md §5); the
    /// registry stores no filesystem path, so a path filter is an id-prefix match (e.g.
    /// `payments/` selects every claim under the `payments` namespace). A prefix that
    /// matches a whole id also matches that one claim.
    path: Option<String>,
    /// Match claims in exactly this connected store (e.g. `github.com/acme/payments`).
    /// This is the exact-address selector the hub-07 tie-break note pointed at.
    store: Option<String>,
    /// Match claims whose derived standing is exactly this (`verified`, `stale`,
    /// `drifted`, `suspect`, `retired`). An unrecognized value is a `400` naming the
    /// accepted set.
    standing: Option<String>,
    /// Match claims that support this target — a decision ref or a claim id a claim
    /// justifies (the `supports` edge). Selects every claim whose `supports` list holds
    /// the target verbatim.
    supports: Option<String>,
}

/// `GET /api/claims`: the live set filtered by `path`, `store`, `standing`, and
/// `supports`, each claim with its derived standing.
///
/// The filters combine with AND. `standing` is parsed against the [`Standing`] enum; an
/// unrecognized value is a `400`. `supports` is resolved through the registry's reverse
/// supports index ([`Registry::claims_supporting`]) — the cross-store query #10 keys on —
/// and intersected with the derived set. The result is in (store, id) order for
/// determinism and carries the model's as-of.
async fn list_claims(State(state): State<AppState>, Query(query): Query<ClaimsQuery>) -> Response {
    // Parse the `standing` filter first: an invalid value is a client error, answered
    // before any store read so a bad request never touches the ledger.
    let standing_filter = match query.standing.as_deref().map(parse_standing) {
        Some(Ok(standing)) => Some(standing),
        Some(Err(reason)) => return problem(StatusCode::BAD_REQUEST, &reason),
        None => None,
    };

    let read = match read_state(&state).await {
        Ok(read) => read,
        Err(error) => return error.into_response(),
    };

    // The `supports` filter resolves to a set of (store, id) keys via the reverse index.
    // Resolved once so the per-claim filter is a cheap membership test.
    let supports_keys = match &query.supports {
        Some(target) => match state.store.claims_supporting(target).await {
            Ok(edges) => Some(
                edges
                    .into_iter()
                    .map(|e| (e.store, e.claim_id.as_str().to_owned()))
                    .collect::<std::collections::BTreeSet<_>>(),
            ),
            Err(error) => {
                return ReadError::new("cannot read the supports index right now", error)
                    .into_response()
            }
        },
        None => None,
    };

    let claims: Vec<StandingResponse> = read
        .model
        .claims
        .iter()
        .filter(|((store, id), standing)| {
            query.path.as_ref().is_none_or(|p| id.starts_with(p))
                && query.store.as_ref().is_none_or(|s| store == s)
                && standing_filter.is_none_or(|want| standing.standing == want)
                && supports_keys
                    .as_ref()
                    .is_none_or(|keys| keys.contains(&(store.clone(), id.clone())))
        })
        .map(|(_, standing)| StandingResponse::bare(standing))
        .collect();

    Json(ClaimsListResponse {
        claims,
        as_of: read.model.as_of,
    })
    .into_response()
}

/// `GET /api/drifted`: every claim whose derived standing is [`Standing::Drifted`].
async fn drifted_set(State(state): State<AppState>) -> Response {
    standing_set(&state, Standing::Drifted).await
}

/// `GET /api/due`: the review queue — every drifted, stale, or due-for-recheck claim.
///
/// This is the deriver's own due set ([`ReadModel::due`](claim_hub_core::ReadModel)), not a
/// `standing == due` filter: due-ness is a union of "needs attention now" states (drifted,
/// stale, or past its recheck cadence), so it is read from the model's computed
/// membership, which a standing-equality filter could not reproduce.
async fn due_set(State(state): State<AppState>) -> Response {
    let read = match read_state(&state).await {
        Ok(read) => read,
        Err(error) => return error.into_response(),
    };
    let claims: Vec<StandingResponse> = read
        .model
        .due
        .iter()
        .filter_map(|key| read.model.claims.get(key).map(StandingResponse::bare))
        .collect();
    Json(ClaimsListResponse {
        claims,
        as_of: read.model.as_of,
    })
    .into_response()
}

/// `GET /api/suspect`: every claim whose derived standing is [`Standing::Suspect`].
///
/// The suspect *propagation* rule (which claims become suspect over the supports graph) is
/// a later deriver rule; this endpoint serves the set today so the surface already carries
/// it, and it is populated the moment that rule lands with no route change.
async fn suspect_set(State(state): State<AppState>) -> Response {
    standing_set(&state, Standing::Suspect).await
}

/// The shared body of `/api/drifted` and `/api/suspect`: every claim of one standing.
async fn standing_set(state: &AppState, want: Standing) -> Response {
    let read = match read_state(state).await {
        Ok(read) => read,
        Err(error) => return error.into_response(),
    };
    let claims: Vec<StandingResponse> = read
        .model
        .claims
        .values()
        .filter(|s| s.standing == want)
        .map(StandingResponse::bare)
        .collect();
    Json(ClaimsListResponse {
        claims,
        as_of: read.model.as_of,
    })
    .into_response()
}

/// `GET /api/claims/{id}/dossier`: a claim's full derivation — statement, check by git
/// reference, standing with as-of, verdict history, evidence, and derived provenance.
///
/// The dossier is the agent's primary read (HUB.md §5): everything the org believes about
/// one claim and how good that belief is right now. A claim the registry does not hold at
/// its tip is a `404` — the dossier's git-referenced statement and check need a live
/// registry entry, so an absent one is an honest 404, never a fabricated standing. Where
/// the id is shared across stores, the lexicographically first store wins (as with
/// `GET /api/claims/{id}`).
///
/// The `standing`, `history`, and `as_of` all come from the one derived model, so the
/// trust judgment is stamped with exactly the inputs it derived from. The descriptive
/// fields — `statement`, `checks`, `commit`, `supports` — are the registry's *current*
/// rendering of the claim, read once more here; normally that is the same registry version
/// the model derived from, and at most one sync ahead of it. That is a safe direction: the
/// body can only ever describe a claim as current-or-newer than the `as_of`, never as more
/// verified than the standing, because the standing is the model's alone.
async fn dossier(state: &AppState, read: &ReadState, id: &str) -> Response {
    let claim_id = match id.parse::<ClaimId>() {
        Ok(id) => id,
        Err(error) => {
            return problem(
                StatusCode::BAD_REQUEST,
                &format!("`{id}` is not a valid claim id: {error}"),
            )
        }
    };

    // The standing locates the store: the read model is keyed by (store, id), so the first
    // matching id fixes the store the rest of the dossier is read against.
    let Some(((store, _), standing)) = read.model.claims.iter().find(|((_, cid), _)| cid == id)
    else {
        return problem(
            StatusCode::NOT_FOUND,
            &format!(
                "no claim `{id}` in the registry — it may not be synced yet, or it was retired \
                 with no verdict history"
            ),
        );
    };

    // The registry entry carries the git-referenced statement, check digests, commit, and
    // supports edges. A claim in the derived model but absent from the registry read is a
    // retirement with only ledger history — no live statement to render, so a `404`.
    let registered = match state.store.claim(store, &claim_id).await {
        Ok(Some(registered)) => registered,
        Ok(None) => {
            return problem(
                StatusCode::NOT_FOUND,
                &format!(
                    "claim `{id}` in store `{store}` is retired (absent from the registry tip); \
                     its history is on the ledger but it has no live statement to render"
                ),
            )
        }
        Err(error) => {
            return ReadError::new("cannot read the claim from the registry right now", error)
                .into_response()
        }
    };

    // The verdict history: every ledger event for this (store, claim), in ascending seq
    // order — the dated evidence a standing derives from (HUB.md §5). Dated evidence is
    // reported to weigh, never to obey.
    let history: Vec<VerdictEntry> = read
        .events
        .iter()
        .filter(|(_, event)| &event.store == store && event.claim == id)
        .filter_map(|(seq, event)| VerdictEntry::from_event(*seq, event))
        .collect();

    let checks: Vec<CheckRef> = registered
        .check_digests
        .iter()
        .enumerate()
        .map(|(index, digest)| CheckRef {
            index,
            digest: digest.clone(),
        })
        .collect();

    Json(DossierResponse {
        id: id.to_owned(),
        store: store.clone(),
        statement: registered.statement,
        commit: registered.commit,
        checks,
        supports: registered.supports,
        standing,
        history,
        as_of: read.model.as_of,
    })
    .into_response()
}

/// The query parameter for `GET /api/feed`: the ledger position to resume after.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FeedQuery {
    /// The last ledger seq the caller has already processed; the feed returns everything
    /// **strictly after** it, in ascending seq order. Absent (or `0`) means from the
    /// start. Pagination is by this seq, never by an offset, so a resumed poll has no gap
    /// and no dupe even as the ledger grows underneath it.
    cursor: Option<i64>,
}

/// `GET /api/feed?cursor=<seq>`: the ledger, pollable from a position.
///
/// The cursor feed of HUB.md §5: an intermittent agent stores the last seq it saw and
/// passes it back to receive only what is new. **Pagination is by ledger seq, not
/// offset** — the scan is `scan_from(cursor)`, whose exclusive lower bound is what makes
/// the cursor resumable with no overlap and no gap. The response carries `next_cursor`
/// (the last seq in this page, to pass back next time) and `ledger_head` (the feed's as-of
/// position), so a poller resumes deterministically. A ledger that only grows means a
/// stable cursor never re-reads an event it already saw.
async fn cursor_feed(State(state): State<AppState>, Query(query): Query<FeedQuery>) -> Response {
    // A negative cursor is nonsense (positions start at 1, 0 means "from the start"); clamp
    // to 0 so a stray `-1` reads from the start rather than erroring — the feed is
    // forgiving on the cursor's floor, exact on its resumption semantics.
    let cursor = Position(query.cursor.unwrap_or(0).max(0));
    let stored = match state.store.scan_from(cursor).await {
        Ok(stored) => stored,
        Err(error) => {
            return ReadError::new("cannot read the ledger feed right now", error).into_response()
        }
    };

    // The head at read time is the feed's as-of position: a poller whose `next_cursor`
    // reaches it knows it is fully caught up.
    let head = match state.store.head().await {
        Ok(head) => head.0,
        Err(error) => {
            return ReadError::new("cannot read the ledger head right now", error).into_response()
        }
    };

    let events: Vec<FeedEntry> = stored.iter().map(FeedEntry::from_stored).collect();
    // The next cursor is the last event's seq in this page (or the caller's cursor when the
    // page is empty, so a caught-up poller passes back the same position and gets nothing).
    let next_cursor = events.last().map_or(cursor.0, |e| e.seq);

    Json(FeedResponse {
        events,
        next_cursor,
        ledger_head: head,
    })
    .into_response()
}

/// Parse a `standing` query value against the [`Standing`] enum's kebab-case wire names.
///
/// Deserializing through the enum's own `serde` names makes the accepted set the single
/// source of truth: a caller filters by the exact string the standing serializes as, and
/// a future `Standing` variant (the enum is `#[non_exhaustive]`, reserved for growth)
/// becomes filterable automatically — there is no hand-kept list here to drift out of step
/// with the enum. An unrecognized value returns the actionable reason, which the handler
/// answers `400` with.
fn parse_standing(value: &str) -> Result<Standing, String> {
    serde_json::from_value::<Standing>(serde_json::Value::String(value.to_owned())).map_err(|_| {
        format!(
            "unknown standing `{value}`; expected one of verified, stale, drifted, suspect, \
             retired"
        )
    })
}

/// A single claim's standing plus (optionally) the read's as-of.
///
/// The single-claim endpoint carries its own `as_of`; a list member does not — the list
/// carries one shared `as_of` at the top level, so repeating it per claim would be noise
/// and would risk a reader mistaking a per-claim as-of for a per-claim derivation, when
/// the whole list derives at one instant. The standing is flattened in, so the body is the
/// standing's own fields (`id`, `store`, `standing`, freshness) plus, when present, an
/// `as_of` object.
#[derive(Debug, Serialize)]
struct StandingResponse<'a> {
    /// The claim's full derived standing.
    #[serde(flatten)]
    standing: &'a ClaimStanding,
    /// The exact inputs this standing was derived from — present on the single-claim
    /// endpoint, omitted in list responses (which carry one shared `as_of`).
    #[serde(skip_serializing_if = "Option::is_none")]
    as_of: Option<AsOf>,
}

impl<'a> StandingResponse<'a> {
    /// A single-claim response carrying its own as-of.
    fn new(standing: &'a ClaimStanding, as_of: AsOf) -> Self {
        Self {
            standing,
            as_of: Some(as_of),
        }
    }

    /// A list-member response with no per-claim as-of.
    fn bare(standing: &'a ClaimStanding) -> Self {
        Self {
            standing,
            as_of: None,
        }
    }
}

/// The body of a claims-list response (`GET /api/claims`, `/api/drifted`, `/api/due`,
/// `/api/suspect`): the matching claims and the one as-of the whole set derived at.
#[derive(Debug, Serialize)]
struct ClaimsListResponse<'a> {
    /// The matching claims, in (store, id) order for determinism, each with its standing
    /// but no per-claim as-of.
    claims: Vec<StandingResponse<'a>>,
    /// The inputs the whole set derived from — one as-of for every claim in it, because
    /// the set is one derivation.
    as_of: AsOf,
}

/// One check of a claim by git reference: its declared index and content digest.
///
/// The dossier references the check *to git at the commit* (HUB.md §5) rather than
/// inlining its source: the registry holds each check's content digest (its stable
/// identity), and the `commit` on the dossier is the sha the claim was read at, so a
/// reader resolves the check's definition from git at that commit. The index is the
/// check's declared position; the digest is the identity the ledger keys verdicts on. Also
/// the shape of a verdict-history entry's `check`.
#[derive(Debug, Serialize)]
struct CheckRef {
    /// The check's zero-based declared position in the claim.
    index: usize,
    /// The check's canonical content digest — the ledger's join key.
    digest: String,
}

/// One verdict in a claim's history: a ledger event rendered for the dossier.
///
/// The dated evidence a standing derives from (HUB.md §5): the verdict, when it was
/// reported, the check it was about, the commit, the producer identity, and any evidence.
/// It is reported to *weigh*, never to obey — a claims surface an agent obeys blindly is an
/// injection channel (PRODUCT.md §6), so the history is presented as dated observations
/// carrying their producer provenance, not as instructions.
#[derive(Debug, Serialize)]
struct VerdictEntry {
    /// The ledger seq of this event — its position in the append-only log.
    seq: u64,
    /// The verdict reported (`held`/`drifted`/`broken`/`unverifiable`).
    verdict: claim_core::Verdict,
    /// Which check the verdict was about, by declared index and content digest.
    check: CheckRef,
    /// When the producer reported it (RFC 3339).
    reported_at: claim_core::Timestamp,
    /// The commit sha the check was reported against.
    commit: String,
    /// The evidence the check recorded, if any (already capped at ingest).
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence: Option<String>,
    /// The verified producer identity behind the verdict, verbatim — the derived
    /// provenance (HUB.md §4): who produced this observation, so the trust judgment is
    /// re-derivable. Not asserted by the claim file (invariant #3).
    producer: serde_json::Map<String, serde_json::Value>,
}

impl VerdictEntry {
    /// Render one ledger event as a history entry, or `None` if the event is not a verdict.
    ///
    /// Only a [`EventKind::Verdict`](claim_hub_core::EventKind) event carries a verdict; a
    /// later kind (a nag, an ack) is not a verdict and must never render as one in the
    /// history — that would be telemetry masquerading as a verdict (invariant #4). The enum
    /// is `#[non_exhaustive]`, so a new kind lands in the `_` arm and is excluded from the
    /// verdict history rather than mislabeled; surfacing it is a deliberate later choice,
    /// not a silent default.
    fn from_event(seq: u64, event: &claim_hub_core::Event) -> Option<Self> {
        match event.kind {
            claim_hub_core::EventKind::Verdict => {}
            _ => return None,
        }
        Some(Self {
            seq,
            verdict: event.verdict,
            check: CheckRef {
                index: event.check.index,
                digest: event.check.digest.clone(),
            },
            reported_at: event.reported_at,
            commit: event.commit.clone(),
            evidence: event.evidence.clone(),
            producer: event.producer.0.clone(),
        })
    }
}

/// A claim's full dossier (`GET /api/claims/{id}/dossier`).
///
/// The statement and check by git reference at `commit`, the derived standing with its
/// as-of, the verdict history, and the derived provenance. Author and PR-approval
/// provenance come from git and the forge (invariant #3); v1 includes what the registry
/// already holds — the commit the claim was read at and each verdict's verified producer —
/// and does not fabricate an author or approval it has not resolved. Richer forge
/// provenance is an additive later read behind the same shape.
#[derive(Debug, Serialize)]
struct DossierResponse<'a> {
    /// The claim's id.
    id: String,
    /// The store it lives in.
    store: String,
    /// The human-and-agent-readable statement — the real source of truth a check only
    /// approximates.
    statement: String,
    /// The commit sha the claim (and its checks) were read at — the git reference the
    /// statement and checks resolve against.
    commit: String,
    /// The claim's checks by git reference: declared index and content digest, resolvable
    /// from git at `commit`.
    checks: Vec<CheckRef>,
    /// The targets this claim supports — the decisions or claims it justifies.
    supports: Vec<String>,
    /// The derived standing over the claim's checks: its conservative verdict, freshness,
    /// due-ness, and skips.
    standing: &'a ClaimStanding,
    /// The verdict history from the ledger, in ascending seq order — the dated evidence the
    /// standing derives from.
    history: Vec<VerdictEntry>,
    /// The inputs the standing derived from.
    as_of: AsOf,
}

/// One event in the cursor feed (`GET /api/feed`): the whole attested envelope plus its
/// ledger seq.
///
/// The feed is the raw ledger, so an entry is the event verbatim with the seq that keys
/// pagination. An agent pages by the seq, weighs the producer identity, and never treats a
/// verdict as an instruction (HUB.md §5).
#[derive(Debug, Serialize)]
struct FeedEntry<'a> {
    /// The ledger seq — the pagination key. Ascending across the page, strictly greater
    /// than the request's cursor.
    seq: i64,
    /// The attested event, verbatim.
    #[serde(flatten)]
    event: &'a claim_hub_core::Event,
}

impl<'a> FeedEntry<'a> {
    fn from_stored(stored: &'a StoredEvent) -> Self {
        Self {
            seq: stored.position.0,
            event: &stored.event,
        }
    }
}

/// The body of a cursor-feed response.
///
/// `next_cursor` is what a poller passes back as `?cursor=` to resume exactly after the
/// last event seen — the seq-based pagination contract. `ledger_head` is the feed's as-of:
/// the head of the ledger scanned, so a caller whose `next_cursor` equals `ledger_head`
/// knows it is fully caught up.
#[derive(Debug, Serialize)]
struct FeedResponse<'a> {
    /// This page's events, ascending by seq, each strictly after the request's cursor.
    events: Vec<FeedEntry<'a>>,
    /// The seq to pass back as `?cursor=` next time — the last event's seq, or the
    /// request's cursor when the page is empty (a caught-up poller stays put).
    next_cursor: i64,
    /// The ledger head at read time — the feed's as-of position. `next_cursor` reaching it
    /// means the poller has seen everything.
    ledger_head: i64,
}

#[cfg(test)]
mod tests;
