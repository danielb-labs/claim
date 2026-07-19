//! The nag-delivery CI glue (hub-12b), exercised against a locally-served hub.
//!
//! The delivery half of the loop (HUB-IMPLEMENTATION.md §3, §4.5 decision 4): the hub
//! **renders** the nag content and serves it at `GET /api/nags`; the scheduled CI lane
//! **pulls** it and **delivers** it to the forge. The pull is `ci/hub-nags.sh`; the render
//! of the pulled JSON to markdown is `ci/nag-deliver.mjs`; the forge upsert is the composite
//! action's post step (a `github-script` call not exercisable off a runner — its
//! idempotency logic mirrors the ingest action's and the existing clock lane's).
//!
//! These tests prove the parts that DON'T need a real runner, end to end and network-free:
//!
//! - a **local hub binary's app** served over a real ephemeral TCP port (so the script's
//!   `curl` reaches a genuine HTTP endpoint), with a synced git mirror (so the router
//!   resolves owners from a real CODEOWNERS) and a drifted verdict on the ledger (so
//!   `/api/nags` has something to serve), all behind the **read-auth layer** with a
//!   hub-minted scoped read token — the same machinery hub-13 ships;
//! - the read credential is injected through the script's `HUB_NAGS_TOKEN` env seam, the same
//!   seam the composite action feeds from a CI secret; and
//! - the pulled JSON is rendered by the real `ci/nag-deliver.mjs` (via `node`), so the whole
//!   pull → render path is the production one, not a fixture.
//!
//! The three Done-when assertions of hub-12b:
//!
//! - [`two_runs_render_one_identical_body`] — idempotent upsert: two pulls render a
//!   byte-identical body, so the marker-keyed forge upsert lands on ONE issue/comment, not a
//!   new one each run.
//! - [`the_delivered_body_matches_the_hubs_rendered_nag_view`] — the delivered content is a
//!   faithful function of the hub's `/api/nags` response (the owner and claim it resolved
//!   appear verbatim; the glue delivers, it does not invent — invariants #4/#6).
//! - [`a_hub_outage_fails_loud_and_leaves_the_previous_issue_intact`] and
//!   [`a_hub_that_never_responds_times_out_loud`] — a hub outage (refused / stalled) fails the
//!   lane loudly and writes nothing to `--out`, so the delivery step leaves the prior standing
//!   issue intact rather than blanking it (invariant #6).
//!
//! The auth arm proves the pull is authenticated: [`a_missing_read_token_fails_loud`] and
//! [`a_wrong_read_token_is_a_loud_401`] — an unauthenticated or wrong-scope pull is a loud
//! failure, never a silent empty view that would blank the issue.

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use claim_hub::app::{AppState, ReadClock};
use claim_hub::authlayer::AuthLayerState;
use claim_hub::metadata::{ResourceMetadata, METADATA_PATH};
use claim_hub::readauth::ReadAuthPolicy;
use claim_hub::scope::Scope;
use claim_hub::token::{self, ScopedToken};
use claim_hub_core::{check_digest, CheckRef, Event, Producer};
use claim_hub_store::{sync_store, ConnectedStore, Ledger, SqliteStore};
use tempfile::TempDir;

const STORE: &str = "github.com/acme/payments";
const CLAIM_ID: &str = "payments/pin";
const OWNER: &str = "@acme/payments";

/// The read clock the served hub derives at — a fixed instant after the drift, so the drifted
/// claim stands drifted at read time (determinism: no wall clock in the read path).
const READ_INSTANT: &str = "2026-07-02T00:00:00Z";

fn ts(s: &str) -> claim_core::Timestamp {
    s.parse().unwrap()
}

// --- the served hub with nags behind read auth -----------------------------------

/// A hub served over a real TCP port whose `/api/nags` returns one owned drift nag, behind
/// the read-auth layer with a hub-minted scoped read token.
struct ServedNagHub {
    addr: SocketAddr,
    /// The raw read-scoped token a client must present. The hub stores only its hash.
    read_token: String,
    /// Held so the fixture's tempdir (and the mirror under it) outlives the served hub.
    _fixture: GitFixture,
    _db_dir: TempDir,
    _server: AbortOnDrop,
}

impl ServedNagHub {
    /// Serve a hub with one drifted, owned claim and a read-scoped token gating `/api/nags`.
    async fn start() -> Self {
        let fixture = GitFixture::with_files(&[
            (".claims/payments/pin.md", &claim_file(CLAIM_ID)),
            (".github/CODEOWNERS", &format!("*  {OWNER}\n")),
        ]);
        let db_dir = TempDir::new().expect("temp dir for the hub database");
        let store = SqliteStore::open(db_dir.path().join("hub.db"))
            .await
            .expect("open a file-backed hub store");

        // Sync the fixture so the registry holds the claim and the mirror holds CODEOWNERS.
        let mirror_root = fixture.dir.path().join("_mirror");
        let connected = ConnectedStore::new(STORE, fixture.url());
        sync_store(&store, &connected, &mirror_root)
            .await
            .expect("sync the fixture");

        // A drifted verdict: the claim now stands drifted, so `/api/nags` serves one nag.
        let pin = claim_of(CLAIM_ID);
        store
            .append(&drift_event(
                &pin,
                "deadbeef",
                "run-1",
                "2026-07-01T00:00:00Z",
            ))
            .await
            .expect("append the drift");

        // Read auth: the IdP-less scoped-token floor, carrying `read` (what `/api` requires).
        let minted = token::mint().expect("mint a read token");
        let read_token = minted.raw().to_owned();
        let policy = ReadAuthPolicy::resolve(
            false,
            None,
            vec![ScopedToken {
                name: "ci-nag-delivery".into(),
                scopes: vec![Scope::Read],
                hash: token::hash_for_config(minted.raw()),
            }],
        )
        .expect("a policy with a token resolves");
        let metadata = ResourceMetadata::new("https://hub.acme.example", None);
        let auth = Arc::new(AuthLayerState::new(policy, metadata, METADATA_PATH));

        let read_clock: ReadClock = Arc::new(|| ts(READ_INSTANT));
        let state = AppState::new(store.clone(), None)
            .with_mirror_root(mirror_root)
            .with_read_clock(read_clock)
            .with_read_auth(auth);
        let app = claim_hub::build_app(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("the bound address");
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve the hub");
        });

        Self {
            addr,
            read_token,
            _fixture: fixture,
            _db_dir: db_dir,
            _server: AbortOnDrop(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

/// A drifted verdict event for the claim's first check, at `commit`/`at`, distinct run.
fn drift_event(claim: &claim_core::Claim, commit: &str, run: &str, at: &str) -> Event {
    let mut producer = serde_json::Map::new();
    producer.insert("run".into(), serde_json::json!(run));
    Event::verdict(
        claim.id.as_str().to_owned(),
        CheckRef {
            index: 0,
            digest: check_digest(&claim.checks[0]),
        },
        claim_core::Verdict::Drifted,
        commit,
        STORE,
        Producer(producer),
        ts(at),
    )
}

fn claim_of(id: &str) -> claim_core::Claim {
    let text =
        format!("---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n---\nStatement.\n");
    claim_core::parse_claim_file(".claims/x.md", &text).expect("valid claim")
}

fn claim_file(id: &str) -> String {
    format!("---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n---\nThe pin holds.\n")
}

// --- driving the pull script and the renderer ------------------------------------

/// The outcome of running `ci/hub-nags.sh`: its exit status, stderr, and the `--out` file's
/// contents (empty on a failed pull, the response body on success).
struct PullRun {
    status: std::process::ExitStatus,
    stderr: String,
    out: String,
}

/// Run `ci/hub-nags.sh` against `hub_url` with `token` (via the `HUB_NAGS_TOKEN` seam),
/// writing the pulled body to a fresh `--out` file whose contents are returned.
///
/// Spawned through `spawn_blocking` so the script's blocking `curl` wait does not tie up a
/// runtime worker the served hub's axum task needs to answer it (the same threading contract
/// the ingest-action tests rely on).
async fn run_pull(hub_url: &str, token: Option<&str>, max_time: Option<u64>) -> PullRun {
    let script = ci_script("hub-nags.sh");
    let hub_url = hub_url.to_owned();
    let token = token.map(str::to_owned);
    let out_dir = TempDir::new().expect("temp dir for the out file");
    let out_path = out_dir.path().join("nags.json");
    let out_for_task = out_path.clone();

    let run = tokio::task::spawn_blocking(move || {
        let mut command = Command::new("bash");
        command
            .arg(script)
            .arg("--hub-url")
            .arg(hub_url)
            .arg("--out")
            .arg(&out_for_task);
        if let Some(max_time) = max_time {
            command.arg("--max-time").arg(max_time.to_string());
        }
        match token {
            Some(token) => {
                command.env("HUB_NAGS_TOKEN", token);
            }
            None => {
                command.env_remove("HUB_NAGS_TOKEN");
            }
        }
        let output = command.output().expect("run the hub-nags script");
        (
            output.status,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    })
    .await
    .expect("the script task ran to completion");

    let out = std::fs::read_to_string(&out_path).unwrap_or_default();
    PullRun {
        status: run.0,
        stderr: run.1,
        out,
    }
}

/// Render a pulled `/api/nags` body to the standing-issue markdown with `ci/nag-deliver.mjs`,
/// returning the rendered body. Panics if `node` is unavailable — the same "gate on the
/// renderer" contract `scripts/check.sh` keeps (node ships on CI runners).
fn render_issue(nags_json: &str) -> String {
    let script = ci_script("nag-deliver.mjs");
    let output = Command::new("node")
        .arg(script)
        .arg("--mode")
        .arg("issue")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(nags_json.as_bytes())?;
            child.wait_with_output()
        })
        .expect("run node nag-deliver.mjs (node must be on PATH; CI has it)");
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "the renderer exits 0 (clean) or 1 (dirty), not {:?}; stderr=<{}>",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The path to a `ci/<name>` script — the workspace root is two levels above this crate.
fn ci_script(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root is two levels above crates/claim-hub")
        .join("ci")
        .join(name)
}

/// A tokio task handle that aborts its task when dropped, so a served hub does not outlive
/// its test.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// --- a local git fixture as the sync remote (no network) -------------------------

struct GitFixture {
    dir: TempDir,
}

impl GitFixture {
    fn with_files(files: &[(&str, &str)]) -> Self {
        let fixture = Self {
            dir: TempDir::new().expect("temp dir"),
        };
        fixture.git(&["init", "-q", "-b", "main"]);
        fixture.git(&["config", "user.name", "Test"]);
        fixture.git(&["config", "user.email", "test@example.com"]);
        fixture.git(&["config", "commit.gpgsign", "false"]);
        for (rel, contents) in files {
            let path = fixture.dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        fixture.git(&["add", "-A"]);
        fixture.git(&["commit", "-q", "-m", "seed"]);
        fixture
    }

    fn url(&self) -> String {
        self.dir.path().to_string_lossy().into_owned()
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

// --- the transport-failure stand-ins ---------------------------------------------

/// A `--hub-url` pointing at a closed port, so the pull's connect is refused.
fn closed_port_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("the bound address");
    drop(listener);
    format!("http://{addr}")
}

/// A listener that accepts connections but never answers — the slow-loris / half-dead hub.
struct StalledHub {
    addr: SocketAddr,
    _server: AbortOnDrop,
}

impl StalledHub {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("the bound address");
        let handle = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        Self {
            addr,
            _server: AbortOnDrop(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

// --- the tests -------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_delivered_body_matches_the_hubs_rendered_nag_view() {
    let hub = ServedNagHub::start().await;
    let pull = run_pull(&hub.base_url(), Some(&hub.read_token), None).await;
    assert!(
        pull.status.success(),
        "an authenticated pull succeeds; stderr=<{}>",
        pull.stderr,
    );

    // The pulled body is the hub's real /api/nags JSON, carrying the owner it resolved.
    let view: serde_json::Value = serde_json::from_str(&pull.out).expect("the pulled body is JSON");
    assert_eq!(view["nags"][0]["owners"][0], serde_json::json!(OWNER));
    assert_eq!(
        view["nags"][0]["claims"][0]["id"],
        serde_json::json!(CLAIM_ID)
    );

    // The delivered body is a faithful function of that view: the claim and the owner the hub
    // resolved appear verbatim — the glue delivers what the hub rendered, never invents it.
    let body = render_issue(&pull.out);
    assert!(
        body.contains(CLAIM_ID),
        "the delivered body names the drifted claim: {body}"
    );
    assert!(
        body.contains(&format!("owner: {OWNER}")),
        "the delivered body carries the owner the hub resolved: {body}",
    );
    assert!(
        body.starts_with("<!-- claim-bot:hub-nag -->"),
        "the delivered issue body opens with the idempotency marker: {body}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_runs_render_one_identical_body() {
    // Idempotency: two scheduled runs pull and render a byte-identical body. Because the
    // forge upsert finds-or-updates by the marker in that body, an identical body edits the
    // ONE existing issue rather than opening a second — two runs, one issue.
    let hub = ServedNagHub::start().await;

    let first = run_pull(&hub.base_url(), Some(&hub.read_token), None).await;
    assert!(
        first.status.success(),
        "first pull ok; stderr=<{}>",
        first.stderr
    );
    let second = run_pull(&hub.base_url(), Some(&hub.read_token), None).await;
    assert!(
        second.status.success(),
        "second pull ok; stderr=<{}>",
        second.stderr
    );

    let body1 = render_issue(&first.out);
    let body2 = render_issue(&second.out);
    assert_eq!(
        body1, body2,
        "two runs render an identical body, so the marker-keyed upsert lands on one issue",
    );
    // Sanity: the body carries the marker the upsert keys on, so "identical" means "same
    // find target," not "both empty."
    assert!(body1.contains("<!-- claim-bot:hub-nag -->"));
    assert!(body1.contains(CLAIM_ID));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hub_outage_fails_loud_and_leaves_the_previous_issue_intact() {
    // A refused connection (the hub is down) must fail the lane loudly and write NOTHING to
    // --out, so the delivery step leaves the previous standing issue intact rather than
    // blanking it over a broken pull (invariant #6).
    let run = run_pull(&closed_port_url(), Some("any-token"), None).await;
    assert!(
        !run.status.success(),
        "a refused connection fails the lane; stderr=<{}>",
        run.stderr,
    );
    assert!(
        run.stderr.contains("unreachable") || run.stderr.contains("failed to complete"),
        "the failure names the unreachable hub: {}",
        run.stderr,
    );
    assert!(
        run.stderr
            .contains("leaving the previous standing issue intact"),
        "the failure states the prior issue is left intact: {}",
        run.stderr,
    );
    assert_eq!(
        run.out, "",
        "a failed pull writes nothing to --out, so the delivery step keeps the prior issue",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hub_that_never_responds_times_out_loud() {
    // A hub that accepts the connection then stalls must fail via --max-time, not hang the
    // lane to the runner's wall-clock, and still leave the prior issue intact.
    let stalled = StalledHub::start().await;
    let run = run_pull(&stalled.base_url(), Some("any-token"), Some(1)).await;
    assert!(
        !run.status.success(),
        "a stalled hub fails the lane; stderr=<{}>",
        run.stderr,
    );
    assert!(
        run.stderr.contains("did not respond within") && run.stderr.contains("timed out"),
        "the failure names the timeout: {}",
        run.stderr,
    );
    assert!(
        run.stderr
            .contains("leaving the previous standing issue intact"),
        "the timeout leaves the prior issue intact: {}",
        run.stderr,
    );
    assert_eq!(run.out, "", "a timed-out pull writes nothing to --out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_read_token_fails_loud() {
    // No HUB_NAGS_TOKEN: the script refuses to pull rather than send an unauthenticated
    // request that the hub would 401 anyway. Either way the lane is loud, never a silent
    // empty view that would blank the issue.
    let hub = ServedNagHub::start().await;
    let run = run_pull(&hub.base_url(), None, None).await;
    assert!(
        !run.status.success(),
        "a missing read token fails the lane; stderr=<{}>",
        run.stderr,
    );
    assert!(
        run.stderr.contains("HUB_NAGS_TOKEN is required"),
        "the failure names the missing credential and how to mint it: {}",
        run.stderr,
    );
    assert_eq!(run.out, "", "no pull happened, so --out is empty");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_read_token_is_a_loud_401() {
    // A bearer token the hub does not know is a 401 the script surfaces loudly, appending
    // nothing to --out — the pull is genuinely authenticated, not a rubber stamp.
    let hub = ServedNagHub::start().await;
    let run = run_pull(&hub.base_url(), Some("not-a-real-token"), None).await;
    assert!(
        !run.status.success(),
        "a wrong read token fails the lane; stderr=<{}>",
        run.stderr,
    );
    assert!(
        run.stderr.contains("rejected the nag pull (HTTP 401") || run.stderr.contains("HTTP 401"),
        "the failure surfaces the hub's 401: {}",
        run.stderr,
    );
    assert!(
        run.stderr
            .contains("leaving the previous standing issue intact"),
        "a rejected pull leaves the prior issue intact: {}",
        run.stderr,
    );
    assert_eq!(run.out, "", "a rejected pull writes nothing to --out");
}
