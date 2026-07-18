//! Integration tests for `claim drift`: the review queue and its exit code.
//!
//! `drift` is a stateless runtime verifier like `check`: it *runs* each claim's
//! checks and lists the claims whose check reports Drifted right now. There is no
//! stored verdict log to read. A held claim never appears; a broken check floors the
//! exit at 2 without making the claim "drifted".

mod common;

use common::TestRepo;
use predicates::prelude::*;

/// A cmd claim whose `run` the test picks, so the check holds or drifts against the
/// seeded `requirements.txt` deterministically.
fn claim_file(id: &str, run: &str, supports: &str) -> String {
    let supports_block = if supports.is_empty() {
        String::new()
    } else {
        format!("supports:\n  - {supports}\n")
    };
    format!(
        "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"{run}\"\n{supports_block}---\nStatement for {id}.\n"
    )
}

/// A grep that holds against the committed `requirements.txt` (which pins 4.2).
const HOLDS: &str = "grep -q 'libfoo==4.2' requirements.txt";
/// A grep for a pin that is not present, so the check drifts.
const DRIFTS: &str = "grep -q 'libfoo==9.9' requirements.txt";

/// A store-ready repo: init'd, with the seeded committed `requirements.txt`.
fn ready_repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo
}

#[test]
fn drift_lists_only_drifted_claims_with_supports_and_exits_one() {
    let repo = ready_repo();

    // A drifted claim (its check reports Drifted now) that supports a decision.
    repo.write_claim(
        "gone",
        &claim_file("gone", DRIFTS, "requirements.txt#libfoo"),
    );
    // A held claim, which must NOT appear.
    repo.write_claim("fine", &claim_file("fine", HOLDS, ""));

    let output = repo
        .claim()
        .args(["--json", "drift"])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["exit"], 1);
    assert_eq!(v["drifted_count"], 1);
    let drifted = v["drifted"].as_array().unwrap();
    assert_eq!(drifted.len(), 1);
    assert_eq!(drifted[0]["id"], "gone");
    assert_eq!(drifted[0]["supports"][0], "requirements.txt#libfoo");
    assert!(drifted[0]["file"].as_str().unwrap().ends_with("gone.md"));
    assert_eq!(drifted[0]["statement"], "Statement for gone.");
}

#[test]
fn drift_exits_zero_when_nothing_has_drifted() {
    let repo = ready_repo();
    repo.write_claim("fine", &claim_file("fine", HOLDS, ""));

    repo.claim()
        .args(["drift"])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("clean"));
}

#[test]
fn drift_human_output_shows_the_statement_and_supports() {
    let repo = ready_repo();
    repo.write_claim(
        "gone",
        &claim_file("gone", DRIFTS, "requirements.txt#libfoo"),
    );

    repo.claim()
        .args(["drift"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("Statement for gone."))
        .stdout(predicate::str::contains("requirements.txt#libfoo"))
        .stdout(predicate::str::contains("no longer true"));
}

#[test]
fn drift_on_an_empty_store_exits_zero() {
    let repo = ready_repo();
    repo.claim().args(["drift"]).assert().code(0);
}

#[test]
fn drift_json_envelope_carries_exit_and_no_errors() {
    let repo = ready_repo();
    repo.write_claim("gone", &claim_file("gone", DRIFTS, ""));

    let output = repo
        .claim()
        .args(["--json", "drift"])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(v["exit"], 1);
    assert_eq!(v["errors"].as_array().unwrap().len(), 0);
}

#[test]
fn a_broken_check_floors_the_exit_at_two_and_does_not_drift() {
    // Golden invariant #1: a check that cannot run is Broken, never a pass and never
    // conflated with Drifted. It floors the exit at 2 (above drift's 1) while the good
    // drifted claim is still triaged.
    let repo = ready_repo();
    repo.write_claim("gone", &claim_file("gone", DRIFTS, ""));
    repo.write_claim(
        "broken",
        "---\nid: broken\nchecks:\n  - kind: cmd\n    run: \"this-binary-does-not-exist-xyz\"\n---\nA claim with a broken check.\n",
    );

    let output = repo
        .claim()
        .args(["--json", "drift"])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).unwrap();
    // The good drifted claim still triaged; the broken one is not counted as drifted.
    assert_eq!(v["drifted_count"], 1);
    assert_eq!(v["drifted"][0]["id"], "gone");
}

#[test]
fn drift_reports_a_load_error_and_exits_two() {
    // A malformed file floors drift's exit at 2 while the good claims triage. A file
    // that opens with a fence declares itself a claim, so malformed YAML under it is a
    // loud error (a fenceless doc would be a skipped non-claim).
    let repo = ready_repo();
    repo.write_claim("gone", &claim_file("gone", DRIFTS, ""));
    repo.write_claim("bad", "---\nchecks: [unterminated\n---\nS.\n");

    let output = repo
        .claim()
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
