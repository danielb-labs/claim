//! Integration tests for `claim init`.

mod common;

use common::TestRepo;
use predicates::prelude::*;

#[test]
fn init_creates_the_store_directories() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();

    assert!(repo.exists(".claims"), ".claims/ must be created");
    assert!(repo.exists(".claims/log"), ".claims/log/ must be created");
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
    // parser is clean.
    let repo = TestRepo::new();
    let output = repo
        .claim()
        .args(["--json", "init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    let trimmed = text.trim_start();
    assert!(
        trimmed.starts_with('{'),
        "stdout must be exactly one JSON object, got: {text}"
    );
}
