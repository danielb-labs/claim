//! Integration tests for `claim init`.

mod common;

use common::TestRepo;
use predicates::prelude::*;

#[test]
fn init_creates_only_the_claims_dir() {
    // The CLI is a stateless verifier: it writes no verdict log, so `init` scaffolds
    // only `.claims/` and never a `.claims/log/` tree.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();

    assert!(repo.exists(".claims"), ".claims/ must be created");
    assert!(
        !repo.exists(".claims/log"),
        "no verdict log tree is created; the CLI stores no verdicts"
    );
}

#[test]
fn init_is_idempotent() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();

    // A second run is not an error and reports the store already present.
    repo.claim()
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("already present"));
}

#[test]
fn init_json_shape_is_stable() {
    let repo = TestRepo::new();
    let output = repo
        .claim()
        .args(["--json", "init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let v: serde_json::Value = serde_json::from_slice(&output).expect("init --json is valid JSON");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["created"], true);
    assert!(
        v["claims_dir"].as_str().unwrap().ends_with(".claims"),
        "claims_dir points at the .claims directory"
    );
    // A re-run reports created: false, still ok.
    let output = repo
        .claim()
        .args(["--json", "init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["created"], false);
}

#[test]
fn init_json_emits_only_json_on_stdout() {
    // A --json run must put nothing but the JSON object on stdout, so a pipe to a
    // parser is clean. Full-parse the whole stdout (not just a `{` prefix), which
    // fails if any stray line precedes or follows the object.
    let repo = TestRepo::new();
    let output = repo
        .claim()
        .args(["--json", "init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_slice(&output).expect("the whole stdout must parse as one JSON object");
    assert_eq!(v["status"], "ok");
}

#[test]
fn init_outside_a_git_repo_warns() {
    // A store outside any git repo is usable but degenerate (`claim add` needs a
    // commit to attribute). init still succeeds, but warns on stderr — never on
    // stdout, so `--json` stays clean.
    let dir = TestRepo::no_git();
    dir.claim()
        .arg("init")
        .assert()
        .success()
        .stderr(predicate::str::contains("not inside a git repository"));

    // The warning does not leak into the JSON object on stdout.
    let output = dir
        .claim()
        .args(["--json", "init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_slice(&output).expect("json stdout stays clean despite the warning");
    assert_eq!(v["status"], "ok");
}
