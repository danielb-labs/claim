//! CLI-level tests over the real `claim-hub` binary: argument parsing and the
//! environment-override path, driven through a spawned process.
//!
//! These cover the thin input wiring the in-process tests cannot: `parse_args`
//! (`--config`/`-c`, a missing value, an unrecognized argument) and that a
//! `CLAIM_HUB_*` variable set on the *spawned* process actually reaches the config
//! through `from_process` → `apply_env`. The environment is set on the child
//! (`Command::env`), never on the test process, so nothing leaks between tests and
//! the runs stay deterministic (CLAUDE.md). Each case exits 1 with a message naming
//! what to fix; none binds a port or reaches the network — every failure is caught
//! at config/boot time before serving.

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
