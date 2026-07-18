//! Integration tests for the `CLAIM_NOW` clock seam (debug-only, loud).
//!
//! These run against the *debug* binary (assert_cmd's default), where the seam is
//! compiled in. A release binary ignores `CLAIM_NOW` entirely — that path is not
//! directly testable here without a release build, but it is enforced by
//! `#[cfg(debug_assertions)]` in `clock.rs` and documented there.

mod common;

use common::TestRepo;
use predicates::prelude::*;

fn claim_file(id: &str) -> String {
    format!(
        "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: on-change\nmax-age: 30d\n---\nStatement for {id}.\n"
    )
}

#[test]
fn claim_now_pins_the_clock_and_warns_it_is_overridden() {
    // C2: the debug seam works — a claim held long ago reads `stale` when `now` is
    // pinned far in the future — AND honoring the override prints a loud warning to
    // stderr, so a debug run never silently uses a fake clock.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("old", &claim_file("old"));
    repo.write_verdict("old", "2026-01-01T00:00:00Z", "held");

    repo.claim_at("2027-01-01T00:00:00Z")
        .args(["--json", "list"])
        .assert()
        .success()
        // The pinned clock drove the status: a year-old held is stale.
        .stdout(predicate::str::contains("\"status\": \"stale\""))
        // And the override is announced on stderr, never silent.
        .stderr(predicate::str::contains("warning:"))
        .stderr(predicate::str::contains("overridden clock"))
        .stderr(predicate::str::contains("CLAIM_NOW"));
}

#[test]
fn a_malformed_claim_now_is_a_loud_error_not_a_silent_fallback() {
    // m7: `clock::now`'s only contract — a malformed override is loud, never a
    // silent fall-through to the wall clock (which would make a pinned test quietly
    // non-deterministic).
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("c", &claim_file("c"));

    repo.claim_at("not-a-timestamp")
        .args(["list"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("CLAIM_NOW"))
        .stderr(predicate::str::contains("not an RFC 3339 timestamp"));
}

#[test]
fn the_override_warning_does_not_contaminate_json_stdout() {
    // The warning goes to stderr, so `--json | jq` stays clean even with the seam
    // active.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("c", &claim_file("c"));

    let output = repo
        .claim_at("2027-01-01T00:00:00Z")
        .args(["--json", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_slice(&output).expect("stdout is clean JSON despite the stderr warning");
    assert_eq!(v["status"], "ok");
}
