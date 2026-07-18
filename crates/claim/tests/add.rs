//! Integration tests for `claim add`, run against a real temp git store.
//!
//! The default path runs the check once and writes on `Held`, touching nothing else
//! in the tree. The optional `--witness-cmd` path is exercised through its scripted
//! form, which perturbs an *isolated* throwaway worktree, never the caller's tree —
//! the property asserted most sharply below (a dirty working-tree file survives a
//! witnessed add untouched), because that is the data-loss regression this design
//! makes impossible.

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
fn add_writes_the_claim_file_and_establishing_log() {
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
        // The handoff anchors at the store root with `git -C`, so it works from a
        // subdirectory (M2).
        .stdout(predicate::str::contains("git -C "))
        .stdout(predicate::str::contains(" add "));

    // The claim file landed at the id-derived path and parses back.
    assert!(repo.exists(".claims/libfoo-pin.md"));
    let file = repo.read(".claims/libfoo-pin.md");
    assert!(file.contains("id: libfoo-pin"));
    assert!(file.contains("max-age: 120d"));
    assert!(file.contains("We pin libfoo at 4.2."));

    // Exactly one birth entry: the establishing Held. The default path witnesses no
    // red, so there is no separate drift entry.
    let entries = repo.log_entries("libfoo-pin");
    assert_eq!(
        entries.len(),
        1,
        "the default add writes one establishing entry"
    );
    assert_eq!(entries[0]["event"]["verdict"], "held");
    let entry = &entries[0];
    assert_eq!(entry["actor"], "Test User <test@example.com>");
    let commit = entry["commit"].as_str().unwrap();
    // A committed repo records the FULL 40-char sha (M3), config-independent — not
    // the abbreviated form, and not the unborn sentinel.
    assert_eq!(commit.len(), 40, "the recorded commit is the full sha");
    assert_ne!(
        commit, "0000000000000000000000000000000000000000",
        "a committed repo resolves a real sha, not the unborn sentinel"
    );
    assert!(
        commit.chars().all(|c| c.is_ascii_hexdigit()),
        "the recorded commit is a hex sha"
    );

    // The working tree is untouched by the default path: the pin is exactly as
    // committed, and no worktree litter was left behind.
    assert_eq!(repo.read("requirements.txt"), "libfoo==4.2\n");
}

#[test]
fn add_is_born_verified_with_a_single_hold() {
    // The birth Held is the latest conclusive verdict, so the claim reads as fresh
    // (verified), not drifted or stale. With no witnessed red there is only the one
    // entry, and it is the pass.
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
        ])
        .assert()
        .success();

    let entries = repo.log_entries("v");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["event"]["verdict"], "held");
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
    let to_commit = v["to_commit"].as_array().unwrap();
    assert_eq!(
        to_commit.len(),
        2,
        "the file plus one establishing log entry"
    );
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
fn negate_when_and_supports_render_and_round_trip() {
    // The whole authoring surface: a negate check on a cadence trigger with two
    // supports (a decision ref with a `#`, and a bare claim id). The negate sense is
    // exercised honestly — the check exits 1 (Held under negate) on the true tree.
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

    // The rendered file is valid and the establishing Held was recorded under the
    // inverted sense.
    let entries = repo.log_entries("payments/pin");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["event"]["verdict"], "held");
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
    // because two files sharing an id share one verdict log.
    let repo = ready_repo();
    // A file named `alias.md` that declares id `dup` (its canonical path would be
    // `dup.md`, so the path check would not catch it).
    repo.write_claim(
        "alias",
        "---\nid: dup\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: on-change\nmax-age: 30d\n---\nExisting claim under an alias filename.\n",
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
fn rejects_a_missing_required_field_without_a_tty() {
    // No --statement, and no TTY under assert_cmd, so it must error naming the flag
    // rather than hang on a prompt.
    let repo = ready_repo();
    repo.claim()
        .args(["add", "--id", "x", "--run", HOLDS, "--max-age", "30d"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--statement"));
}

// --- Optional witnessed-red: extra confidence, in an isolated worktree. ---

#[test]
fn witness_cmd_records_the_observed_red_as_evidence() {
    // `--witness-cmd` makes the fact false in a throwaway worktree; the check goes
    // Drifted there, and that observation is recorded on the establishing entry.
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

    // Still one entry (the establishing Held), now carrying the witnessed-red note.
    let entries = repo.log_entries("real");
    assert_eq!(
        entries.len(),
        1,
        "witnessing adds evidence to the establishing entry, not a second entry"
    );
    assert_eq!(entries[0]["event"]["verdict"], "held");
    assert!(
        entries[0]["event"]["evidence"]
            .as_str()
            .unwrap()
            .contains("witnessed-red"),
        "the establishing entry records the witnessed-red evidence"
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
    // Only the file and one log entry are ever written; the witness added no entry.
    assert_eq!(v["to_commit"].as_array().unwrap().len(), 2);
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
    assert!(
        repo.path()
            .join(".claims/log/decoration")
            .read_dir()
            .is_err(),
        "no log entries on a refused witness"
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
    std::fs::create_dir_all(outer.path().join(".claims/log")).unwrap();
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

// --- m1: non-TTY human-mode with a missing field refuses clearly. ---

#[test]
fn non_tty_human_mode_with_a_missing_field_refuses_clearly() {
    // m1 regression. Without a TTY and with a required field absent, prompting is
    // impossible; it must refuse pointing at the flag, not die with a confusing
    // "input ended". assert_cmd provides no TTY. (The witness flow is no longer
    // required, so a fully-specified add needs no TTY at all.)
    let repo = ready_repo();
    repo.claim()
        .args(["add", "--id", "x", "--run", HOLDS, "--max-age", "30d"])
        .write_stdin("") // non-TTY stdin
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--statement"));
}

// --- Git edge: unborn HEAD (default path). ---

#[test]
fn add_on_an_unborn_head_uses_the_sentinel_commit() {
    // A brand-new repo with no commit yet. HEAD does not resolve, so the birth
    // verdict records the documented all-zero sentinel — and, critically, a
    // non-empty commit, so the entry is valid (claim-core rejects an empty commit).
    // The default path needs no worktree and no commit, so it works on an unborn HEAD.
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
        ])
        .assert()
        .success();

    let entries = repo.log_entries("fresh");
    assert_eq!(entries.len(), 1);
    // The sentinel is the full 40-char null object name — non-empty (so the entry is
    // valid) and width-consistent with a real sha.
    assert_eq!(
        entries[0]["commit"], "0000000000000000000000000000000000000000",
        "an unborn HEAD records the documented 40-char sentinel"
    );
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
