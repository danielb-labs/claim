//! Read-authentication integration tests: the whole layer over a real app, no network.
//!
//! These are the hub-13 "done when" bar, driven end to end through the assembled axum app
//! via `tower::ServiceExt::oneshot` — no bound port, no real IdP. The JWKS is **injected**
//! (the same `TestJwksSource` the ingest tests use, publishing a fixture RSA key), so the
//! IdP-JWT path runs its real RS256 signature check against a key the test controls. Every
//! token's own `exp` is anchored to real time (a JWT's expiry is not injectable), which is
//! the one real-time input the ingest harness already documents; nothing else reaches wall
//! clock.
//!
//! Coverage, matched to the item's DONE-WHEN list:
//! - a missing token → 401 with the RFC 9728 metadata pointer;
//! - a bad-signature / expired / wrong-aud / wrong-iss / unknown-kid token → 401 (counted);
//! - an unreachable IdP → 503 (never a silent allow);
//! - a valid IdP token → 200; a valid hub-minted scoped token → 200;
//! - a scoped token lacking the scope → 403;
//! - the hashed-at-rest property (a raw token never persisted; the stored form is a hash);
//! - the explicit open-reads opt-in → 200 unauthenticated, and without it → 401.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use claim_core::Timestamp;
use claim_hub::app::{AppState, ReadClock};
use claim_hub::authlayer::AuthLayerState;
use claim_hub::metadata::{ResourceMetadata, METADATA_PATH};
use claim_hub::oidc::{JwksCache, JwksSource};
use claim_hub::readauth::{ReadAuthPolicy, ReadTokenVerifier, SharedReadVerifier};
use claim_hub::scope::Scope;
use claim_hub::token::{self, ScopedToken};
use claim_hub_store::SqliteStore;
use http_body_util::BodyExt;
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::json;
use tower::ServiceExt;

use common::{TestClock, TestJwksSource, TEST_KID};

/// The read IdP the tests trust — a customer IdP, distinct from the ingest GitHub issuer.
const READ_ISSUER: &str = "https://idp.acme.example";
/// The hub's own identifier a read token's `aud` must equal (the RFC 9728 resource).
const READ_AUDIENCE: &str = "https://hub.acme.example";

/// The fixture RSA private key the `TestJwksSource` publishes the public half of. Signing a
/// read JWT with it makes the RS256 signature verify against the injected JWKS.
const SIGNING_KEY_PEM: &str = include_str!("fixtures/oidc_signing_key.pem");
/// A different key, for the forged-signature case: a token signed with it but presented
/// under the published kid fails the signature check.
const WRONG_KEY_PEM: &str = include_str!("fixtures/oidc_wrong_key.pem");

/// A representative read JWT's claims, mirroring what a customer IdP issues.
struct ReadToken {
    issuer: String,
    audience: String,
    scope: Option<String>,
    ttl_secs: i64,
}

impl ReadToken {
    /// A valid read token: the trusted issuer and audience, the `read` scope, an hour of life.
    fn valid() -> Self {
        Self {
            issuer: READ_ISSUER.to_owned(),
            audience: READ_AUDIENCE.to_owned(),
            scope: Some("read".to_owned()),
            ttl_secs: 3600,
        }
    }
}

/// Sign a read token's claims with `key_pem` under the published kid, RS256.
///
/// `exp`/`iat` are anchored to real `now` (a JWT's expiry is validated against the system
/// clock inside `jsonwebtoken`, which no parameter overrides); `ttl_secs` negative signs an
/// already-expired token.
fn sign_read(token: &ReadToken, key_pem: &str) -> String {
    let iat = Timestamp::now().as_second();
    let mut body = json!({
        "iss": token.issuer,
        "aud": token.audience,
        "iat": iat,
        "exp": iat + token.ttl_secs,
        "sub": "user-123",
    });
    if let Some(scope) = &token.scope {
        body["scope"] = json!(scope);
    }
    let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(TEST_KID.to_owned());
    let key = EncodingKey::from_rsa_pem(key_pem.as_bytes()).expect("valid RSA PEM");
    encode(&header, &body, &key).expect("sign read token")
}

/// The read-token verifier over an injected JWKS, with a controllable JWKS-refresh clock.
/// Generic over the source so the "IdP unreachable" case injects a failing source through
/// the same path.
fn read_verifier<S: JwksSource + 'static>(source: S, jwks_clock: &TestClock) -> SharedReadVerifier {
    let cache = JwksCache::with_debounce(source, common::TEST_DEBOUNCE_MS, jwks_clock.monotonic());
    Arc::new(ReadTokenVerifier::with_cache(
        READ_ISSUER,
        READ_AUDIENCE,
        cache,
    ))
}

/// The RFC 9728 metadata + layer state for a resolved policy, pointing at the well-known path.
fn auth_state(policy: ReadAuthPolicy) -> Arc<AuthLayerState> {
    let issuer = if policy_has_verifier(&policy) {
        Some(READ_ISSUER.to_owned())
    } else {
        None
    };
    let metadata = ResourceMetadata::new(READ_AUDIENCE, issuer);
    Arc::new(AuthLayerState::new(policy, metadata, METADATA_PATH))
}

/// Whether the policy carries an IdP verifier — used only to shape the metadata document in
/// the test helper (the policy does not expose this beyond `verifier()`).
fn policy_has_verifier(policy: &ReadAuthPolicy) -> bool {
    policy.verifier().is_some()
}

/// Build an app over a fresh in-memory store with the given resolved read-auth policy, plus
/// one registered claim so `/api/claims` has something to return on a 200.
async fn app_with_policy(policy: ReadAuthPolicy) -> (axum::Router, Arc<AuthLayerState>) {
    let store = SqliteStore::open_in_memory().await.unwrap();
    // A stable read clock so the derivation is deterministic (no wall clock in the read path).
    let read_clock: ReadClock = Arc::new(|| "2026-07-19T00:00:00Z".parse::<Timestamp>().unwrap());
    let auth = auth_state(policy);
    let state = AppState::new(store, None)
        .with_read_clock(read_clock)
        .with_read_auth(auth.clone());
    (claim_hub::build_app(state), auth)
}

/// GET a path with an optional bearer token, returning the whole response (status + headers).
async fn get(app: &axum::Router, path: &str, token: Option<&str>) -> axum::response::Response {
    let mut builder = Request::builder().uri(path);
    if let Some(token) = token {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    app.clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

/// A hub-minted scoped token config entry, from a raw token and its scopes.
fn scoped(name: &str, raw: &str, scopes: Vec<Scope>) -> ScopedToken {
    ScopedToken {
        name: name.into(),
        scopes,
        hash: token::hash_for_config(raw),
    }
}

/// A policy with an IdP verifier and no tokens.
fn idp_policy(source: TestJwksSource, jwks_clock: &TestClock) -> ReadAuthPolicy {
    ReadAuthPolicy::resolve(false, Some(read_verifier(source, jwks_clock)), vec![])
        .expect("a policy with an issuer resolves")
}

// A protected read route used across the tests. `/api/claims` is representative of the
// whole protected group; the layer wraps the group, so covering one route proves the gate.
const PROTECTED: &str = "/api/claims";

#[tokio::test]
async fn a_missing_token_is_401_with_the_metadata_pointer() {
    let clock = TestClock::default();
    let (app, _auth) =
        app_with_policy(idp_policy(TestJwksSource::with_signing_key(), &clock)).await;
    let response = get(&app, PROTECTED, None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let challenge = response
        .headers()
        .get(axum::http::header::WWW_AUTHENTICATE)
        .expect("a 401 carries a WWW-Authenticate challenge")
        .to_str()
        .unwrap();
    assert!(
        challenge.contains("Bearer") && challenge.contains(METADATA_PATH),
        "the challenge points at the metadata document: {challenge}"
    );
}

#[tokio::test]
async fn the_metadata_document_is_served_unauthenticated() {
    // RFC 9728: the discovery document must be readable before a client can authenticate.
    let clock = TestClock::default();
    let (app, _auth) =
        app_with_policy(idp_policy(TestJwksSource::with_signing_key(), &clock)).await;
    let response = get(&app, METADATA_PATH, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["resource"], READ_AUDIENCE);
    assert_eq!(body["authorization_servers"][0], READ_ISSUER);
    assert_eq!(body["scopes_supported"][0], "read");
}

#[tokio::test]
async fn a_valid_idp_token_is_200() {
    let clock = TestClock::default();
    let (app, _auth) =
        app_with_policy(idp_policy(TestJwksSource::with_signing_key(), &clock)).await;
    let token = sign_read(&ReadToken::valid(), SIGNING_KEY_PEM);
    let response = get(&app, PROTECTED, Some(&token)).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_bad_signature_token_is_a_counted_401() {
    let clock = TestClock::default();
    let (app, auth) = app_with_policy(idp_policy(TestJwksSource::with_signing_key(), &clock)).await;
    // Signed with the wrong key but presented under the published kid: the signature fails.
    let token = sign_read(&ReadToken::valid(), WRONG_KEY_PEM);
    let response = get(&app, PROTECTED, Some(&token)).await;
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "a forgery is a 401, not a 503"
    );
    assert_eq!(auth.rejection_count(), 1, "a bad token is counted");
}

#[tokio::test]
async fn an_expired_token_is_a_counted_401() {
    let clock = TestClock::default();
    let (app, auth) = app_with_policy(idp_policy(TestJwksSource::with_signing_key(), &clock)).await;
    let mut claims = ReadToken::valid();
    claims.ttl_secs = -3600; // expired an hour ago
    let token = sign_read(&claims, SIGNING_KEY_PEM);
    let response = get(&app, PROTECTED, Some(&token)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(auth.rejection_count(), 1);
}

#[tokio::test]
async fn a_wrong_audience_token_is_a_counted_401() {
    let clock = TestClock::default();
    let (app, auth) = app_with_policy(idp_policy(TestJwksSource::with_signing_key(), &clock)).await;
    let mut claims = ReadToken::valid();
    claims.audience = "https://someone-else.example".to_owned();
    let token = sign_read(&claims, SIGNING_KEY_PEM);
    let response = get(&app, PROTECTED, Some(&token)).await;
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "a token minted for another service is rejected, not replayed here"
    );
    assert_eq!(auth.rejection_count(), 1);
}

#[tokio::test]
async fn a_wrong_issuer_token_is_a_counted_401() {
    let clock = TestClock::default();
    let (app, auth) = app_with_policy(idp_policy(TestJwksSource::with_signing_key(), &clock)).await;
    let mut claims = ReadToken::valid();
    claims.issuer = "https://evil-idp.example".to_owned();
    let token = sign_read(&claims, SIGNING_KEY_PEM);
    let response = get(&app, PROTECTED, Some(&token)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(auth.rejection_count(), 1);
}

#[tokio::test]
async fn a_token_with_no_scope_claim_is_403_not_200() {
    // Authenticated but unauthorized: a valid IdP token that carries no `read` scope must
    // not reach a read handler — a 403, never a silent grant.
    let clock = TestClock::default();
    let (app, auth) = app_with_policy(idp_policy(TestJwksSource::with_signing_key(), &clock)).await;
    let mut claims = ReadToken::valid();
    claims.scope = Some("openid profile".to_owned()); // no `read`
    let token = sign_read(&claims, SIGNING_KEY_PEM);
    let response = get(&app, PROTECTED, Some(&token)).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(auth.rejection_count(), 1, "a scope violation is counted");
}

#[tokio::test]
async fn an_unreachable_idp_is_503_not_a_silent_allow() {
    // The JWKS fetch fails: the hub could not verify, so it answers 503 and does NOT let the
    // request through and does NOT count a rejection (it judged nothing). Never a silent allow.
    let clock = TestClock::default();
    let source = common::FailingJwksSource;
    let policy = ReadAuthPolicy::resolve(false, Some(read_verifier(source, &clock)), vec![])
        .expect("resolves");
    let (app, auth) = app_with_policy(policy).await;
    let token = sign_read(&ReadToken::valid(), SIGNING_KEY_PEM);
    let response = get(&app, PROTECTED, Some(&token)).await;
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "an unreachable IdP is a 503, never a 200 or a 401"
    );
    assert_eq!(
        auth.rejection_count(),
        0,
        "a 503 is not a rejection: the hub judged nothing"
    );
}

#[tokio::test]
async fn an_hs256_token_is_rejected_not_confused_for_rs256() {
    // Algorithm-confusion: a token whose header says HS256 (symmetric), signed with the
    // RSA public modulus as an HMAC key — the classic RS/HS confusion. RS256 is pinned in
    // the validation, so the token is rejected before any HMAC path runs. A 401, never a 200.
    let clock = TestClock::default();
    let (app, _auth) =
        app_with_policy(idp_policy(TestJwksSource::with_signing_key(), &clock)).await;
    let iat = Timestamp::now().as_second();
    let body = json!({
        "iss": READ_ISSUER, "aud": READ_AUDIENCE, "iat": iat, "exp": iat + 3600, "scope": "read",
    });
    let mut header = Header::new(jsonwebtoken::Algorithm::HS256);
    header.kid = Some(TEST_KID.to_owned());
    let token = encode(&header, &body, &EncodingKey::from_secret(b"anything")).unwrap();
    let response = get(&app, PROTECTED, Some(&token)).await;
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "an HS256 token must not be accepted where RS256 is pinned"
    );
}

#[tokio::test]
async fn an_alg_none_token_is_rejected() {
    // The `alg: none` (unsigned) attack: a token with no signature must never authenticate.
    // `jsonwebtoken` refuses to even construct a `none` token via `encode`, so the token is
    // hand-assembled as `header.payload.` with an empty signature.
    use base64::Engine;
    let clock = TestClock::default();
    let (app, _auth) =
        app_with_policy(idp_policy(TestJwksSource::with_signing_key(), &clock)).await;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = b64.encode(json!({ "alg": "none", "kid": TEST_KID, "typ": "JWT" }).to_string());
    let iat = Timestamp::now().as_second();
    let payload = b64.encode(
        json!({ "iss": READ_ISSUER, "aud": READ_AUDIENCE, "iat": iat, "exp": iat + 3600, "scope": "read" })
            .to_string(),
    );
    let token = format!("{header}.{payload}.");
    let response = get(&app, PROTECTED, Some(&token)).await;
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "an unsigned `alg: none` token must never authenticate"
    );
}

#[tokio::test]
async fn a_valid_hub_minted_scoped_token_is_200() {
    // The IdP-less floor: a hub-minted token matching a configured hash, carrying `read`.
    let minted = token::mint().unwrap();
    let policy = ReadAuthPolicy::resolve(
        false,
        None,
        vec![scoped("ci", minted.raw(), vec![Scope::Read])],
    )
    .expect("a policy with a token resolves");
    let (app, _auth) = app_with_policy(policy).await;
    let response = get(&app, PROTECTED, Some(minted.raw())).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_wrong_hub_token_is_a_counted_401() {
    let minted = token::mint().unwrap();
    let policy = ReadAuthPolicy::resolve(
        false,
        None,
        vec![scoped("ci", minted.raw(), vec![Scope::Read])],
    )
    .expect("resolves");
    let (app, auth) = app_with_policy(policy).await;
    // A different hub-shaped token: the prefix routes it to the token floor, where it matches
    // nothing, so it is a 401 — not reinterpreted as a JWT.
    let response = get(&app, PROTECTED, Some("claimhub_deadbeef")).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(auth.rejection_count(), 1);
}

#[tokio::test]
async fn a_scoped_token_lacking_the_scope_is_403() {
    // A hub-minted token with only `act` (not `read`): authenticated, but not permitted on a
    // read route — the "read broadly, act narrowly" rule made a 403.
    let minted = token::mint().unwrap();
    let policy = ReadAuthPolicy::resolve(
        false,
        None,
        vec![scoped("act-only", minted.raw(), vec![Scope::Act])],
    )
    .expect("resolves");
    let (app, auth) = app_with_policy(policy).await;
    let response = get(&app, PROTECTED, Some(minted.raw())).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(auth.rejection_count(), 1);
}

#[tokio::test]
async fn a_hub_token_is_stored_as_a_hash_never_the_raw_token() {
    // The hashed-at-rest property, end to end: the config entry holds a sha256 hash, and the
    // raw token appears nowhere in the stored form.
    let minted = token::mint().unwrap();
    let entry = scoped("ci", minted.raw(), vec![Scope::Read]);
    assert!(entry.hash.starts_with("sha256:"));
    assert_ne!(entry.hash, minted.raw());
    assert!(
        !entry.hash.contains(minted.raw()),
        "the raw token must not appear in the stored hash"
    );
    // The stored hash still authenticates the raw token (a 200), proving the round trip.
    let policy = ReadAuthPolicy::resolve(false, None, vec![entry]).expect("resolves");
    let (app, _auth) = app_with_policy(policy).await;
    assert_eq!(
        get(&app, PROTECTED, Some(minted.raw())).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn open_reads_optin_serves_unauthenticated_200() {
    // The explicit private-network opt-in: no credential, a 200.
    let policy = ReadAuthPolicy::resolve(true, None, vec![]).expect("open reads resolves");
    let (app, _auth) = app_with_policy(policy).await;
    let response = get(&app, PROTECTED, None).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "opted-in open reads serve with no token"
    );
}

#[tokio::test]
async fn without_the_optin_the_same_read_is_401() {
    // The mirror of the opt-in test: with authed-everything (a token floor configured), the
    // same unauthenticated read is a 401 — proving the opt-in is what changed the outcome.
    let minted = token::mint().unwrap();
    let policy = ReadAuthPolicy::resolve(
        false,
        None,
        vec![scoped("ci", minted.raw(), vec![Scope::Read])],
    )
    .expect("resolves");
    let (app, _auth) = app_with_policy(policy).await;
    let response = get(&app, PROTECTED, None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_rotated_key_heals_via_one_debounced_refresh() {
    // The read verifier shares the ingest gate's debounced JWKS cache: a token whose kid the
    // cache does not yet hold refreshes at most once per window, and heals after the window.
    // This proves read auth inherits the same rate-limited refresh (the DoS guard), not a
    // second unbounded one — a flood within the window drives no extra fetch.
    let clock = TestClock::default();
    let source = TestJwksSource::empty_then_signing_key();
    let source_handle = source.clone();
    let policy = ReadAuthPolicy::resolve(false, Some(read_verifier(source, &clock)), vec![])
        .expect("resolves");
    let (app, _auth) = app_with_policy(policy).await;
    let token = sign_read(&ReadToken::valid(), SIGNING_KEY_PEM);

    // First request: the cold fetch publishes only the empty set, and the same-window refresh
    // is *suppressed* by the debounce — so the kid stays unknown and the read is a 401. One
    // fetch, not one-per-request: the amplification cap holds even for a novel kid.
    let first = get(&app, PROTECTED, Some(&token)).await;
    assert_eq!(
        first.status(),
        StatusCode::UNAUTHORIZED,
        "within the window the kid stays unknown"
    );
    assert_eq!(
        source_handle.fetch_count(),
        1,
        "only the cold fetch — the refresh is debounced"
    );

    // Advance past the debounce window: now the next request is allowed one refresh, which
    // publishes the rotated-in key, and the read heals.
    clock.advance_ms(common::TEST_DEBOUNCE_MS + 1);
    let second = get(&app, PROTECTED, Some(&token)).await;
    assert_eq!(
        second.status(),
        StatusCode::OK,
        "after the window the rotated-in key heals the read"
    );
    assert_eq!(
        source_handle.fetch_count(),
        2,
        "exactly one more fetch after the window"
    );
}

#[tokio::test]
async fn status_is_reachable_without_a_credential_even_when_authed() {
    // /status is health, deliberately outside the read-auth layer: a monitor must reach it
    // with no token even on an authed-everything hub.
    let minted = token::mint().unwrap();
    let policy = ReadAuthPolicy::resolve(
        false,
        None,
        vec![scoped("ci", minted.raw(), vec![Scope::Read])],
    )
    .expect("resolves");
    let (app, _auth) = app_with_policy(policy).await;
    assert_eq!(get(&app, "/status", None).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_ui_and_llms_surfaces_are_also_gated() {
    // The layer covers the whole read group, not just /api: the UI and /llms.txt are gated
    // too, so there is no read surface the layer misses.
    let minted = token::mint().unwrap();
    let policy = ReadAuthPolicy::resolve(
        false,
        None,
        vec![scoped("ci", minted.raw(), vec![Scope::Read])],
    )
    .expect("resolves");
    let (app, _auth) = app_with_policy(policy).await;
    for path in ["/ui/queue", "/llms.txt", "/mcp"] {
        let unauth = get(&app, path, None).await;
        assert_eq!(
            unauth.status(),
            StatusCode::UNAUTHORIZED,
            "{path} must require a credential"
        );
    }
    // And with the token, the UI and llms.txt serve (the MCP transport needs its own POST
    // handshake, so a bare GET is not asserted 200 here — the point is the gate covers it).
    assert_eq!(
        get(&app, "/ui/queue", Some(minted.raw())).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        get(&app, "/llms.txt", Some(minted.raw())).await.status(),
        StatusCode::OK
    );
}
