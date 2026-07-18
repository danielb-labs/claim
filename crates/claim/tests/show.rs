//! Integration tests for `claim show <id>`: print one claim's full definition.
//!
//! `show` is the static counterpart to `claim check <id>` — it reads the claim and
//! prints everything the file holds (statement, checks, supports, hub) but runs
//! nothing. These tests drive the built binary against a real temp store and assert
//! the exit code, the human content, and the `--json` shape, including the loud
//! unknown-id path (exit 2, id named) that must never degrade to an empty success.

mod common;

use common::TestRepo;
use predicates::prelude::*;

/// A plain cmd claim with the given id. The check body is inert — `show` never runs
/// it.
fn claim_file(id: &str) -> String {
    format!("---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n---\nStatement for {id}.\n")
}

/// A store with a couple of unremarkable claims plus a rich one carrying a skip and
/// hub hints, for the "renders everything" cases.
fn seeded_store() -> TestRepo {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("a-fact", &claim_file("a-fact"));
    repo.write_claim(
        "payments/libfoo-pin",
        "---\nid: payments/libfoo-pin\nchecks:\n  - kind: cmd\n    run: \"grep -q libfoo requirements.txt\"\n    negate: true\n    skip:\n      reason: windows CI has no grep\n      unless: \"test -x /usr/bin/grep\"\n      until: 2027-01-01\nsupports:\n  - requirements.txt#libfoo\n  - other-claim\nhub:\n  recheck: 30d\n  max-age: 120d\n---\nWe pin libfoo at 4.2.\n",
    );
    repo
}

/// Parse `show --json` into its envelope object, asserting it is one object with
/// `status: ok`.
fn show_json(repo: &TestRepo, id: &str) -> serde_json::Value {
    let output = repo
        .claim()
        .args(["--json", "show", id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).expect("show --json is one object");
    assert_eq!(v["status"], "ok");
    v
}

#[test]
fn prints_an_existing_claims_definition_and_exits_0() {
    let repo = seeded_store();
    repo.claim()
        .args(["show", "payments/libfoo-pin"])
        .assert()
        .success()
        .stdout(predicate::str::contains("payments/libfoo-pin"))
        .stdout(predicate::str::contains("We pin libfoo at 4.2."))
        .stdout(predicate::str::contains("grep -q libfoo requirements.txt"))
        .stdout(predicate::str::contains("requirements.txt#libfoo"));
}

#[test]
fn json_carries_the_structured_definition() {
    let repo = seeded_store();
    let v = show_json(&repo, "payments/libfoo-pin");

    assert_eq!(v["id"], "payments/libfoo-pin");
    assert!(v["file"].as_str().unwrap().ends_with("libfoo-pin.md"));
    assert_eq!(v["statement"], "We pin libfoo at 4.2.");

    // Checks reuse the core model's serialization: a `kind` discriminator beside
    // that kind's fields.
    let checks = v["checks"].as_array().expect("checks is an array");
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["kind"], "cmd");
    assert_eq!(checks[0]["run"], "grep -q libfoo requirements.txt");
    assert_eq!(checks[0]["negate"], true);

    // Supports serialize transparently as their written strings.
    let supports = v["supports"].as_array().expect("supports is an array");
    assert_eq!(supports.len(), 2);
    assert_eq!(supports[0], "requirements.txt#libfoo");

    // Hub hints serialize as their `<N>d` strings, `max-age` kebab-cased.
    assert_eq!(v["hub"]["recheck"], "30d");
    assert_eq!(v["hub"]["max-age"], "120d");
}

#[test]
fn renders_a_skip_and_its_guards() {
    let repo = seeded_store();

    // Human output shows the skip's reason and both guards.
    repo.claim()
        .args(["show", "payments/libfoo-pin"])
        .assert()
        .success()
        .stdout(predicate::str::contains("skip: windows CI has no grep"))
        .stdout(predicate::str::contains("unless: test -x /usr/bin/grep"))
        .stdout(predicate::str::contains("until: 2027-01-01T00:00:00Z"));

    // JSON carries the skip as the core model serializes it, `until` as RFC 3339.
    let v = show_json(&repo, "payments/libfoo-pin");
    let skip = &v["checks"][0]["skip"];
    assert_eq!(skip["reason"], "windows CI has no grep");
    assert_eq!(skip["unless"], "test -x /usr/bin/grep");
    assert_eq!(skip["until"], "2027-01-01T00:00:00Z");
}

#[test]
fn a_claim_with_no_hub_hints_omits_the_block() {
    let repo = seeded_store();
    // The plain `a-fact` claim carries no hub hints and no supports.
    repo.claim()
        .args(["show", "a-fact"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Statement for a-fact."))
        // No hub block is printed, and no supports section.
        .stdout(predicate::str::contains("hub:").not())
        .stdout(predicate::str::contains("supports:").not());

    // In JSON an empty hub is an empty object — the CLI invents no cadence.
    let v = show_json(&repo, "a-fact");
    assert!(
        v["hub"].as_object().unwrap().is_empty(),
        "a hint-free claim carries an empty hub object, never an invented default"
    );
    assert!(v["supports"].as_array().unwrap().is_empty());
}

#[test]
fn an_unknown_id_is_a_loud_exit_2_naming_the_id() {
    let repo = seeded_store();
    // A typo must not print an empty success — it is exit 2, naming the id, per
    // invariant #6 (loud, never a stale green).
    repo.claim()
        .args(["show", "auth/nocycles"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no claim with id 'auth/nocycles'"));
}

#[test]
fn an_unknown_id_in_json_is_an_error_object_with_a_kind() {
    let repo = seeded_store();
    let output = repo
        .claim()
        .args(["--json", "show", "nope"])
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).expect("error is one JSON object");
    assert_eq!(v["status"], "error");
    assert_eq!(v["kind"], "invalid-input");
    assert!(v["error"].as_str().unwrap().contains("nope"));
}

#[test]
fn a_broken_target_file_surfaces_its_parse_error_not_not_found() {
    // The requested id names a file that fails to parse. `show` must report *why* it
    // could not be shown (the parse error), so a typo and a broken file are
    // distinguishable — a blank success would be exactly the quiet failure the tool
    // exists to prevent.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    // A file that opens with a fence (so it declares itself a claim) but has broken
    // YAML: it is a loud load error keyed to its path, `.claims/broken.md` → id
    // `broken`.
    repo.write_claim("broken", "---\nchecks: [unterminated\n---\nS.\n");
    repo.claim()
        .args(["show", "broken"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "claim 'broken' could not be loaded",
        ));
}

#[test]
fn a_malformed_sibling_does_not_stop_showing_a_good_claim() {
    // A different id in the load errors is not `show`'s concern: the requested claim
    // loaded cleanly, so it is shown and the command exits 0. Store-wide health is
    // `list`/`check`, not `show`.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("good", &claim_file("good"));
    repo.write_claim("bad", "---\nchecks: [unterminated\n---\nS.\n");
    repo.claim()
        .args(["show", "good"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Statement for good."));
}
