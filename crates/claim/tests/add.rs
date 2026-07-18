//! Integration tests for `claim add`, run against a real temp git store.
//!
//! `add` runs the check once and writes the claim file on `Held`, touching nothing
//! else in the tree and writing no verdict log — the CLI is a stateless verifier, so
//! the whole of a successful add is the one file to commit. The optional
//! `--witness-cmd` path is exercised through its scripted form, which perturbs an
//! *isolated* throwaway worktree, never the caller's tree — the property asserted
//! most sharply below (a dirty working-tree file survives a witnessed add untouched),
//! because that is the data-loss regression this design makes impossible.

mod common;

use common::TestRepo;
use predicates::prelude::*;

/// A grep that holds against the committed `requirements.txt`.
const HOLDS: &str = "grep -q 'libfoo==4.2' requirements.txt";
/// A command that makes the fact false by rewriting the pinned line.
const MAKE_RED: &str = "echo 'libfoo==5.0' > requirements.txt";

/// A store-ready repo: init'd, one committed file the checks act on.
fn ready_repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo
}

// --- Happy path: the default is a single passing check, no tree perturbation. ---

#[test]
fn add_writes_the_claim_file_and_no_verdict_log() {
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
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Created claim 'libfoo-pin'"))
        .stdout(predicate::str::contains(
            "The check held against the current tree at commit",
        ))
        // The handoff anchors at the store root with `git -C`, so it works from a
        // subdirectory (M2).
        .stdout(predicate::str::contains("git -C "))
        .stdout(predicate::str::contains(" add "));

    // The claim file landed at the id-derived path and parses back. The single-scalar
    // frontmatter fields are rendered quoted (the injection-hardening: a newline in a
    // scalar is refused, and the value is quoted so it cannot confuse the scanner).
    // `--max-age` is written under the optional `hub:` subfield.
    assert!(repo.exists(".claims/libfoo-pin.md"));
    let file = repo.read(".claims/libfoo-pin.md");
    assert!(file.contains("id: \"libfoo-pin\""));
    assert!(file.contains("max-age: \"120d\""));
    assert!(file.contains("hub:"), "max-age lives under hub:");
    assert!(file.contains("We pin libfoo at 4.2."));

    // The CLI stores nothing: no verdict log tree is ever created.
    assert!(!repo.exists(".claims/log"), "add writes no verdict log");

    // The working tree is untouched by the default path: the pin is exactly as
    // committed, and no worktree litter was left behind.
    assert_eq!(repo.read("requirements.txt"), "libfoo==4.2\n");
}

#[test]
fn add_without_max_age_omits_the_hub_block() {
    // `--max-age` is optional (a hub hint). Omitted, the claim carries no `hub:` block
    // at all, and the add still succeeds against a holding check.
    let repo = ready_repo();
    repo.claim()
        .args([
            "add",
            "--id",
            "no-window",
            "--statement",
            "S.",
            "--run",
            HOLDS,
        ])
        .assert()
        .success();

    let file = repo.read(".claims/no-window.md");
    assert!(
        !file.contains("hub:"),
        "no hub block without --max-age: {file}"
    );
    assert!(
        !file.contains("max-age"),
        "no max-age without the flag: {file}"
    );
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
    // The default path witnesses no red, so the confidence flag is false — a fully
    // verified claim, not a penalized one.
    assert_eq!(v["witnessed_red"], false);
    assert_eq!(v["actor"], "Test User <test@example.com>");
    // The root is present (M2) so a subdir caller can resolve the root-relative
    // `file`/`to_commit`. It is an absolute path to a directory that contains the
    // written claim file (compared via canonical paths, since macOS resolves
    // /var → /private/var and the two forms would otherwise differ).
    let root = std::path::Path::new(v["root"].as_str().expect("root is a string"));
    assert!(root.is_absolute(), "root is absolute: {}", root.display());
    assert!(
        root.join(v["file"].as_str().unwrap()).exists(),
        "root + file resolves to the written claim"
    );
    assert_eq!(
        std::fs::canonicalize(root).unwrap(),
        std::fs::canonicalize(repo.path()).unwrap(),
        "root is the repo root"
    );
    // The commit is the full sha (M3), not the abbreviated form.
    assert_eq!(v["commit"].as_str().unwrap().len(), 40);
    // Only the claim file is written — no verdict — so `to_commit` is the file alone.
    let to_commit = v["to_commit"].as_array().unwrap();
    assert_eq!(to_commit.len(), 1, "only the claim file to commit, no log");
    assert_eq!(to_commit[0], ".claims/c.md");
}

#[test]
fn add_succeeds_in_a_dirty_tree_without_witness() {
    // The default path never touches or inspects the working tree beyond writing the
    // claim, so a dirty tree is fine and the pre-existing edit survives.
    let repo = ready_repo();
    repo.write("requirements.txt", "libfoo==4.2\nDIRTY EDIT\n");

    repo.claim()
        .args([
            "add",
            "--id",
            "d",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
        ])
        .assert()
        .success();

    assert!(repo.exists(".claims/d.md"));
    assert_eq!(
        repo.read("requirements.txt"),
        "libfoo==4.2\nDIRTY EDIT\n",
        "the default path leaves the dirty tree exactly as it was"
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
        ])
        .assert()
        .success();
    assert!(
        repo.exists(".claims/payments/libfoo-pin.md"),
        "a namespaced id nests under .claims/"
    );
}

#[test]
fn negate_and_supports_render_and_round_trip() {
    // The whole authoring surface: a negate check with two supports (a decision ref
    // with a `#`, and a bare claim id). The negate sense is exercised honestly — the
    // check exits 1 (Held under negate) on the true tree.
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
            "--max-age",
            "90d",
            "--supports",
            "requirements.txt#libfoo",
            "--supports",
            "other-claim",
        ])
        .assert()
        .success();

    let file = repo.read(".claims/payments/pin.md");
    assert!(file.contains("negate: true"), "negate is rendered: {file}");
    // A decision ref with a `#` is quoted so YAML does not read it as a comment.
    assert!(
        file.contains("- \"requirements.txt#libfoo\""),
        "a decision ref is quoted: {file}"
    );
    assert!(
        file.contains("- other-claim"),
        "a bare id is rendered: {file}"
    );
    assert!(
        file.contains("max-age: \"90d\""),
        "max-age under hub: {file}"
    );
    // No verdict log is written.
    assert!(!repo.exists(".claims/log"), "no verdict log");
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
    ];
    repo.claim().args(args).assert().success();
    repo.claim()
        .args(args)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("already exists"));
}

#[test]
fn rejects_an_id_already_declared_in_a_differently_named_file() {
    // C1: the canonical-path check misses a claim declaring the same id from a
    // differently named file. `add` must scan every claim's id, not just the path,
    // because two files sharing an id are an ambiguous store.
    let repo = ready_repo();
    // A file named `alias.md` that declares id `dup` (its canonical path would be
    // `dup.md`, so the path check would not catch it).
    repo.write_claim(
        "alias",
        "---\nid: dup\nchecks:\n  - kind: cmd\n    run: \"true\"\n---\nExisting claim under an alias filename.\n",
    );

    repo.claim()
        .args([
            "add",
            "--id",
            "dup",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("already declared in"))
        .stderr(predicate::str::contains("alias.md"));
    // The canonical file was never written.
    assert!(!repo.exists(".claims/dup.md"));
}

#[test]
fn rejects_a_check_that_is_drifted() {
    // The fact is already false: the grep is for a pin that is not present. Recording
    // an already-false fact is refused — that guarantee stays.
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
fn rejects_a_check_that_is_broken() {
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
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Broken"));
    assert!(!repo.exists(".claims/x.md"));
}

#[test]
fn a_broken_refusal_prints_the_check_diagnostic() {
    // Regression: on a refused establish `add` must show the check's evidence in human
    // mode, "so the author sees why" — a broken command's diagnostic (the shell's
    // "not found") is exactly the actionable line a refusal must not swallow. The
    // refactor once dropped it by moving the evidence print to the success path only.
    let repo = ready_repo();
    repo.claim()
        .args([
            "add",
            "--id",
            "x",
            "--statement",
            "S.",
            "--run",
            "definitely-not-a-real-binary-xyzzy",
            "--max-age",
            "30d",
        ])
        .assert()
        .code(2)
        // The narration labels the refused verdict and the evidence carries the
        // shell's own diagnostic, so the author can act on it.
        .stderr(predicate::str::contains("[check] Broken"))
        .stderr(predicate::str::contains(
            "definitely-not-a-real-binary-xyzzy",
        ))
        .stderr(predicate::str::contains("not found"));
    assert!(!repo.exists(".claims/x.md"));
}

#[test]
fn a_drifted_refusal_prints_the_check_evidence() {
    // The same contract on a Drifted refusal: the check's output (here the grep is
    // silent, but the verdict label is still narrated) must be surfaced before the
    // error, not swallowed.
    let repo = ready_repo();
    // A check whose output is visible on drift: grep a line that is not present but
    // echo a diagnostic to stderr first, so there is evidence to show.
    repo.claim()
        .args([
            "add",
            "--id",
            "x",
            "--statement",
            "S.",
            "--run",
            "echo 'pin is 4.2 not 9.9' >&2; grep -q 'libfoo==9.9' requirements.txt",
            "--max-age",
            "30d",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("[check] Drifted"))
        .stderr(predicate::str::contains("pin is 4.2 not 9.9"));
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
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no claim store found"));
}

#[test]
fn rejects_a_missing_required_field_by_default() {
    // No --statement and no --interactive, so add is headless by default: it must
    // error naming the flag, never prompt (which could hang an agent under a PTY).
    let repo = ready_repo();
    repo.claim()
        .args(["add", "--id", "x", "--run", HOLDS, "--max-age", "30d"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--statement"))
        // The error's actionability for a human is load-bearing: it must name the
        // `--interactive` escape hatch, not just the missing flag.
        .stderr(predicate::str::contains("--interactive"));
}

// --- Supports: an unresolvable target warns at author time but does not fail. ---

#[test]
fn add_warns_on_an_unresolvable_supports_but_still_creates_the_claim() {
    // A GitHub-slug anchor (`#approved-dependencies`) against a file whose heading
    // reads "Approved dependencies" does not resolve — `#anchor` is a literal text
    // scan, not a slug. The author must see a warning now, not a surprise UNRESOLVED
    // at `check` time. It is a warning, not a hard failure: a forward reference is
    // legitimate, so the claim is still created (exit 0).
    let repo = ready_repo();
    repo.write(
        "DECISIONS.md",
        "# Approved dependencies\n\nserde is fine.\n",
    );
    repo.git(&["add", "DECISIONS.md"]);
    repo.git(&["commit", "-q", "-m", "add decisions"]);

    repo.claim()
        .args([
            "add",
            "--id",
            "dep-note",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
            "--supports",
            "DECISIONS.md#approved-dependencies",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("does not resolve"))
        .stderr(predicate::str::contains(
            "DECISIONS.md#approved-dependencies",
        ));

    // The claim was created despite the warning.
    assert!(repo.exists(".claims/dep-note.md"));
}

#[test]
fn add_does_not_warn_when_every_supports_resolves() {
    // The mirror of the above: an anchor whose literal words appear in the file
    // resolves, so no warning is printed. Guards against a warning that fires on
    // valid input (which would train authors to ignore it).
    let repo = ready_repo();
    repo.write(
        "DECISIONS.md",
        "# Approved dependencies\n\nserde is fine.\n",
    );
    repo.git(&["add", "DECISIONS.md"]);
    repo.git(&["commit", "-q", "-m", "add decisions"]);

    repo.claim()
        .args([
            "add",
            "--id",
            "dep-note",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
            // The literal words are present, so the anchor resolves.
            "--supports",
            "DECISIONS.md#Approved dependencies",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("does not resolve").not());

    assert!(repo.exists(".claims/dep-note.md"));
}

#[test]
fn supports_warning_in_json_mode_goes_to_stderr_and_leaves_stdout_clean() {
    // `warn_unresolved_supports` claims a `--json` caller still sees the warning
    // without its stdout being contaminated. Prove it for a tool-emitted warning:
    // in `--json` mode the unresolvable-support warning is on stderr, while stdout
    // remains exactly the single parseable JSON object a consumer reads.
    let repo = ready_repo();
    repo.write(
        "DECISIONS.md",
        "# Approved dependencies\n\nserde is fine.\n",
    );
    repo.git(&["add", "DECISIONS.md"]);
    repo.git(&["commit", "-q", "-m", "add decisions"]);

    let output = repo
        .claim()
        .args([
            "--json",
            "add",
            "--id",
            "dep-note",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
            "--supports",
            "DECISIONS.md#approved-dependencies",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("does not resolve"))
        .get_output()
        .clone();

    // stdout is one clean JSON object, uncontaminated by the warning.
    let v: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is a single valid JSON object");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["id"], "dep-note");
    // The warning is not on stdout — it belongs to stderr alone.
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.contains("does not resolve"),
        "the warning must not leak onto stdout: {stdout}"
    );
}

// --- Optional witnessed-red: extra confidence, in an isolated worktree. ---

#[test]
fn witness_cmd_narrates_the_observed_red_and_writes_only_the_file() {
    // `--witness-cmd` makes the fact false in a throwaway worktree; the check goes
    // Drifted there, and that observation is narrated for the author's confidence. No
    // verdict is written — the whole of the add is still the one claim file.
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
        .success()
        .stderr(predicate::str::contains("witnessed red"))
        .stdout(predicate::str::contains(
            "witnessed failing in an isolated worktree",
        ));

    assert!(repo.exists(".claims/real.md"));
    assert!(
        !repo.exists(".claims/log"),
        "witnessing writes no verdict log"
    );
}

#[test]
fn witness_cmd_json_marks_witnessed_red_true() {
    let repo = ready_repo();
    let output = repo
        .claim()
        .args([
            "--json",
            "add",
            "--id",
            "w",
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
    let v: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(v["witnessed_red"], true);
    assert_eq!(v["status"], "ok");
    // Only the claim file is ever written; the witness added no entry and no log.
    assert_eq!(v["to_commit"].as_array().unwrap().len(), 1);
    assert_eq!(v["to_commit"][0], ".claims/w.md");
}

#[test]
fn witness_cmd_never_touches_the_working_tree() {
    // THE data-loss regression that must now be impossible. A pre-existing
    // uncommitted edit to a tracked file — the exact thing the old mandatory restore
    // could destroy — MUST survive a witnessed add untouched, because the witness
    // perturbs only an isolated worktree. Also: the tree the check acts on is
    // unchanged, and no worktree litter is left in the repo.
    let repo = ready_repo();
    // A tracked file with committed content, then an uncommitted edit the user cares
    // about.
    repo.write("notes.txt", "committed content\n");
    repo.git(&["add", "notes.txt"]);
    repo.git(&["commit", "-q", "-m", "add notes"]);
    repo.write("notes.txt", "MY UNCOMMITTED EDIT\n");
    // A pre-existing untracked file too, which a `git clean`-style restore would eat.
    repo.write("scratch-untracked.txt", "keep me\n");

    repo.claim()
        .args([
            "add",
            "--id",
            "safe",
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

    // The uncommitted tracked edit is exactly as the user left it.
    assert_eq!(
        repo.read("notes.txt"),
        "MY UNCOMMITTED EDIT\n",
        "a witnessed add must not revert the user's uncommitted work"
    );
    // The untracked file survived.
    assert_eq!(
        repo.read("scratch-untracked.txt"),
        "keep me\n",
        "a witnessed add must not delete untracked files"
    );
    // The file the perturbation rewrote (in isolation) is back to the committed pin
    // in the real tree — the perturbation never reached here.
    assert_eq!(
        repo.read("requirements.txt"),
        "libfoo==4.2\n",
        "the witness perturbation was confined to the isolated worktree"
    );
    // No leftover worktree checkout in the repo.
    assert!(
        !repo.exists("requirements.txt.orig") && repo.exists(".claims/safe.md"),
        "the claim was written and no perturbation artifact leaked into the repo"
    );
}

#[test]
fn refuses_when_the_witnessed_check_does_not_go_red() {
    // `--witness-cmd` requires an observed Drifted. A check that stays Held after the
    // perturbation (here: `true`, which ignores the tree) cannot be confirmed to
    // discriminate, so the witnessed add is refused — nothing is written.
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
        "a check that never goes red under --witness-cmd must not be recorded"
    );
}

#[test]
fn witness_cmd_is_refused_on_an_unborn_head() {
    // The isolated worktree checks out a commit, which an unborn repo does not have.
    // `--witness-cmd` is refused with the fix; the default (no-witness) path would
    // still work here.
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
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unborn HEAD"))
        .stderr(predicate::str::contains("--witness-cmd"));
    assert!(
        !repo.exists(".claims/fresh.md"),
        "nothing written on refusal"
    );
}

#[test]
fn witness_cmd_does_not_run_when_the_establishing_check_is_doomed() {
    // Regression: `--witness-cmd` must run only AFTER the id is confirmed new and the
    // establishing check holds — it must NOT fire for an add that is already going to
    // be refused. Here the establishing check is already Drifted (the pin is not 9.9),
    // so the add fails fast; a side-effecting witness command must never execute.
    let repo = ready_repo();
    // A witness that records that it ran, at an absolute path OUTSIDE the isolated
    // worktree, so its execution would be visible in the real repo.
    let sentinel = repo.path().join("witness-ran-sentinel");
    let witness = format!(
        "touch {}; echo 'libfoo==5.0' > requirements.txt",
        sentinel.display()
    );

    repo.claim()
        .args([
            "add",
            "--id",
            "doomed",
            "--statement",
            "S.",
            "--run",
            "grep -q 'libfoo==9.9' requirements.txt", // already drifted
            "--max-age",
            "30d",
            "--witness-cmd",
            &witness,
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Drifted"));

    assert!(
        !sentinel.exists(),
        "the witness command must not run for an add whose establishing check is doomed"
    );
    assert!(
        !repo.exists(".claims/doomed.md"),
        "nothing is written for a doomed add"
    );
}

#[test]
fn witness_cmd_does_not_run_for_a_duplicate_id() {
    // The same fail-fast guarantee for a duplicate id: the witness (a side-effecting
    // command) must not run when the add is doomed by an id that already exists.
    let repo = ready_repo();
    let base = [
        "add",
        "--id",
        "dup",
        "--statement",
        "S.",
        "--run",
        HOLDS,
        "--max-age",
        "30d",
    ];
    repo.claim().args(base).assert().success();

    let sentinel = repo.path().join("dup-witness-sentinel");
    let witness = format!(
        "touch {}; echo 'libfoo==5.0' > requirements.txt",
        sentinel.display()
    );
    repo.claim()
        .args(base)
        .args(["--witness-cmd", &witness])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("already exists"));
    assert!(
        !sentinel.exists(),
        "the witness command must not run when the id already exists"
    );
}

// --- M1: `--json` stdout stays a single clean JSON object even if the witness
// prints. ---

#[test]
fn json_stdout_is_clean_even_when_the_witness_prints() {
    // M1 regression. The witness command runs via `sh -c` inheriting stdout; a
    // witness that echoes (most real ones do) would contaminate the JSON on stdout.
    // The ENTIRE stdout must parse as one JSON object.
    let repo = ready_repo();
    let output = repo
        .claim()
        .args([
            "--json",
            "add",
            "--id",
            "noisy",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
            "--witness-cmd",
            "echo NOISE_TO_STDOUT; echo 'libfoo==5.0' > requirements.txt",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = String::from_utf8(output).unwrap();
    assert!(
        !text.contains("NOISE_TO_STDOUT"),
        "witness stdout must not leak onto the tool's stdout: {text}"
    );
    let v: serde_json::Value =
        serde_json::from_str(&text).expect("the whole stdout must parse as one JSON object");
    assert_eq!(v["status"], "ok");
}

// --- M2: the git-add handoff works from a subdirectory. ---

#[test]
fn handoff_and_root_work_from_a_subdirectory() {
    // M2 regression. `to_commit` is root-relative; run from a subdir, a plain
    // `git add .claims/x.md` would fail with "did not match any files". The printed
    // handoff anchors with `git -C <root>`, and the JSON carries `root`, so an agent
    // in a subdir can resolve everything.
    let repo = ready_repo();
    std::fs::create_dir_all(repo.path().join("src/deep")).unwrap();

    let output = repo
        .claim()
        .current_dir(repo.path().join("src/deep"))
        .args([
            "--json",
            "add",
            "--id",
            "sub",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let v: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let root = std::path::Path::new(v["root"].as_str().unwrap());
    // root + file (both root-relative) resolves to the real file, from anywhere.
    assert!(root.join(v["file"].as_str().unwrap()).exists());
    for path in v["to_commit"].as_array().unwrap() {
        assert!(
            root.join(path.as_str().unwrap()).exists(),
            "each to_commit path resolves under root"
        );
    }
}

// --- m3: discovery stops at the git-repository boundary. ---

#[test]
fn discovery_does_not_adopt_a_store_across_the_git_boundary() {
    // m3 regression. A `.claims/` above the repo's own `.git` belongs to a different
    // repo (or a stray $HOME/.claims); adopting it would stamp provenance from the
    // wrong repo. The walk stops at the git boundary, so an inner repo with no store
    // of its own does not reach the outer store.
    let outer = TestRepo::new(); // has a .git and (after this) a .claims above inner
    std::fs::create_dir_all(outer.path().join(".claims")).unwrap();
    let inner = outer.path().join("inner");
    std::fs::create_dir_all(&inner).unwrap();
    // Make `inner` its own git repository, so its `.git` is the boundary.
    outer.git(&["-C", inner.to_str().unwrap(), "init", "-q"]);

    outer
        .claim()
        .current_dir(&inner)
        .args([
            "add",
            "--id",
            "x",
            "--statement",
            "S.",
            "--run",
            "true",
            "--max-age",
            "30d",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no claim store found"));
}

// --- m4: a corrupt HEAD is loud, not masked as unborn. ---

#[test]
fn corrupt_head_is_loud_not_masked_as_unborn() {
    // m4 regression. Previously any `rev-parse HEAD` failure inside a work tree
    // yielded the unborn sentinel, masking a corrupt HEAD. A genuinely broken HEAD
    // must fail loudly, never silently record the all-zero sentinel.
    let repo = ready_repo();
    // Garbage in .git/HEAD: not a valid symref or sha.
    std::fs::write(repo.path().join(".git/HEAD"), "this is not a ref\n").unwrap();

    repo.claim()
        .args([
            "add",
            "--id",
            "x",
            "--statement",
            "S.",
            "--run",
            "true",
            "--max-age",
            "30d",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("HEAD").or(predicate::str::contains("git repository")));
    assert!(!repo.exists(".claims/x.md"));
}

// --- m5: error JSON carries a stable machine `kind`. ---

#[test]
fn error_json_carries_a_stable_kind() {
    // m5 regression. Several failure modes, each with its stable kind, so item-7
    // agents branch on the machine value rather than English prose.
    let repo = ready_repo();

    // duplicate-id: add once, then again.
    let dup = [
        "--json",
        "add",
        "--id",
        "d",
        "--statement",
        "S.",
        "--run",
        HOLDS,
        "--max-age",
        "30d",
    ];
    repo.claim().args(dup).assert().success();
    assert_error_kind(&repo, &dup, "duplicate-id");

    // drifted-green: the fact is already false.
    assert_error_kind(
        &repo,
        &[
            "--json",
            "add",
            "--id",
            "dg",
            "--statement",
            "S.",
            "--run",
            "grep -q 'nope' requirements.txt",
            "--max-age",
            "30d",
        ],
        "drifted-green",
    );

    // not-witnessed: the check does not go red under --witness-cmd.
    assert_error_kind(
        &repo,
        &[
            "--json",
            "add",
            "--id",
            "nw",
            "--statement",
            "S.",
            "--run",
            "true",
            "--max-age",
            "30d",
            "--witness-cmd",
            MAKE_RED,
        ],
        "not-witnessed",
    );
}

/// Run a `--json add` expected to fail and assert its error object's `kind`.
fn assert_error_kind(repo: &TestRepo, args: &[&str], expected_kind: &str) {
    let output = repo
        .claim()
        .args(args)
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let v: serde_json::Value =
        serde_json::from_slice(&output).expect("json error object on stderr");
    assert_eq!(v["status"], "error");
    assert_eq!(v["kind"], expected_kind, "kind for args {args:?}");
}

// --- Default (headless) mode with a missing field refuses; `-i` opts into prompts. ---

#[test]
fn default_mode_with_a_missing_field_refuses_clearly() {
    // m1 regression. By default (no --interactive) a missing required field must
    // refuse pointing at the flag and must never read stdin — even when stdin has
    // content, as here — so it can never hang or die with a confusing "input ended".
    let repo = ready_repo();
    repo.claim()
        .args(["add", "--id", "x", "--run", HOLDS, "--max-age", "30d"])
        .write_stdin("ignored: default mode never reads stdin")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--statement"));
}

#[test]
fn interactive_prompts_for_missing_fields_and_reads_stdin() {
    // `-i` is the opt-in that turns a missing required field into a prompt instead of
    // an error. With everything but --statement supplied, `-i` prompts for the one
    // missing field and reads the answer from stdin — proving the prompt path keys on
    // the flag, not on a detected terminal (assert_cmd provides no TTY).
    let repo = ready_repo();
    repo.claim()
        .args([
            "add",
            "-i",
            "--id",
            "greeted",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
        ])
        .write_stdin("The service greets on boot.\n")
        .assert()
        .success();

    let file = repo.read(".claims/greeted.md");
    assert!(
        file.contains("The service greets on boot."),
        "the statement read from stdin under -i must be recorded: {file}"
    );
}

#[test]
fn interactive_reads_missing_fields_in_order_from_stdin() {
    // With no flags and `-i`, add prompts id, statement, run in that order, one stdin
    // line each (`--max-age` is optional and never prompted for). Distinguishable
    // values prove the mapping: a swapped order would feed a non-command into `--run`
    // (refused) or land the statement in the wrong field, so a clean success with each
    // value in its slot is the ordering proof — a silent line-to-field mismatch would
    // be a data-integrity bug.
    let repo = ready_repo();
    repo.claim()
        .args(["add", "-i"])
        .write_stdin(format!("ordered-id\nThe ordered statement.\n{HOLDS}\n"))
        .assert()
        .success();

    let file = repo.read(".claims/ordered-id.md");
    assert!(
        file.contains("id: \"ordered-id\""),
        "id from stdin line 1: {file}"
    );
    assert!(
        file.contains("The ordered statement."),
        "statement from stdin line 2: {file}"
    );
    // No max-age was supplied, so the claim carries no hub block.
    assert!(
        !file.contains("hub:"),
        "no hub block without --max-age: {file}"
    );
}

#[test]
fn interactive_with_eof_stdin_errors_clearly_instead_of_hanging() {
    // `-i` with fields missing and stdin at EOF: the first prompt reads zero bytes and
    // must bail with a clear "input ended" error and a non-zero exit — never hang, and
    // never silently accept an empty field. assert_cmd would hang here if the process
    // blocked, so reaching the assertion at all is part of the guarantee.
    let repo = ready_repo();
    repo.claim()
        .args(["add", "-i"])
        .write_stdin("")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("input ended"));
}

// --- Git edge: unborn HEAD (default path). ---

#[test]
fn add_on_an_unborn_head_uses_the_sentinel_commit() {
    // A brand-new repo with no commit yet. HEAD does not resolve, so the reported
    // provenance commit is the documented all-zero sentinel. The default path needs no
    // worktree and no commit, so it works on an unborn HEAD.
    let repo = TestRepo::unborn();
    repo.claim().arg("init").assert().success();
    let out = repo
        .claim()
        .args([
            "--json",
            "add",
            "--id",
            "fresh",
            "--statement",
            "S.",
            "--run",
            HOLDS,
            "--max-age",
            "30d",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    // The sentinel is the full 40-char null object name.
    assert_eq!(
        v["commit"], "0000000000000000000000000000000000000000",
        "an unborn HEAD reports the documented 40-char sentinel"
    );
    assert!(repo.exists(".claims/fresh.md"));
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
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("user.name").or(predicate::str::contains("user.email")));
    assert!(!repo.exists(".claims/x.md"));
}
