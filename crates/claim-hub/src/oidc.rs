//! The ingest gate's OIDC verification: the trust root of the whole ledger.
//!
//! Every verdict on the hub's ledger is trusted because of *who produced it* (HUB.md
//! §4, the SLSA framing): the CI pipeline that ran the check, proven by a GitHub
//! Actions OIDC id-token, not a shared secret that could leak or be forged. This
//! module verifies that token and turns it into a [`VerifiedProducer`] — the identity
//! block recorded verbatim on every event it produces — or a typed
//! [`AuthReject`] the gate answers with a 4xx and counts.
//!
//! What "verify" means here, in order (each a distinct rejection):
//!
//! 1. **Signature** against the issuer's published JWKS. The token's `kid` selects the
//!    key; an unknown `kid` triggers a JWKS refresh (keys rotate), and only then is it
//!    rejected. That refresh is **rate-limited** (`kid` is attacker-controlled and read
//!    before the signature is checked, so an un-throttled fetch-per-unknown-kid is an
//!    amplification vector) — see [`JwksCache`]. A token signed by a key not in the
//!    issuer's set — a forgery — fails.
//! 2. **`iss`** equals the configured issuer (`token.actions.githubusercontent.com`) and
//!    is *present*: a token omitting `iss`/`aud`/`exp` is rejected, not waved through, so
//!    the issuer/audience pinning is never hollow (`set_required_spec_claims`).
//! 3. **`aud`** equals the hub's own configured audience. This is what stops a token
//!    minted for *another* service from being replayed at this hub.
//! 4. **`exp`** is in the future (validated by `jsonwebtoken` with a small leeway).
//! 5. **`repository`** is a **connected store**. A validly-signed token from a repo the
//!    hub does not track is rejected: the hub ingests only for stores it mirrors, so a
//!    verdict it could never derive a standing for never lands.
//!
//! The verified claims — issuer, repository, workflow, ref, run id, sha, and every
//! other claim the token carried — are recorded **verbatim** (HUB.md §4), so the trust
//! judgment stays re-derivable rather than distilled into named fields at the door
//! (invariant #3). No static-token lane and no unattested lane exist (§4.5.1): a
//! developer's local `claim check` is a terminal report, never hub telemetry.
//!
//! The JWKS is fetched through an injectable [`JwksSource`], so tests supply a key set
//! directly and the whole verification path runs with no network (deterministic, per
//! CLAUDE.md's testing rule). The cache and its refresh-on-unknown-`kid` are a page of
//! our own code ([`JwksCache`]), deliberately not a wrapper crate (§3 veto 5).

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::sync::RwLock;

/// The GitHub Actions OIDC issuer — the `iss` every runner token carries and the
/// authority whose JWKS signs them.
pub const GITHUB_ACTIONS_ISSUER: &str = "https://token.actions.githubusercontent.com";

/// The JWKS endpoint the issuer publishes its signing keys at. The
/// [`HttpJwksSource`] fetches this; the keys rotate, so an unknown `kid` refreshes it.
pub const GITHUB_ACTIONS_JWKS_URL: &str =
    "https://token.actions.githubusercontent.com/.well-known/jwks";

/// A source of the issuer's JWKS, injectable so tests supply keys without network.
///
/// The production implementation ([`HttpJwksSource`]) fetches over HTTPS with
/// `reqwest`; a test implementation returns a fixed [`JwkSet`] built from a keypair it
/// controls, so the *same* verification path (kid lookup, signature check) runs
/// deterministically and offline. Fetching is fallible and async: a network fault is a
/// reason the gate cannot verify *right now*, distinct from a token that is definitely
/// invalid.
pub trait JwksSource: Send + Sync {
    /// Fetch the issuer's current JWKS.
    ///
    /// # Errors
    ///
    /// Returns an [`OidcError`] when the key set cannot be retrieved or parsed — a
    /// transient inability to verify, which the gate surfaces as "cannot verify now"
    /// (a 5xx-class fault), never as a token being invalid.
    fn fetch(&self) -> impl std::future::Future<Output = Result<JwkSet, OidcError>> + Send;
}

/// The production JWKS source: fetch the issuer's keys over HTTPS.
///
/// Holds a `reqwest` client (rustls, so no system OpenSSL is linked) and the JWKS URL.
/// The client is reused across fetches (connection pooling); a fetch happens rarely —
/// once at first verify, and again only when an unknown `kid` forces a refresh.
#[derive(Clone)]
pub struct HttpJwksSource {
    client: reqwest::Client,
    url: String,
}

impl HttpJwksSource {
    /// A source fetching the GitHub Actions JWKS from `url` (normally
    /// [`GITHUB_ACTIONS_JWKS_URL`]).
    ///
    /// Building the client can fail if the TLS backend cannot initialize; that is a
    /// boot-time fault the caller surfaces, not a per-request error.
    ///
    /// # Errors
    ///
    /// [`OidcError::Fetch`] if the `reqwest` client cannot be constructed.
    pub fn new(url: impl Into<String>) -> Result<Self, OidcError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| OidcError::Fetch(e.to_string()))?;
        Ok(Self {
            client,
            url: url.into(),
        })
    }
}

impl JwksSource for HttpJwksSource {
    async fn fetch(&self) -> Result<JwkSet, OidcError> {
        let response = self
            .client
            .get(&self.url)
            .send()
            .await
            .map_err(|e| OidcError::Fetch(format!("requesting JWKS from {}: {e}", self.url)))?;
        let response = response.error_for_status().map_err(|e| {
            OidcError::Fetch(format!("JWKS endpoint {} returned an error: {e}", self.url))
        })?;
        response
            .json::<JwkSet>()
            .await
            .map_err(|e| OidcError::Fetch(format!("parsing JWKS from {}: {e}", self.url)))
    }
}

/// A monotonic millisecond clock, injectable so the debounce is deterministic in tests.
///
/// The refresh debounce ([`JwksCache`]) compares "now" against the last successful fetch,
/// so it needs a clock. A monotonic source (not wall-clock) is what a rate limit wants —
/// a backward clock jump must not re-open the fetch window. Production reads a process
/// monotonic instant; a test injects a counter it advances, so the debounce is tested
/// without sleeping (CLAUDE.md's determinism rule).
pub type MonotonicMillis = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The default refresh-debounce window: at most one JWKS refresh per this many
/// milliseconds, however many novel `kid`s arrive.
///
/// GitHub rotates its Actions signing keys on the order of hours, so a 60-second floor
/// between refreshes never blocks a legitimate rotation from healing (the next request
/// after the window refreshes), while it caps the unauthenticated amplification a flood
/// of forged tokens carrying novel `kid`s could otherwise drive: `kid` is read from the
/// token header *before* signature verification, so without this an attacker chooses the
/// hub's outbound JWKS request rate. One fetch per minute is a hard ceiling on that.
const DEFAULT_REFRESH_DEBOUNCE_MS: u64 = 60_000;

/// A cache of the issuer's JWKS that refreshes on an unknown key id, rate-limited.
///
/// The issuer rotates signing keys, so a token can legitimately carry a `kid` the
/// cached set does not yet have. The cache resolves a `kid` against its current set,
/// and on a miss fetches once more before giving up — so key rotation heals without a
/// redeploy, while a genuinely unknown key (a forgery) still fails after the refresh.
///
/// **The refresh is debounced** (a default 60-second window; see
/// `DEFAULT_REFRESH_DEBOUNCE_MS`): because `kid` is attacker-controlled and read before
/// the signature is checked, an un-throttled refresh-per-unknown-kid lets an
/// unauthenticated flood of forged tokens drive the hub's outbound JWKS request rate — an
/// amplification/DoS vector against the issuer and the hub. So a miss triggers a refresh
/// at most once per window; within the window a novel `kid` is rejected against the
/// cached set with no new fetch. Concurrent misses are single-flighted by the write lock:
/// the first refreshes, the rest re-check the now-current set under the same lock rather
/// than each firing their own fetch.
///
/// The set is behind an [`RwLock`]: reads (the common path) take the read lock; a
/// refresh takes the write lock briefly. It starts empty and populates lazily on the
/// first verification, so construction does no IO.
pub struct JwksCache<S: JwksSource> {
    source: S,
    keys: RwLock<Option<JwkSet>>,
    /// The monotonic ms of the last successful fetch, or `None` if never fetched. Behind
    /// the same write lock discipline as `keys` (via `refresh_locked`), so the debounce
    /// check and the fetch are atomic and concurrent misses coalesce.
    last_fetch_ms: RwLock<Option<u64>>,
    debounce_ms: u64,
    clock: MonotonicMillis,
}

impl<S: JwksSource> JwksCache<S> {
    /// A cache over `source`, initially empty (the first resolve fetches), with the
    /// default debounce window and a process-monotonic clock.
    pub fn new(source: S) -> Self {
        // A process-relative monotonic clock: `Instant` since a fixed start, in ms. Never
        // goes backward, and needs no wall-clock time.
        let start = std::time::Instant::now();
        let clock: MonotonicMillis =
            Arc::new(move || start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
        Self::with_debounce(source, DEFAULT_REFRESH_DEBOUNCE_MS, clock)
    }

    /// A cache with an explicit debounce window and clock, for tests that drive the
    /// rate limit deterministically.
    pub fn with_debounce(source: S, debounce_ms: u64, clock: MonotonicMillis) -> Self {
        Self {
            source,
            keys: RwLock::new(None),
            last_fetch_ms: RwLock::new(None),
            debounce_ms,
            clock,
        }
    }

    /// Resolve `kid` to a verifying [`DecodingKey`], refreshing the JWKS once on a miss —
    /// but no more than once per debounce window.
    ///
    /// Tries the cached set first; if the `kid` is absent (or the cache is empty), it
    /// refreshes (subject to the debounce) and tries again. A `kid` still absent after
    /// that is [`OidcError::UnknownKey`] — an invalid token (no published key signed it),
    /// not a transient fault. A `kid` absent *and* the refresh suppressed by the debounce
    /// is also `UnknownKey`: within the window the hub answers from the cache it has,
    /// rather than fetching per forged token. A fetch that itself fails is
    /// [`OidcError::Fetch`].
    ///
    /// # Errors
    ///
    /// [`OidcError::UnknownKey`] if `kid` is not in the issuer's set (after any allowed
    /// refresh); [`OidcError::Fetch`] if a fetch was attempted and failed;
    /// [`OidcError::Key`] if the matched JWK cannot be turned into a decoding key.
    async fn decoding_key(&self, kid: &str) -> Result<DecodingKey, OidcError> {
        // Populate the cache on first use. The initial fetch is not debounced against
        // "never" — a cold cache must fetch — but it does record the fetch time, so an
        // immediately-following unknown-kid miss is debounced.
        if self.is_empty().await {
            self.refresh_if_due(true).await?;
        }
        if let Some(key) = self.lookup(kid).await? {
            return Ok(key);
        }
        // A populated cache lacking the kid: refresh at most once per window, then retry.
        // If the window suppressed the refresh, the kid stays unknown — no fetch fired.
        self.refresh_if_due(false).await?;
        self.lookup(kid)
            .await?
            .ok_or_else(|| OidcError::UnknownKey(kid.to_owned()))
    }

    /// Whether the cache has never been populated.
    async fn is_empty(&self) -> bool {
        self.keys.read().await.is_none()
    }

    /// Refresh the JWKS if the debounce window has elapsed since the last fetch (or if
    /// `force`, used for the mandatory cold-cache initial fetch).
    ///
    /// Takes the `last_fetch_ms` write lock for the whole check-and-fetch, so concurrent
    /// callers single-flight: the first inside the window fetches and stamps the time; a
    /// second, now seeing a recent stamp, does not fetch again. A suppressed refresh is a
    /// silent success (`Ok(())`) — the caller then re-checks the cache and, still missing
    /// the kid, rejects it as unknown.
    async fn refresh_if_due(&self, force: bool) -> Result<(), OidcError> {
        let now = (self.clock)();
        let mut last = self.last_fetch_ms.write().await;
        if !force {
            if let Some(prev) = *last {
                if now.saturating_sub(prev) < self.debounce_ms {
                    // Within the window: do not fetch. Bounds the outbound request rate a
                    // flood of novel, attacker-chosen `kid`s can drive.
                    return Ok(());
                }
            }
        }
        let fresh = self.source.fetch().await?;
        *self.keys.write().await = Some(fresh);
        *last = Some(now);
        Ok(())
    }

    /// Look up `kid` in the cached set, returning its decoding key if present.
    ///
    /// `Ok(None)` means the cache is empty or does not hold `kid`. `Err` means the
    /// matched key was malformed (a real fault, not a miss).
    async fn lookup(&self, kid: &str) -> Result<Option<DecodingKey>, OidcError> {
        let guard = self.keys.read().await;
        let Some(set) = guard.as_ref() else {
            return Ok(None);
        };
        match set.find(kid) {
            Some(jwk) => DecodingKey::from_jwk(jwk)
                .map(Some)
                .map_err(|e| OidcError::Key(format!("JWK {kid}: {e}"))),
            None => Ok(None),
        }
    }
}

/// The OIDC trust anchor and JWKS cache the ingest gate verifies against.
///
/// One per hub, shared across requests (behind an [`Arc`] in the app state). It holds
/// the configured issuer and audience, the set of connected-store repositories a token
/// must name, and the [`JwksCache`]. Generic over the [`JwksSource`] so production wires
/// [`HttpJwksSource`] and tests wire a fixed key set — the verification logic is
/// identical either way.
pub struct OidcVerifier<S: JwksSource> {
    issuer: String,
    audience: String,
    /// The connected stores, keyed by the token's `repository` claim (e.g.
    /// `acme/payments`) mapping to the canonical store id events record (e.g.
    /// `github.com/acme/payments`).
    repositories: HashSet<String>,
    cache: JwksCache<S>,
}

impl<S: JwksSource> OidcVerifier<S> {
    /// A verifier trusting `issuer`/`audience`, accepting only tokens whose
    /// `repository` is in `repositories`, resolving keys through `source`.
    ///
    /// `repositories` is the set of `repository` claim values (owner/repo) the hub
    /// tracks; a token naming any other repository is rejected however valid its
    /// signature.
    pub fn new(
        issuer: impl Into<String>,
        audience: impl Into<String>,
        repositories: impl IntoIterator<Item = String>,
        source: S,
    ) -> Self {
        Self::with_cache(issuer, audience, repositories, JwksCache::new(source))
    }

    /// A verifier over a caller-built [`JwksCache`], so a test can supply a cache with a
    /// controllable clock and debounce window to drive the refresh rate limit
    /// deterministically. Production uses [`new`](OidcVerifier::new).
    pub fn with_cache(
        issuer: impl Into<String>,
        audience: impl Into<String>,
        repositories: impl IntoIterator<Item = String>,
        cache: JwksCache<S>,
    ) -> Self {
        Self {
            issuer: issuer.into(),
            audience: audience.into(),
            repositories: repositories.into_iter().collect(),
            cache,
        }
    }

    /// Verify one OIDC id-token, returning the verified producer or a typed rejection.
    ///
    /// The full check chain of the module docs: signature (via the `kid`'s key, with a
    /// refresh on an unknown `kid`), `iss`, `aud`, `exp`, and repository-is-connected.
    /// On success the returned [`VerifiedProducer`] carries the token's claims verbatim,
    /// ready to record on every event this push produces.
    ///
    /// # Errors
    ///
    /// A [`VerifyError`] separating a **rejection** ([`VerifyError::Reject`], the token
    /// is invalid — the gate answers 4xx and counts it) from an **infrastructure
    /// fault** ([`VerifyError::Unavailable`], the JWKS could not be fetched — the gate
    /// answers 503, since it cannot verify *right now* and must not reject a possibly
    /// valid token as forged).
    pub async fn verify(&self, token: &str) -> Result<VerifiedProducer, VerifyError> {
        let header = decode_header(token).map_err(|e| {
            VerifyError::Reject(AuthReject::Malformed(format!("token header: {e}")))
        })?;
        let kid = header.kid.ok_or_else(|| {
            VerifyError::Reject(AuthReject::Malformed("token has no `kid`".into()))
        })?;

        let key = match self.cache.decoding_key(&kid).await {
            Ok(key) => key,
            Err(OidcError::UnknownKey(kid)) => {
                return Err(VerifyError::Reject(AuthReject::UnknownKey(kid)))
            }
            Err(OidcError::Key(reason)) => {
                return Err(VerifyError::Reject(AuthReject::Malformed(reason)))
            }
            Err(OidcError::Fetch(reason)) => return Err(VerifyError::Unavailable(reason)),
        };

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.set_audience(&[self.audience.as_str()]);
        // Require `iss` and `aud` to be *present*, not merely correct-when-present.
        // `jsonwebtoken` rejects a present-but-wrong `iss`/`aud` but, by default, lets a
        // token that OMITS them through — which would make the issuer/audience pinning
        // hollow: a token minted with no `aud` at all would sail past `set_audience`.
        // `exp` stays required (its default). Making all three required means a token
        // missing any of them is a `MissingRequiredClaim` rejection, mapped by
        // `reject_from_jwt`.
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        // `exp` is validated by default (with jsonwebtoken's small leeway); `iat`/`nbf`
        // are validated when present. Nothing here disables those.
        let data = decode::<OidcClaims>(token, &key, &validation)
            .map_err(|e| VerifyError::Reject(reject_from_jwt(&e)))?;

        let claims = data.claims;
        if !self.repositories.contains(&claims.repository) {
            return Err(VerifyError::Reject(AuthReject::UnconnectedRepository(
                claims.repository,
            )));
        }
        Ok(VerifiedProducer::new(claims))
    }
}

/// Map a `jsonwebtoken` decode error to the specific rejection reason it represents.
///
/// The variants are separated so the producer's 4xx names exactly what failed — an
/// expired token, a wrong audience, a wrong issuer, a bad signature, a required claim
/// missing — rather than a generic "invalid". A token that *omits* `iss`/`aud`/`exp`
/// surfaces here as `MissingRequiredClaim` (because [`OidcVerifier::verify`] marks all
/// three required), which is a named `Malformed` rejection, not a silent pass — the fix
/// that closes the hollow-pinning hole. Anything else (a malformed structure) is also a
/// malformed token.
fn reject_from_jwt(error: &jsonwebtoken::errors::Error) -> AuthReject {
    use jsonwebtoken::errors::ErrorKind;
    match error.kind() {
        ErrorKind::ExpiredSignature => AuthReject::Expired,
        ErrorKind::InvalidAudience => AuthReject::WrongAudience,
        ErrorKind::InvalidIssuer => AuthReject::WrongIssuer,
        ErrorKind::InvalidSignature => AuthReject::BadSignature,
        ErrorKind::MissingRequiredClaim(claim) => {
            AuthReject::Malformed(format!("token is missing the required `{claim}` claim"))
        }
        other => AuthReject::Malformed(format!("token failed validation: {other:?}")),
    }
}

/// The standard and GitHub-specific claims the ingest gate reads from an OIDC token.
///
/// The gate *branches* on `repository` (the connected-store check, and the store id),
/// `sha` (the commit an event records), and `run_id` (the dedup run). `iss` is a
/// **required** field here as belt-and-suspenders over the `set_required_spec_claims`
/// pinning in [`OidcVerifier::verify`]: a token that omits it fails to deserialize, so
/// issuer presence is enforced at two layers, not one. (`aud` is enforced by
/// `set_required_spec_claims` alone — its JWT value may be a string *or* an array, so
/// typing it here would risk rejecting a valid array-`aud` token; the spec-claim check
/// validates presence and value without constraining the shape.) Every other claim is
/// preserved verbatim in the producer block via [`VerifiedProducer`].
/// `#[serde(deny_unknown_fields)]` is deliberately **absent**: an OIDC token carries many
/// standard claims (`aud`, `exp`, `iat`, `nbf`, `sub`, `jti`, and GitHub's `workflow`,
/// `ref`, `actor`, ...), and the rest are captured whole in `rest`.
#[derive(Debug, Clone, Deserialize)]
struct OidcClaims {
    /// The `iss` claim — required, so a token omitting the issuer never deserializes.
    /// Recorded back into the producer block verbatim (it is pulled out of `rest` by
    /// being named here).
    iss: String,
    /// The `repository` claim (owner/repo), checked against the connected stores and
    /// mapped to the canonical store id.
    repository: String,
    /// The commit sha the workflow ran against — the `commit` an event records.
    #[serde(default)]
    sha: Option<String>,
    /// The workflow run id — the run component of the ledger's dedup key (HUB.md §2).
    /// Recorded into the producer block as `run` so the store's dedup keys on it.
    #[serde(default)]
    run_id: Option<String>,
    /// Every other claim in the token, retained so the producer block is verbatim (the
    /// trust judgment stays re-derivable, HUB.md §4). Flattened, so `aud`/`workflow`/
    /// `ref`/... land here without being named above.
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

/// A verified pipeline identity: the token's claims, ready to record on every event.
///
/// Built only by [`OidcVerifier::verify`], so a `VerifiedProducer` existing is proof the
/// token passed every check. It exposes the two values the ingest gate needs to build an
/// event — the [`store`](VerifiedProducer::store) the claim lives in and the
/// [`commit`](VerifiedProducer::commit) it was checked at — and the
/// [`producer`](VerifiedProducer::producer) block recorded verbatim.
#[derive(Debug, Clone)]
pub struct VerifiedProducer {
    store: String,
    commit: Option<String>,
    producer: claim_hub_core::Producer,
}

impl VerifiedProducer {
    /// Build the verified producer from the decoded claims.
    ///
    /// The `producer` block is every claim the token carried — the named fields
    /// re-inserted beside the flattened rest — plus a `run` key set to the workflow run
    /// id, which is the name the storage layer's dedup key reads (HUB.md §2). Recording
    /// the claims whole keeps the trust judgment re-derivable (HUB.md §4); adding `run`
    /// is the one normalization, so a GitHub token's `run_id` and a future producer's
    /// own run identifier both dedup through one key.
    fn new(claims: OidcClaims) -> Self {
        let store = repository_to_store(&claims.repository);
        let commit = claims.sha.clone();

        let mut block = claims.rest;
        // `iss` and `repository` are named fields (pulled out of `rest`), so re-insert
        // them to keep the block a verbatim record of every verified claim.
        block.insert("iss".to_owned(), serde_json::Value::String(claims.iss));
        block.insert(
            "repository".to_owned(),
            serde_json::Value::String(claims.repository),
        );
        if let Some(sha) = claims.sha {
            block.insert("sha".to_owned(), serde_json::Value::String(sha));
        }
        // `run` is the dedup key's run component (the storage layer reads `producer.run`).
        // GitHub spells it `run_id`; carry both so the raw claim survives verbatim and
        // the dedup key resolves. A token with no run id yields no `run`, and the ledger
        // append rejects that run-less verdict loudly (invariant #6).
        if let Some(run_id) = claims.run_id {
            block.insert(
                "run_id".to_owned(),
                serde_json::Value::String(run_id.clone()),
            );
            block.insert("run".to_owned(), serde_json::Value::String(run_id));
        }

        Self {
            store,
            commit,
            producer: claim_hub_core::Producer(block),
        }
    }

    /// The canonical store id the verified repository maps to (e.g.
    /// `github.com/acme/payments`), as events and the registry key on it.
    #[must_use]
    pub fn store(&self) -> &str {
        &self.store
    }

    /// The commit sha the workflow ran against, if the token carried one.
    #[must_use]
    pub fn commit(&self) -> Option<&str> {
        self.commit.as_deref()
    }

    /// The verified identity block, recorded verbatim on every event this push produces.
    #[must_use]
    pub fn producer(&self) -> &claim_hub_core::Producer {
        &self.producer
    }
}

/// Map a GitHub `repository` claim (`owner/repo`) to the canonical store id events and
/// the registry use (`github.com/owner/repo`).
///
/// The registry and the connected-store config name a GitHub store as
/// `github.com/owner/repo`, but the OIDC token's `repository` claim is the bare
/// `owner/repo`. This is the one place the two spellings meet; keeping it a named
/// function makes the mapping obvious and testable, and gives a single seam if a later
/// forge names stores differently.
fn repository_to_store(repository: &str) -> String {
    format!("github.com/{repository}")
}

/// The outcome of a failed verification: a rejection the producer caused, or a fault
/// the hub hit.
///
/// The split is load-bearing (invariant #1, #6): a *rejection* is a token that is
/// definitely invalid — the gate answers 4xx and counts it. An *unavailable* is the
/// hub being unable to verify *right now* (the JWKS could not be fetched); answering
/// 4xx there would call a possibly-valid token forged, so the gate answers 503 and the
/// producer retries — a broken verifier never manufactures a rejection.
#[derive(Debug)]
pub enum VerifyError {
    /// The token is invalid; the specific reason is named for the producer's 4xx.
    Reject(AuthReject),
    /// Verification could not be performed (JWKS unreachable); a transient fault, not a
    /// verdict on the token. The string is the operator-facing detail.
    Unavailable(String),
}

/// Why a token was rejected, each mapping to a producer-facing reason.
///
/// Every variant is a *loud* 4xx (invariant #6): the producer is told exactly what
/// failed so it can fix it, and the rejection is counted at `/status`. None of these
/// is ever coerced toward acceptance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthReject {
    /// No signing key in the issuer's set (even after a refresh) matches the token's
    /// `kid` — nothing the issuer published signed it. A forgery, or a key so old it
    /// has rotated out.
    UnknownKey(String),
    /// The signature did not verify against the resolved key — the token was altered or
    /// signed by the wrong key.
    BadSignature,
    /// The token's `exp` is in the past.
    Expired,
    /// The token's `aud` is not this hub's configured audience — it was minted for a
    /// different service and replayed here.
    WrongAudience,
    /// The token's `iss` is not the configured issuer.
    WrongIssuer,
    /// The token's `repository` is not a connected store; the hub ingests only for the
    /// stores it mirrors.
    UnconnectedRepository(String),
    /// The token could not be parsed or was missing a required piece (a `kid`, a claim).
    Malformed(String),
}

impl AuthReject {
    /// A one-line, producer-facing reason naming what to fix, for the 4xx body and the
    /// log. Never leaks a secret; states the failed check.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            AuthReject::UnknownKey(kid) => {
                format!("token signing key `{kid}` is not in the issuer's published JWKS")
            }
            AuthReject::BadSignature => "token signature did not verify".to_owned(),
            AuthReject::Expired => "token has expired".to_owned(),
            AuthReject::WrongAudience => {
                "token audience does not match this hub's configured audience".to_owned()
            }
            AuthReject::WrongIssuer => "token issuer is not the trusted OIDC issuer".to_owned(),
            AuthReject::UnconnectedRepository(repo) => {
                format!("repository `{repo}` is not a connected store on this hub")
            }
            AuthReject::Malformed(detail) => format!("malformed OIDC token: {detail}"),
        }
    }
}

/// An internal error resolving a key or fetching the JWKS.
///
/// Kept crate-internal: the public surface is [`VerifyError`], which folds these into a
/// rejection versus an unavailability. Separating them here keeps `decoding_key`'s
/// three failure modes distinct (a genuine unknown key, a malformed key, a fetch
/// fault) so `verify` can route each correctly.
#[derive(Debug)]
pub enum OidcError {
    /// The `kid` is not in the issuer's set even after a refresh — an invalid token.
    UnknownKey(String),
    /// A matched JWK could not be turned into a decoding key — a malformed key.
    Key(String),
    /// The JWKS could not be fetched or parsed — a transient fault.
    Fetch(String),
}

impl std::fmt::Display for OidcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OidcError::UnknownKey(kid) => write!(f, "unknown signing key id `{kid}`"),
            OidcError::Key(reason) => write!(f, "malformed signing key: {reason}"),
            OidcError::Fetch(reason) => write!(f, "could not fetch JWKS: {reason}"),
        }
    }
}

impl std::error::Error for OidcError {}

/// A `dyn`-compatible token verifier, so the app state can hold *a* verifier without
/// naming the [`JwksSource`] behind it.
///
/// [`OidcVerifier`] is generic over its source (HTTP in production, an injected key set
/// in tests), but axum's shared state must be one concrete type. This trait erases the
/// source: production wires an `Arc<OidcVerifier<HttpJwksSource>>` and a test wires an
/// `Arc<OidcVerifier<TestJwksSource>>`, both as `Arc<dyn TokenVerifier>`. The method
/// returns a boxed future (async methods are not `dyn`-compatible directly), which costs
/// one allocation per verification — negligible against the network round trip a real
/// verify may do.
pub trait TokenVerifier: Send + Sync {
    /// Verify one OIDC id-token; see [`OidcVerifier::verify`] for the contract.
    fn verify<'a>(
        &'a self,
        token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<VerifiedProducer, VerifyError>> + Send + 'a>>;
}

impl<S: JwksSource + 'static> TokenVerifier for OidcVerifier<S> {
    fn verify<'a>(
        &'a self,
        token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<VerifiedProducer, VerifyError>> + Send + 'a>> {
        Box::pin(OidcVerifier::verify(self, token))
    }
}

/// The shared, thread-safe verifier the app state holds — source-erased behind
/// [`TokenVerifier`]. Handlers clone the `Arc` cheaply.
pub type SharedVerifier = Arc<dyn TokenVerifier>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn repository_maps_to_the_canonical_github_store_id() {
        assert_eq!(
            repository_to_store("acme/payments"),
            "github.com/acme/payments"
        );
    }

    /// A JWKS source that always returns an empty set (so every `kid` misses) and counts
    /// fetches — for the debounce tests, which assert the *number of outbound fetches*.
    struct CountingEmptySource {
        fetches: Arc<AtomicU64>,
    }

    impl JwksSource for CountingEmptySource {
        async fn fetch(&self) -> Result<JwkSet, OidcError> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::from_value(serde_json::json!({ "keys": [] })).unwrap())
        }
    }

    /// A cache over a counting empty source, with an explicit debounce and a controllable
    /// clock, plus a handle to the fetch counter and the clock's millis.
    fn debounced_cache(
        debounce_ms: u64,
    ) -> (
        JwksCache<CountingEmptySource>,
        Arc<AtomicU64>,
        Arc<AtomicU64>,
    ) {
        let fetches = Arc::new(AtomicU64::new(0));
        let now = Arc::new(AtomicU64::new(0));
        let clock_now = now.clone();
        let clock: MonotonicMillis = Arc::new(move || clock_now.load(Ordering::SeqCst));
        let cache = JwksCache::with_debounce(
            CountingEmptySource {
                fetches: fetches.clone(),
            },
            debounce_ms,
            clock,
        );
        (cache, fetches, now)
    }

    #[tokio::test]
    async fn many_unknown_kids_in_one_window_fetch_at_most_once() {
        // The amplification cap at the cache layer: the cold fetch populates once; every
        // subsequent unknown-kid lookup within the window is answered from the cache with
        // no new fetch, however many distinct kids arrive.
        let (cache, fetches, _now) = debounced_cache(60_000);
        for n in 0..100 {
            let err = cache
                .decoding_key(&format!("kid-{n}"))
                .await
                .expect_err("every kid is unknown against an empty set");
            assert!(matches!(err, OidcError::UnknownKey(_)));
        }
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            1,
            "100 distinct unknown kids in one window drove exactly one fetch"
        );
    }

    #[tokio::test]
    async fn a_refresh_is_allowed_again_after_the_window_passes() {
        // The debounce is a floor, not a permanent lock: once the window elapses, a miss is
        // allowed to refresh again. Two windows → at most two fetches.
        let (cache, fetches, now) = debounced_cache(1_000);
        let _ = cache.decoding_key("a").await; // cold fetch (1)
        let _ = cache.decoding_key("b").await; // within window: suppressed
        assert_eq!(fetches.load(Ordering::SeqCst), 1);

        now.store(1_001, Ordering::SeqCst); // past the window
        let _ = cache.decoding_key("c").await; // refresh allowed (2)
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            2,
            "a new window permits one more refresh"
        );
    }

    #[tokio::test]
    async fn a_fetch_failure_surfaces_and_does_not_poison_the_debounce() {
        // A source whose first fetch fails: the cold fetch errors (a transient fault), and
        // because it did not succeed the last-fetch time is not stamped, so the next
        // attempt may fetch again rather than being wrongly debounced against a failure.
        struct FailFirst {
            calls: Arc<AtomicU64>,
        }
        impl JwksSource for FailFirst {
            async fn fetch(&self) -> Result<JwkSet, OidcError> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err(OidcError::Fetch("boom".into()))
                } else {
                    Ok(serde_json::from_value(serde_json::json!({ "keys": [] })).unwrap())
                }
            }
        }
        let calls = Arc::new(AtomicU64::new(0));
        let now = Arc::new(AtomicU64::new(0));
        let clock: MonotonicMillis = Arc::new({
            let now = now.clone();
            move || now.load(Ordering::SeqCst)
        });
        let cache = JwksCache::with_debounce(
            FailFirst {
                calls: calls.clone(),
            },
            60_000,
            clock,
        );
        // First: the cold fetch fails outright.
        assert!(matches!(
            cache.decoding_key("a").await,
            Err(OidcError::Fetch(_))
        ));
        // Second: not debounced against the failure — it fetches again (now succeeds),
        // then rejects the unknown kid against the empty set.
        assert!(matches!(
            cache.decoding_key("a").await,
            Err(OidcError::UnknownKey(_))
        ));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a failed fetch did not lock out a retry"
        );
    }

    #[test]
    fn a_verified_producer_records_run_and_repository_verbatim() {
        // The producer block must carry the raw claims plus the `run` the dedup key
        // reads. Build the claims as they would decode and check the block.
        let mut rest = serde_json::Map::new();
        rest.insert(
            "workflow".to_owned(),
            serde_json::Value::String("verify".to_owned()),
        );
        let claims = OidcClaims {
            iss: GITHUB_ACTIONS_ISSUER.to_owned(),
            repository: "acme/payments".to_owned(),
            sha: Some("8f2c0a1".to_owned()),
            run_id: Some("1234567890".to_owned()),
            rest,
        };
        let producer = VerifiedProducer::new(claims);
        assert_eq!(producer.store(), "github.com/acme/payments");
        assert_eq!(producer.commit(), Some("8f2c0a1"));
        let block = &producer.producer().0;
        // The raw claims survive, including the named `iss` re-inserted verbatim.
        assert_eq!(block["iss"], serde_json::json!(GITHUB_ACTIONS_ISSUER));
        assert_eq!(block["workflow"], serde_json::json!("verify"));
        assert_eq!(block["repository"], serde_json::json!("acme/payments"));
        assert_eq!(block["run_id"], serde_json::json!("1234567890"));
        // And `run` is normalized in for the dedup key.
        assert_eq!(block["run"], serde_json::json!("1234567890"));
    }

    #[test]
    fn a_producer_with_no_run_id_carries_no_run() {
        // A token with no run id yields no `run` key; the ledger append rejects that
        // run-less verdict downstream (invariant #6), rather than this layer inventing a
        // run.
        let claims = OidcClaims {
            iss: GITHUB_ACTIONS_ISSUER.to_owned(),
            repository: "acme/payments".to_owned(),
            sha: Some("abc".to_owned()),
            run_id: None,
            rest: serde_json::Map::new(),
        };
        let producer = VerifiedProducer::new(claims);
        assert!(!producer.producer().0.contains_key("run"));
    }

    #[test]
    fn reject_reasons_name_what_failed() {
        assert!(AuthReject::Expired.reason().contains("expired"));
        assert!(AuthReject::WrongAudience.reason().contains("audience"));
        assert!(AuthReject::BadSignature.reason().contains("signature"));
        assert!(AuthReject::UnconnectedRepository("acme/x".to_owned())
            .reason()
            .contains("acme/x"));
    }
}
