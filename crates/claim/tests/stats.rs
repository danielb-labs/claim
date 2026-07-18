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

    // `gone` is flappy: it drifts, is re-held, then drifts again. Two *fired drift*
    // events, but one unique claim that ever drifted — the distinction M3 turns on.
    // Latest conclusive verdict is Drifted, so its status is Drifted.
    repo.write_claim("gone", &claim_file("gone", "120d"));
    repo.write_verdict("gone", "2026-07-08T00:00:00Z", "held");
    repo.write_verdict("gone", "2026-07-10T00:00:00Z", "drifted");
    repo.write_verdict("gone", "2026-07-12T00:00:00Z", "held");
    repo.write_verdict("gone", "2026-07-16T00:00:00Z", "drifted");

    repo.write_claim("old", &claim_file("old", "30d"));
    repo.write_verdict("old", "2026-01-01T00:00:00Z", "held");

    // `closed` is retired, and its (pre-retirement) held is the *oldest* of any
    // claim. It must NOT pull the staleness frontier back: a retired claim is closed,
    // not due (m2). The frontier stays `old`'s 2026-01-01.
    repo.write_claim("closed", &claim_file("closed", "120d"));
    repo.write_verdict("closed", "2025-12-01T00:00:00Z", "held");
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

    // Verdict totals across all logs: held from fresh(1)/gone(2)/old(1)/closed(1) =
    // 5; drifted 2 from gone (it flapped); broken 1 and unverifiable 1 from never.
    assert_eq!(v["verdicts"]["held"], 5);
    assert_eq!(v["verdicts"]["drifted"], 2);
    assert_eq!(v["verdicts"]["broken"], 1);
    assert_eq!(v["verdicts"]["unverifiable"], 1);

    // Derived rollups.
    assert_eq!(
        v["drifts_caught"], 1,
        "only `gone` ever drifted (unique claim)"
    );
    assert_eq!(v["never_passed"], 1, "only `never` has no held");
    assert_eq!(v["overdue"], 3, "drifted + the two stale");
    // Oldest last-verified is `old`'s 2026-01-01 — NOT `closed`'s older 2025-12-01,
    // which is excluded because `closed` is retired (m2).
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
    // The rate itself is absent — never a number. Its denominator is *fired drift
    // events* (M3): `gone` flapped twice, so 2 — not `drifts_caught` (1 unique
    // claim), which is reported separately. This is the unit PRODUCT.md section 9
    // measures the false-alarm rate over.
    assert_eq!(far["available"], false);
    assert!(far.get("rate").is_none(), "no fabricated rate field");
    assert_eq!(
        far["denominator"], 2,
        "total Drifted verdicts, not unique claims"
    );
    assert_eq!(
        v["drifts_caught"], 1,
        "the unique-claim signal stays distinct"
    );
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
    // Opens with a fence, so it is parsed and its malformed YAML is a loud error; a
    // fenceless doc would be skipped as a non-claim rather than reported.
    repo.write_claim("bad", "---\nchecks: [unterminated\n---\nS.\n");

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

#[test]
fn stats_counts_a_future_only_held_as_never_passed() {
    // M1: a claim whose ONLY Held is timestamped after `now` (clock skew or forgery)
    // has not certified present freshness — `compute_status` excludes it, so the
    // claim reads stale with no last-verified. It must therefore count as
    // never_passed; deriving that from a raw "any Held in history" scan would let the
    // future Held drop it out of the honesty count.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("future", &claim_file("future", "30d"));
    // NOW is 2026-07-18; this Held is a year ahead.
    repo.write_verdict("future", "2027-07-18T00:00:00Z", "held");

    let out = repo
        .claim_at(NOW)
        .args(["--json", "stats"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        v["by_status"]["stale"], 1,
        "the future Held does not verify it"
    );
    assert_eq!(
        v["never_passed"], 1,
        "no past-or-present Held ⇒ never passed"
    );
    assert_eq!(
        v["verdicts"]["held"], 1,
        "the raw log tally still counts it"
    );
    assert_eq!(
        v["oldest_last_verified"],
        serde_json::Value::Null,
        "a future Held is not a last-verified instant"
    );
}

#[test]
fn stats_survives_a_corrupt_verdict_log_and_stays_exit_zero() {
    // M2: a malformed verdict-LOG entry (not just a bad claim file) must degrade like
    // any other per-file fault — the claim is skipped, its log named, the rest still
    // counted, exit 0.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("good", &claim_file("good", "120d"));
    repo.write_verdict("good", "2026-07-15T00:00:00Z", "held");
    repo.write_claim("corrupt", &claim_file("corrupt", "120d"));
    // A log directory holding a non-JSON entry file makes `read_entries` fail for
    // this one claim.
    repo.write(
        ".claims/log/corrupt/2026-07-15T00-00-00Z-0000.json",
        "{ not json",
    );

    let out = repo
        .claim_at(NOW)
        .args(["--json", "stats"])
        .assert()
        .success() // still exit 0 — informational
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["total"], 1, "only the readable claim is tallied");
    assert_eq!(v["by_status"]["verified"], 1, "`good` still verified");
    let errors = v["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert!(errors[0]["file"]
        .as_str()
        .unwrap()
        .contains(".claims/log/corrupt/"));
    assert!(errors[0]["message"]
        .as_str()
        .unwrap()
        .contains("verdict log could not be read"));
}
