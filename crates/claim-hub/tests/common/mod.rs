//! Shared harness for the hub's integration tests (ingest and the walking skeleton).
//!
//! The whole point is a **deterministic, network-free** verification path: the tests
//! sign their own OIDC id-tokens with a fixed RSA key and verify them against an
//! injected JWKS built from that key's public components. No real GitHub, no clock
//! reaching into real time — the app's clock and the JWKS source are both parameters
//! (CLAUDE.md's determinism rule). A forged token is signed with a *different* key, so
//! the signature genuinely fails against the injected set.
//!
//! Shared by more than one test binary (`ingest.rs` and `skeleton.rs`), each of which
//! uses a subset of these helpers, so `#![allow(dead_code)]` keeps the binary that does
//! not exercise a given helper from warning — the helper is live in the *other* binary.

#![allow(dead_code)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use claim_core::{Claim, Timestamp};
use claim_hub::app::{AppState, Clock};
use claim_hub::oidc::{JwksSource, OidcError, OidcVerifier};
use claim_hub_store::{RegisteredClaim, Registry, SqliteStore};
use http_body_util::BodyExt;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::json;
use tower::ServiceExt;

/// The test issuer and audience the app trusts and the tokens carry.
pub const TEST_ISSUER: &str = "https://token.actions.githubusercontent.com";
pub const TEST_AUDIENCE: &str = "https://hub.acme.example";
/// The connected repository (and the store it maps to). A token for any other
/// repository is rejected.
pub const TEST_REPOSITORY: &str = "acme/payments";
pub const TEST_STORE: &str = "github.com/acme/payments";
/// The signing key's id, published in the injected JWKS. A token names it in its header.
pub const TEST_KID: &str = "test-key-1";

/// A fixed instant every ingest test stamps its events at, so the appended event is
/// exact and comparable. Well before the tokens' `exp`, so an unexpired token is valid.
pub const INGEST_INSTANT: &str = "2026-07-18T12:00:00Z";

/// The PEM of the RSA private key the tests sign valid tokens with. Its public
/// components are published in the injected JWKS under [`TEST_KID`].
const SIGNING_KEY_PEM: &str = include_str!("../fixtures/oidc_signing_key.pem");
/// A *different* RSA private key, for the forged-signature test: a token signed with it
/// but presented under [`TEST_KID`] fails verification against the published key.
const WRONG_KEY_PEM: &str = include_str!("../fixtures/oidc_wrong_key.pem");

/// The base64url RSA modulus (`n`) of the signing key, for the injected JWKS. Computed
/// once from the fixture key; paired with the standard exponent `AQAB` (65537).
const SIGNING_KEY_N: &str = "3hH-i_453jmtKreB-0eTSU5ZZoIDrEgoSBYiiInwkBak6yF8OZGvMwRl-TkP0GVbO2QSEXcWXwDJIzweGBqG-bQg3aPhL7X7S-iDHK4DCxJMdyIBMrSQByXhqrlFak1d_onJfwlmiBJ0Qn-QJwAcnPbbSeoVclIY1drRDGS4ePdhCieGtjvelfd8tVPFauni9Ji6rtyJ55A1PbC63dmIKDUkS8hwQqizH47niEo9RwbmLdjf5LiAoWKoVrG9mLlwf02ZxsyMtdsasvAzglE5YjJNtCHfA4RW7HQitlyT4e5AH1YCF4LQZFCsbeCYc_dU7HQaZw8v9_GCA7QFiclfiQ";

/// A JWKS source that returns a scripted sequence of key sets, never touching the
/// network.
///
/// Injected in place of the production HTTP source so every verification runs against
/// the signing key's published components deterministically. It counts fetches (so a test
/// can assert the cache refreshed) and serves a *sequence* of sets — each fetch advances
/// to the next, the last repeating — so a test can script "first fetch misses the kid,
/// second fetch has it" to exercise refresh-on-unknown-kid.
#[derive(Clone)]
pub struct TestJwksSource {
    sets: Arc<Vec<JwkSet>>,
    fetches: Arc<std::sync::atomic::AtomicUsize>,
}

impl TestJwksSource {
    /// A source publishing the signing key under [`TEST_KID`] on every fetch.
    pub fn with_signing_key() -> Self {
        Self::sequence(vec![signing_key_jwks()])
    }

    /// A source whose first fetch returns an *empty* set (the kid is unknown) and whose
    /// second and later fetches publish the signing key — so a verification refreshes
    /// once on the unknown kid, then succeeds.
    pub fn empty_then_signing_key() -> Self {
        Self::sequence(vec![empty_jwks(), signing_key_jwks()])
    }

    /// A source that only ever returns an empty set — the kid is never published, so a
    /// verification refreshes once and then rejects (no infinite loop).
    pub fn sequence_of_empty() -> Self {
        Self::sequence(vec![empty_jwks()])
    }

    fn sequence(sets: Vec<JwkSet>) -> Self {
        assert!(!sets.is_empty(), "at least one key set");
        Self {
            sets: Arc::new(sets),
            fetches: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// How many times the JWKS was fetched — the refresh count a cache test asserts.
    pub fn fetch_count(&self) -> usize {
        self.fetches.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl JwksSource for TestJwksSource {
    async fn fetch(&self) -> Result<JwkSet, OidcError> {
        let n = self
            .fetches
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Advance through the scripted sequence; the last set repeats.
        let index = n.min(self.sets.len() - 1);
        Ok(self.sets[index].clone())
    }
}

/// The JWKS publishing the signing key under [`TEST_KID`].
fn signing_key_jwks() -> JwkSet {
    serde_json::from_value(json!({
        "keys": [{
            "kty": "RSA",
            "use": "sig",
            "alg": "RS256",
            "kid": TEST_KID,
            "n": SIGNING_KEY_N,
            "e": "AQAB",
        }]
    }))
    .expect("valid test JWKS")
}

/// An empty JWKS — no keys, so any kid is unknown.
fn empty_jwks() -> JwkSet {
    serde_json::from_value(json!({ "keys": [] })).expect("valid empty JWKS")
}

/// A controllable monotonic-ms clock for the JWKS refresh debounce, so a test drives the
/// rate limit without sleeping (CLAUDE.md's determinism rule). Starts at 0; `advance_ms`
/// moves it forward.
#[derive(Clone, Default)]
pub struct TestClock(Arc<std::sync::atomic::AtomicU64>);

impl TestClock {
    /// Move the clock forward by `ms`.
    pub fn advance_ms(&self, ms: u64) {
        self.0.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
    }

    /// The clock closure the `JwksCache` reads "now" through.
    fn monotonic(&self) -> claim_hub::oidc::MonotonicMillis {
        let inner = self.0.clone();
        Arc::new(move || inner.load(std::sync::atomic::Ordering::SeqCst))
    }
}

/// The debounce window the tests use, in ms. Small and explicit so a test can step over
/// it with one `advance_ms`; the production default is 60s.
pub const TEST_DEBOUNCE_MS: u64 = 1_000;

/// Build the app over a fresh in-memory store, verifying tokens against `source`, with a
/// fixed event clock. Returns the app and the store. The JWKS refresh clock starts at 0
/// and is never advanced here — fine for the tests that verify one token (a single cold
/// fetch, no debounced refresh); the refresh tests use [`app_with_jwks_clock`].
pub async fn app_with(source: TestJwksSource) -> (axum::Router, SqliteStore) {
    let (app, store, _clock) = app_with_jwks_clock(source).await;
    (app, store)
}

/// Like [`app_with`] but also returns the [`TestClock`] driving the JWKS refresh
/// debounce, so a refresh test advances it past [`TEST_DEBOUNCE_MS`] to allow the next
/// refresh.
pub async fn app_with_jwks_clock(source: TestJwksSource) -> (axum::Router, SqliteStore, TestClock) {
    let store = SqliteStore::open_in_memory().await.unwrap();
    let jwks_clock = TestClock::default();
    let cache =
        claim_hub::oidc::JwksCache::with_debounce(source, TEST_DEBOUNCE_MS, jwks_clock.monotonic());
    let verifier = OidcVerifier::with_cache(
        TEST_ISSUER,
        TEST_AUDIENCE,
        [TEST_REPOSITORY.to_owned()],
        cache,
    );
    let clock: Clock = Arc::new(ingest_instant);
    let state = AppState::new(store.clone(), Some(Arc::new(verifier))).with_clock(clock);
    (claim_hub::build_app(state), store, jwks_clock)
}

/// The fixed ingest instant, parsed — the deterministic clock's value.
pub fn ingest_instant() -> Timestamp {
    INGEST_INSTANT.parse().expect("valid RFC 3339 instant")
}

/// The claims of a test OIDC token, mirroring a GitHub Actions id-token.
///
/// The two the gate branches on are `aud`/`iss`/`exp` (validated) and `repository`/`sha`/
/// `run_id` (recorded); the rest are representative GitHub claims so the producer block
/// is realistic and the verbatim-recording is exercised.
pub struct TokenClaims {
    pub issuer: String,
    pub audience: String,
    pub repository: String,
    pub sha: Option<String>,
    pub run_id: Option<String>,
    /// Seconds from the token's `iat` to its `exp`. Negative for an already-expired token.
    pub ttl_secs: i64,
}

impl TokenClaims {
    /// A valid token for the connected repository: right issuer, audience, repository, an
    /// hour of life, a commit and run id.
    pub fn valid() -> Self {
        Self {
            issuer: TEST_ISSUER.to_owned(),
            audience: TEST_AUDIENCE.to_owned(),
            repository: TEST_REPOSITORY.to_owned(),
            sha: Some("8f2c0a1b3d4e5f60718293a4b5c6d7e8f9012345".to_owned()),
            run_id: Some("1234567890".to_owned()),
            ttl_secs: 3600,
        }
    }
}

/// Sign a token's claims into a JWT with the fixture signing key and [`TEST_KID`].
///
/// A JWT's `exp` is validated by `jsonwebtoken` against the **real** wall clock, which no
/// parameter can override, so a token's own validity window (`iat`/`exp`) is anchored to
/// real time here: `iat = now`, `exp = now + ttl_secs`. `ttl_secs` positive is a valid
/// token, negative an already-expired one. This is the *only* real-time input in the
/// harness, and it is inherent to how JWT expiry works — the hub's own view of *when it
/// saw* the verdict (the event's `reported_at`) is still the deterministic injected
/// clock, independent of the token's validity window.
pub fn sign_token(claims: &TokenClaims) -> String {
    sign_token_with(claims, SIGNING_KEY_PEM)
}

/// Sign with a caller-chosen key — the *wrong* key for the forged-signature test.
pub fn sign_token_with_wrong_key(claims: &TokenClaims) -> String {
    sign_token_with(claims, WRONG_KEY_PEM)
}

/// Sign a valid token with the fixture key but **omit** a named claim (`iss` or `aud`),
/// for the missing-claim tests. A genuinely RS256-signed token whose only defect is the
/// absent claim, so it proves the gate rejects a missing `iss`/`aud` rather than letting
/// hollow pinning through.
pub fn sign_token_omitting(claim: &str) -> String {
    let mut body = valid_body();
    body.as_object_mut()
        .unwrap()
        .remove(claim)
        .unwrap_or_else(|| panic!("`{claim}` was expected in the token body to remove"));
    sign_body(&body, SIGNING_KEY_PEM)
}

/// The claim body of a valid GitHub Actions-shaped token, anchored to real time.
fn valid_body() -> serde_json::Value {
    let claims = TokenClaims::valid();
    // Real `now`: a JWT's exp is checked against the system clock inside `jsonwebtoken`,
    // which is not injectable, so the token's validity window is the one thing anchored
    // to real time. The recorded event timestamp stays deterministic (the app clock).
    let iat = Timestamp::now().as_second();
    let exp = iat + claims.ttl_secs;
    json!({
        "iss": claims.issuer,
        "aud": claims.audience,
        "iat": iat,
        "exp": exp,
        "repository": claims.repository,
        "repository_owner": "acme",
        "workflow": "verify",
        "ref": "refs/heads/main",
        "sha": claims.sha,
        "run_id": claims.run_id,
        "actor": "octocat",
        "job_workflow_ref": "acme/payments/.github/workflows/verify.yml@refs/heads/main",
    })
}

fn sign_token_with(claims: &TokenClaims, key_pem: &str) -> String {
    let iat = Timestamp::now().as_second();
    let exp = iat + claims.ttl_secs;
    let body = json!({
        "iss": claims.issuer,
        "aud": claims.audience,
        "iat": iat,
        "exp": exp,
        "repository": claims.repository,
        "repository_owner": "acme",
        "workflow": "verify",
        "ref": "refs/heads/main",
        "sha": claims.sha,
        "run_id": claims.run_id,
        "actor": "octocat",
        "job_workflow_ref": "acme/payments/.github/workflows/verify.yml@refs/heads/main",
    });
    sign_body(&body, key_pem)
}

/// Sign a valid token but present it under a caller-chosen `kid` in the header — for the
/// amplification-cap test, where a flood of forged tokens carries distinct, never-
/// published kids. Signed with the real key so the *only* defect is the unknown kid.
pub fn sign_token_with_kid(claims: &TokenClaims, kid: &str) -> String {
    let iat = Timestamp::now().as_second();
    let exp = iat + claims.ttl_secs;
    let body = json!({
        "iss": claims.issuer,
        "aud": claims.audience,
        "iat": iat,
        "exp": exp,
        "repository": claims.repository,
        "sha": claims.sha,
        "run_id": claims.run_id,
    });
    let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(kid.to_owned());
    let key = EncodingKey::from_rsa_pem(SIGNING_KEY_PEM.as_bytes()).expect("valid RSA PEM");
    encode(&header, &body, &key).expect("sign token")
}

/// Sign an arbitrary claim body with `key_pem` under [`TEST_KID`], RS256.
fn sign_body(body: &serde_json::Value, key_pem: &str) -> String {
    let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(TEST_KID.to_owned());
    let key = EncodingKey::from_rsa_pem(key_pem.as_bytes()).expect("valid RSA PEM");
    encode(&header, body, &key).expect("sign token")
}

/// Seed the registry with a claim parsed from `frontmatter`, so its check digests and
/// `hub:` hints are stored and the ingest gate/deriver can read them. Returns the parsed
/// claim (whose checks the caller uses to compute expected digests).
pub async fn seed_claim(store: &SqliteStore, file: &str, frontmatter: &str) -> Claim {
    let text = format!("---\n{frontmatter}\n---\nStatement body.\n");
    let claim = claim_core::parse_claim_file(file, &text).expect("valid claim fixture");
    let registered = RegisteredClaim::from_claim(&claim, "seedcommit");
    store
        .replace_store(TEST_STORE, &[registered])
        .await
        .expect("seed registry");
    claim
}

/// POST a `claim check --json` body to `/ingest` with a bearer token, returning the
/// status and parsed JSON body.
pub async fn post_ingest(
    app: &axum::Router,
    token: Option<&str>,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method("POST").uri("/ingest");
    if let Some(token) = token {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let request = builder.body(Body::from(body.to_owned())).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

/// GET `/api/claims/{id}` and return the parsed JSON body (the derived standing with its
/// as-of), for tests that read the hub's one read endpoint over the assembled app.
pub async fn get_claim(app: &axum::Router, id: &str) -> serde_json::Value {
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
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// Back up a live hub to one self-contained file at `dst`, via the store's online backup
/// (`VACUUM INTO`), then assert `dst` carries no `-wal`/`-shm` sidecar. This is the
/// self-host backup the docs promise, taken against a *running* hub: a consistent snapshot
/// under a read transaction that cannot lose a committed event to a racing checkpoint — the
/// data-loss a bare `cp hub.db` risks (invariants #4 and #6). A restore is then a plain copy
/// of this one file.
pub async fn backup_database(store: &SqliteStore, dst: &std::path::Path) {
    store.backup(dst).await.expect("online backup the hub");
    for suffix in ["-wal", "-shm"] {
        let mut name = dst.as_os_str().to_owned();
        name.push(suffix);
        assert!(
            !std::path::Path::new(&name).exists(),
            "an online backup produces no {suffix} sidecar"
        );
    }
}

/// Read `/status` and return the parsed body.
pub async fn get_status(app: &axum::Router) -> serde_json::Value {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// A minimal one-check `claim check --json` report body for `claim_id`, with the given
/// verdict and optional evidence — what a producer would POST. The sole check is
/// declared index 0.
pub fn one_check_report(claim_id: &str, verdict: &str, evidence: Option<&str>) -> String {
    let mut check = json!({
        "index": 0,
        "verdict": verdict,
        "end": { "kind": "exited", "code": if verdict == "held" { 0 } else { 1 } },
        "detail": "exit",
    });
    if let Some(evidence) = evidence {
        check["evidence"] = json!(evidence);
    }
    json!({
        "status": "ok",
        "exit": if verdict == "held" { 0 } else { 1 },
        "checked": 1,
        "ran": 1,
        "skipped": 0,
        "claims": [{
            "id": claim_id,
            "file": ".claims/pin.md",
            "checks": [check],
            "skipped": [],
            "supports": [],
            "exit": if verdict == "held" { 0 } else { 1 },
        }],
        "errors": [],
    })
    .to_string()
}
