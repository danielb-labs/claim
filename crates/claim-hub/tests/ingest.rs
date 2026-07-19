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
async fn a_skipped_check_before_a_run_check_files_the_verdict_under_the_right_digest() {
    // The load-bearing correctness fix for issue #18: a two-check claim where check 0 is
    // skipped and check 1 runs. The CLI compacts the skipped check out, so the report's
    // `checks` holds only the survivor — at array offset 0, but declared index 1. The
    // hub must key the digest on the *declared* index (1 → digest of check B), never on
    // the array offset (0 → digest of check A). Keying by offset would file B's verdict
    // under A's identity — the wrong-check failure the digest exists to prevent.
    let two_check = "id: payments/two\nchecks:\n  - kind: cmd\n    run: \"true\"\n  - kind: cmd\n    run: \"false\"";
    let (app, store) = app_with(TestJwksSource::with_signing_key()).await;
    let claim = seed_claim(&store, ".claims/two.md", two_check).await;
    let digest_a = check_digest(&claim.checks[0]);
    let digest_b = check_digest(&claim.checks[1]);
    assert_ne!(
        digest_a, digest_b,
        "the two checks have distinct identities"
    );

    // The report a skip-then-run claim produces: only the surviving check, declared
    // index 1, drifted.
    let token = sign_token(&TokenClaims::valid());
    let body = serde_json::json!({
        "status": "ok", "exit": 1, "checked": 1, "ran": 1, "skipped": 1,
        "claims": [{
            "id": "payments/two", "file": ".claims/two.md",
            "checks": [{ "index": 1, "verdict": "drifted", "end": { "kind": "exited", "code": 1 }, "detail": "exit 1" }],
            "skipped": [{ "index": 0, "reason": "parked" }],
            "supports": [], "exit": 1
        }],
        "errors": []
    })
    .to_string();
    let (status, json) = post_ingest(&app, Some(&token), &body).await;
    assert_eq!(status, 200, "the push is accepted: {json}");

    let events = store.scan_from(Position(0)).await.unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0].event;
    assert_eq!(
        event.check.index, 1,
        "the declared index is recorded, not the offset"
    );
    assert_eq!(
        event.check.digest, digest_b,
        "the drift is filed under check B's identity (declared index 1), NOT check A's"
    );
    assert_ne!(
        event.check.digest, digest_a,
        "keying by array offset 0 would have mis-filed it under A — the bug this guards"
    );
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
async fn a_token_missing_its_issuer_is_rejected_not_silently_accepted() {
    // Issuer pinning must not be hollow: `jsonwebtoken` rejects a present-but-wrong
    // `iss` but, without required-spec-claims, would let a token that OMITS `iss`
    // through. A real RS256-signed token with no `iss` claim must be rejected and
    // counted, not accepted with the pinning silently skipped.
    let (app, store) = app_with(TestJwksSource::with_signing_key()).await;
    seed_claim(&store, ".claims/pin.md", PIN_CLAIM).await;

    let token = sign_token_omitting("iss");
    let body = one_check_report("payments/libfoo-pin", "held", None);
    let (status, json) = post_ingest(&app, Some(&token), &body).await;

    assert_eq!(status, 401, "a token with no issuer is rejected: {json}");
    assert!(
        json["error"].as_str().unwrap().contains("iss")
            || json["error"].as_str().unwrap().contains("issuer"),
        "the reason names the missing issuer: {json}"
    );
    assert_no_events(&store).await;
    assert_eq!(get_status(&app).await["rejection_count"], 1);
}

#[tokio::test]
async fn a_token_missing_its_audience_is_rejected_not_silently_accepted() {
    // The same hole for audience: a token minted with no `aud` at all would sail past
    // `set_audience` without required-spec-claims. It must be rejected and counted.
    let (app, store) = app_with(TestJwksSource::with_signing_key()).await;
    seed_claim(&store, ".claims/pin.md", PIN_CLAIM).await;

    let token = sign_token_omitting("aud");
    let body = one_check_report("payments/libfoo-pin", "held", None);
    let (status, json) = post_ingest(&app, Some(&token), &body).await;

    assert_eq!(status, 401, "a token with no audience is rejected: {json}");
    assert!(
        json["error"].as_str().unwrap().contains("aud")
            || json["error"].as_str().unwrap().contains("audience"),
        "the reason names the missing audience: {json}"
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
                "checks": [{ "index": 0, "verdict": "held", "end": { "kind": "exited", "code": 0 }, "detail": "exit 0" }],
                "skipped": [], "supports": [], "exit": 0
            },
            {
                "id": "payments/not-registered", "file": ".claims/x.md",
                "checks": [{ "index": 0, "verdict": "held", "end": { "kind": "exited", "code": 0 }, "detail": "exit 0" }],
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
async fn a_rotated_key_heals_on_a_refresh_once_the_debounce_window_has_passed() {
    // Refresh-on-unknown-kid, now rate-limited. The first fetch returns an empty set (the
    // kid is unknown); the *same request* cannot also refresh — that would be two fetches
    // in one window, exactly the amplification the debounce caps — so it is rejected. Once
    // the debounce window passes, the next request is allowed to refresh, the source now
    // publishes the signing key, and verification succeeds. This proves both that a
    // rotated key heals without a redeploy AND that the heal costs one fetch per window.
    let source = TestJwksSource::empty_then_signing_key();
    let fetches = source.clone();
    let (app, store, jwks_clock) = app_with_jwks_clock(source).await;
    seed_claim(&store, ".claims/pin.md", PIN_CLAIM).await;

    let token = sign_token(&TokenClaims::valid());
    let body = one_check_report("payments/libfoo-pin", "held", None);

    // Request 1 (t=0): cold fetch returns empty; the refresh-on-miss is within the same
    // window as that cold fetch, so it is suppressed and the kid is rejected as unknown.
    let (status1, _json1) = post_ingest(&app, Some(&token), &body).await;
    assert_eq!(
        status1, 401,
        "within the window, the unknown kid is rejected"
    );
    assert_eq!(
        fetches.fetch_count(),
        1,
        "only the cold fetch fired, not a refresh"
    );

    // Advance past the debounce window: the next miss is now allowed to refresh.
    jwks_clock.advance_ms(TEST_DEBOUNCE_MS + 1);

    // Request 2: the refresh is due, fetches the (now-published) signing key, verifies.
    let (status2, json2) = post_ingest(&app, Some(&token), &body).await;
    assert_eq!(
        status2, 200,
        "after the window, the refresh heals the rotation: {json2}"
    );
    assert_eq!(
        fetches.fetch_count(),
        2,
        "exactly one refresh, after the window"
    );

    // A third verify (a redelivery) reuses the now-populated cache: no further fetch.
    let (status3, _json3) = post_ingest(&app, Some(&token), &body).await;
    assert_eq!(status3, 200);
    assert_eq!(fetches.fetch_count(), 2, "a known kid needs no fetch");
}

#[tokio::test]
async fn many_distinct_unknown_kids_within_the_window_trigger_at_most_one_fetch() {
    // The amplification cap: `kid` is attacker-controlled and read before signature
    // verification, so without a debounce each novel kid forces an outbound JWKS fetch.
    // Here the source always publishes the signing key, so the *first* request's cold
    // fetch populates the cache; then a flood of requests bearing distinct unknown kids
    // (forged tokens) arrives within the window. None may trigger a further fetch — the
    // total outbound fetch count stays capped, regardless of how many novel kids arrive.
    let source = TestJwksSource::with_signing_key();
    let fetches = source.clone();
    let (app, store, _jwks_clock) = app_with_jwks_clock(source).await;
    seed_claim(&store, ".claims/pin.md", PIN_CLAIM).await;

    // One legitimate request populates the cache (one cold fetch).
    let good = sign_token(&TokenClaims::valid());
    let body = one_check_report("payments/libfoo-pin", "held", None);
    let (ok, _) = post_ingest(&app, Some(&good), &body).await;
    assert_eq!(ok, 200);
    assert_eq!(
        fetches.fetch_count(),
        1,
        "the cold fetch populated the cache"
    );

    // A flood of forged tokens, each with a distinct, never-published kid, all inside the
    // debounce window. Each is rejected — and, crucially, none drives a new fetch.
    for n in 0..50 {
        let forged = sign_token_with_kid(&TokenClaims::valid(), &format!("attacker-kid-{n}"));
        let (status, _json) = post_ingest(&app, Some(&forged), &body).await;
        assert_eq!(status, 401, "each forged unknown-kid token is rejected");
    }
    assert_eq!(
        fetches.fetch_count(),
        1,
        "50 distinct unknown kids in one window triggered no additional fetch — the \
         amplification vector is capped"
    );
}

#[tokio::test]
async fn an_unknown_kid_that_never_resolves_is_rejected() {
    // The source only ever returns an empty set: the token's kid is unknown, so it is a
    // rejection (a forgery, or a key rotated fully out), not an infinite refresh loop.
    // The cold fetch fires once; the same-window refresh is suppressed by the debounce, so
    // exactly one fetch happens and the token is rejected.
    let source = TestJwksSource::sequence_of_empty();
    let fetches = source.clone();
    let (app, store, _jwks_clock) = app_with_jwks_clock(source).await;
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
    assert_eq!(
        fetches.fetch_count(),
        1,
        "one cold fetch; the same-window refresh is capped"
    );
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
