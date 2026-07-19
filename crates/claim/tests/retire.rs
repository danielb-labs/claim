//! Integration tests for `claim retire <id> --note`: closing a claim on purpose.
//!
//! Retirement removes the claim's definition file from the working tree (a `git rm`
//! the user commits). There is no stored retirement event — the changelog is git
//! history — so the load-bearing property is that the file is *gone* afterward and
//! the output names exactly what to `git rm` and commit, with the note as the reason.
//! Every test drives the built binary against a throwaway git repo (see
//! [`common::TestRepo`]).

mod common;

use common::TestRepo;
use predicates::prelude::*;

/// A cmd claim whose check body is inert (`retire` never runs it).
fn claim_file(id: &str) -> String {
    format!("---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n---\nStatement for {id}.\n")
}

#[test]
fn retire_removes_the_claim_file_and_names_what_to_commit() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("pin", &claim_file("pin"));

    repo.claim()
        .args(["retire", "pin", "--note", "libfoo 5.0 shipped; re-reviewed"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Retired claim 'pin'"))
        .stdout(predicate::str::contains("libfoo 5.0 shipped; re-reviewed"))
        // The removal is a working-tree delete the user commits as a `git rm`
        // (invariant #4).
        .stdout(predicate::str::contains("Removed"))
        .stdout(predicate::str::contains("git -C"))
        .stdout(predicate::str::contains("rm .claims/pin.md"));

    // The claim ceases to exist: its file is gone from the working tree.
    assert!(
        !repo.exists(".claims/pin.md"),
        "the retired claim file is removed from the working tree"
    );
}

#[test]
fn retire_is_allowed_on_any_claim_regardless_of_its_check() {
    // Retirement runs no check, so it closes any claim on purpose. The file is gone
    // afterward whatever the fact's state was.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("pin", &claim_file("pin"));

    repo.claim()
        .args(["retire", "pin", "--note", "superseded"])
        .assert()
        .success();
    assert!(!repo.exists(".claims/pin.md"));
}

#[test]
fn retire_requires_a_note() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("pin", &claim_file("pin"));

    // clap rejects the missing required --note as a usage error (exit 2), and the
    // claim file is left in place.
    repo.claim()
        .args(["retire", "pin"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--note"));
    assert!(
        repo.exists(".claims/pin.md"),
        "nothing removed on a usage error"
    );
}

#[test]
fn retire_of_an_unknown_id_errors_and_removes_nothing() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("pin", &claim_file("pin"));

    repo.claim()
        .args(["retire", "does-not-exist", "--note", "x"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "no claim with id 'does-not-exist'",
        ));
    // The real claim is untouched by a phantom retire.
    assert!(repo.exists(".claims/pin.md"));
}

#[test]
fn retire_of_a_duplicate_id_reports_declared_more_than_once_and_removes_nothing() {
    // Two files declare id `dup`, so both are dropped as ambiguous. `retire` must say
    // "declared more than once", not a false "no such claim" — the same latent bug
    // `show` had, fixed once in the shared resolver. Neither file is removed.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim(
        "one",
        "---\nid: dup\nchecks:\n  - kind: cmd\n    run: \"true\"\n---\nFirst.\n",
    );
    repo.write_claim(
        "two",
        "---\nid: dup\nchecks:\n  - kind: cmd\n    run: \"true\"\n---\nSecond.\n",
    );

    repo.claim()
        .args(["retire", "dup", "--note", "x"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "claim 'dup' is declared more than once",
        ))
        .stderr(predicate::str::contains("no claim with id 'dup'").not());
    // A refused retire removes neither conflicting file.
    assert!(repo.exists(".claims/one.md"));
    assert!(repo.exists(".claims/two.md"));
}

#[test]
fn retire_json_shape_carries_the_essentials() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("pin", &claim_file("pin"));

    let out = repo
        .claim()
        .args(["--json", "retire", "pin", "--note", "closed for good"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("stdout is one JSON object");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["id"], "pin");
    assert_eq!(v["note"], "closed for good");
    // `file` is the removed claim file, relative to `root`. There is no stored event,
    // so no commit/actor/log fields.
    assert_eq!(v["file"], ".claims/pin.md");
    assert!(
        v.get("commit").is_none(),
        "retire records no verdict commit"
    );
    assert!(v.get("actor").is_none(), "retire records no actor");
    // `root` is load-bearing: an agent invoked from a subdirectory resolves the
    // root-relative `file` against it.
    assert!(
        !v["root"].as_str().unwrap().is_empty(),
        "root is present and non-empty"
    );
    assert!(!repo.exists(".claims/pin.md"), "the file was removed");
}

#[test]
fn retire_rejects_a_blank_note_before_removing_anything() {
    // clap requires --note present but accepts an empty/whitespace value; a
    // reasonless retirement defeats the invariant the note enforces, so it is
    // rejected loudly and the file is left in place.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("pin", &claim_file("pin"));

    repo.claim()
        .args(["retire", "pin", "--note", "   "])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("the retirement note is empty"));
    assert!(
        repo.exists(".claims/pin.md"),
        "nothing removed on a blank note"
    );

    // The JSON error carries the machine kind.
    let out = repo
        .claim()
        .args(["--json", "retire", "pin", "--note", ""])
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["kind"], "invalid-input");
    assert!(repo.exists(".claims/pin.md"));
}

#[test]
fn retire_unknown_id_json_error_carries_a_kind() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();

    let out = repo
        .claim()
        .args(["--json", "retire", "nope", "--note", "x"])
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let v: serde_json::Value =
        serde_json::from_slice(&out).expect("the error is a JSON object on stderr");
    assert_eq!(v["status"], "error");
    assert_eq!(v["kind"], "invalid-input");
}
