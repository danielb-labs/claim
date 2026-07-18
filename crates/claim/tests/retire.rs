//! Integration tests for `claim retire <id> --note`: closing a claim on purpose.
//!
//! Every test drives the built binary against a throwaway git repo (see
//! [`common::TestRepo`]). Retirement writes a log entry but runs no check, so no
//! clock pinning is needed unless status is asserted.

mod common;

use common::TestRepo;
use predicates::prelude::*;

/// A cmd claim whose check body is inert (`retire` never runs it).
fn claim_file(id: &str) -> String {
    format!(
        "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: on-change\nmax-age: 30d\n---\nStatement for {id}.\n"
    )
}

#[test]
fn retire_appends_a_retire_adjudication_with_the_note() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("pin", &claim_file("pin"));

    repo.claim()
        .args(["retire", "pin", "--note", "libfoo 5.0 shipped; re-reviewed"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Retired claim 'pin'"))
        // The human output tells the user what to commit (invariant #4).
        .stdout(predicate::str::contains("git -C"))
        .stdout(predicate::str::contains(".claims/log/pin/"));

    // Exactly one entry, an adjudication carrying the note.
    let entries = repo.log_entries("pin");
    assert_eq!(entries.len(), 1);
    let event = &entries[0]["event"];
    assert_eq!(event["type"], "adjudication");
    assert_eq!(event["action"]["action"], "retire");
    assert_eq!(event["action"]["note"], "libfoo 5.0 shipped; re-reviewed");
    // Provenance is git-derived, not typed in.
    assert_eq!(entries[0]["actor"], "Test User <test@example.com>");
    assert_eq!(entries[0]["commit"].as_str().unwrap().len(), 40);
}

#[test]
fn retired_claim_reads_as_retired_via_list_and_log() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("pin", &claim_file("pin"));
    // A prior passing verdict, to prove retirement is terminal over a later Held.
    repo.write_verdict("pin", "2026-07-10T00:00:00Z", "held");

    repo.claim()
        .args(["retire", "pin", "--note", "closed"])
        .assert()
        .success();

    // The computed status is Retired (derived from the log, never stored).
    repo.claim()
        .args(["list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("retired"));

    // `log` shows the whole history ending in the retirement — history preserved.
    let out = repo
        .claim()
        .args(["--json", "log", "pin"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let entries = v["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "the prior held plus the retirement");
    assert_eq!(entries[0]["verdict"], "held");
    assert_eq!(entries[1]["event"], "adjudication");
    assert_eq!(entries[1]["verdict"], "retire");
}

#[test]
fn retire_is_allowed_on_any_claim_not_only_drifted() {
    // A perfectly verified claim can still be retired (the world changed).
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("pin", &claim_file("pin"));
    repo.write_verdict("pin", "2026-07-17T00:00:00Z", "held");

    repo.claim()
        .args(["retire", "pin", "--note", "superseded"])
        .assert()
        .success();
    assert_eq!(repo.log_count("pin"), 2);
}

#[test]
fn retire_requires_a_note() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("pin", &claim_file("pin"));

    // clap rejects the missing required --note as a usage error (exit 2), and
    // nothing is written.
    repo.claim()
        .args(["retire", "pin"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--note"));
    assert_eq!(repo.log_count("pin"), 0);
}

#[test]
fn retire_of_an_unknown_id_errors_and_writes_nothing() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("pin", &claim_file("pin"));

    repo.claim()
        .args(["retire", "does-not-exist", "--note", "x"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "no claim with id 'does-not-exist'",
        ));
    // No stray log directory for the phantom id.
    assert!(!repo.exists(".claims/log/does-not-exist"));
}

#[test]
fn retire_json_shape_carries_the_essentials() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("pin", &claim_file("pin"));

    let out = repo
        .claim()
        .args(["--json", "retire", "pin", "--note", "closed for good"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("stdout is one JSON object");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["id"], "pin");
    assert_eq!(v["note"], "closed for good");
    assert_eq!(v["commit"].as_str().unwrap().len(), 40);
    assert_eq!(v["actor"], "Test User <test@example.com>");
    let to_commit = v["to_commit"].as_array().unwrap();
    assert_eq!(to_commit.len(), 1);
    assert!(to_commit[0]
        .as_str()
        .unwrap()
        .starts_with(".claims/log/pin/"));
}

#[test]
fn retire_unknown_id_json_error_carries_a_kind() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();

    let out = repo
        .claim()
        .args(["--json", "retire", "nope", "--note", "x"])
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let v: serde_json::Value =
        serde_json::from_slice(&out).expect("the error is a JSON object on stderr");
    assert_eq!(v["status"], "error");
    assert_eq!(v["kind"], "invalid-input");
}
