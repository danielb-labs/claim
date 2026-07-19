//! Seed a file-backed hub for the backup-restore exercise: sync a git fixture and
//! ingest one attested verdict, writing a real SQLite database the restore step copies.
//!
//! This is the seeding half of `scripts/hub-backup-restore.sh` (hub-15). It stands up the
//! **real** hub over a real file — the real `claim-hub-store` sync, the real ingest gate
//! (OIDC signature verification against an injected JWKS built from the committed test
//! key, envelope validation, verbatim append) — so the database it leaves behind is the
//! shape a production ingest produces, not a hand-written row. The shell script then boots
//! the real `claim-hub` server binary over this file, backs the file up, restores it into a
//! fresh location, and asserts the restored hub derives an identical answer.
//!
//! It is network-free and deterministic: a local git repository is the sync remote, and the
//! JWKS is injected (no GitHub, no wall-clock in the derivation) — the same discipline the
//! in-process integration tests keep (CLAUDE.md). The token's own `exp` is anchored to real
//! time, inherent to JWT expiry, exactly as the test harness documents; nothing the hub
//! *derives* depends on it.
//!
//! Usage: `cargo run -p claim-hub --example seed_hub -- <db-path> <fixture-git-dir>`, where
//! `<fixture-git-dir>` is a git repository carrying a `.claims/` store (the script builds
//! one). On success it prints the seeded claim id to stdout, which the script reads back.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use claim_core::Timestamp;
use claim_hub::app::{AppState, Clock};
use claim_hub::oidc::{JwksSource, OidcError, OidcVerifier};
use claim_hub_store::{sync_store, ConnectedStore, SqliteStore};
use http_body_util::BodyExt;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::json;
use tower::ServiceExt;

/// The connected store id and the fixture's repository the token attests to. The store id
/// is `github.com/<repo>`; the token's `repository` claim is the bare `<repo>`.
const STORE: &str = "github.com/acme/payments";
const REPOSITORY: &str = "acme/payments";
const ISSUER: &str = "https://token.actions.githubusercontent.com";
const AUDIENCE: &str = "https://hub.seed.example";
const KID: &str = "test-key-1";

/// The claim the fixture carries and the verdict attests. A 30-day window gives the read a
/// concrete `stale_at` to compare across the restore.
const CLAIM_ID: &str = "payments/libfoo-pin";

/// A fixed ingest instant, so the seeded verdict's `reported_at` (and thus the derived
/// `verified_as_of`/`stale_at`) is deterministic and identical before and after restore.
const INGEST_INSTANT: &str = "2026-07-18T12:00:00Z";

/// The committed test signing key (shared with the integration harness) and its published
/// RSA modulus. The verifier checks the token's signature against the JWKS built from this
/// modulus, so the ingest path runs its real signature check — not a bypass.
const SIGNING_KEY_PEM: &str = include_str!("../tests/fixtures/oidc_signing_key.pem");
const SIGNING_KEY_N: &str = "3hH-i_453jmtKreB-0eTSU5ZZoIDrEgoSBYiiInwkBak6yF8OZGvMwRl-TkP0GVbO2QSEXcWXwDJIzweGBqG-bQg3aPhL7X7S-iDHK4DCxJMdyIBMrSQByXhqrlFak1d_onJfwlmiBJ0Qn-QJwAcnPbbSeoVclIY1drRDGS4ePdhCieGtjvelfd8tVPFauni9Ji6rtyJ55A1PbC63dmIKDUkS8hwQqizH47niEo9RwbmLdjf5LiAoWKoVrG9mLlwf02ZxsyMtdsasvAzglE5YjJNtCHfA4RW7HQitlyT4e5AH1YCF4LQZFCsbeCYc_dU7HQaZw8v9_GCA7QFiclfiQ";

/// A JWKS source publishing the seeding key, injected so the real verifier's signature
/// check runs against a key we control — no network, deterministic.
#[derive(Clone)]
struct SeedJwks;

impl JwksSource for SeedJwks {
    async fn fetch(&self) -> Result<JwkSet, OidcError> {
        Ok(serde_json::from_value(json!({
            "keys": [{
                "kty": "RSA", "use": "sig", "alg": "RS256",
                "kid": KID, "n": SIGNING_KEY_N, "e": "AQAB",
            }]
        }))
        .expect("valid seeding JWKS"))
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("seed_hub: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let db_path = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: seed_hub <db-path> <fixture-git-dir>"))?;
    let fixture_dir = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: seed_hub <db-path> <fixture-git-dir>"))?;

    // The real file-backed store: open creates and migrates it, the self-host first-boot
    // path. The script points this at a fresh directory.
    let store = SqliteStore::open(&db_path).await?;

    // Real registry sync over the local git fixture — the same code path production sync
    // runs, only the remote is a local path so there is no network.
    let mirror_root = Path::new(&db_path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("_mirror");
    let connected = ConnectedStore::new(STORE, &fixture_dir);
    let outcome = sync_store(&store, &connected, &mirror_root).await?;
    anyhow::ensure!(
        outcome.claims_indexed >= 1,
        "the fixture indexed no claims (looked in {fixture_dir}/.claims)"
    );

    // Drive the REAL ingest gate: build the app with the real OIDC verifier over the
    // injected JWKS and a fixed ingest clock, then POST a signed, attested verdict. This
    // exercises signature verification, envelope validation, and verbatim append against
    // the real file store — the seed is a genuinely ingested verdict, not a fabricated row.
    let verifier = OidcVerifier::new(ISSUER, AUDIENCE, [REPOSITORY.to_owned()], SeedJwks);
    let ingest_clock: Clock = Arc::new(|| INGEST_INSTANT.parse::<Timestamp>().expect("instant"));
    let state = AppState::new(store, Some(Arc::new(verifier))).with_clock(ingest_clock);
    let app = claim_hub::build_app(state);

    let token = sign_seed_token();
    let body = held_report(CLAIM_ID);
    let request = Request::builder()
        .method("POST")
        .uri("/ingest")
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::from(body))
        .expect("build ingest request");
    let response = app.oneshot(request).await.expect("ingest response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("read ingest body")
        .to_bytes();
    anyhow::ensure!(
        status.is_success(),
        "ingest rejected the seed verdict ({status}): {}",
        String::from_utf8_lossy(&bytes)
    );

    // The script reads this to know which claim to query on both hubs.
    println!("{CLAIM_ID}");
    Ok(())
}

/// Sign a valid GitHub-Actions-shaped id-token for the seed verdict with the committed key.
///
/// The token's `iat`/`exp` are anchored to real time because `jsonwebtoken` validates `exp`
/// against the system clock, which no parameter overrides — the same inherent real-time
/// input the test harness documents. It is irrelevant to what the hub derives from the
/// ingested event.
fn sign_seed_token() -> String {
    let now = Timestamp::now().as_second();
    let body = json!({
        "iss": ISSUER,
        "aud": AUDIENCE,
        "iat": now,
        "exp": now + 3600,
        "repository": REPOSITORY,
        "repository_owner": "acme",
        "workflow": "verify",
        "ref": "refs/heads/main",
        "sha": "8f2c0a1b3d4e5f60718293a4b5c6d7e8f9012345",
        "run_id": "seed-run-1",
    });
    let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(KID.to_owned());
    let key = EncodingKey::from_rsa_pem(SIGNING_KEY_PEM.as_bytes()).expect("valid RSA PEM");
    encode(&header, &body, &key).expect("sign seed token")
}

/// A one-check `held` `claim check --json` report body for `claim_id` — what a producer
/// POSTs. The sole check is declared index 0, matching the fixture claim's one check.
fn held_report(claim_id: &str) -> String {
    json!({
        "status": "ok",
        "exit": 0,
        "checked": 1,
        "ran": 1,
        "skipped": 0,
        "claims": [{
            "id": claim_id,
            "file": ".claims/pin.md",
            "checks": [{
                "index": 0,
                "verdict": "held",
                "end": { "kind": "exited", "code": 0 },
                "detail": "exit 0",
                "evidence": "libfoo==4.2",
            }],
            "skipped": [],
            "supports": [],
            "exit": 0,
        }],
        "errors": [],
    })
    .to_string()
}
