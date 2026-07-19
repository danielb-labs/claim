//! Ingest-gate integration tests: the trust-critical write path, end to end.
//!
//! Every test drives the assembled app in-process via [`tower::ServiceExt::oneshot`]
//! with a **mocked JWKS** and a **fixed clock** — no network, no wall-clock time. Valid
//! tokens are signed with a fixture RSA key whose public components the injected JWKS
//! publishes; a forged token is signed with a different key. The registry is seeded so a
//! check's digest resolves by position. These cover the item's Done-when: a valid push
//! appends verbatim; a forged signature, an expired token, a wrong audience, and an
//! unconnected repository each reject 4xx and append nothing; a malformed envelope names
//! the field; a redelivery dedups; over-cap evidence is truncated-with-marker; the
//! rejection counter increments and shows at `/status`.

mod common;

use claim_hub_core::check_digest;
use claim_hub_store::{Ledger, Position};
use common::*;

/// The frontmatter of a one-cmd-check claim the tests seed into the registry.
const PIN_CLAIM: &str = "id: payments/libfoo-pin\nchecks:\n  - kind: cmd\n    run: \"grep -q 'libfoo==4.2' requirements.txt\"";

#[tokio::test]
async fn a_valid_token_and_envelope_appends_verbatim_and_returns_the_position() {
    let (app, store) = app_with(TestJwksSource::with_signing_key()).await;
    let claim = seed_claim(&store, ".claims/pin.md", PIN_CLAIM).await;
    let expected_digest = check_digest(&claim.checks[0]);

    let token = sign_token(&TokenClaims::valid());
    let body = one_check_report("payments/libfoo-pin", "held", Some("libfoo==4.2"));
    let (status, json) = post_ingest(&app, Some(&token), &body).await;

    assert_eq!(status, 200, "a valid push is accepted: {json}");
    assert_eq!(json["accepted"], 1);
    let position = json["positions"][0]["position"].as_i64().unwrap();
    assert_eq!(json["positions"][0]["new"], true);
    assert_eq!(position, 1, "first event lands at position 1");

    // The event is on the ledger verbatim, with the verified identity and the
    // registry-derived digest — not anything the report claimed about itself.
    let events = store.scan_from(Position(0)).await.unwrap();
    assert_eq!(events.len(), 1, "exactly one event appended");
    let event = &events[0].event;
    assert_eq!(event.claim, "payments/libfoo-pin");
    assert_eq!(event.verdict, claim_core::Verdict::Held);
    assert_eq!(event.check.index, 0);
    assert_eq!(
        event.check.digest, expected_digest,
        "the digest is the registry's, computed from the check definition (issue #18)"
    );
    assert_eq!(
        event.store, TEST_STORE,
        "store comes from the verified repository"
    );
    assert_eq!(
        event.commit, "8f2c0a1b3d4e5f60718293a4b5c6d7e8f9012345",
        "commit comes from the token's sha, not the report"
    );
    assert_eq!(event.evidence.as_deref(), Some("libfoo==4.2"));
    assert_eq!(
        event.reported_at,
        ingest_instant(),
        "the ingest clock stamped it"
    );

    // The producer block holds the verified identity verbatim (HUB.md §4).
    let producer = &event.producer.0;
    assert_eq!(producer["iss"], serde_json::json!(TEST_ISSUER));
    assert_eq!(producer["repository"], serde_json::json!(TEST_REPOSITORY));
    assert_eq!(producer["workflow"], serde_json::json!("verify"));
    assert_eq!(producer["run_id"], serde_json::json!("1234567890"));
    // `run` is normalized in for the dedup key.
    assert_eq!(producer["run"], serde_json::json!("1234567890"));

    // A valid push counts no rejection.
    assert_eq!(get_status(&app).await["rejection_count"], 0);
    assert_eq!(get_status(&app).await["ledger_head"], 1);
}

#[tokio::test]
async fn a_forged_signature_is_rejected_4xx_and_appends_nothing() {
    // A token signed with the *wrong* key but presented under the published kid: its
    // signature does not verify against the JWKS's key. A forgery.
    let (app, store) = app_with(TestJwksSource::with_signing_key()).await;
    seed_claim(&store, ".claims/pin.md", PIN_CLAIM).await;

    let token = sign_token_with_wrong_key(&TokenClaims::valid());
    let body = one_check_report("payments/libfoo-pin", "held", None);
    let (status, json) = post_ingest(&app, Some(&token), &body).await;

    assert_eq!(status, 401, "a forged signature is rejected: {json}");
    assert!(
        json["error"].as_str().unwrap().contains("signature"),
        "the reason names the signature: {json}"
    );
    assert_no_events(&store).await;
    assert_eq!(get_status(&app).await["rejection_count"], 1, "counted");
}

#[tokio::test]
async fn an_expired_token_is_rejected_4xx_and_appends_nothing() {
    let (app, store) = app_with(TestJwksSource::with_signing_key()).await;
    seed_claim(&store, ".claims/pin.md", PIN_CLAIM).await;

    let mut claims = TokenClaims::valid();
    claims.ttl_secs = -3600; // expired an hour before the ingest instant.
    let token = sign_token(&claims);
    let body = one_check_report("payments/libfoo-pin", "held", None);
    let (status, json) = post_ingest(&app, Some(&token), &body).await;

    assert_eq!(status, 401, "an expired token is rejected: {json}");
    assert!(
        json["error"].as_str().unwrap().contains("expired"),
        "the reason names expiry: {json}"
    );
    assert_no_events(&store).await;
    assert_eq!(get_status(&app).await["rejection_count"], 1);
}

#[tokio::test]
async fn a_wrong_audience_is_rejected_4xx_and_appends_nothing() {
    let (app, store) = app_with(TestJwksSource::with_signing_key()).await;
    seed_claim(&store, ".claims/pin.md", PIN_CLAIM).await;

    let mut claims = TokenClaims::valid();
    claims.audience = "https://some-other-hub.example".to_owned();
    let token = sign_token(&claims);
    let body = one_check_report("payments/libfoo-pin", "held", None);
    let (status, json) = post_ingest(&app, Some(&token), &body).await;

    assert_eq!(status, 401, "a wrong audience is rejected: {json}");
    assert!(
        json["error"].as_str().unwrap().contains("audience"),
        "the reason names the audience: {json}"
    );
    assert_no_events(&store).await;
    assert_eq!(get_status(&app).await["rejection_count"], 1);
}

#[tokio::test]
async fn an_unconnected_repository_is_rejected_4xx_and_appends_nothing() {
    let (app, store) = app_with(TestJwksSource::with_signing_key()).await;
    seed_claim(&store, ".claims/pin.md", PIN_CLAIM).await;

    // A validly-signed token from a repository the hub does not track.
    let mut claims = TokenClaims::valid();
    claims.repository = "acme/not-connected".to_owned();
    let token = sign_token(&claims);
    let body = one_check_report("payments/libfoo-pin", "held", None);
    let (status, json) = post_ingest(&app, Some(&token), &body).await;

    assert_eq!(
        status, 403,
        "an unconnected repository is authentic but unauthorized: {json}"
    );
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .contains("acme/not-connected"),
        "the reason names the repository: {json}"
    );
    assert_no_events(&store).await;
    assert_eq!(get_status(&app).await["rejection_count"], 1);
}

#[tokio::test]
async fn a_malformed_envelope_is_rejected_naming_the_field() {
    let (app, store) = app_with(TestJwksSource::with_signing_key()).await;
    seed_claim(&store, ".claims/pin.md", PIN_CLAIM).await;

    let token = sign_token(&TokenClaims::valid());
    // A report missing the required top-level `exit` field.
    let body = r#"{
        "status": "ok", "checked": 1, "ran": 1, "skipped": 0,
        "claims": [], "errors": []
    }"#;
    let (status, json) = post_ingest(&app, Some(&token), body).await;

    assert_eq!(status, 400, "a malformed envelope is a 400: {json}");
    assert!(
        json["error"].as_str().unwrap().contains("exit"),
        "the reason names the missing field: {json}"
    );
    assert_no_events(&store).await;
    assert_eq!(get_status(&app).await["rejection_count"], 1);
}

#[tokio::test]
async fn a_claim_not_in_the_registry_is_rejected_not_fabricated() {
    // The registry knows nothing of this claim (not synced): the gate cannot honestly
    // identify the check, so it refuses rather than filing under a fabricated digest.
    let (app, store) = app_with(TestJwksSource::with_signing_key()).await;
    // Deliberately do NOT seed the claim.
    let token = sign_token(&TokenClaims::valid());
    let body = one_check_report("payments/libfoo-pin", "held", None);
    let (status, json) = post_ingest(&app, Some(&token), &body).await;

    assert_eq!(status, 400, "an unknown claim/check is rejected: {json}");
    assert!(
        json["error"].as_str().unwrap().contains("not be synced")
            || json["error"].as_str().unwrap().contains("registry"),
        "the reason explains the unsynced registry: {json}"
    );
    assert_no_events(&store).await;
    assert_eq!(get_status(&app).await["rejection_count"], 1);
}

#[tokio::test]
async fn a_push_with_one_unknown_claim_appends_nothing_at_all() {
    // A report carrying a registered claim *and* an unregistered one: the whole push is
    // refused and appends nothing, so a partial write can never leave the good claim's
    // verdict on the ledger while the bad one's is dropped — the rejection is atomic.
    let (app, store) = app_with(TestJwksSource::with_signing_key()).await;
    seed_claim(&store, ".claims/pin.md", PIN_CLAIM).await;

    let token = sign_token(&TokenClaims::valid());
    let body = serde_json::json!({
        "status": "ok", "exit": 0, "checked": 2, "ran": 2, "skipped": 0,
        "claims": [
            {
                "id": "payments/libfoo-pin", "file": ".claims/pin.md",
                "checks": [{ "verdict": "held", "end": { "kind": "exited", "code": 0 }, "detail": "exit 0" }],
                "skipped": [], "supports": [], "exit": 0
            },
            {
                "id": "payments/not-registered", "file": ".claims/x.md",
                "checks": [{ "verdict": "held", "end": { "kind": "exited", "code": 0 }, "detail": "exit 0" }],
                "skipped": [], "supports": [], "exit": 0
            }
        ],
        "errors": []
    })
    .to_string();
    let (status, json) = post_ingest(&app, Some(&token), &body).await;

    assert_eq!(status, 400, "the whole push is refused: {json}");
    assert_no_events(&store).await;
    assert_eq!(get_status(&app).await["rejection_count"], 1);
}

#[tokio::test]
async fn a_redelivery_dedups_to_the_original_success_and_adds_no_row() {
    let (app, store) = app_with(TestJwksSource::with_signing_key()).await;
    seed_claim(&store, ".claims/pin.md", PIN_CLAIM).await;

    let token = sign_token(&TokenClaims::valid());
    let body = one_check_report("payments/libfoo-pin", "held", None);

    let (status1, json1) = post_ingest(&app, Some(&token), &body).await;
    assert_eq!(status1, 200);
    assert_eq!(json1["positions"][0]["new"], true);
    let first_position = json1["positions"][0]["position"].as_i64().unwrap();

    // The same run reporting the same check is the same observation (HUB.md §2): a
    // redelivery. It succeeds, returns the *original* position, and adds no row.
    let (status2, json2) = post_ingest(&app, Some(&token), &body).await;
    assert_eq!(status2, 200, "a redelivery is idempotent success: {json2}");
    assert_eq!(
        json2["positions"][0]["new"], false,
        "deduped, not newly appended"
    );
    assert_eq!(
        json2["positions"][0]["position"].as_i64().unwrap(),
        first_position,
        "the original position is returned"
    );

    let events = store.scan_from(Position(0)).await.unwrap();
    assert_eq!(events.len(), 1, "the redelivery added no second row");
    assert_eq!(get_status(&app).await["ledger_head"], 1);
    assert_eq!(get_status(&app).await["rejection_count"], 0);
}

#[tokio::test]
async fn over_cap_evidence_is_stored_truncated_with_a_marker() {
    use claim_hub_core::EVIDENCE_CAP;
    let (app, store) = app_with(TestJwksSource::with_signing_key()).await;
    seed_claim(&store, ".claims/pin.md", PIN_CLAIM).await;

    let huge = "x".repeat(EVIDENCE_CAP + 4096);
    let token = sign_token(&TokenClaims::valid());
    let body = one_check_report("payments/libfoo-pin", "drifted", Some(&huge));
    let (status, _json) = post_ingest(&app, Some(&token), &body).await;
    assert_eq!(status, 200);

    let events = store.scan_from(Position(0)).await.unwrap();
    let evidence = events[0].event.evidence.as_deref().unwrap();
    assert!(evidence.len() < huge.len(), "evidence shrank");
    assert!(
        evidence.ends_with("[evidence truncated at ingest]"),
        "the cut is marked, never silently dropped (invariant #6)"
    );
}

#[tokio::test]
async fn a_missing_bearer_token_is_401_but_not_a_counted_rejection() {
    // No identity to judge: a 401, but not part of the turned-away-telemetry count,
    // which tracks pushes the hub judged and refused.
    let (app, store) = app_with(TestJwksSource::with_signing_key()).await;
    seed_claim(&store, ".claims/pin.md", PIN_CLAIM).await;
    let body = one_check_report("payments/libfoo-pin", "held", None);
    let (status, json) = post_ingest(&app, None, &body).await;
    assert_eq!(status, 401, "no bearer token: {json}");
    assert_no_events(&store).await;
    assert_eq!(
        get_status(&app).await["rejection_count"],
        0,
        "an unauthenticated request is not a judged-and-refused rejection"
    );
}

#[tokio::test]
async fn the_jwks_cache_refreshes_once_on_an_unknown_kid_then_succeeds() {
    // The first JWKS fetch returns an empty set (the token's kid is unknown), so the
    // cache refreshes; the second fetch publishes the signing key, so verification then
    // succeeds. This proves refresh-on-unknown-kid — and that a rotated key heals without
    // a redeploy — with no network.
    let source = TestJwksSource::empty_then_signing_key();
    let fetches = source.clone();
    let (app, store) = app_with(source).await;
    seed_claim(&store, ".claims/pin.md", PIN_CLAIM).await;

    let token = sign_token(&TokenClaims::valid());
    let body = one_check_report("payments/libfoo-pin", "held", None);
    let (status, json) = post_ingest(&app, Some(&token), &body).await;

    assert_eq!(
        status, 200,
        "verification succeeds after the refresh finds the rotated key: {json}"
    );
    // Exactly two fetches: the initial (empty) resolve miss, then the refresh.
    assert_eq!(
        fetches.fetch_count(),
        2,
        "one initial fetch, one refresh on the unknown kid"
    );

    // A second, unrelated verify (a redelivery) reuses the now-populated cache: no
    // further fetch, since the kid is known.
    let (status2, _json2) = post_ingest(&app, Some(&token), &body).await;
    assert_eq!(status2, 200);
    assert_eq!(
        fetches.fetch_count(),
        2,
        "the cache is reused; no extra fetch for a known kid"
    );
}

#[tokio::test]
async fn an_unknown_kid_that_never_resolves_is_rejected_after_one_refresh() {
    // The source only ever returns an empty set: the token's kid is unknown even after a
    // refresh, so it is a rejection (a forgery, or a key rotated fully out) — not an
    // infinite refresh loop. Signed with the real key, so only the kid-not-published
    // condition rejects it.
    let source = TestJwksSource::sequence_of_empty();
    let fetches = source.clone();
    let (app, store) = app_with(source).await;
    seed_claim(&store, ".claims/pin.md", PIN_CLAIM).await;

    let token = sign_token(&TokenClaims::valid());
    let body = one_check_report("payments/libfoo-pin", "held", None);
    let (status, json) = post_ingest(&app, Some(&token), &body).await;

    assert_eq!(status, 401, "an unknown key is a rejection: {json}");
    assert!(
        json["error"].as_str().unwrap().contains("JWKS")
            || json["error"].as_str().unwrap().contains("key"),
        "the reason names the missing key: {json}"
    );
    assert_no_events(&store).await;
    assert_eq!(get_status(&app).await["rejection_count"], 1);
    // One initial fetch plus exactly one refresh, then it gives up — no loop.
    assert_eq!(fetches.fetch_count(), 2, "one refresh, then reject");
}

/// Assert the ledger is empty — a rejected push appended nothing (invariant #4).
async fn assert_no_events(store: &claim_hub_store::SqliteStore) {
    let events = store.scan_from(Position(0)).await.unwrap();
    assert!(
        events.is_empty(),
        "a rejected push must append no event, found {}",
        events.len()
    );
}
