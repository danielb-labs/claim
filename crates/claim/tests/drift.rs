//! Integration tests for `claim drift`: the review queue and its exit code.

mod common;

use common::TestRepo;
use predicates::prelude::*;

const NOW: &str = "2026-07-17T00:00:00Z";

fn claim_file(id: &str, supports: &str) -> String {
    let supports_block = if supports.is_empty() {
        String::new()
    } else {
        format!("supports:\n  - {supports}\n")
    };
    format!(
        "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: on-change\nmax-age: 120d\n{supports_block}---\nStatement for {id}.\n"
    )
}

#[test]
fn drift_lists_only_drifted_claims_with_supports_and_exits_one() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();

    // A drifted claim (latest verdict drifted) that supports a decision.
    repo.write_claim("gone", &claim_file("gone", "requirements.txt#libfoo"));
    repo.write_verdict("gone", "2026-07-10T00:00:00Z", "held");
    repo.write_verdict("gone", "2026-07-15T00:00:00Z", "drifted");

    // A verified claim, which must NOT appear.
    repo.write_claim("fine", &claim_file("fine", ""));
    repo.write_verdict("fine", "2026-07-16T00:00:00Z", "held");

    // A stale claim (overdue but not drifted), which also must NOT appear — drift
    // is drift, not staleness.
    repo.write_claim("stale", &claim_file("stale", ""));
    // No verdicts → stale, not drifted.

    let output = repo
        .claim_at(NOW)
        .args(["--json", "drift"])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["drifted_count"], 1);
    let drifted = v["drifted"].as_array().unwrap();
    assert_eq!(drifted.len(), 1);
    assert_eq!(drifted[0]["id"], "gone");
    assert_eq!(drifted[0]["supports"][0], "requirements.txt#libfoo");
    assert!(drifted[0]["file"].as_str().unwrap().ends_with("gone.md"));
}

#[test]
fn drift_exits_zero_when_nothing_has_drifted() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("fine", &claim_file("fine", ""));
    repo.write_verdict("fine", "2026-07-16T00:00:00Z", "held");

    repo.claim_at(NOW)
        .args(["drift"])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("clean"));
}

#[test]
fn drift_human_output_shows_the_statement_and_supports() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("gone", &claim_file("gone", "requirements.txt#libfoo"));
    repo.write_verdict("gone", "2026-07-15T00:00:00Z", "drifted");

    repo.claim_at(NOW)
        .args(["drift"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("Statement for gone."))
        .stdout(predicate::str::contains("requirements.txt#libfoo"))
        .stdout(predicate::str::contains("no longer true"));
}

#[test]
fn drift_on_an_empty_store_exits_zero() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.claim_at(NOW).args(["drift"]).assert().code(0);
}

#[test]
fn drift_json_envelope_carries_now_and_exit() {
    // m5: the drift envelope includes `now` and `exit`, matching `check`.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("gone", &claim_file("gone", ""));
    repo.write_verdict("gone", "2026-07-15T00:00:00Z", "drifted");

    let output = repo
        .claim_at(NOW)
        .args(["--json", "drift"])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert!(
        v["now"].is_string(),
        "the envelope records the computed-at instant"
    );
    assert_eq!(v["exit"], 1);
    assert_eq!(v["errors"].as_array().unwrap().len(), 0);
}

#[test]
fn drift_reports_a_load_error_and_exits_two() {
    // M1: a malformed file floors drift's exit at 2 while the good claims triage.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("gone", &claim_file("gone", ""));
    repo.write_verdict("gone", "2026-07-15T00:00:00Z", "drifted");
    repo.write_claim("bad", "not a claim\n");

    let output = repo
        .claim_at(NOW)
        .args(["--json", "drift"])
        .assert()
        .code(2) // a load error is the loudest condition, above drift's 1
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).unwrap();
    // The good drifted claim still triaged.
    assert_eq!(v["drifted_count"], 1);
    assert_eq!(v["drifted"][0]["id"], "gone");
    // The bad file reported.
    let errors = v["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert!(errors[0]["file"].as_str().unwrap().ends_with("bad.md"));
}
