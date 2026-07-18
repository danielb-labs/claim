//! Integration tests for `claim add`, run against a real temp git store.
//!
//! The witnessed-red workflow is exercised through its scriptable form
//! (`--witness-cmd`/`--restore-cmd`), which is the mechanized, deterministic path;
//! the interactive prompts are not driven here (they need a TTY), but the flags
//! reach the same `witness`/`require_drift` logic.

mod common;

use common::TestRepo;
use predicates::prelude::*;

/// A grep that holds against the committed `requirements.txt`.
const HOLDS: &str = "grep -q 'libfoo==4.2' requirements.txt";
/// A command that makes the fact false by rewriting the pinned line.
const MAKE_RED: &str = "echo 'libfoo==5.0' > requirements.txt";
/// A command that restores the pinned line.
const MAKE_GREEN: &str = "echo 'libfoo==4.2' > requirements.txt";

/// A store-ready repo: init'd, one committed file the checks act on.
fn ready_repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo
}

// --- Happy path. ---

#[test]
fn add_writes_the_claim_file_and_birth_log() {
    let repo = ready_repo();
    repo.claim()
        .args([
            "add",
            "--id",
            "libfoo-pin",
            "--statement",
            "We pin libfoo at 4.2.",
            "--run",
            HOLDS,
            "--max-age",
            "120d",
            "--witness-cmd",
            MAKE_RED,
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("witnessed red"))
        .stdout(predicate::str::contains("Created claim 'libfoo-pin'"))
        .stdout(predicate::str::contains("git add"));

    // The claim file landed at the id-derived path and parses back.
    assert!(repo.exists(".claims/libfoo-pin.md"));
    let file = repo.read(".claims/libfoo-pin.md");
    assert!(file.contains("id: libfoo-pin"));
    assert!(file.contains("max-age: 120d"));
    assert!(file.contains("We pin libfoo at 4.2."));

    // Two birth entries: a witnessed Drifted, then the establishing Held, in order,
    // both carrying the resolved commit and actor.
    let entries = repo.log_entries("libfoo-pin");
    assert_eq!(entries.len(), 2, "a witnessed add writes two entries");
    assert_eq!(entries[0]["event"]["verdict"], "drifted");
    assert_eq!(entries[1]["event"]["verdict"], "held");
    for entry in &entries {
        assert_eq!(entry["actor"], "Test User <test@example.com>");
        let commit = entry["commit"].as_str().unwrap();
        assert!(!commit.is_empty(), "commit must be a resolved sha");
        assert_ne!(
            commit, "0000000000000000000000000000000000000000",
            "a committed repo resolves a real sha, not the unborn sentinel"
        );
    }
    assert!(
        entries[0]["event"]["evidence"]
            .as_str()
            .unwrap()
            .contains("witnessed-red"),
        "the drift entry records the witnessed-red evidence"
    );

    // The working tree was restored: the pin is back.
    assert_eq!(repo.read("requirements.txt"), "libfoo==4.2\n");
}

#[test]
fn add_json_shape_is_stable() {
    let repo = ready_repo();
    let output = repo
        .claim()
        .args([
            "--json",
            "add",
            "--id",
            "c",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
            "--witness-cmd",
            MAKE_RED,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let v: serde_json::Value = serde_json::from_slice(&output).expect("add --json is valid JSON");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["id"], "c");
    assert_eq!(v["file"], ".claims/c.md");
    assert_eq!(v["verdict"], "held");
    assert_eq!(v["witnessed_red"], true);
    assert_eq!(v["actor"], "Test User <test@example.com>");
    let to_commit = v["to_commit"].as_array().unwrap();
    assert_eq!(to_commit.len(), 3, "the file plus two log entries");
    assert_eq!(to_commit[0], ".claims/c.md");
}

#[test]
fn add_produces_a_born_verified_claim() {
    // The birth Held must be the latest conclusive verdict, so the claim reads as
    // fresh — not left Drifted by the witnessed red. Asserted through the log order
    // the status computation depends on: entries[] is filename-sorted, and the
    // filename leads with the (fixed-width, chronological) stamp, so entries[1] is
    // the later one and it must be the Held.
    let repo = ready_repo();
    repo.claim()
        .args([
            "add",
            "--id",
            "v",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
            "--witness-cmd",
            MAKE_RED,
        ])
        .assert()
        .success();

    let entries = repo.log_entries("v");
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0]["event"]["verdict"], "drifted",
        "the earlier entry (by fixed-width stamp) is the witnessed drift"
    );
    assert_eq!(
        entries[1]["event"]["verdict"], "held",
        "the later entry is the establishing Held, so the claim is born verified"
    );
    // And the parsed instants confirm strict ordering (a string compare would be
    // wrong: jiff drops trailing zeros, so `.204367Z` vs `.204367001Z` do not
    // compare lexically).
    let drift_at: claim_core::Timestamp = entries[0]["at"].as_str().unwrap().parse().unwrap();
    let held_at: claim_core::Timestamp = entries[1]["at"].as_str().unwrap().parse().unwrap();
    assert!(
        held_at > drift_at,
        "the establishing Held ({held_at}) must be strictly after the witnessed Drift ({drift_at})"
    );
}

#[test]
fn namespaced_id_nests_the_file() {
    let repo = ready_repo();
    repo.claim()
        .args([
            "add",
            "--id",
            "payments/libfoo-pin",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
            "--witness-cmd",
            MAKE_RED,
        ])
        .assert()
        .success();
    assert!(
        repo.exists(".claims/payments/libfoo-pin.md"),
        "a namespaced id nests under .claims/"
    );
}

#[test]
fn negate_when_and_supports_render_and_round_trip() {
    // The whole authoring surface: a negate check on a cadence trigger with two
    // supports (a decision ref with a `#`, and a bare claim id). The negate sense
    // is exercised honestly — the check exits 1 (Held under negate) on the true
    // tree and exits 0 (Drifted under negate) when the forbidden pin appears — so
    // the witnessed-red proves the inverted check discriminates too.
    let repo = ready_repo();
    repo.claim()
        .args([
            "add",
            "--id",
            "payments/pin",
            "--statement",
            "libfoo 5.0 must be absent.",
            "--run",
            "grep -q 'libfoo==5.0' requirements.txt",
            "--negate",
            "--when",
            "every 30d",
            "--max-age",
            "90d",
            "--supports",
            "requirements.txt#libfoo",
            "--supports",
            "other-claim",
            "--witness-cmd",
            MAKE_RED,
        ])
        .assert()
        .success();

    let file = repo.read(".claims/payments/pin.md");
    assert!(file.contains("negate: true"), "negate is rendered: {file}");
    assert!(
        file.contains("when: every 30d"),
        "trigger is rendered: {file}"
    );
    // A decision ref with a `#` is quoted so YAML does not read it as a comment.
    assert!(
        file.contains("- \"requirements.txt#libfoo\""),
        "a decision ref is quoted: {file}"
    );
    assert!(
        file.contains("- other-claim"),
        "a bare id is rendered: {file}"
    );

    // The rendered file is valid: the store contains it and the log recorded the
    // witnessed drift under the inverted sense.
    let entries = repo.log_entries("payments/pin");
    assert_eq!(entries[0]["event"]["verdict"], "drifted");
    assert_eq!(entries[1]["event"]["verdict"], "held");
}

// --- Rejections. ---

#[test]
fn rejects_duplicate_id() {
    let repo = ready_repo();
    let args = [
        "add",
        "--id",
        "dup",
        "--statement",
        "S.",
        "--run",
        HOLDS,
        "--max-age",
        "30d",
        "--witness-cmd",
        MAKE_RED,
    ];
    repo.claim().args(args).assert().success();
    repo.claim()
        .args(args)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("already exists"));
}

#[test]
fn rejects_a_green_run_that_is_drifted() {
    // The fact is already false: the grep is for a pin that is not present.
    let repo = ready_repo();
    repo.claim()
        .args([
            "add",
            "--id",
            "x",
            "--statement",
            "S.",
            "--run",
            "grep -q 'libfoo==9.9' requirements.txt",
            "--max-age",
            "30d",
            "--witness-cmd",
            MAKE_RED,
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Drifted"))
        .stderr(predicate::str::contains("already false"));
    assert!(
        !repo.exists(".claims/x.md"),
        "nothing is written on rejection"
    );
}

#[test]
fn rejects_a_green_run_that_is_broken() {
    let repo = ready_repo();
    repo.claim()
        .args([
            "add",
            "--id",
            "x",
            "--statement",
            "S.",
            "--run",
            "this-binary-does-not-exist-anywhere",
            "--max-age",
            "30d",
            "--witness-cmd",
            MAKE_RED,
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Broken"));
    assert!(!repo.exists(".claims/x.md"));
}

#[test]
fn rejects_an_invalid_id() {
    let repo = ready_repo();
    repo.claim()
        .args([
            "add",
            "--id",
            "Bad_Id",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
            "--witness-cmd",
            MAKE_RED,
        ])
        .assert()
        .code(2)
        // The reason comes from claim-core's ClaimId validator, reused not
        // reimplemented.
        .stderr(predicate::str::contains("lowercase letters"));
}

#[test]
fn rejects_an_invalid_max_age() {
    let repo = ready_repo();
    repo.claim()
        .args([
            "add",
            "--id",
            "ok",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "banana",
            "--witness-cmd",
            MAKE_RED,
        ])
        .assert()
        .code(2)
        // The reason comes from claim-core's Days parser.
        .stderr(predicate::str::contains("day count"));
}

#[test]
fn rejects_when_no_store_exists() {
    // A repo with no `claim init`.
    let repo = TestRepo::new();
    repo.claim()
        .args([
            "add",
            "--id",
            "x",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
            "--witness-cmd",
            MAKE_RED,
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no claim store found"));
}

#[test]
fn rejects_a_missing_required_field_without_a_tty() {
    // No --statement, and no TTY under assert_cmd, so it must error naming the flag
    // rather than hang on a prompt.
    let repo = ready_repo();
    repo.claim()
        .args([
            "add",
            "--id",
            "x",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
            "--witness-cmd",
            MAKE_RED,
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--statement"));
}

// --- Witnessed-red: the heart of invariant #5. ---

#[test]
fn refuses_when_the_check_does_not_go_red() {
    // The default path REQUIRES an observed Drifted. A check that stays Held after
    // the perturbation (here: `true`, which ignores the tree) is decoration and is
    // refused — nothing is written.
    let repo = ready_repo();
    repo.claim()
        .args([
            "add",
            "--id",
            "decoration",
            "--statement",
            "S.",
            "--run",
            "true",
            "--max-age",
            "30d",
            "--witness-cmd",
            MAKE_RED,
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("still reports Held"));
    assert!(
        !repo.exists(".claims/decoration.md"),
        "a check that never goes red must not be recorded"
    );
    assert!(
        repo.path()
            .join(".claims/log/decoration")
            .read_dir()
            .is_err(),
        "no log entries on a refused witness"
    );
}

#[test]
fn witnessed_red_succeeds_when_the_check_actually_goes_red() {
    // The positive of the pair: the same MAKE_RED perturbation the decoration check
    // ignored does drive a real grep to Drifted, so the add succeeds and records the
    // witnessed red.
    let repo = ready_repo();
    repo.claim()
        .args([
            "add",
            "--id",
            "real",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
            "--witness-cmd",
            MAKE_RED,
        ])
        .assert()
        .success();
    let entries = repo.log_entries("real");
    assert_eq!(entries[0]["event"]["verdict"], "drifted");
}

#[test]
fn refuses_when_the_tree_is_not_restored_to_green() {
    // If restoration leaves the fact false, the confirm-green run is not Held and the
    // add is refused. A --restore-cmd that does not actually restore stages this.
    let repo = ready_repo();
    repo.claim()
        .args([
            "add",
            "--id",
            "norestore",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
            "--witness-cmd",
            MAKE_RED,
            "--restore-cmd",
            "true", // does nothing; pin stays broken
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("not restored"));
    assert!(!repo.exists(".claims/norestore.md"));
}

#[test]
fn unwitnessed_records_the_claim_marked_unverified() {
    // The escape hatch: no witnessed red, but the claim is recorded with a loud
    // note and a warning, never silently trusted.
    let repo = ready_repo();
    repo.claim()
        .args([
            "add",
            "--id",
            "uw",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
            "--unwitnessed",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("WARNING"))
        .stdout(predicate::str::contains("--unwitnessed"));

    // Exactly one entry (the establishing Held), and it is marked unwitnessed so a
    // later `list --unverified` can surface it.
    let entries = repo.log_entries("uw");
    assert_eq!(entries.len(), 1, "unwitnessed writes only the birth Held");
    assert_eq!(entries[0]["event"]["verdict"], "held");
    assert!(
        entries[0]["event"]["evidence"]
            .as_str()
            .unwrap()
            .contains("unwitnessed"),
        "the log itself records that the check was never witnessed failing"
    );
}

#[test]
fn unwitnessed_json_marks_witnessed_red_false() {
    let repo = ready_repo();
    let output = repo
        .claim()
        .args([
            "--json",
            "add",
            "--id",
            "uwj",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
            "--unwitnessed",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(v["witnessed_red"], false);
    assert_eq!(v["status"], "ok");
}

#[test]
fn json_add_without_a_witness_is_refused() {
    // --json implies a script with no TTY; witnessing needs either --witness-cmd or
    // --unwitnessed. Neither given, it must refuse rather than hang.
    let repo = ready_repo();
    repo.claim()
        .args([
            "--json",
            "add",
            "--id",
            "x",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("witness"));
}

// --- Git edge: unborn HEAD. ---

#[test]
fn add_on_an_unborn_head_uses_the_sentinel_commit() {
    // A brand-new repo with no commit yet. HEAD does not resolve, so the birth
    // verdict records the documented all-zero sentinel — and, critically, a
    // non-empty commit, so the entry is valid (claim-core rejects an empty commit).
    // The perturbation creates no untracked file to worry the git restore, but
    // there is no commit to `git checkout` from, so --restore-cmd supplies the undo.
    let repo = TestRepo::unborn();
    repo.claim().arg("init").assert().success();
    repo.claim()
        .args([
            "add",
            "--id",
            "fresh",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
            "--witness-cmd",
            MAKE_RED,
            "--restore-cmd",
            MAKE_GREEN,
        ])
        .assert()
        .success();

    let entries = repo.log_entries("fresh");
    for entry in &entries {
        assert_eq!(
            entry["commit"], "0000000000000000000000000000000000000000",
            "an unborn HEAD records the documented sentinel"
        );
        assert!(
            !entry["commit"].as_str().unwrap().is_empty(),
            "the sentinel keeps the entry's commit non-empty and valid"
        );
    }
}

#[test]
fn add_fails_with_no_git_identity() {
    // Determinism guard: with user.name/email unset in a repo isolated from ambient
    // config, provenance cannot resolve an actor and the add is refused before
    // anything is written.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.git(&["config", "--unset", "user.name"]);
    repo.git(&["config", "--unset", "user.email"]);
    repo.claim()
        // Isolate from any global identity so the local unset is authoritative.
        .env("HOME", repo.path())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args([
            "add",
            "--id",
            "x",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
            "--witness-cmd",
            MAKE_RED,
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("user.name").or(predicate::str::contains("user.email")));
    assert!(!repo.exists(".claims/x.md"));
}
