//! Read authentication: the OAuth 2.1 resource-server verifier and the resolved policy.
//!
//! The hub's read surfaces — the JSON API (`/api`), the UI and its twins (`/ui`,
//! `/llms.txt`), and the MCP (`/mcp`) — are protected by a bearer credential
//! (HUB-IMPLEMENTATION.md §1.11, §4.5 decision 5). This module holds the two credential
//! kinds and the policy that resolves how a hub authenticates a read:
//!
//! 1. **IdP bearer JWTs** ([`ReadTokenVerifier`]): the hub is an OAuth 2.1 resource server
//!    validating a Bearer JWT against a configured issuer's JWKS — the customer's IdP is
//!    the authorization server. This reuses the exact `jsonwebtoken`-plus-JWKS machinery
//!    the ingest gate uses ([`crate::oidc`]), with a different trust anchor (the read
//!    issuer/audience) and a different payload (scopes, not a producer). RS256 is pinned;
//!    `iss`/`aud`/`exp` are required-present; a bad token is a counted [`ReadReject`], an
//!    unreachable IdP is a [`ReadVerifyError::Unavailable`] (a 503, never a silent allow).
//!
//! 2. **Hub-minted scoped tokens** ([`crate::token`]): the IdP-less floor, so a hub with no
//!    external IdP still authenticates reads rather than serving them open. Stored hashed;
//!    matched in constant time.
//!
//! The [`ReadAuthPolicy`] is the resolved decision the auth layer enforces. **Secure by
//! default is enforced at construction** ([`ReadAuthPolicy::resolve`]): a hub that is not
//! explicitly opened, has no issuer, and has no scoped token cannot authenticate anyone, so
//! it is a *loud boot error*, never a hub that silently serves open reads. The only way to
//! open reads is the explicit `open_reads = true` opt-in.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;

use crate::oidc::{JwksCache, JwksSource, OidcError};
use crate::scope::GrantedScopes;
use crate::token::ScopedToken;

/// The verified identity behind an authenticated read: its scopes and how it authenticated.
///
/// The auth layer produces one of these on a successful authentication and hands its
/// [`scopes`](ReadPrincipal::scopes) to the scope check. It records the credential *kind*
/// only for diagnostics — the authorization decision is the scopes, never the kind.
#[derive(Debug, Clone)]
pub struct ReadPrincipal {
    /// The scopes this principal was granted — from a hub-minted token's config entry or an
    /// IdP JWT's `scope` claim. The route's [`RequiredScope`](crate::scope::RequiredScope)
    /// is checked against exactly these.
    scopes: GrantedScopes,
    /// How this principal authenticated, for a log line — never a secret, never the token.
    kind: PrincipalKind,
}

impl ReadPrincipal {
    /// A principal that authenticated with a hub-minted scoped token.
    #[must_use]
    pub fn from_scoped_token(token: &ScopedToken) -> Self {
        Self {
            scopes: token.granted_scopes(),
            kind: PrincipalKind::ScopedToken,
        }
    }

    /// A principal that authenticated with an IdP-issued bearer JWT.
    #[must_use]
    pub fn from_idp(scopes: GrantedScopes) -> Self {
        Self {
            scopes,
            kind: PrincipalKind::Idp,
        }
    }

    /// The principal's granted scopes, for the route's scope check.
    #[must_use]
    pub fn scopes(&self) -> &GrantedScopes {
        &self.scopes
    }

    /// How the principal authenticated, for diagnostics.
    #[must_use]
    pub fn kind(&self) -> PrincipalKind {
        self.kind
    }
}

/// How a read principal authenticated — for a log line only, never an authorization input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrincipalKind {
    /// A hub-minted scoped token (the IdP-less floor).
    ScopedToken,
    /// An IdP-issued OAuth 2.1 bearer JWT.
    Idp,
}

impl PrincipalKind {
    /// A short label for a diagnostic span — no secret.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            PrincipalKind::ScopedToken => "scoped-token",
            PrincipalKind::Idp => "idp-jwt",
        }
    }
}

/// The claims the read-JWT verifier reads from an IdP bearer token.
///
/// Only `scope` is read from the payload here; `exp`/`iss`/`aud` are validated by
/// `jsonwebtoken` itself (required-present via `set_required_spec_claims`, exactly as the
/// ingest gate seals them), so they need no struct field. `scope` is the OAuth 2.1
/// space-delimited grant string; absent, the principal holds no scopes and so satisfies no
/// route (a `403`, never an accidental grant). `aud` is deliberately *not* typed (a JWT's
/// `aud` may be a string or an array; typing it would risk rejecting a valid array-`aud`),
/// so `set_required_spec_claims` validates its presence and value without constraining its
/// shape — the same reasoning as the ingest gate's claims.
#[derive(Debug, Clone, Deserialize)]
struct ReadClaims {
    /// The OAuth 2.1 `scope` string (space-delimited), if present. Absent means no scopes.
    #[serde(default)]
    scope: Option<String>,
}

/// The read-side OAuth 2.1 resource-server verifier: a Bearer JWT against a configured
/// issuer's JWKS.
///
/// Structurally the ingest gate's [`OidcVerifier`](crate::oidc::OidcVerifier) with a read
/// payload: same `JwksCache`, same RS256 pin, same required-present `iss`/`aud`/`exp`, same
/// Reject-versus-Unavailable split. What differs is the trust anchor (the read issuer and
/// audience, the customer's IdP) and the output (a [`GrantedScopes`] from the token's
/// `scope` claim, not a producer identity). Generic over the [`JwksSource`] so production
/// wires the HTTP source and tests inject a fixed key set — no network in tests.
pub struct ReadTokenVerifier<S: JwksSource> {
    issuer: String,
    audience: String,
    cache: JwksCache<S>,
}

impl<S: JwksSource> ReadTokenVerifier<S> {
    /// A verifier trusting `issuer`/`audience`, resolving keys through `source`'s JWKS.
    pub fn new(issuer: impl Into<String>, audience: impl Into<String>, source: S) -> Self {
        Self::with_cache(issuer, audience, JwksCache::new(source))
    }

    /// A verifier over a caller-built [`JwksCache`], so a test can supply a cache with a
    /// controllable clock and debounce window (the JWKS-refresh rate limit) deterministically.
    pub fn with_cache(
        issuer: impl Into<String>,
        audience: impl Into<String>,
        cache: JwksCache<S>,
    ) -> Self {
        Self {
            issuer: issuer.into(),
            audience: audience.into(),
            cache,
        }
    }

    /// Verify one IdP bearer JWT, returning the granted scopes or a typed failure.
    ///
    /// The check chain mirrors the ingest gate: resolve the `kid` (refreshing the JWKS at
    /// most once per debounce window on a miss — the amplification cap), pin RS256, require
    /// `iss`/`aud`/`exp` present *and* correct, verify the signature, then read the `scope`
    /// claim into a [`GrantedScopes`]. RS256 is pinned by constructing the `Validation` with
    /// exactly `Algorithm::RS256`, so a token asserting `alg: none` or an HS* algorithm
    /// (the classic algorithm-confusion attacks) is rejected before any key is applied.
    ///
    /// # Errors
    ///
    /// A [`ReadVerifyError`] separating a **rejection** ([`ReadVerifyError::Reject`], a `401`
    /// the layer counts) from an **infrastructure fault** ([`ReadVerifyError::Unavailable`],
    /// a `503` — the JWKS was unreachable, so a possibly-valid token is not called forged).
    pub async fn verify(&self, token: &str) -> Result<GrantedScopes, ReadVerifyError> {
        let header = decode_header(token).map_err(|e| {
            ReadVerifyError::Reject(ReadReject::Malformed(format!("token header: {e}")))
        })?;
        // Pin the algorithm from the trusted `Validation`, never from the attacker-supplied
        // header. `decode` below rejects a token whose header `alg` is not RS256, so
        // `alg: none` and RS/HS confusion cannot slip a forged or unsigned token through.
        let kid = header.kid.ok_or_else(|| {
            ReadVerifyError::Reject(ReadReject::Malformed("token has no `kid`".into()))
        })?;

        let key = match self.cache.decoding_key(&kid).await {
            Ok(key) => key,
            Err(OidcError::UnknownKey(kid)) => {
                return Err(ReadVerifyError::Reject(ReadReject::UnknownKey(kid)))
            }
            Err(OidcError::Key(reason)) => {
                return Err(ReadVerifyError::Reject(ReadReject::Malformed(reason)))
            }
            Err(OidcError::Fetch(reason)) => return Err(ReadVerifyError::Unavailable(reason)),
        };

        let scopes = verify_claims(token, &key, &self.issuer, &self.audience)?;
        Ok(scopes)
    }
}

/// Validate a token's signature and standard claims against `key`, returning its scopes.
///
/// Split out so the RS256 pin and the required-claims sealing are one testable unit,
/// independent of the JWKS cache. RS256 is the only accepted algorithm; `iss`, `aud`, and
/// `exp` are required-present (a token omitting any is a `MissingRequiredClaim` rejection,
/// not a hollow pass); `exp` is checked with `jsonwebtoken`'s small leeway.
fn verify_claims(
    token: &str,
    key: &DecodingKey,
    issuer: &str,
    audience: &str,
) -> Result<GrantedScopes, ReadVerifyError> {
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[audience]);
    // Require `exp`/`iss`/`aud` present, so pinning is never hollow — a token that OMITS
    // `aud` must not sail past `set_audience` (the exact seal the ingest gate applies).
    validation.set_required_spec_claims(&["exp", "iss", "aud"]);
    let data = decode::<ReadClaims>(token, key, &validation)
        .map_err(|e| ReadVerifyError::Reject(reject_from_jwt(&e)))?;
    Ok(data
        .claims
        .scope
        .as_deref()
        .map(GrantedScopes::from_scope_claim)
        .unwrap_or_default())
}

/// Map a `jsonwebtoken` decode error to the read-side rejection it represents.
///
/// The same mapping the ingest gate uses, so the two auth surfaces name the same failures
/// the same way. A token that omits `iss`/`aud`/`exp` surfaces as `MissingRequiredClaim`
/// (because the validation marks all three required), a named rejection — never a silent
/// pass.
fn reject_from_jwt(error: &jsonwebtoken::errors::Error) -> ReadReject {
    use jsonwebtoken::errors::ErrorKind;
    match error.kind() {
        ErrorKind::ExpiredSignature => ReadReject::Expired,
        ErrorKind::InvalidAudience => ReadReject::WrongAudience,
        ErrorKind::InvalidIssuer => ReadReject::WrongIssuer,
        ErrorKind::InvalidSignature => ReadReject::BadSignature,
        ErrorKind::MissingRequiredClaim(claim) => {
            ReadReject::Malformed(format!("token is missing the required `{claim}` claim"))
        }
        other => ReadReject::Malformed(format!("token failed validation: {other:?}")),
    }
}

/// The outcome of a failed read-JWT verification: a rejection, or an unavailability.
///
/// The split is load-bearing (invariant #1, #6): a *rejection* is a token that is
/// definitely invalid — the layer answers `401` and counts it. An *unavailable* is the hub
/// being unable to verify right now (JWKS unreachable); answering `401` there would call a
/// possibly-valid token forged, so the layer answers `503` and the client retries — a
/// broken verifier never manufactures a rejection or a silent allow.
#[derive(Debug)]
pub enum ReadVerifyError {
    /// The token is invalid; the specific reason is named for the `401`.
    Reject(ReadReject),
    /// Verification could not be performed (JWKS unreachable); a transient fault, a `503`.
    Unavailable(String),
}

/// Why a read JWT was rejected, each mapping to a `401` reason. Never coerced to accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadReject {
    /// No signing key in the issuer's set (even after a refresh) matches the token's `kid`.
    UnknownKey(String),
    /// The signature did not verify against the resolved key.
    BadSignature,
    /// The token's `exp` is in the past.
    Expired,
    /// The token's `aud` is not this hub's configured read audience.
    WrongAudience,
    /// The token's `iss` is not the configured read issuer.
    WrongIssuer,
    /// The token could not be parsed or was missing a required piece (a `kid`, a claim).
    Malformed(String),
}

impl ReadReject {
    /// A one-line, client-facing reason naming what failed, for the `401` body and the log.
    /// Never echoes the token or a secret.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            ReadReject::UnknownKey(kid) => {
                format!("token signing key `{kid}` is not in the issuer's published JWKS")
            }
            ReadReject::BadSignature => "token signature did not verify".to_owned(),
            ReadReject::Expired => "token has expired".to_owned(),
            ReadReject::WrongAudience => {
                "token audience does not match this hub's configured read audience".to_owned()
            }
            ReadReject::WrongIssuer => "token issuer is not the configured read issuer".to_owned(),
            ReadReject::Malformed(detail) => format!("malformed bearer token: {detail}"),
        }
    }
}

/// A `dyn`-compatible read-JWT verifier, so the resolved policy holds *a* verifier without
/// naming the [`JwksSource`] behind it — production wires the HTTP source, a test wires an
/// injected key set, both as `Arc<dyn ReadVerifier>`.
pub trait ReadVerifier: Send + Sync {
    /// Verify one IdP bearer JWT; see [`ReadTokenVerifier::verify`] for the contract.
    fn verify<'a>(
        &'a self,
        token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<GrantedScopes, ReadVerifyError>> + Send + 'a>>;
}

impl<S: JwksSource + 'static> ReadVerifier for ReadTokenVerifier<S> {
    fn verify<'a>(
        &'a self,
        token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<GrantedScopes, ReadVerifyError>> + Send + 'a>> {
        Box::pin(ReadTokenVerifier::verify(self, token))
    }
}

/// The shared, source-erased read verifier the policy holds.
pub type SharedReadVerifier = Arc<dyn ReadVerifier>;

/// The resolved read-auth policy the auth layer enforces.
///
/// Built once at boot by [`resolve`](ReadAuthPolicy::resolve) from the config, so the
/// dangerous default (a hub open by accident) is impossible to construct: `resolve` refuses
/// a policy that authenticates no one and is not explicitly opened. The layer reads three
/// things from it — whether reads are open, the IdP verifier (if any), and the scoped-token
/// floor (if any) — and never re-derives the secure-default decision at request time.
pub struct ReadAuthPolicy {
    /// When `true`, reads are open — no credential required. The explicit private-network
    /// opt-in (`open_reads = true`); `false` is authed-everything.
    open_reads: bool,
    /// The IdP bearer-JWT verifier, `None` when no read issuer is configured.
    verifier: Option<SharedReadVerifier>,
    /// The hub-minted scoped tokens (the IdP-less floor), empty when none are configured.
    tokens: Vec<ScopedToken>,
}

/// Why resolving a read-auth policy failed at boot — always a misconfiguration that would
/// otherwise open reads by accident or stand up a gate that can never admit anyone.
#[derive(Debug, PartialEq, Eq)]
pub enum PolicyError {
    /// The dangerous regression: authed-everything is in force (reads not opened) but the
    /// hub has no way to authenticate a read — no issuer, no scoped token. Rather than
    /// silently serve open reads (or reject every read forever), boot fails loudly and
    /// names the fix.
    NoAuthenticator,
    /// A configured scoped token is malformed (no scopes, or a bad hash) — it could never
    /// admit anyone, so it is a loud boot error, not a silent dead entry (invariant #6).
    MalformedToken(String),
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyError::NoAuthenticator => write!(
                f,
                "read auth is authed-everything (the secure default) but nothing can \
                 authenticate a read: configure a `[read_auth.issuer]` (an OAuth 2.1 IdP), \
                 add a hub-minted `[[read_auth.tokens]]` entry (run `claim-hub mint-token`), \
                 or — only for a trusted private network — set `[read_auth] open_reads = true` \
                 to serve reads with no authentication"
            ),
            PolicyError::MalformedToken(name) => write!(
                f,
                "read-auth token `{name}` is malformed: it needs at least one scope and a \
                 `hash` of the form `sha256:<64 hex>` (run `claim-hub mint-token` to produce one)"
            ),
        }
    }
}

impl std::error::Error for PolicyError {}

impl ReadAuthPolicy {
    /// Resolve a policy from its inputs, enforcing secure-by-default at construction.
    ///
    /// The one place the secure default is decided (§4.5 decision 5). The rule:
    ///
    /// - `open_reads == true` → reads are open. The explicit opt-in; any issuer/tokens are
    ///   still accepted (a client may present a credential) but none is required.
    /// - `open_reads == false` (the default) → **at least one authenticator must exist**: a
    ///   read issuer, or one well-formed scoped token. With neither, this returns
    ///   [`PolicyError::NoAuthenticator`] — a loud boot failure, never a hub that quietly
    ///   serves open reads. This is the invariant that makes "open by accident" impossible.
    ///
    /// Every configured token is validated here; a malformed one (no scopes, or a bad hash)
    /// is [`PolicyError::MalformedToken`], not a silently dropped entry.
    ///
    /// # Errors
    ///
    /// [`PolicyError::NoAuthenticator`] when authed-everything is in force with no
    /// authenticator; [`PolicyError::MalformedToken`] when a configured token cannot admit.
    pub fn resolve(
        open_reads: bool,
        verifier: Option<SharedReadVerifier>,
        tokens: Vec<ScopedToken>,
    ) -> Result<Self, PolicyError> {
        for token in &tokens {
            if !token.is_well_formed() {
                return Err(PolicyError::MalformedToken(token.name.clone()));
            }
        }
        if !open_reads && verifier.is_none() && tokens.is_empty() {
            return Err(PolicyError::NoAuthenticator);
        }
        Ok(Self {
            open_reads,
            verifier,
            tokens,
        })
    }

    /// Whether reads are open (the explicit opt-in). When `true`, the auth layer serves a
    /// protected route with no credential required.
    #[must_use]
    pub fn open_reads(&self) -> bool {
        self.open_reads
    }

    /// The IdP bearer-JWT verifier, if a read issuer is configured.
    #[must_use]
    pub fn verifier(&self) -> Option<&SharedReadVerifier> {
        self.verifier.as_ref()
    }

    /// Find a hub-minted token matching `presented`, by constant-time hash comparison.
    ///
    /// Returns the matching token's principal, or `None`. Every configured token is checked
    /// (no early return on the first hash mismatch beyond the per-token constant-time
    /// compare), so the *number* of configured tokens is not leaked by which one matched;
    /// the per-comparison time is already constant via [`ScopedToken::matches`].
    #[must_use]
    pub fn match_scoped_token(&self, presented: &str) -> Option<ReadPrincipal> {
        let mut found: Option<&ScopedToken> = None;
        for token in &self.tokens {
            if token.matches(presented) {
                found = Some(token);
            }
        }
        found.map(ReadPrincipal::from_scoped_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::Scope;
    use crate::token;

    fn scoped(name: &str, raw: &str, scopes: Vec<Scope>) -> ScopedToken {
        ScopedToken {
            name: name.into(),
            scopes,
            hash: token::hash_for_config(raw),
        }
    }

    /// Resolve and expect success, without requiring `ReadAuthPolicy: Debug` (the policy
    /// holds a `dyn` verifier, which is not `Debug`, so `.unwrap()` is unavailable).
    fn resolve_ok(
        open_reads: bool,
        verifier: Option<SharedReadVerifier>,
        tokens: Vec<ScopedToken>,
    ) -> ReadAuthPolicy {
        match ReadAuthPolicy::resolve(open_reads, verifier, tokens) {
            Ok(policy) => policy,
            Err(e) => panic!("expected a resolved policy, got error: {e}"),
        }
    }

    /// Resolve and expect a specific error.
    fn resolve_err(
        open_reads: bool,
        verifier: Option<SharedReadVerifier>,
        tokens: Vec<ScopedToken>,
    ) -> PolicyError {
        match ReadAuthPolicy::resolve(open_reads, verifier, tokens) {
            Ok(_) => panic!("expected a resolution error, got a policy"),
            Err(e) => e,
        }
    }

    #[test]
    fn authed_default_with_no_authenticator_is_a_loud_boot_error() {
        // The dangerous regression made impossible: authed-everything (open_reads=false)
        // with no issuer and no token cannot resolve — it is a NoAuthenticator boot error,
        // never a hub that silently serves open reads.
        assert_eq!(
            resolve_err(false, None, vec![]),
            PolicyError::NoAuthenticator
        );
    }

    #[test]
    fn authed_default_with_a_scoped_token_resolves() {
        let policy = resolve_ok(
            false,
            None,
            vec![scoped("ci", "claimhub_x", vec![Scope::Read])],
        );
        assert!(!policy.open_reads());
    }

    #[test]
    fn open_reads_opt_in_resolves_with_no_authenticator() {
        // The explicit private-network opt-in is the ONLY way to a no-authenticator policy.
        let policy = resolve_ok(true, None, vec![]);
        assert!(policy.open_reads());
    }

    #[test]
    fn a_malformed_token_fails_resolution_by_name() {
        let bad = ScopedToken {
            name: "broken".into(),
            scopes: vec![Scope::Read],
            hash: "not-a-hash".into(),
        };
        assert_eq!(
            resolve_err(false, None, vec![bad]),
            PolicyError::MalformedToken("broken".into())
        );
    }

    #[test]
    fn a_scopeless_token_fails_resolution() {
        let scopeless = scoped("empty", "claimhub_y", vec![]);
        assert_eq!(
            resolve_err(false, None, vec![scopeless]),
            PolicyError::MalformedToken("empty".into())
        );
    }

    #[test]
    fn a_matching_token_yields_its_principal_and_scopes() {
        let policy = resolve_ok(
            false,
            None,
            vec![scoped("ci", "claimhub_secret", vec![Scope::Read])],
        );
        let principal = policy
            .match_scoped_token("claimhub_secret")
            .expect("the right token matches");
        assert!(principal.scopes().contains(Scope::Read));
        assert_eq!(principal.kind(), PrincipalKind::ScopedToken);
        assert!(
            policy.match_scoped_token("claimhub_wrong").is_none(),
            "a wrong token matches nothing"
        );
    }

    #[test]
    fn reject_reasons_name_what_failed_and_carry_no_token() {
        assert!(ReadReject::Expired.reason().contains("expired"));
        assert!(ReadReject::WrongAudience.reason().contains("audience"));
        assert!(ReadReject::WrongIssuer.reason().contains("issuer"));
        assert!(ReadReject::BadSignature.reason().contains("signature"));
    }

    /// A JWKS source that never returns keys — enough to reach the header-parsing branches
    /// of `verify` without a real key set (those reject before any key is resolved).
    struct EmptySource;
    impl JwksSource for EmptySource {
        async fn fetch(&self) -> Result<jsonwebtoken::jwk::JwkSet, OidcError> {
            Ok(serde_json::from_value(serde_json::json!({ "keys": [] })).unwrap())
        }
    }

    #[tokio::test]
    async fn a_non_jwt_string_is_a_malformed_rejection_not_a_panic() {
        // A garbage bearer that is not a JWT at all is rejected at header parsing — a
        // Malformed reject, never a panic or an accidental pass.
        let verifier = ReadTokenVerifier::new("iss", "aud", EmptySource);
        match verifier.verify("not-a-jwt").await {
            Err(ReadVerifyError::Reject(ReadReject::Malformed(_))) => {}
            other => panic!("expected a Malformed rejection, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_token_with_no_kid_is_rejected() {
        // A JWT header with no `kid`: there is no key to select, so it is a Malformed reject.
        // Hand-built header.payload.sig with `alg: RS256` and no `kid`.
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = b64.encode(serde_json::json!({ "alg": "RS256", "typ": "JWT" }).to_string());
        let payload = b64.encode(serde_json::json!({ "iss": "iss" }).to_string());
        let token = format!("{header}.{payload}.sig");
        let verifier = ReadTokenVerifier::new("iss", "aud", EmptySource);
        match verifier.verify(&token).await {
            Err(ReadVerifyError::Reject(ReadReject::Malformed(reason))) => {
                assert!(reason.contains("kid"), "names the missing kid: {reason}");
            }
            other => panic!("expected a Malformed(kid) rejection, got {other:?}"),
        }
    }
}
