//! The hub ingest Action's core flow, exercised against a locally-served hub.
//!
//! The Action (`.github/actions/hub-ingest/action.yml`) pushes attested verdicts from a
//! CI lane to the hub. Its runner-agnostic core is `ci/hub-ingest.sh`: run `claim check
//! --json`, POST the report to the hub's `/ingest` with the OIDC token, and **fail loudly
//! on any non-2xx**. These tests prove that flow end to end without a real GitHub runner
//! and with no network to GitHub:
//!
//! - a **local hub binary's app** is served over a real ephemeral TCP port (so the
//!   script's `curl` reaches a genuine HTTP endpoint), with the **same injected/test JWKS
//!   the ingest tests use** — a minted token the hub accepts, verified against a key set
//!   the test controls;
//! - the script's OIDC-token acquisition is bypassed through its `HUB_INGEST_TOKEN`
//!   injection seam (the same seam the Action's own token step feeds), so the
//!   runner-specific token mint is never reached;
//! - a real `.claims/` store and the real in-tree `claim` binary produce the report the
//!   script pushes — the CLI→hub wire is exercised for real, not faked.
//!
//! The three Done-when assertions of hub-12a:
//!
//! - [`a_valid_push_succeeds_and_the_hub_records_the_event`] — a valid push returns 2xx,
//!   the script exits 0, and the hub's ledger holds the event.
//! - [`an_ingest_rejection_fails_the_step_with_the_hubs_reason`] — an ingest **rejection**
//!   (an unknown claim; a bad token) makes the step **fail** with the hub's reason in the
//!   output, and appends nothing.
//! - [`a_non_2xx_is_never_swallowed`] — the script's exit code is non-zero on every
//!   non-2xx, so a rejected push can never pass as green (CLAUDE.md invariants #1/#6).
//!
//! The telemetry-is-pushed-regardless arm covers both non-held verdicts: a drift
//! ([`a_drifted_report_is_still_pushed_not_swallowed`]) and a **broken** check
//! ([`a_broken_report_is_still_pushed_not_swallowed`]) each still land on the ledger,
//! because a broken verdict is exactly the rot the hub exists to surface (invariant #1).
//!
//! The hub-transport failure arm covers the two ways the hub can be unreachable without a
//! non-2xx: a **connection refused** ([`a_connection_refused_fails_the_step_loudly`]) and
//! a hub that accepts the connection but **never responds**
//! ([`a_hub_that_never_responds_times_out_loudly`]) — both must fail loudly rather than
//! hang the lane to the runner's wall-clock (invariants #1/#6).

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use claim_hub::app::{AppState, Clock};
use claim_hub::oidc::OidcVerifier;
use claim_hub_store::{Ledger, Position, SqliteStore};
use common::*;
use tempfile::TempDir;

/// The frontmatter of the one-check claim the tests seed into the registry AND write into
/// the on-disk `.claims/` store the `claim` binary checks. The two must agree: the
/// registry keys the check by its content digest, so the report's claim/check identity
/// only resolves if the same definition is registered.
const PIN_ID: &str = "payments/libfoo-pin";
const PIN_FRONTMATTER: &str = "id: payments/libfoo-pin\nchecks:\n  - kind: cmd\n    run: \"true\"";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_valid_push_succeeds_and_the_hub_records_the_event() {
    let hub = ServedHub::start_with_seeded_claim().await;
    let claims_dir = claims_store_with(PIN_ID, PIN_FRONTMATTER);

    // A valid token (the injected seam bypasses the runner mint) and the real report from
    // the real `claim` binary. The push must be accepted and the hub must record it.
    let token = sign_token(&TokenClaims::valid());
    let run = run_script(&hub, claims_dir.path(), Some(&token), TEST_AUDIENCE).await;

    assert!(
        run.status.success(),
        "a valid push exits 0; stdout=<{}> stderr=<{}>",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stderr.contains("OK") && run.stderr.contains("accepted"),
        "the script reports the hub accepted the push: {}",
        run.stderr
    );

    // The hub's ledger holds exactly the one event the push carried.
    let events = hub.store.scan_from(Position(0)).await.unwrap();
    assert_eq!(events.len(), 1, "the valid push landed one event");
    assert_eq!(events[0].event.claim, PIN_ID);
    assert_eq!(events[0].event.verdict, Some(claim_core::Verdict::Held));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ingest_rejection_fails_the_step_with_the_hubs_reason() {
    // A hub that has synced NOTHING: the claim the report names is unknown to the
    // registry, so ingest rejects the push (400) with a reason. The script must fail with
    // that reason surfaced, and the ledger must stay empty.
    let hub = ServedHub::start_empty().await;
    let claims_dir = claims_store_with(PIN_ID, PIN_FRONTMATTER);

    let token = sign_token(&TokenClaims::valid());
    let run = run_script(&hub, claims_dir.path(), Some(&token), TEST_AUDIENCE).await;

    assert!(
        !run.status.success(),
        "an ingest rejection fails the step; stdout=<{}> stderr=<{}>",
        run.stdout,
        run.stderr
    );
    // The hub's reason for refusing an unsynced claim is surfaced, not swallowed.
    assert!(
        run.stderr.contains("rejected the push")
            && (run.stderr.contains("not be synced") || run.stderr.contains("registry")),
        "the hub's rejection reason is printed: {}",
        run.stderr
    );
    // A rejected push appends nothing (invariant #4).
    let events = hub.store.scan_from(Position(0)).await.unwrap();
    assert!(events.is_empty(), "a rejected push wrote no event");
    // The hub counted the rejection (invariant #6): a monitor can see telemetry refused.
    assert_eq!(hub.store_rejection_count().await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_token_fails_the_step_with_the_hubs_reason_and_writes_nothing() {
    // The token's audience is wrong for this hub: the hub rejects it 401. The script must
    // fail with the hub's reason, and nothing lands. This is the "bad token" arm of the
    // Done-when.
    let hub = ServedHub::start_with_seeded_claim().await;
    let claims_dir = claims_store_with(PIN_ID, PIN_FRONTMATTER);

    let mut claims = TokenClaims::valid();
    claims.audience = "https://some-other-hub.example".to_owned();
    let token = sign_token(&claims);
    let run = run_script(&hub, claims_dir.path(), Some(&token), TEST_AUDIENCE).await;

    assert!(
        !run.status.success(),
        "a bad token fails the step; stderr=<{}>",
        run.stderr
    );
    assert!(
        run.stderr.contains("rejected the push") && run.stderr.contains("audience"),
        "the hub's 401 reason (wrong audience) is surfaced: {}",
        run.stderr
    );
    let events = hub.store.scan_from(Position(0)).await.unwrap();
    assert!(events.is_empty(), "a bad-token push wrote no event");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_2xx_is_never_swallowed() {
    // The load-bearing invariant (#1/#6): whatever the hub refuses, the script's exit code
    // is non-zero. Here the token is missing entirely, so the hub answers 401 with no
    // bearer; the script must still fail — a rejected/absent-identity push never passes as
    // green. Combined with the rejection test above, this pins that no non-2xx path exits 0.
    let hub = ServedHub::start_with_seeded_claim().await;
    let claims_dir = claims_store_with(PIN_ID, PIN_FRONTMATTER);

    // No token injected AND no GitHub runner env: the script fails before POSTing, because
    // it has no identity to attach. That is itself a loud non-zero, not a silent green.
    let run = run_script(&hub, claims_dir.path(), None, TEST_AUDIENCE).await;
    assert!(
        !run.status.success(),
        "with no obtainable token the script fails loudly rather than pushing anonymously: \
         stderr=<{}>",
        run.stderr
    );
    assert!(
        run.stderr.contains("no OIDC token available"),
        "the script names why it cannot proceed: {}",
        run.stderr
    );
    // Nothing was pushed.
    let events = hub.store.scan_from(Position(0)).await.unwrap();
    assert!(events.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drifted_report_is_still_pushed_not_swallowed() {
    // A drift is telemetry the hub must receive — the CLI's exit 1 must NOT stop the push
    // (that would hide exactly the rot the hub exists to surface, invariant #6). Seed and
    // check a claim whose check drifts (`false` exits 1), and assert the push still lands a
    // `drifted` event and the script succeeds (the ingest was accepted).
    let drift_frontmatter = "id: payments/libfoo-pin\nchecks:\n  - kind: cmd\n    run: \"false\"";
    let hub = ServedHub::start_with_claim(drift_frontmatter).await;
    let claims_dir = claims_store_with(PIN_ID, drift_frontmatter);

    let token = sign_token(&TokenClaims::valid());
    let run = run_script(&hub, claims_dir.path(), Some(&token), TEST_AUDIENCE).await;

    assert!(
        run.status.success(),
        "a drifted verdict is pushed, not swallowed; the ingest itself succeeded: \
         stdout=<{}> stderr=<{}>",
        run.stdout,
        run.stderr
    );
    let events = hub.store.scan_from(Position(0)).await.unwrap();
    assert_eq!(events.len(), 1, "the drift was pushed as telemetry");
    assert_eq!(events[0].event.verdict, Some(claim_core::Verdict::Drifted));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_report_is_still_pushed_not_swallowed() {
    // A broken verdict is the load-bearing arm of invariant #1: a check that could not run
    // is telemetry the hub must receive, never a swallowed green. Seed a check that is
    // killed by a signal (`kill -KILL $$` — a death-by-signal outcome, unconditionally
    // `Broken`), so the CLI reports `broken` (exit 2). The push must still land a `Broken`
    // event and the script must succeed — the ingest was accepted, and it is the hub's job
    // to nag on the broken fact, not the lane's to hide it.
    let broken_frontmatter =
        "id: payments/libfoo-pin\nchecks:\n  - kind: cmd\n    run: \"kill -KILL $$\"";
    let hub = ServedHub::start_with_claim(broken_frontmatter).await;
    let claims_dir = claims_store_with(PIN_ID, broken_frontmatter);

    let token = sign_token(&TokenClaims::valid());
    let run = run_script(&hub, claims_dir.path(), Some(&token), TEST_AUDIENCE).await;

    assert!(
        run.status.success(),
        "a broken verdict is pushed, not swallowed; the ingest itself succeeded: \
         stdout=<{}> stderr=<{}>",
        run.stdout,
        run.stderr
    );
    let events = hub.store.scan_from(Position(0)).await.unwrap();
    assert_eq!(
        events.len(),
        1,
        "the broken verdict was pushed as telemetry"
    );
    assert_eq!(events[0].event.verdict, Some(claim_core::Verdict::Broken));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_refused_fails_the_step_loudly() {
    // A hub that cannot be reached at all (nothing listening on the port) must fail the
    // lane loudly — a connection failure is a non-delivery, and invariant #6 forbids it
    // degrading to a stale green. Point the script at a closed port and assert a non-zero
    // exit with the transport reason, and that nothing was recorded anywhere reachable.
    let closed_url = closed_port_url();
    let claims_dir = claims_store_with(PIN_ID, PIN_FRONTMATTER);

    let token = sign_token(&TokenClaims::valid());
    let run = run_script_at(&closed_url, claims_dir.path(), Some(&token), TEST_AUDIENCE).await;

    assert!(
        !run.status.success(),
        "a refused connection fails the step; stdout=<{}> stderr=<{}>",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stderr.contains("failed to complete") || run.stderr.contains("unreachable"),
        "the script names the transport failure: {}",
        run.stderr
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hub_that_never_responds_times_out_loudly() {
    // The slow-loris / half-dead hub: it accepts the TCP connection but never sends a
    // response. Without `--max-time` the script would hang to the runner's wall-clock;
    // with it, the step must fail loudly and name the timeout (invariants #1/#6). A tiny
    // `--max-time` keeps the test fast; a listener that accepts but never answers stands
    // in for the stalled hub.
    let stalled = StalledHub::start().await;
    let claims_dir = claims_store_with(PIN_ID, PIN_FRONTMATTER);

    let token = sign_token(&TokenClaims::valid());
    let run = run_script_with_max_time(
        &stalled.base_url(),
        claims_dir.path(),
        Some(&token),
        TEST_AUDIENCE,
        1,
    )
    .await;

    assert!(
        !run.status.success(),
        "a hub that never responds fails the step; stdout=<{}> stderr=<{}>",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stderr.contains("did not respond") && run.stderr.contains("timed out"),
        "the script names the timeout as the reason: {}",
        run.stderr
    );
}

/// A hub served over a real ephemeral TCP port, so the script's `curl` reaches a genuine
/// HTTP endpoint (not the in-process `oneshot` the other tests use — the script speaks
/// real HTTP). Holds the store so a test reads the ledger back, and the bound address so
/// it builds the `--hub-url`. The serving task runs until the test drops; a file-backed
/// store (a tempdir) is used so a real multi-connection server is not squeezed through the
/// single-connection `:memory:` pool.
struct ServedHub {
    addr: SocketAddr,
    store: SqliteStore,
    _db_dir: TempDir,
    // The serving task is aborted on drop; the `AbortOnDrop` guard makes that explicit.
    _server: AbortOnDrop,
}

impl ServedHub {
    /// Serve a hub whose registry has synced the standard held claim.
    async fn start_with_seeded_claim() -> Self {
        Self::start_with_claim(PIN_FRONTMATTER).await
    }

    /// Serve a hub whose registry has synced the claim described by `frontmatter`.
    async fn start_with_claim(frontmatter: &str) -> Self {
        let hub = Self::start_empty().await;
        seed_claim(&hub.store, ".claims/pin.md", frontmatter).await;
        hub
    }

    /// Serve a hub whose registry is empty (nothing synced) — for the rejection test.
    async fn start_empty() -> Self {
        let db_dir = TempDir::new().expect("temp dir for the hub database");
        let store = SqliteStore::open(db_dir.path().join("hub.db"))
            .await
            .expect("open a file-backed hub store");

        // The same test verifier the ingest tests use: a mocked JWKS built from the
        // fixture signing key, so a minted token verifies with no network to GitHub.
        let verifier = OidcVerifier::new(
            TEST_ISSUER,
            TEST_AUDIENCE,
            [TEST_REPOSITORY.to_owned()],
            TestJwksSource::with_signing_key(),
        );
        let clock: Clock = Arc::new(ingest_instant);
        let state = AppState::new(store.clone(), Some(Arc::new(verifier))).with_clock(clock);
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
            store,
            _db_dir: db_dir,
            _server: AbortOnDrop(handle),
        }
    }

    /// The hub's base URL the script POSTs to.
    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The hub's rejection count, read straight from the store (the `/status` source).
    async fn store_rejection_count(&self) -> i64 {
        use claim_hub_store::Rejections;
        self.store.rejection_count().await.unwrap()
    }
}

/// A tokio task handle that aborts its task when dropped, so a served hub does not outlive
/// its test.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The outcome of running the script: exit status and captured output.
struct ScriptRun {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

/// Run `ci/hub-ingest.sh` against `hub`, checking the `.claims/` store at `claims_dir`.
///
/// A thin wrapper over [`run_script_at`] that supplies the served hub's URL; see it for
/// the seam and threading contract.
async fn run_script(
    hub: &ServedHub,
    claims_dir: &Path,
    token: Option<&str>,
    audience: &str,
) -> ScriptRun {
    run_script_at(&hub.base_url(), claims_dir, token, audience).await
}

/// Run `ci/hub-ingest.sh` against an arbitrary `hub_url`, checking the `.claims/` store at
/// `claims_dir`, with the script's default `--max-time`.
///
/// Taking a bare URL (not a `ServedHub`) lets the transport-failure tests point the script
/// at a closed port or a stalled listener. See [`run_script_with_max_time`] for the seam
/// and threading contract.
async fn run_script_at(
    hub_url: &str,
    claims_dir: &Path,
    token: Option<&str>,
    audience: &str,
) -> ScriptRun {
    run_script_with_max_time(hub_url, claims_dir, token, audience, None).await
}

/// Run `ci/hub-ingest.sh` against `hub_url`, optionally overriding `--max-time` (seconds).
///
/// `token` is injected through the script's `HUB_INGEST_TOKEN` seam (the runner-agnostic
/// path); `None` leaves it unset AND scrubs the GitHub runner env, so the script's
/// token-acquisition fails loudly rather than reaching a real endpoint. `audience` is what
/// the script would mint the token for — passed through so a test can prove the argument
/// is wired, though with an injected token the value is not used to mint. `max_time`, when
/// `Some`, bounds each HTTP request so the timeout path is reached fast in the test.
///
/// The script is spawned through [`tokio::task::spawn_blocking`], so its blocking wait for
/// the child process does not tie up a runtime worker the served hub's axum task needs to
/// answer the script's `curl` — without this the (few-threaded) test runtime deadlocks: the
/// script blocks waiting for a response the server never gets scheduled to send.
async fn run_script_with_max_time(
    hub_url: &str,
    claims_dir: &Path,
    token: Option<&str>,
    audience: &str,
    max_time: impl Into<Option<u64>>,
) -> ScriptRun {
    let script = script_path();
    let hub_url = hub_url.to_owned();
    let audience = audience.to_owned();
    let claims_dir = claims_dir.to_path_buf();
    let claim_bin = claim_binary();
    let token = token.map(str::to_owned);
    let max_time = max_time.into();

    tokio::task::spawn_blocking(move || {
        let mut command = Command::new("bash");
        command
            .arg(script)
            .arg("--hub-url")
            .arg(hub_url)
            .arg("--audience")
            .arg(audience)
            .arg("--claims-dir")
            .arg(claims_dir)
            .arg("--claim-bin")
            .arg(claim_bin);
        if let Some(max_time) = max_time {
            command.arg("--max-time").arg(max_time.to_string());
        }

        // Scrub any ambient GitHub-runner OIDC env so the injection seam is the ONLY token
        // source under test — a developer running this on a self-hosted runner cannot leak a
        // real endpoint into the "no token" case.
        command.env_remove("ACTIONS_ID_TOKEN_REQUEST_URL");
        command.env_remove("ACTIONS_ID_TOKEN_REQUEST_TOKEN");
        match token {
            Some(token) => {
                command.env("HUB_INGEST_TOKEN", token);
            }
            None => {
                command.env_remove("HUB_INGEST_TOKEN");
            }
        }

        let output = command.output().expect("run the hub-ingest script");
        ScriptRun {
            status: output.status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    })
    .await
    .expect("the script task ran to completion")
}

/// A `--hub-url` pointing at a port with nothing listening, so the script's POST is
/// refused. Bind an ephemeral port to learn a free one, then drop the listener so the
/// port is closed by the time the script connects. A subsequent bind by another process is
/// possible but vanishingly unlikely in a test run, and a *different* listener would still
/// not answer `/ingest` with an accepted envelope, so the test stays loud either way.
fn closed_port_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("the bound address");
    drop(listener);
    format!("http://{addr}")
}

/// A listener that accepts connections but never answers — the slow-loris / half-dead hub.
/// It stands in for a hub that takes the TCP connection then stalls, so the script's
/// `--max-time` is what must end the wait.
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
        // Accept every connection and then hold it open forever, reading nothing and
        // writing nothing, so the client's request never receives a response. Holding the
        // accepted sockets in a vec keeps them from being closed (which would let curl see
        // EOF and fail fast instead of timing out).
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

/// Write a real `.claims/` store containing one claim, so the in-tree `claim` binary
/// produces a genuine `--json` report. Returns the tempdir root the store lives under (the
/// `--claims-dir` the script checks).
fn claims_store_with(id: &str, frontmatter: &str) -> TempDir {
    let dir = TempDir::new().expect("temp dir for the .claims store");
    let claims = dir.path().join(".claims");
    std::fs::create_dir_all(&claims).expect("create .claims");
    let file = claims.join(format!("{}.md", id.rsplit('/').next().unwrap()));
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let text = format!("---\n{frontmatter}\n---\nThe libfoo pin holds.\n");
    std::fs::write(&file, text).expect("write the claim file");
    dir
}

/// The path to `ci/hub-ingest.sh` — the workspace root is two levels above this crate.
fn script_path() -> PathBuf {
    workspace_root().join("ci").join("hub-ingest.sh")
}

/// The workspace root: `crates/claim-hub` is `<root>/crates/claim-hub`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root is two levels above crates/claim-hub")
        .to_path_buf()
}

/// The in-tree `claim` binary the tests point the script at, so the report the script
/// pushes is the CLI's real output, not a fixture.
///
/// `claim` is a different workspace crate, and `cargo test -p claim-hub` does not set
/// `CARGO_BIN_EXE_claim`, so the path is computed from this test binary's own location —
/// `target/<profile>/deps/<test>` puts `claim` at `target/<profile>/claim`. Under the gate
/// (`cargo test --workspace`) it is already built. As a standalone-run fallback the binary
/// is built once if absent, behind a global lock so concurrent test threads never spawn
/// competing `cargo build` processes (which would serialize on the cargo build lock and
/// stall the run).
fn claim_binary() -> PathBuf {
    use std::sync::OnceLock;

    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let profile_dir = std::env::current_exe()
                .expect("the test executable's path")
                .parent()
                .expect("test binaries live in target/<profile>/deps")
                .parent()
                .expect("the profile dir is deps' parent")
                .to_path_buf();
            let bin = profile_dir.join(if cfg!(windows) { "claim.exe" } else { "claim" });
            if !bin.exists() {
                let status = Command::new(env!("CARGO"))
                    .args(["build", "-p", "claim"])
                    .current_dir(workspace_root())
                    .status()
                    .expect("build the claim binary");
                assert!(status.success(), "building the claim binary failed");
            }
            assert!(
                bin.exists(),
                "the claim binary was not found or built at {}",
                bin.display()
            );
            bin
        })
        .clone()
}
