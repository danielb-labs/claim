//! Integration tests for the router / nag engine (hub-11).
//!
//! Every test is **network-free** (a local git fixture as the sync remote so CODEOWNERS
//! resolves from a real mirror) and **clock-injected** (the router's `now` is a parameter),
//! per CLAUDE.md's determinism rule. They pin the "done when" properties of the item:
//!
//! - a drift transition fires **exactly once across a restart** — the load-bearing
//!   fire-once test, proven by dropping the router (its only memory is the ledger) and
//!   reviving it, then asserting the transition does not re-fire;
//! - a **clock-crossing stale** fires with no new verdict — time advances, nothing else;
//! - a **no-owner** transition routes to the dead-letter queue and the served view shows it;
//! - **one commit breaking N claims is one grouped nag**, not N;
//! - a **lapsed skip `until`** fires; and
//! - the rendered nag content is **served** at `GET /api/nags` for the CI glue to pull.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use claim_core::{Timestamp, Verdict};
use claim_hub::app::AppState;
use claim_hub::router::Router;
use claim_hub_core::{check_digest, CheckRef, Event, Producer};
use claim_hub_store::{sync_store, ConnectedStore, Ledger, Position, SqliteStore};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

const STORE: &str = "github.com/acme/payments";

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

/// A drifted verdict event for a claim's first check, at `commit` and `at`, with a distinct
/// producer run so each is a fresh ledger row.
fn verdict_event(
    claim: &claim_core::Claim,
    verdict: Verdict,
    commit: &str,
    run: &str,
    at: &str,
) -> Event {
    let mut producer = serde_json::Map::new();
    producer.insert("run".into(), serde_json::json!(run));
    Event::verdict(
        claim.id.as_str().to_owned(),
        CheckRef {
            index: 0,
            digest: check_digest(&claim.checks[0]),
        },
        verdict,
        commit,
        STORE,
        Producer(producer),
        ts(at),
    )
}

/// Parse a claim from frontmatter for building matching verdict events.
fn claim_of(frontmatter: &str) -> claim_core::Claim {
    let text = format!("---\n{frontmatter}\n---\nStatement body.\n");
    claim_core::parse_claim_file(".claims/x.md", &text).expect("valid claim")
}

/// A local git repository fixture used as a sync remote — no network — carrying claim files
/// and a CODEOWNERS. Its mirror is where the router resolves owners from.
struct GitFixture {
    dir: TempDir,
}

impl GitFixture {
    /// A fresh repo on `main` with the given files (path → contents), committed.
    fn with_files(files: &[(&str, &str)]) -> Self {
        let fixture = Self {
            dir: TempDir::new().expect("temp dir"),
        };
        fixture.git(&["init", "-q", "-b", "main"]);
        fixture.git(&["config", "user.name", "Test"]);
        fixture.git(&["config", "user.email", "test@example.com"]);
        fixture.git(&["config", "commit.gpgsign", "false"]);
        for (rel, contents) in files {
            fixture.write(rel, contents);
        }
        fixture.git(&["add", "-A"]);
        fixture.git(&["commit", "-q", "-m", "seed"]);
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
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }
}

/// A claim file's frontmatter for id `id` with the given extra lines (hub hints, skip).
fn claim_file(id: &str, extra: &str) -> String {
    format!(
        "---\nid: {id}\n{extra}checks:\n  - kind: cmd\n    run: \"true\"\n---\nThe fact holds.\n"
    )
}

/// Sync `fixture` into `store` and return the mirror root the router reads CODEOWNERS from.
async fn sync(store: &SqliteStore, fixture: &GitFixture) -> PathBuf {
    let mirror_root = fixture.dir.path().join("_mirror");
    let connected = ConnectedStore::new(STORE, fixture.url());
    sync_store(store, &connected, &mirror_root)
        .await
        .expect("sync the fixture");
    mirror_root
}

/// A router over `store`, resolving owners from `mirror_root`, deriving with no config
/// window (freshness comes from the claims' own `hub.max-age`).
fn router(store: SqliteStore, mirror_root: &Path) -> Router {
    Router::new(
        store,
        Some(mirror_root.to_path_buf()),
        claim_hub_core::DeriverConfig::default(),
    )
}

/// The count of `nag` events on the ledger — the fires that have been recorded.
async fn nag_count(store: &SqliteStore) -> usize {
    store
        .scan_from(Position(0))
        .await
        .unwrap()
        .iter()
        .filter(|s| claim_hub::router::is_nag(&s.event))
        .count()
}

// ---- (1) a drift transition fires exactly once across a restart ----

#[tokio::test]
async fn a_drift_fires_once_and_never_again_across_a_restart() {
    let fixture = GitFixture::with_files(&[
        (".claims/payments/pin.md", &claim_file("payments/pin", "")),
        (".github/CODEOWNERS", "*  @acme/payments\n"),
    ]);
    let store = SqliteStore::open_in_memory().await.unwrap();
    let mirror_root = sync(&store, &fixture).await;

    // A drifted verdict for the claim: it now stands drifted.
    let pin = claim_of("id: payments/pin\nchecks:\n  - kind: cmd\n    run: \"true\"");
    store
        .append(&verdict_event(
            &pin,
            Verdict::Drifted,
            "deadbeef",
            "run-1",
            "2026-07-01T00:00:00Z",
        ))
        .await
        .unwrap();

    let now = ts("2026-07-02T00:00:00Z");

    // First pass fires the drift once and records the owner.
    let r1 = router(store.clone(), &mirror_root);
    let view = r1.run_once(now).await.unwrap();
    assert_eq!(view.fired_this_pass, 1, "the drift fires once");
    assert_eq!(view.nags.len(), 1, "one owned nag");
    assert_eq!(
        view.nags[0].owners,
        vec!["@acme/payments"],
        "owner resolved"
    );
    assert_eq!(nag_count(&store).await, 1, "one nag mark on the ledger");

    // Second pass, same router, same inputs: nothing new fires (idempotent).
    let view = r1.run_once(now).await.unwrap();
    assert_eq!(view.fired_this_pass, 0, "no re-fire on a repeat pass");
    assert_eq!(nag_count(&store).await, 1, "still one nag mark");

    // KILL AND REVIVE: drop the router entirely and build a fresh one over the SAME store.
    // Its only memory is the ledger, which it re-scans — so it must derive the drift as
    // already-fired and not re-nag. This is the load-bearing restart-safety proof.
    drop(r1);
    let r2 = router(store.clone(), &mirror_root);
    let view = r2.run_once(now).await.unwrap();
    assert_eq!(
        view.fired_this_pass, 0,
        "a revived router does not re-fire a transition already on the ledger"
    );
    assert_eq!(
        nag_count(&store).await,
        1,
        "the ledger still holds exactly one nag mark after the restart"
    );
    // The transition is still live and still surfaced (owned), just not re-fired.
    assert_eq!(
        view.nags.len(),
        1,
        "the drift is still surfaced after restart"
    );
    assert!(
        !view.nags[0].fired_this_pass,
        "surfaced but not newly fired"
    );
}

// ---- (2) a clock-crossing stale fires with no new verdict ----

#[tokio::test]
async fn a_clock_crossing_stale_fires_with_no_new_verdict() {
    let fixture = GitFixture::with_files(&[
        (
            ".claims/payments/pin.md",
            &claim_file("payments/pin", "hub:\n  max-age: 30d\n"),
        ),
        (".github/CODEOWNERS", "*  @acme/payments\n"),
    ]);
    let store = SqliteStore::open_in_memory().await.unwrap();
    let mirror_root = sync(&store, &fixture).await;

    // One HELD verdict at day 0. Within the window the claim is verified — no transition.
    let pin = claim_of(
        "id: payments/pin\nhub:\n  max-age: 30d\nchecks:\n  - kind: cmd\n    run: \"true\"",
    );
    store
        .append(&verdict_event(
            &pin,
            Verdict::Held,
            "c0",
            "run-1",
            "2026-07-01T00:00:00Z",
        ))
        .await
        .unwrap();

    let r = router(store.clone(), &mirror_root);

    // Day 10: fresh, no transition, no fire.
    let view = r.run_once(ts("2026-07-11T00:00:00Z")).await.unwrap();
    assert_eq!(view.fired_this_pass, 0, "fresh claim fires nothing");
    assert_eq!(nag_count(&store).await, 0);

    // Day 40: the 30-day window lapsed. NO new verdict was appended — only the clock moved.
    // The claim is now stale, and the router fires a stale nag.
    let view = r.run_once(ts("2026-08-11T00:00:00Z")).await.unwrap();
    assert_eq!(
        view.fired_this_pass, 1,
        "a claim aging into stale by the clock alone fires a nag"
    );
    assert_eq!(view.nags.len(), 1);
    assert_eq!(
        view.nags[0].transition,
        claim_hub_core::Transition::Stale,
        "the transition is stale"
    );
    // The ledger still holds only the one HELD verdict plus the one nag — no verdict was
    // fabricated to age the claim (invariant #3).
    let events = store.scan_from(Position(0)).await.unwrap();
    let verdicts = events
        .iter()
        .filter(|s| s.event.verdict == Some(Verdict::Held))
        .count();
    assert_eq!(verdicts, 1, "no new verdict was written to age the claim");
    assert_eq!(nag_count(&store).await, 1, "one stale nag");
}

// ---- (3) a no-owner transition dead-letters and the view shows it ----

#[tokio::test]
async fn a_no_owner_transition_routes_to_the_dead_letter_queue() {
    // A claim with NO CODEOWNERS file at all: the drift has no resolvable owner, so it must
    // dead-letter — visible, never silently dropped (invariant #6).
    let fixture = GitFixture::with_files(&[(
        ".claims/payments/orphan.md",
        &claim_file("payments/orphan", ""),
    )]);
    let store = SqliteStore::open_in_memory().await.unwrap();
    let mirror_root = sync(&store, &fixture).await;

    let orphan = claim_of("id: payments/orphan\nchecks:\n  - kind: cmd\n    run: \"true\"");
    store
        .append(&verdict_event(
            &orphan,
            Verdict::Drifted,
            "c1",
            "run-1",
            "2026-07-01T00:00:00Z",
        ))
        .await
        .unwrap();

    let r = router(store.clone(), &mirror_root);
    let view = r.run_once(ts("2026-07-02T00:00:00Z")).await.unwrap();

    assert!(view.nags.is_empty(), "no owned nag — there is no owner");
    assert_eq!(view.dead_letters.len(), 1, "the drift is a dead-letter");
    assert!(
        view.dead_letters[0].owners.is_empty(),
        "a dead-letter has no owners"
    );
    // It still fired a nag mark: a dead-letter is a real, recorded transition (fire-once
    // still applies), just with nobody to route to.
    assert_eq!(
        view.fired_this_pass, 1,
        "the dead-lettered transition fires once"
    );
}

// ---- (4) one commit breaking N claims is one grouped nag ----

#[tokio::test]
async fn one_commit_breaking_three_claims_is_one_grouped_nag() {
    let fixture = GitFixture::with_files(&[
        (".claims/payments/a.md", &claim_file("payments/a", "")),
        (".claims/payments/b.md", &claim_file("payments/b", "")),
        (".claims/payments/c.md", &claim_file("payments/c", "")),
        (".github/CODEOWNERS", "*  @acme/payments\n"),
    ]);
    let store = SqliteStore::open_in_memory().await.unwrap();
    let mirror_root = sync(&store, &fixture).await;

    // Three claims, all drifted at the SAME commit — one refactor broke all three.
    for (n, id) in [
        ("run-a", "payments/a"),
        ("run-b", "payments/b"),
        ("run-c", "payments/c"),
    ] {
        let claim = claim_of(&format!(
            "id: {id}\nchecks:\n  - kind: cmd\n    run: \"true\""
        ));
        store
            .append(&verdict_event(
                &claim,
                Verdict::Drifted,
                "onecommit",
                n,
                "2026-07-01T00:00:00Z",
            ))
            .await
            .unwrap();
    }

    let r = router(store.clone(), &mirror_root);
    let view = r.run_once(ts("2026-07-02T00:00:00Z")).await.unwrap();

    assert_eq!(
        view.nags.len(),
        1,
        "one commit → one grouped nag, not three"
    );
    assert_eq!(
        view.nags[0].claims.len(),
        3,
        "all three broken claims are in the one group"
    );
    assert_eq!(view.fired_this_pass, 1, "one grouped fire, not three");
    assert_eq!(nag_count(&store).await, 1, "one nag mark for the group");
}

// ---- (5) a lapsed skip `until` fires ----

#[tokio::test]
async fn a_lapsed_skip_until_fires() {
    let fixture = GitFixture::with_files(&[
        (
            ".claims/payments/parked.md",
            "---\nid: payments/parked\nchecks:\n  - kind: cmd\n    run: \"true\"\n    skip:\n      reason: parked\n      until: 2026-07-15\n---\nParked for now.\n",
        ),
        (".github/CODEOWNERS", "*  @acme/payments\n"),
    ]);
    let store = SqliteStore::open_in_memory().await.unwrap();
    let mirror_root = sync(&store, &fixture).await;

    let r = router(store.clone(), &mirror_root);

    // Before the until: no lapsed-skip transition.
    let view = r.run_once(ts("2026-07-10T00:00:00Z")).await.unwrap();
    let lapsed_before = view
        .nags
        .iter()
        .chain(&view.dead_letters)
        .any(|n| n.transition == claim_hub_core::Transition::LapsedSkip);
    assert!(!lapsed_before, "the skip has not lapsed yet");

    // After the until (2026-07-15): the deferred check is due again — a lapsed-skip fires.
    let view = r.run_once(ts("2026-07-20T00:00:00Z")).await.unwrap();
    let lapsed = view
        .nags
        .iter()
        .find(|n| n.transition == claim_hub_core::Transition::LapsedSkip);
    assert!(lapsed.is_some(), "a lapsed skip until fires: {view:?}");
    assert!(
        view.fired_this_pass >= 1,
        "the lapsed skip fired a nag mark"
    );
}

// ---- (6) the rendered nag content is served at GET /api/nags ----

#[tokio::test]
async fn the_rendered_nag_content_is_served_for_delivery() {
    let fixture = GitFixture::with_files(&[
        (".claims/payments/pin.md", &claim_file("payments/pin", "")),
        (".github/CODEOWNERS", "*  @acme/payments\n"),
    ]);
    let store = SqliteStore::open_in_memory().await.unwrap();
    let mirror_root = sync(&store, &fixture).await;

    let pin = claim_of("id: payments/pin\nchecks:\n  - kind: cmd\n    run: \"true\"");
    store
        .append(&verdict_event(
            &pin,
            Verdict::Drifted,
            "c1",
            "run-1",
            "2026-07-01T00:00:00Z",
        ))
        .await
        .unwrap();

    // The app over the store with a fixed read clock and the mirror root, so `GET /api/nags`
    // resolves owners.
    let read_clock: claim_hub::app::ReadClock = Arc::new(|| ts("2026-07-02T00:00:00Z"));
    let state = AppState::new(store.clone(), None)
        .with_read_clock(read_clock)
        .with_mirror_root(mirror_root);
    let app = claim_hub::build_app(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/nags")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(
        body["nags"].as_array().unwrap().len(),
        1,
        "one nag served: {body}"
    );
    assert_eq!(body["nags"][0]["transition"], "drifted");
    assert_eq!(body["nags"][0]["owners"][0], "@acme/payments");
    assert_eq!(body["nags"][0]["claims"][0]["id"], "payments/pin");
    assert_eq!(
        body["nags"][0]["claims"][0]["statement"], "The fact holds.",
        "the rendered content carries the statement"
    );
    // A read serves but fires nothing: no nag mark was appended by the GET.
    assert_eq!(
        nag_count(&store).await,
        0,
        "GET /api/nags is a read — it appends no nag mark"
    );
}
