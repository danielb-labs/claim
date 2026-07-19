//! CLI-level tests over the real `claim-hub` binary: argument parsing and the
//! environment-override path, driven through a spawned process.
//!
//! These cover the thin input wiring the in-process tests cannot: `parse_args`
//! (`--config`/`-c`, a missing value, an unrecognized argument) and that a
//! `CLAIM_HUB_*` variable set on the *spawned* process actually reaches the config
//! through `from_process` → `apply_env`. The environment is set on the child
//! (`Command::env`), never on the test process, so nothing leaks between tests and
//! the runs stay deterministic (CLAUDE.md). Most cases exit 1 with a message naming
//! what to fix, caught at config/boot time before serving.
//!
//! One case ([`boots_config_less_from_env_and_status_reports_truthful_zeros`]) is the
//! exception: it proves the empty-volume contract — no `hub.toml` and no `--config`,
//! only `CLAIM_HUB_LISTEN`/`CLAIM_HUB_DATABASE`, so the binary must fall back to an
//! empty config and serve. That one boots the process for real, binds a loopback port,
//! reads `/status`, and kills it. It is the exact path `docker run` against an empty
//! volume hits.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::str::contains;

/// A fresh invocation of the built `claim-hub` binary, with a clean environment so a
/// stray `CLAIM_HUB_*` or `RUST_LOG` in the runner cannot perturb a case. Individual
/// tests add back only the variables they mean to exercise.
fn hub() -> Command {
    let mut cmd = Command::cargo_bin("claim-hub").expect("the claim-hub binary is built");
    cmd.env_clear();
    cmd
}

#[test]
fn config_flag_without_a_value_is_a_named_usage_error() {
    hub()
        .arg("--config")
        .assert()
        .failure()
        .code(1)
        .stderr(contains("`--config` needs a path argument"));
}

#[test]
fn an_unrecognized_argument_is_named_with_usage() {
    hub()
        .arg("--frobnicate")
        .assert()
        .failure()
        .code(1)
        .stderr(contains("unrecognized argument `--frobnicate`"))
        .stderr(contains("usage: claim-hub"));
}

#[test]
fn the_short_config_alias_is_accepted_and_reports_a_missing_file() {
    // `-c` is the `--config` alias: it reaches config loading, which then fails
    // naming the missing file — proving the alias wired the path through, not that a
    // usage error swallowed it.
    hub()
        .args(["-c", "/no/such/hub/config.toml"])
        .assert()
        .failure()
        .code(1)
        .stderr(contains("config `/no/such/hub/config.toml`"));
}

#[test]
fn a_malformed_listen_env_override_reaches_the_config_and_is_named() {
    // The env var is set on the *spawned* process. A valid config file sets `listen`;
    // `CLAIM_HUB_LISTEN` overrides it with a malformed value, and boot fails naming
    // the variable. Were `from_process` to read a differently-named variable, this
    // override would not apply and the run would proceed past config — so this case
    // catches a renamed `CLAIM_HUB_LISTEN`.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("hub.toml");
    std::fs::write(&config_path, "listen = \"127.0.0.1:8080\"\n").unwrap();
    hub()
        .env("CLAIM_HUB_LISTEN", "not-an-address")
        .arg("--config")
        .arg(&config_path)
        .assert()
        .failure()
        .code(1)
        .stderr(contains("CLAIM_HUB_LISTEN"))
        .stderr(contains("invalid socket address syntax"));
}

#[test]
fn a_database_env_override_reaches_the_config() {
    // `CLAIM_HUB_DATABASE` set on the child points the database at a directory, which
    // SQLite cannot open as a file — so boot fails naming *that* path, proving the
    // override reached the config (the file names a different, openable path). A
    // renamed `CLAIM_HUB_DATABASE` in `from_process` would leave the file's path in
    // place and this directory would never be named.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("hub.toml");
    let file_db = dir.path().join("from-file.db");
    std::fs::write(
        &config_path,
        format!("database = {:?}\n", file_db.to_str().unwrap()),
    )
    .unwrap();
    let dir_as_db = dir.path().to_str().unwrap();
    hub()
        .env("CLAIM_HUB_DATABASE", dir_as_db)
        .arg("--config")
        .arg(&config_path)
        .assert()
        .failure()
        .code(1)
        .stderr(contains("opening the hub database"))
        .stderr(contains(dir_as_db));
}

#[test]
fn a_config_less_boot_without_a_read_auth_decision_fails_loudly() {
    // Secure-by-default (§4.5 decision 5): a hub with no config and no read-auth env opt-in
    // is authed-everything with no authenticator, which cannot serve anyone — so it must
    // FAIL the boot loudly, never silently serve open reads. This is the dangerous
    // regression the item warns about, guarded end-to-end through the real binary.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("hub.db");
    let port = free_loopback_port();
    hub()
        .current_dir(dir.path())
        .env("CLAIM_HUB_LISTEN", format!("127.0.0.1:{port}"))
        .env("CLAIM_HUB_DATABASE", &db_path)
        .assert()
        .failure()
        .code(1)
        .stderr(contains("read auth"))
        .stderr(contains("open_reads"));
}

#[test]
fn boots_config_less_with_open_reads_optin_and_status_reports_truthful_zeros() {
    // The empty-volume contract (HUB-IMPLEMENTATION.md §1.13): no `hub.toml`, no
    // `--config`, only `CLAIM_HUB_*` env overrides — so the binary must fall back to an
    // empty config and serve. `CLAIM_HUB_OPEN_READS=true` is the explicit, secure opt-in
    // that lets an empty-volume hub serve reads with no authenticator on a trusted private
    // network — the ONLY way to that state (without it the boot fails loudly, proven above).
    // This is the exact command `docker run` against a fresh volume issues once the operator
    // has made the read-auth decision. The child's working directory is an empty temp dir, so
    // there is provably no default `hub.toml` to read, and the port is a fresh loopback bind.
    let dir = tempfile::tempdir().unwrap();
    assert!(
        !dir.path().join("hub.toml").exists(),
        "the temp working directory must have no default config file"
    );
    let db_path = dir.path().join("hub.db");
    let port = free_loopback_port();

    let bin = assert_cmd::cargo::cargo_bin("claim-hub");
    let mut child = std::process::Command::new(bin)
        .current_dir(dir.path())
        .env_clear()
        .env("CLAIM_HUB_LISTEN", format!("127.0.0.1:{port}"))
        .env("CLAIM_HUB_DATABASE", &db_path)
        .env("CLAIM_HUB_OPEN_READS", "true")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the config-less hub");

    let status = poll_status(port, Duration::from_secs(20));
    // Kill before asserting so the process never outlives the test, whatever the result.
    let _ = child.kill();
    let _ = child.wait();

    let status = status.unwrap_or_else(|| {
        panic!("the config-less hub never served /status on port {port}");
    });
    assert!(
        status.contains("\"ledger_head\":0") || status.contains("\"ledger_head\": 0"),
        "empty ledger reports head 0: {status}"
    );
    assert!(
        status.contains("\"registry_version\":0") || status.contains("\"registry_version\": 0"),
        "empty registry reports version 0: {status}"
    );
    assert!(
        db_path.exists(),
        "the hub created its database on first boot at {}",
        db_path.display()
    );
}

/// Reserve a free loopback TCP port by binding it and reading the assigned number, then
/// releasing it. The hub re-binds it milliseconds later; a race is possible but rare, and
/// a lost race fails loudly as a boot error rather than a wrong answer.
fn free_loopback_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind an ephemeral loopback port")
        .local_addr()
        .expect("read the assigned port")
        .port()
}

/// Poll `GET /status` on the loopback port until it answers `200 OK`, returning the
/// response body, or `None` if `deadline` elapses first. A minimal raw-socket HTTP/1.0
/// request so the test needs no HTTP client dependency; the hub is a local loopback
/// server, so no real network is touched.
fn poll_status(port: u16, deadline: Duration) -> Option<String> {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if let Some(body) = try_status(port) {
            return Some(body);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// One `GET /status` attempt: connect, send an HTTP/1.0 request, read the whole
/// response, and return the body when the status line is `200 OK`. Any connection or
/// read failure (the hub is not up yet) is `None`, so the caller retries.
fn try_status(port: u16) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    // HTTP/1.0 with an explicit close so the server closes the socket after the body,
    // letting `read_to_string` reach EOF without parsing `Content-Length`.
    stream
        .write_all(b"GET /status HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let (head, body) = response.split_once("\r\n\r\n")?;
    if head.lines().next()?.contains("200") {
        Some(body.to_owned())
    } else {
        None
    }
}
