//! Integration tests for `claim log <id>`: the definition-plus-history join.

mod common;

use common::TestRepo;
use predicates::prelude::*;

fn claim_file(id: &str) -> String {
    format!(
        "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"grep -q libfoo requirements.txt\"\n    when: on-change\nmax-age: 120d\nsupports:\n  - requirements.txt#libfoo\n---\nWe pin libfoo at 4.2.\n"
    )
}

#[test]
fn log_shows_definition_and_history_in_order() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("pin", &claim_file("pin"));
    // Three entries, written out of order to prove `log` orders by timestamp.
    repo.write_verdict("pin", "2026-07-15T00:00:00Z", "drifted");
    repo.write_verdict("pin", "2026-07-10T00:00:00Z", "held");
    repo.write_verdict("pin", "2026-07-12T00:00:00Z", "broken");

    let output = repo
        .claim()
        .args(["--json", "log", "pin"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).unwrap();

    // The definition half.
    assert_eq!(v["id"], "pin");
    assert_eq!(v["definition"]["statement"], "We pin libfoo at 4.2.");
    assert_eq!(v["definition"]["max_age"], "120d");
    assert_eq!(v["definition"]["checks"][0]["kind"], "cmd");
    assert_eq!(v["definition"]["checks"][0]["when"], "on-change");
    assert_eq!(v["definition"]["supports"][0], "requirements.txt#libfoo");

    // The history half: chronological order regardless of write order.
    let entries = v["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0]["verdict"], "held"); // 07-10
    assert_eq!(entries[1]["verdict"], "broken"); // 07-12
    assert_eq!(entries[2]["verdict"], "drifted"); // 07-15
                                                  // Each entry carries actor and commit.
    assert_eq!(entries[0]["actor"], "Test User <test@example.com>");
    assert_eq!(entries[0]["commit"].as_str().unwrap().len(), 40);
}

#[test]
fn log_human_output_shows_the_statement_and_verdicts() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("pin", &claim_file("pin"));
    repo.write_verdict("pin", "2026-07-10T00:00:00Z", "held");

    repo.claim()
        .args(["log", "pin"])
        .assert()
        .success()
        .stdout(predicate::str::contains("We pin libfoo at 4.2."))
        .stdout(predicate::str::contains("max-age: 120d"))
        .stdout(predicate::str::contains("held"))
        .stdout(predicate::str::contains("requirements.txt#libfoo"));
}

#[test]
fn log_of_a_claim_with_no_verdicts_says_so() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("pin", &claim_file("pin"));

    let output = repo
        .claim()
        .args(["--json", "log", "pin"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(v["entries"].as_array().unwrap().len(), 0);

    repo.claim()
        .args(["log", "pin"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no verdicts yet"));
}

#[test]
fn log_of_an_unknown_id_errors() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("pin", &claim_file("pin"));

    repo.claim()
        .args(["log", "does-not-exist"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "no claim with id 'does-not-exist'",
        ));
}

#[test]
fn log_unknown_id_json_error_carries_a_kind() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();

    let output = repo
        .claim()
        .args(["--json", "log", "nope"])
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let v: serde_json::Value =
        serde_json::from_slice(&output).expect("the error is a JSON object on stderr");
    assert_eq!(v["status"], "error");
    assert_eq!(v["kind"], "invalid-input");
}
