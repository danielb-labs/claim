//! Integration tests for `claim stats`: the pilot instrumentation.
//!
//! The store here mixes verified, drifted, stale, retired, and never-verified
//! claims, plus every verdict kind, so the counts exercise each branch. `now` is
//! pinned via `CLAIM_NOW` so the status breakdown (which depends on staleness) is
//! deterministic. The honesty assertion — that the two kill metrics are *absent*,
//! not fabricated — is the point of the verb.

mod common;

use common::TestRepo;
use predicates::prelude::*;

const NOW: &str = "2026-07-18T00:00:00Z";

fn claim_file(id: &str, max_age: &str) -> String {
    format!(
        "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: on-change\nmax-age: {max_age}\n---\nStatement for {id}.\n"
    )
}

/// A store with one claim of each status plus varied verdict kinds:
/// - `fresh`: held recently -> verified.
/// - `gone`: held then drifted -> drifted (and a "drift caught").
/// - `old`: held long ago, past a short max-age -> stale.
/// - `closed`: held then retired -> retired.
/// - `never`: no verdicts, plus one broken and one unverifiable -> stale, never
///   passed.
fn mixed_store() -> TestRepo {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();

    repo.write_claim("fresh", &claim_file("fresh", "120d"));
    repo.write_verdict("fresh", "2026-07-15T00:00:00Z", "held");

    repo.write_claim("gone", &claim_file("gone", "120d"));
    repo.write_verdict("gone", "2026-07-10T00:00:00Z", "held");
    repo.write_verdict("gone", "2026-07-16T00:00:00Z", "drifted");

    repo.write_claim("old", &claim_file("old", "30d"));
    repo.write_verdict("old", "2026-01-01T00:00:00Z", "held");

    repo.write_claim("closed", &claim_file("closed", "120d"));
    repo.write_verdict("closed", "2026-07-10T00:00:00Z", "held");
    repo.write_retirement("closed", "2026-07-12T00:00:00Z", "superseded");

    repo.write_claim("never", &claim_file("never", "30d"));
    repo.write_verdict("never", "2026-07-14T00:00:00Z", "broken");
    repo.write_verdict("never", "2026-07-15T00:00:00Z", "unverifiable");

    repo
}

#[test]
fn stats_counts_statuses_and_verdicts_across_the_store() {
    let repo = mixed_store();
    let out = repo
        .claim_at(NOW)
        .args(["--json", "stats"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();

    assert_eq!(v["total"], 5);
    // Status breakdown partitions the total.
    assert_eq!(v["by_status"]["verified"], 1); // fresh
    assert_eq!(v["by_status"]["drifted"], 1); // gone
    assert_eq!(v["by_status"]["stale"], 2); // old, never
    assert_eq!(v["by_status"]["retired"], 1); // closed

    // Verdict totals across all logs: held from fresh/gone/old/closed = 4; drifted 1
    // from gone; broken 1 and unverifiable 1 from never.
    assert_eq!(v["verdicts"]["held"], 4);
    assert_eq!(v["verdicts"]["drifted"], 1);
    assert_eq!(v["verdicts"]["broken"], 1);
    assert_eq!(v["verdicts"]["unverifiable"], 1);

    // Derived rollups.
    assert_eq!(v["drifts_caught"], 1, "only `gone` ever drifted");
    assert_eq!(v["never_passed"], 1, "only `never` has no held");
    assert_eq!(v["overdue"], 3, "drifted + the two stale");
    // Oldest last-verified is `old`'s 2026-01-01 held.
    assert_eq!(v["oldest_last_verified"], "2026-01-01T00:00:00Z");
}

#[test]
fn stats_does_not_fabricate_the_kill_metrics() {
    let repo = mixed_store();
    let out = repo
        .claim_at(NOW)
        .args(["--json", "stats"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();

    let far = &v["needs_human_input"]["false_alarm_rate"];
    // The rate itself is absent — never a number — and the denominator it *does*
    // have is the drifts-caught count.
    assert_eq!(far["available"], false);
    assert!(far.get("rate").is_none(), "no fabricated rate field");
    assert_eq!(far["denominator"], 1);
    assert!(far["needs"].as_str().unwrap().contains("false alarm"));

    let mpc = &v["needs_human_input"]["minutes_per_claim"];
    assert_eq!(mpc["available"], false);
    assert!(mpc.get("minutes").is_none(), "no fabricated minutes field");
    assert_eq!(mpc["denominator"], 5, "the total-claims denominator");
}

#[test]
fn stats_human_output_states_the_metrics_need_human_input() {
    let repo = mixed_store();
    repo.claim_at(NOW)
        .args(["stats"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Claims: 5"))
        .stdout(predicate::str::contains("Drifts caught"))
        // The kill metrics are shown as unavailable with the reason, not as numbers.
        .stdout(predicate::str::contains("false-alarm rate: not available"))
        .stdout(predicate::str::contains("minutes per claim: not available"));
}

#[test]
fn stats_on_an_empty_store_is_all_zeros_and_exits_zero() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();

    let out = repo
        .claim_at(NOW)
        .args(["--json", "stats"])
        .assert()
        .success() // informational: always exit 0
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["total"], 0);
    assert_eq!(v["drifts_caught"], 0);
    assert_eq!(v["never_passed"], 0);
    assert_eq!(v["oldest_last_verified"], serde_json::Value::Null);
    assert_eq!(v["needs_human_input"]["false_alarm_rate"]["denominator"], 0);
}

#[test]
fn stats_reports_a_load_error_without_aborting_or_failing() {
    // A malformed file is reported in `errors` but stats still counts the good
    // claims and exits 0 (informational).
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("good", &claim_file("good", "120d"));
    repo.write_verdict("good", "2026-07-15T00:00:00Z", "held");
    repo.write_claim("bad", "not a claim, no frontmatter\n");

    let out = repo
        .claim_at(NOW)
        .args(["--json", "stats"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["total"], 1, "the good claim still counts");
    let errors = v["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["file"], ".claims/bad.md");
}
