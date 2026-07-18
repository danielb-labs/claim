//! Integration tests for `claim amend <id>`: fixing a claim in place.
//!
//! The load-bearing assertion here is the amend guarantee — an amend cannot green a
//! claim whose new fact is false: the amended check is run against the current tree
//! and a non-`Held` result writes nothing. Amend writes no verdict (the changelog is
//! git history), so a successful amend's whole output is the one rewritten file to
//! commit. Both are exercised against a real temp git repo whose `requirements.txt`
//! is the tree the checks read.

mod common;

use common::TestRepo;
use predicates::prelude::*;

/// A cmd claim over `requirements.txt`. `run` is chosen by the test so the check
/// holds or drifts against the seeded tree deterministically.
fn claim_file(id: &str, run: &str, statement: &str) -> String {
    format!(
        "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"{run}\"\nhub:\n  max-age: 30d\n---\n{statement}\n"
    )
}

/// Seed a claim whose check currently *drifts*: the tree pins libfoo 5.0 but the
/// check asserts 4.2. This is the mid-drift state amend brings back to the truth.
fn drifted_repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    // The world moved: the tree now has 5.0, so a "pinned at 4.2" check drifts.
    repo.write("requirements.txt", "libfoo==5.0\n");
    repo.write_claim(
        "pin",
        &claim_file(
            "pin",
            "grep -q libfoo==4.2 requirements.txt",
            "We pin libfoo at 4.2.",
        ),
    );
    repo
}

#[test]
fn amend_to_the_new_truth_rewrites_the_file_and_writes_no_verdict() {
    let repo = drifted_repo();

    repo.claim()
        .args([
            "amend",
            "pin",
            "--statement",
            "We pin libfoo at 5.0.",
            "--run",
            "grep -q libfoo==5.0 requirements.txt",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Amended claim 'pin'"))
        .stdout(predicate::str::contains(
            "confirmed Held against the current tree",
        ));

    // The file is rewritten in place, and re-parseable (it went through the same
    // render-then-parse path as `add`). The new statement and check are on disk.
    let file = repo.read(".claims/pin.md");
    assert!(file.contains("We pin libfoo at 5.0."));
    assert!(file.contains("grep -q libfoo==5.0 requirements.txt"));
    assert!(
        !file.contains("We pin libfoo at 4.2."),
        "the old statement is replaced, not appended"
    );

    // Amend writes no verdict — the CLI stores nothing.
    assert!(!repo.exists(".claims/log"), "amend writes no verdict log");
}

#[test]
fn amend_is_refused_when_the_amended_check_does_not_hold() {
    // The amend guarantee: you cannot amend to a fact that is still false. Here the
    // tree is 5.0 but the amended check still asserts 4.2 — it drifts, so the amend
    // is refused and NOTHING is written.
    let repo = drifted_repo();

    repo.claim()
        .args([
            "amend",
            "pin",
            "--statement",
            "We pin libfoo at 4.2 (still wrong).",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Drifted"))
        .stderr(predicate::str::contains("Nothing was written"));

    // The file is untouched.
    assert!(repo
        .read(".claims/pin.md")
        .contains("We pin libfoo at 4.2."));
    assert!(!repo.read(".claims/pin.md").contains("still wrong"));
}

#[test]
fn amend_is_refused_when_the_amended_check_is_broken() {
    let repo = drifted_repo();
    let before = repo.read(".claims/pin.md");
    // A command that cannot run maps to Broken, never a pass — refused, nothing
    // written.
    repo.claim()
        .args(["amend", "pin", "--run", "this-binary-does-not-exist --nope"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Broken"))
        .stderr(predicate::str::contains("Nothing was written"));
    // The file is byte-for-byte unchanged (m4): a Broken refusal writes nothing.
    assert_eq!(repo.read(".claims/pin.md"), before);
}

#[test]
fn amend_changing_check_and_statement_together() {
    let repo = drifted_repo();
    let out = repo
        .claim()
        .args([
            "--json",
            "amend",
            "pin",
            "--statement",
            "We pin libfoo at 5.0.",
            "--run",
            "grep -q libfoo==5.0 requirements.txt",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["verdict"], "held");
    let changed: Vec<&str> = v["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert!(changed.contains(&"statement"));
    assert!(changed.contains(&"run"));
    // `root` is load-bearing for resolving the root-relative `to_commit` paths from a
    // subdirectory (m6).
    assert!(
        !v["root"].as_str().unwrap().is_empty(),
        "root is present and non-empty"
    );
    // Only the rewritten file is committed — no verdict is written.
    let to_commit = v["to_commit"].as_array().unwrap();
    assert_eq!(to_commit.len(), 1, "only the rewritten file to commit");
    assert_eq!(to_commit[0], ".claims/pin.md");
}

#[test]
fn amend_with_no_fields_is_a_no_op_error() {
    let repo = drifted_repo();
    let before = repo.read(".claims/pin.md");
    repo.claim()
        .args(["amend", "pin"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("changed nothing"));
    assert_eq!(repo.read(".claims/pin.md"), before, "no-op writes nothing");
}

#[test]
fn amend_with_only_unchanged_fields_is_a_no_op_error() {
    // Passing the current statement verbatim is not a change.
    let repo = drifted_repo();
    repo.claim()
        .args(["amend", "pin", "--statement", "We pin libfoo at 4.2."])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("changed nothing"));
}

#[test]
fn amend_no_op_json_error_carries_the_no_change_kind() {
    let repo = drifted_repo();
    let out = repo
        .claim()
        .args(["--json", "amend", "pin"])
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["status"], "error");
    assert_eq!(v["kind"], "no-change");
}

#[test]
fn amend_rejects_the_id_flag() {
    // The id is not amendable; clap has no --id for amend, so it is a usage error.
    let repo = drifted_repo();
    repo.claim()
        .args(["amend", "pin", "--id", "renamed"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unexpected argument '--id'"));
}

#[test]
fn amend_negate_requires_run() {
    // --negate is only meaningful with --run, so it cannot silently un-negate a
    // check on an amend that does not touch it.
    let repo = drifted_repo();
    repo.claim()
        .args(["amend", "pin", "--negate"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--run"));
}

#[test]
fn amend_of_an_unknown_id_errors() {
    let repo = drifted_repo();
    repo.claim()
        .args(["amend", "ghost", "--statement", "x"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no claim with id 'ghost'"));
}

#[test]
fn amend_refuses_a_multi_check_claim_rather_than_dropping_a_check() {
    // A claim with two checks cannot be faithfully re-rendered by the single-cmd
    // renderer, so amend refuses loudly instead of silently dropping the second.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    let two_checks = "---\nid: multi\nchecks:\n  - kind: cmd\n    run: \"true\"\n  - kind: cmd\n    run: \"true\"\nhub:\n  max-age: 30d\n---\nTwo checks.\n";
    repo.write_claim("multi", two_checks);

    repo.claim()
        .args(["amend", "multi", "--statement", "changed"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("has 2 checks"));
    // Untouched.
    assert!(repo.read(".claims/multi.md").contains("Two checks."));
}

#[test]
fn amend_refuses_a_skip_bearing_check_rather_than_dropping_the_skip() {
    // The `add`-shaped draft the renderer round-trips carries no skip, so amending a
    // skip-bearing check would silently drop the skip — a data loss amend must refuse
    // loudly, like the multi-check case above.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    let with_skip = "---\nid: parked\nchecks:\n  - kind: cmd\n    run: \"true\"\n    skip:\n      reason: no runner in CI\n      unless: \"true\"\nhub:\n  max-age: 30d\n---\nA parked claim.\n";
    repo.write_claim("parked", with_skip);

    repo.claim()
        .args(["amend", "parked", "--statement", "changed"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("skip"));
    // The skip survived untouched.
    assert!(repo
        .read(".claims/parked.md")
        .contains("reason: no runner in CI"));
}

#[test]
fn amend_only_max_age_keeps_the_check_and_statement() {
    // A pure max-age bump: the check still holds (tree already matches), only the
    // window changes, and the statement is untouched. max-age lives under `hub:`.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim(
        "pin",
        &claim_file(
            "pin",
            "grep -q libfoo==4.2 requirements.txt",
            "We pin libfoo at 4.2.",
        ),
    );

    let out = repo
        .claim()
        .args(["--json", "amend", "pin", "--max-age", "90d"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let changed: Vec<&str> = v["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert_eq!(changed, ["max-age"]);
    // The re-rendered file quotes single-scalar frontmatter fields (the shared
    // renderer's injection-hardening) and keeps max-age under hub.
    let file = repo.read(".claims/pin.md");
    assert!(file.contains("max-age: \"90d\""));
    assert!(file.contains("hub:"), "max-age lives under hub:");
    assert!(file.contains("We pin libfoo at 4.2."));
}

#[test]
fn amend_refuses_a_single_non_cmd_check() {
    // m5: a lone agent/human check is not the cmd shape amend re-renders, so it is
    // refused with its own message (distinct from the multi-check case), not silently
    // dropped or executed.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    let agent = "---\nid: world\nchecks:\n  - kind: agent\n    instruction: read the changelog\nhub:\n  max-age: 90d\n---\nlibfoo fixed the CJK bug.\n";
    repo.write_claim("world", agent);

    repo.claim()
        .args(["amend", "world", "--statement", "changed"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("non-cmd check"));
    // Untouched.
    assert!(repo
        .read(".claims/world.md")
        .contains("libfoo fixed the CJK bug."));
}

#[test]
fn amend_supports_replaces_the_set() {
    // m3 (replacement): passing --supports replaces the whole set — an existing
    // target is gone, the new one present.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    let with_support = "---\nid: pin\nchecks:\n  - kind: cmd\n    run: \"grep -q libfoo==4.2 requirements.txt\"\nsupports:\n  - requirements.txt#libfoo\n---\nWe pin libfoo at 4.2.\n";
    repo.write_claim("pin", with_support);

    let out = repo
        .claim()
        .args(["--json", "amend", "pin", "--supports", "docs/adr-7.md#pin"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let changed: Vec<&str> = v["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert_eq!(changed, ["supports"]);
    let file = repo.read(".claims/pin.md");
    assert!(file.contains("docs/adr-7.md#pin"), "new target present");
    assert!(
        !file.contains("requirements.txt#libfoo"),
        "old target replaced, not merged"
    );
}

#[test]
fn amend_without_supports_flag_preserves_existing_supports() {
    // m3 (preservation): an amend that does not pass --supports keeps the existing
    // targets — amend never silently drops edges it was not told to touch.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    let with_support = "---\nid: pin\nchecks:\n  - kind: cmd\n    run: \"grep -q libfoo==4.2 requirements.txt\"\nsupports:\n  - requirements.txt#libfoo\nhub:\n  max-age: 30d\n---\nWe pin libfoo at 4.2.\n";
    repo.write_claim("pin", with_support);

    repo.claim()
        .args(["amend", "pin", "--max-age", "90d"])
        .assert()
        .success();
    let file = repo.read(".claims/pin.md");
    assert!(
        file.contains("requirements.txt#libfoo"),
        "existing support preserved across an unrelated amend"
    );
    assert!(file.contains("max-age: \"90d\""));
}
