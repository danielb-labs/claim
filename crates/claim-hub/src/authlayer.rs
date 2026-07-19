//! The read-auth middleware: one outer layer over every read surface.
//!
//! This is the layer that makes read auth *uniform* — it wraps the protected router (the
//! JSON API `/api`, the UI and twins `/ui`+`/llms.txt`, and the MCP `/mcp`) as a single
//! tower layer, so no read route can be reached without passing through it and a new read
//! route added later is covered the moment it is mounted inside the protected group. The
//! deliberately-unauthenticated surfaces — `/status` (health), the RFC 9728 metadata
//! document, and the OIDC-authenticated `/ingest` — are mounted *outside* this layer, so
//! their exposure is a visible, reviewed decision in [`crate::build_app`], not an accident.
//!
//! The decision, in order (invariant #6 — every branch that cannot authenticate is a loud
//! `401`/`403`/`503`, never a silent allow):
//!
//! 1. **Open reads?** If the policy is the explicit `open_reads` opt-in, pass through. This
//!    is the *only* path that serves a protected route with no credential, and it exists
//!    only because [`ReadAuthPolicy::resolve`] refused to build unless it was opted into or
//!    an authenticator was configured.
//! 2. **A bearer token?** No `Authorization: Bearer …` → `401` with the RFC 9728
//!    `WWW-Authenticate` pointer, so the client discovers how to authenticate.
//! 3. **Authenticate.** A hub-minted token (recognized by its `claimhub_` prefix) is matched
//!    against the hashed floor in constant time; anything else is verified as an IdP JWT
//!    against the configured issuer's JWKS. A rejection is a counted `401`; an unreachable
//!    IdP is a `503` (never a silent allow); success yields a [`ReadPrincipal`].
//! 4. **Authorize.** The route's [`RequiredScope`] is checked against the principal's
//!    grant. A missing scope is a `403` — authenticated but not permitted.
//!
//! The counted-rejection metric ([`AuthLayerState::rejection_count`]) is the read-auth analogue
//! of the ingest gate's rejection counter: a rising count is the hub telling an operator
//! that reads are being turned away, distinct from the *ingest* rejection count `/status`
//! already surfaces (conflating the two would hide which surface is being probed). It is an
//! in-process counter, not a stored one — a read `401` is operational health, not ledger
//! telemetry (invariant #4).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use crate::http::problem_with_headers;
use crate::metadata::ResourceMetadata;
use crate::readauth::{ReadAuthPolicy, ReadPrincipal, ReadVerifyError};
use crate::scope::RequiredScope;

/// The `Authorization: Bearer <token>` scheme prefix a read credential arrives under.
const BEARER_PREFIX: &str = "Bearer ";

/// The prefix a hub-minted token carries, used to route a credential to the scoped-token
/// path instead of the IdP-JWT path. Kept in sync with [`crate::token`]'s prefix by a test
/// (`the_hub_token_prefix_matches_the_token_module`).
const HUB_TOKEN_PREFIX: &str = "claimhub_";

/// The shared state the read-auth layer reads: the resolved policy, the metadata document
/// and its pointer, and the counted-rejection metric.
///
/// One per hub, behind an [`Arc`] in the layer closure and in [`crate::build_app`]'s state.
/// Holding the *resolved* [`ReadAuthPolicy`] means the secure-default decision was already
/// made and validated at boot ([`ReadAuthPolicy::resolve`]); the layer never re-derives it
/// per request. It also carries the RFC 9728 [`ResourceMetadata`] document so the
/// unauthenticated well-known route can serve it, and the URL to point a `401` at.
pub struct AuthLayerState {
    /// The resolved policy: whether reads are open, the IdP verifier, the scoped-token floor.
    policy: ReadAuthPolicy,
    /// The RFC 9728 protected-resource metadata document, served unauthenticated at the
    /// well-known path so a client discovers how to authenticate.
    metadata: ResourceMetadata,
    /// The URL of the metadata document (root-relative), for the `WWW-Authenticate`
    /// challenge on a `401`.
    metadata_url: String,
    /// How many reads the layer has turned away (a `401`/`403`) — the read-auth rejection
    /// metric. In-process, not stored: a read rejection is operational health, not ledger
    /// telemetry (invariant #4).
    rejections: AtomicU64,
}

impl AuthLayerState {
    /// State over a resolved `policy`, serving `metadata` and pointing `401`s at
    /// `metadata_url`.
    #[must_use]
    pub fn new(
        policy: ReadAuthPolicy,
        metadata: ResourceMetadata,
        metadata_url: impl Into<String>,
    ) -> Self {
        Self {
            policy,
            metadata,
            metadata_url: metadata_url.into(),
            rejections: AtomicU64::new(0),
        }
    }

    /// The RFC 9728 metadata document, for the unauthenticated well-known route to serve.
    #[must_use]
    pub fn metadata(&self) -> &ResourceMetadata {
        &self.metadata
    }

    /// How many reads have been turned away (`401`/`403`) since boot — the read-auth
    /// rejection metric, for a test to assert and an operator to observe.
    #[must_use]
    pub fn rejection_count(&self) -> u64 {
        self.rejections.load(Ordering::SeqCst)
    }

    /// Count one turned-away read.
    fn count_rejection(&self) {
        self.rejections.fetch_add(1, Ordering::SeqCst);
    }
}

/// The axum middleware function the layer runs for every protected request.
///
/// Enforces the four-step decision of the module docs against `required` — the scope the
/// wrapped routes demand (v1: [`RequiredScope::READ`]). On success it inserts the
/// authenticated [`ReadPrincipal`] into the request extensions (so a future handler can
/// read *who* asked) and calls `next`. On any failure it returns the terminal response and
/// `next` is never called, so an unauthenticated request never reaches a read handler.
///
/// `state` and `required` are bound per protected group when the layer is constructed
/// ([`protect`]); axum passes the [`Request`] and [`Next`].
pub async fn authorize(
    state: Arc<AuthLayerState>,
    required: RequiredScope,
    request: Request,
    next: Next,
) -> Response {
    // 1. The explicit open-reads opt-in: the only unauthenticated path to a read handler,
    //    and only reachable because `resolve` refused a no-authenticator authed policy.
    if state.policy.open_reads() {
        return next.run(request).await;
    }

    // 2. A bearer credential, or a 401 pointing at the metadata document.
    let Some(token) = bearer_token(request.headers()) else {
        state.count_rejection();
        return unauthorized(&state, "missing `Authorization: Bearer <token>` header");
    };

    // 3. Authenticate: the hub-minted floor (by prefix), else an IdP JWT.
    let principal = match authenticate(&state.policy, token).await {
        Ok(principal) => principal,
        Err(AuthOutcome::Reject(reason)) => {
            state.count_rejection();
            return unauthorized(&state, &reason);
        }
        Err(AuthOutcome::Unavailable(detail)) => {
            // The IdP could not be reached: a 503, not a 401 and not a silent allow — the
            // hub could not judge the token, so it must not call a possibly-valid one forged
            // and must not let it through (invariant #1, #6). Not counted as a rejection: no
            // token was judged invalid.
            tracing::warn!(%detail, "read auth could not verify a token (IdP/JWKS unavailable)");
            return problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "cannot verify the token right now (identity provider unreachable); retry",
            );
        }
    };

    // 4. Authorize: the route's required scope against the principal's grant.
    if !required.is_satisfied_by(principal.scopes()) {
        state.count_rejection();
        tracing::warn!(
            principal = principal.kind().label(),
            required = required.0.as_str(),
            "read auth: authenticated principal lacks the required scope"
        );
        return problem(
            StatusCode::FORBIDDEN,
            &format!(
                "authenticated, but this token lacks the `{}` scope required for this surface",
                required.0.as_str()
            ),
        );
    }

    tracing::debug!(
        principal = principal.kind().label(),
        "read auth: authorized"
    );
    let mut request = request;
    request.extensions_mut().insert(principal);
    next.run(request).await
}

/// The internal outcome of authenticating a presented credential.
///
/// Separated from the `Ok(ReadPrincipal)` so the layer maps a rejection to a `401` and an
/// unavailability to a `503` — the load-bearing split of invariant #1: a broken verifier
/// never manufactures a rejection.
enum AuthOutcome {
    /// The credential is invalid; the reason is named for the `401` and the log.
    Reject(String),
    /// The IdP could not be reached; a `503`, not a verdict on the credential.
    Unavailable(String),
}

/// Authenticate `presented` against the policy: the hub-minted floor first (by prefix),
/// else an IdP JWT.
///
/// A `claimhub_`-prefixed credential is only ever checked against the scoped-token floor —
/// never sent to the JWKS verifier — so a hub token can never trigger an outbound JWKS
/// fetch, and a JWT is never hashed against the token floor. A hub-token prefix that
/// matches no configured token is a rejection (not an IdP fallthrough), so a revoked or
/// typo'd hub token fails loudly rather than being reinterpreted as a JWT.
async fn authenticate(
    policy: &ReadAuthPolicy,
    presented: &str,
) -> Result<ReadPrincipal, AuthOutcome> {
    if presented.starts_with(HUB_TOKEN_PREFIX) {
        return policy.match_scoped_token(presented).ok_or_else(|| {
            AuthOutcome::Reject("bearer token is not a recognized hub token".into())
        });
    }
    let Some(verifier) = policy.verifier() else {
        // No IdP configured and the credential is not a hub token: nothing can verify it.
        return Err(AuthOutcome::Reject(
            "bearer token is neither a recognized hub token nor verifiable (no IdP configured)"
                .into(),
        ));
    };
    match verifier.verify(presented).await {
        Ok(scopes) => Ok(ReadPrincipal::from_idp(scopes)),
        // `reason()` is the client-facing message; it never echoes the token or a secret.
        Err(ReadVerifyError::Reject(reject)) => Err(AuthOutcome::Reject(reject.reason())),
        Err(ReadVerifyError::Unavailable(detail)) => Err(AuthOutcome::Unavailable(detail)),
    }
}

/// Extract the bearer token from an `Authorization` header, if present and well-formed.
///
/// Returns the token with the `Bearer ` scheme stripped and trimmed, or `None` when the
/// header is absent, not UTF-8, or not a bearer credential. The token is not validated here.
fn bearer_token(headers: &header::HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?;
    let value = value.to_str().ok()?;
    value.strip_prefix(BEARER_PREFIX).map(str::trim)
}

/// A `401` carrying the RFC 9728 `WWW-Authenticate` pointer to the metadata document.
///
/// Every unauthenticated read gets this, so a client always learns the `Bearer` scheme and
/// the metadata URL to discover the authorization server from — the "done when" bar's
/// "401 with the metadata pointer".
fn unauthorized(state: &AuthLayerState, reason: &str) -> Response {
    let challenge = ResourceMetadata::www_authenticate(&state.metadata_url);
    problem_with_headers(
        StatusCode::UNAUTHORIZED,
        reason,
        &[(header::WWW_AUTHENTICATE, challenge)],
    )
}

/// A plain `{ "error": … }` problem response, the shared hub error shape.
fn problem(status: StatusCode, reason: &str) -> Response {
    crate::http::problem(status, reason)
}

/// Wrap `router` with the read-auth layer over `state`, enforcing `required`.
///
/// Applies the [`authorize`] middleware to `router` as one outer tower layer, so every
/// route in `router` is covered uniformly and a route added to `router` later inherits the
/// gate for free. Binding `state` and `required` here means a future act-route group can be
/// wrapped with `protect(act_routes, state, RequiredScope(Scope::Act))` and inherit the
/// identical authentication path with a narrower authorization — the scope model shipping
/// ahead of the act endpoints (the item's requirement 4).
///
/// `S` is the router's state type; the middleware carries its own `state` in the closure,
/// so it composes with a router that still awaits `with_state`.
pub fn protect<S>(
    router: axum::Router<S>,
    state: Arc<AuthLayerState>,
    required: RequiredScope,
) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.layer(axum::middleware::from_fn(
        move |request: Request, next: Next| {
            let state = state.clone();
            async move { authorize(state, required, request, next).await }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token;

    #[test]
    fn the_hub_token_prefix_matches_the_token_module() {
        // The prefix the layer routes on must match the prefix `mint` stamps, or a real hub
        // token would be sent to the JWKS verifier. A minted token starts with it.
        let minted = token::mint().unwrap();
        assert!(minted.raw().starts_with(HUB_TOKEN_PREFIX));
    }

    #[test]
    fn bearer_token_strips_the_scheme() {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer claimhub_abc".parse().unwrap(),
        );
        assert_eq!(bearer_token(&headers), Some("claimhub_abc"));
    }

    #[test]
    fn bearer_token_rejects_a_non_bearer_or_absent_header() {
        let empty = header::HeaderMap::new();
        assert_eq!(bearer_token(&empty), None);
        let mut basic = header::HeaderMap::new();
        basic.insert(header::AUTHORIZATION, "Basic Zm9vOmJhcg==".parse().unwrap());
        assert_eq!(bearer_token(&basic), None);
    }
}
