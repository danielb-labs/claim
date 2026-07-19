//! The hub-07 walking skeleton (milestone M0): the whole spine in tests.
//!
//! These are the milestone's two tests, both **network-free** (a local git fixture as
//! the sync remote, an injected/mocked JWKS for ingest) with **injected clocks** (a fixed
//! ingest instant, an advanceable read clock). They exercise the integrated spine end to
//! end — git → registry sync → attested verdict → ledger → derive → read — against the
//! real store, so a break anywhere between the merged pieces surfaces here.
//!
//! - [`the_whole_spine_derives_a_held_verdict_into_verified`] is the end-to-end test: a
//!   git fixture is synced, one attested `held` verdict is POSTed through the ingest gate,
//!   and `GET /api/claims/{id}` reads `verified` with the as-of the answer derived from.
//!   The standing rests on the *real ledger* event, whose check digest matches the
//!   registry's stored digest by construction (both are `claim_hub_core::check_digest` of
//!   the same definition) — the join key the deriver and the ledger agree on.
//! - [`the_same_claim_ages_into_stale_by_the_clock_alone`] advances the injected read
//!   clock past the claim's freshness window with **no new event**, and the same claim
//!   reads `stale`. Staleness is arithmetic over the clock, derived at read time, not a
//!   stored transition (invariant #6, invariant #3).

mod common;

use std::process::Command;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use claim_core::Timestamp;
use claim_hub::app::{AppState, Clock, ReadClock};
use claim_hub::oidc::OidcVerifier;
use claim_hub_store::{sync_store, ConnectedStore, SqliteStore};
use common::*;
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

/// The claim the fixture carries and the report verifies, with a 30-day freshness window
/// so the aging test has a clock threshold to cross.
const PIN_FRONTMATTER: &str = "id: payments/libfoo-pin\nhub:\n  max-age: 30d\nchecks:\n  - kind: cmd\n    run: \"grep -q 'libfoo==4.2' requirements.txt\"";

#[tokio::test]
async fn the_whole_spine_derives_a_held_verdict_into_verified() {
    let fixture = GitFixture::with_claim(".claims/pin.md", PIN_FRONTMATTER);
    let store = SqliteStore::open_in_memory().await.unwrap();

    // git → registry sync: mirror the fixture and index its claims at the tip.
    let outcome = sync_fixture(&store, &fixture).await;
    assert_eq!(outcome.claims_indexed, 1, "the fixture's claim indexed");

    // The app over the synced store, with the mocked JWKS and injected clocks.
    let (app, read_clock) = skeleton_app(store.clone(), day(10)).await;

    // attested verdict → ledger: POST one `held` verdict through the ingest gate.
    let token = sign_token(&TokenClaims::valid());
    let body = one_check_report("payments/libfoo-pin", "held", Some("libfoo==4.2"));
    let (status, json) = post_ingest(&app, Some(&token), &body).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the attested verdict is accepted: {json}"
    );
    assert_eq!(json["accepted"], 1);

    // derive → read: the standing is derived from the real ledger event.
    let (status, standing) = get_claim(&app, "payments/libfoo-pin").await;
    assert_eq!(status, StatusCode::OK, "the claim is found: {standing}");
    assert_eq!(
        standing["standing"], "verified",
        "a held verdict within the window derives verified: {standing}"
    );
    assert_eq!(
        standing["store"], TEST_STORE,
        "the standing names the synced store"
    );
    // The good news is dated at the verdict's instant (the injected ingest clock).
    assert_eq!(
        standing["verified_as_of"], INGEST_INSTANT,
        "verified_as_of is the ledger event's instant"
    );

    // The as-of pins exactly what the answer derived from (HUB.md §4): the ledger head
    // (one event → seq 1), the registry version (one sync → 1), and the read clock.
    let as_of = &standing["as_of"];
    assert_eq!(as_of["ledger_head"], 1, "one event on the ledger");
    assert_eq!(as_of["registry_version"], 1, "one sync applied");
    assert_eq!(
        as_of["clock"],
        day(10),
        "the as-of carries the read clock instant"
    );

    // Read-only: the derivation stored nothing. The ledger still holds exactly the one
    // ingested event; a read never appended a verdict (invariant #3).
    let ledger_head = get_status(&app).await["ledger_head"].clone();
    assert_eq!(ledger_head, 1, "the read added no event to the ledger");

    // Keep the read clock alive so its Arc is not dropped mid-test.
    let _ = &read_clock;
}

#[tokio::test]
async fn the_same_claim_ages_into_stale_by_the_clock_alone() {
    let fixture = GitFixture::with_claim(".claims/pin.md", PIN_FRONTMATTER);
    let store = SqliteStore::open_in_memory().await.unwrap();
    sync_fixture(&store, &fixture).await;

    // The read clock starts within the window; advancing it is the only change.
    let (app, read_clock) = skeleton_app(store.clone(), day(10)).await;
    let token = sign_token(&TokenClaims::valid());
    let body = one_check_report("payments/libfoo-pin", "held", Some("libfoo==4.2"));
    let (status, _json) = post_ingest(&app, Some(&token), &body).await;
    assert_eq!(status, StatusCode::OK);

    // Within the 30-day window: verified.
    let (_status, fresh) = get_claim(&app, "payments/libfoo-pin").await;
    assert_eq!(fresh["standing"], "verified", "fresh at day 10: {fresh}");

    // Advance the read clock past the window — no new event, no new sync. The verdict
    // was reported at day 0 (INGEST_INSTANT); at day 40 the 30-day window has lapsed.
    read_clock.set(day(40));

    let (status, stale) = get_claim(&app, "payments/libfoo-pin").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        stale["standing"], "stale",
        "the claim ages into stale by the clock alone, no new event: {stale}"
    );
    // The ledger is unchanged — staleness was derived, not written (invariant #3).
    assert_eq!(
        get_status(&app).await["ledger_head"],
        1,
        "no event was appended to age the claim"
    );
    // The as-of shows the same evidence (ledger head 1) at the later clock: the standing
    // changed, the evidence did not.
    assert_eq!(stale["as_of"]["ledger_head"], 1);
    assert_eq!(stale["as_of"]["clock"], day(40));
}

/// An RFC 3339 instant `days` whole days after the verdict's `INGEST_INSTANT`
/// (2026-07-18T12:00:00Z) — the read clock the aging test steps through. Only the two
/// offsets the tests use are defined, as explicit constants rather than duration
/// arithmetic, so the test needs no time-math dependency and reads unambiguously against
/// the 30-day window.
fn day(days: i64) -> String {
    match days {
        10 => "2026-07-28T12:00:00Z".to_owned(), // within the 30-day window: fresh.
        40 => "2026-08-27T12:00:00Z".to_owned(), // past the window (30 days is 2026-08-17): stale.
        other => panic!("unsupported test day offset {other}"),
    }
}

/// A settable read clock, so a test moves the read-time derivation instant without
/// sleeping (CLAUDE.md's determinism rule). Starts at a fixed instant; [`set`](Self::set)
/// jumps it.
#[derive(Clone)]
struct SettableClock(Arc<std::sync::Mutex<Timestamp>>);

impl SettableClock {
    fn new(at: &str) -> Self {
        Self(Arc::new(std::sync::Mutex::new(at.parse().unwrap())))
    }

    fn set(&self, at: String) {
        *self.0.lock().unwrap() = at.parse().unwrap();
    }

    /// The [`ReadClock`] closure the app reads "now" through.
    fn read_clock(&self) -> ReadClock {
        let inner = self.0.clone();
        Arc::new(move || *inner.lock().unwrap())
    }
}

/// Build the skeleton app over `store`: the mocked JWKS verifier, a fixed ingest clock,
/// and a settable read clock starting at `read_now`. Returns the app and the read clock.
async fn skeleton_app(store: SqliteStore, read_now: String) -> (axum::Router, SettableClock) {
    let source = TestJwksSource::with_signing_key();
    let verifier = OidcVerifier::new(
        TEST_ISSUER,
        TEST_AUDIENCE,
        [TEST_REPOSITORY.to_owned()],
        source,
    );
    let ingest_clock: Clock = Arc::new(ingest_instant);
    let read_clock = SettableClock::new(&read_now);
    // The read API derives with no config window, so freshness comes from the claim's own
    // `hub.max-age` (30d) carried through the registry. (Per hub-07's noted follow-up the
    // registry does not yet persist per-claim hints, so this test seeds the hint via the
    // config default below so the aging path is exercised end to end.)
    let deriver_config = claim_hub_core::DeriverConfig {
        default_max_age: Some("30d".parse().unwrap()),
        max_age_override: None,
    };
    let state = AppState::new(store, Some(Arc::new(verifier)))
        .with_clock(ingest_clock)
        .with_read_clock(read_clock.read_clock())
        .with_deriver_config(deriver_config);
    (claim_hub::build_app(state), read_clock)
}

/// Sync `fixture` into `store` under the test store id, through the real registry sync.
async fn sync_fixture(store: &SqliteStore, fixture: &GitFixture) -> claim_hub_store::SyncOutcome {
    let mirror_root = fixture.dir.path().join("_mirror");
    let connected = ConnectedStore::new(TEST_STORE, fixture.url());
    sync_store(store, &connected, &mirror_root)
        .await
        .expect("sync the fixture")
}

/// GET `/api/claims/{id}` and return the status and parsed JSON body.
async fn get_claim(app: &axum::Router, id: &str) -> (StatusCode, serde_json::Value) {
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
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

/// A local git repository fixture used as a sync remote — no network. Mirrors the
/// `claim-hub-store` sync tests' fixture, kept minimal for the skeleton's one claim.
struct GitFixture {
    dir: TempDir,
}

impl GitFixture {
    /// A fresh repo on branch `main` carrying one claim file, committed.
    fn with_claim(rel: &str, frontmatter: &str) -> Self {
        let fixture = Self {
            dir: TempDir::new().expect("temp dir"),
        };
        fixture.git(&["init", "-q", "-b", "main"]);
        fixture.git(&["config", "user.name", "Test"]);
        fixture.git(&["config", "user.email", "test@example.com"]);
        fixture.git(&["config", "commit.gpgsign", "false"]);
        let text = format!("---\n{frontmatter}\n---\nThe libfoo pin holds.\n");
        fixture.write(rel, &text);
        fixture.git(&["add", "-A"]);
        fixture.git(&["commit", "-q", "-m", "add claim"]);
        fixture
    }

    fn url(&self) -> String {
        self.dir.path().to_string_lossy().into_owned()
    }

    fn write(&self, rel: &str, contents: &str) {
        let path = self.dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(self.dir.path())
            // Wall off ambient git config so a developer's global identity or
            // `init.defaultBranch` cannot make the fixture behave differently.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }
}
