//! Integration tests for `claim check`, against real temp git stores.
//!
//! The exit-code contract (0 held, 1 review, 2 broken, highest wins) and the
//! `--report-only` no-write guarantee are the adversarial targets, so each has a
//! direct test. Time is pinned with the `CLAIM_NOW` seam where the due decision
//! needs it; otherwise checks are driven against a controlled tree.

mod common;

use common::TestRepo;
use predicates::prelude::*;

/// A claim file whose cmd check holds iff `requirements.txt` pins libfoo at 4.2.
fn pin_claim(id: &str, when: &str) -> String {
    format!(
        "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"grep -q 'libfoo==4.2' requirements.txt\"\n    when: {when}\nmax-age: 120d\n---\nWe pin libfoo at 4.2.\n"
    )
}

/// A store with a git identity and a committed `requirements.txt` pinning 4.2.
fn ready_repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo
}

#[test]
fn a_holding_check_is_held_appended_and_exit_zero() {
    let repo = ready_repo();
    repo.write_claim("pin", &pin_claim("pin", "on-change"));

    repo.claim()
        .args(["check", "--all"])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("held"));

    // The verdict was appended.
    assert_eq!(
        repo.log_count("pin"),
        1,
        "a persisting run writes one verdict"
    );
    let entries = repo.log_entries("pin");
    assert_eq!(entries[0]["event"]["verdict"], "held");
}

#[test]
fn a_failing_check_is_drifted_and_exit_one() {
    let repo = ready_repo();
    repo.write_claim("pin", &pin_claim("pin", "on-change"));
    // Break the fact.
    repo.write("requirements.txt", "libfoo==5.0\n");

    repo.claim()
        .args(["check", "--all"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("drifted"));

    assert_eq!(repo.log_entries("pin")[0]["event"]["verdict"], "drifted");
}

#[test]
fn a_broken_check_is_broken_and_exit_two() {
    let repo = ready_repo();
    // A command that cannot run: the grep target is fine, but the command itself
    // exits non-0/1 (127 for not-found) → Broken.
    repo.write_claim(
        "broken",
        "---\nid: broken\nchecks:\n  - kind: cmd\n    run: \"this-binary-does-not-exist-xyz\"\n    when: on-change\nmax-age: 120d\n---\nA claim with a broken check.\n",
    );

    repo.claim()
        .args(["check", "--all"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains("broken"));
}

#[test]
fn report_only_writes_no_log_entry_but_still_reports_and_sets_exit() {
    let repo = ready_repo();
    repo.write_claim("pin", &pin_claim("pin", "on-change"));
    repo.write("requirements.txt", "libfoo==5.0\n"); // drifted

    repo.claim()
        .args(["check", "--all", "--report-only"])
        .assert()
        .code(1) // the exit code is still set from the verdict
        .stdout(predicate::str::contains("drifted"))
        .stdout(predicate::str::contains("report-only"));

    // Nothing was written: no log directory, no entries.
    assert_eq!(
        repo.log_count("pin"),
        0,
        "--report-only must not append any verdict"
    );
    assert!(
        !repo.exists(".claims/log/pin"),
        "--report-only must not even create the log directory"
    );
}

#[test]
fn report_only_needs_no_git_identity() {
    // The fork-PR mode must work where no git identity is configured (a persisting
    // run would fail resolving the actor). Build a repo with NO user.name/email.
    let repo = TestRepo::no_identity();
    repo.claim().arg("init").assert().success();
    repo.write_claim("pin", &pin_claim("pin", "on-change"));

    repo.claim()
        .args(["check", "--all", "--report-only"])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("held"));

    // And a persisting run in the same repo *does* fail for lack of identity,
    // confirming report-only genuinely skipped provenance rather than getting lucky.
    repo.claim()
        .args(["check", "--all"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("user.name").or(predicate::str::contains("user.email")));
}

#[test]
fn a_persisting_run_with_nothing_selected_needs_no_identity() {
    // A persisting `--due` run that selects nothing (the every-30d claim is not yet
    // due) must not fail for a missing git identity it would never use: provenance
    // is resolved only when there is work to persist.
    let repo = TestRepo::no_identity();
    repo.claim().arg("init").assert().success();
    repo.write_claim("slow", &pin_claim("slow", "every 30d"));
    repo.write_verdict("slow", "2026-07-15T00:00:00Z", "held"); // 2 days before NOW

    repo.claim_at("2026-07-17T00:00:00Z")
        .args(["check", "--due"])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("No due claims"));
}

#[test]
fn due_skips_a_not_yet_due_claim_and_runs_an_overdue_and_on_change_one() {
    let repo = ready_repo();
    // Three claims: a slow one recently run (not due), a slow one long overdue
    // (due), and an on-change one (always due).
    repo.write_claim("slow-fresh", &pin_claim("slow-fresh", "every 30d"));
    repo.write_claim("slow-stale", &pin_claim("slow-stale", "every 30d"));
    repo.write_claim("fast", &pin_claim("fast", "on-change"));

    // slow-fresh last ran 5 days before `now`; slow-stale 200 days before.
    repo.write_verdict("slow-fresh", "2026-07-12T00:00:00Z", "held");
    repo.write_verdict("slow-stale", "2026-01-01T00:00:00Z", "held");

    let now = "2026-07-17T00:00:00Z";
    repo.claim_at(now).args(["check", "--due"]).assert().code(0);

    // slow-fresh must NOT have a new verdict (still just the one we seeded).
    assert_eq!(
        repo.log_count("slow-fresh"),
        1,
        "a not-yet-due every-30d claim is skipped by --due"
    );
    // slow-stale ran (seeded 1 + this run's 1 = 2).
    assert_eq!(
        repo.log_count("slow-stale"),
        2,
        "an overdue every-30d claim runs under --due"
    );
    // fast ran (no seed, so exactly this run's 1).
    assert_eq!(
        repo.log_count("fast"),
        1,
        "an on-change claim always runs under --due"
    );
}

#[test]
fn an_unresolved_support_is_exit_one_even_when_the_check_holds() {
    let repo = ready_repo();
    // A claim that holds but supports a decision ref whose file does not exist.
    repo.write_claim(
        "orphan",
        "---\nid: orphan\nchecks:\n  - kind: cmd\n    run: \"grep -q 'libfoo==4.2' requirements.txt\"\n    when: on-change\nmax-age: 120d\nsupports:\n  - deleted-decision.md#anchor\n---\nSupports a deleted decision.\n",
    );

    repo.claim()
        .args(["check", "--all"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("held"))
        .stdout(predicate::str::contains("UNRESOLVED support"));
}

#[test]
fn highest_code_wins_across_a_mixed_store() {
    let repo = ready_repo();
    // Held, drifted, and broken claims together → overall exit 2.
    repo.write_claim("held", &pin_claim("held", "on-change"));
    repo.write_claim(
        "broken",
        "---\nid: broken\nchecks:\n  - kind: cmd\n    run: \"nonexistent-cmd-abc\"\n    when: on-change\nmax-age: 120d\n---\nBroken.\n",
    );
    // A drifted claim: grep for a string not present.
    repo.write_claim(
        "drifted",
        "---\nid: drifted\nchecks:\n  - kind: cmd\n    run: \"grep -q 'not-in-file' requirements.txt\"\n    when: on-change\nmax-age: 120d\n---\nDrifted.\n",
    );

    repo.claim().args(["check", "--all"]).assert().code(2);
}

#[test]
fn an_agent_check_is_unverifiable_exit_one_never_a_pass() {
    let repo = ready_repo();
    repo.write_claim(
        "agentic",
        "---\nid: agentic\nchecks:\n  - kind: agent\n    instruction: investigate the changelog\n    when: every 30d\nmax-age: 120d\n---\nNeeds an agent to verify.\n",
    );

    repo.claim()
        .args(["check", "--all"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("unverifiable"));
}

#[test]
fn check_json_shape_is_stable() {
    let repo = ready_repo();
    repo.write_claim("pin", &pin_claim("pin", "on-change"));

    let output = repo
        .claim()
        .args(["--json", "check", "--all"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();

    let v: serde_json::Value =
        serde_json::from_slice(&output).expect("check --json is one JSON object");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["selection"], "all");
    assert_eq!(v["report_only"], false);
    assert_eq!(v["exit"], 0);
    assert_eq!(v["checked"], 1);
    let claims = v["claims"].as_array().unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0]["id"], "pin");
    assert_eq!(claims[0]["persisted"], true);
    assert_eq!(claims[0]["checks"][0]["verdict"], "held");
    assert_eq!(claims[0]["exit"], 0);
}

#[test]
fn check_with_no_claims_exits_zero() {
    let repo = ready_repo();
    repo.claim()
        .args(["check", "--all"])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("No"));
}
