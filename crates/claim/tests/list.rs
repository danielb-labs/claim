//! Integration tests for `claim list`: the plain inventory and its filters.
//!
//! `list` runs nothing and computes no status — the CLI stores no verdicts, so
//! there is no history to derive a status from. It reports what the store contains:
//! id, statement, file, supports count. The filters (path, supports, text) narrow
//! the corpus with AND semantics.

mod common;

use common::TestRepo;
use predicates::prelude::*;

/// A cmd claim with the given id. The check body is inert here — `list` never runs
/// it.
fn claim_file(id: &str) -> String {
    format!("---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n---\nStatement for {id}.\n")
}

fn seeded_store() -> TestRepo {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("fresh", &claim_file("fresh"));
    repo.write_claim("gone", &claim_file("gone"));
    repo.write_claim("old", &claim_file("old"));
    repo
}

/// Parse the `list --json` envelope and return its `claims` array. `list` emits a
/// self-describing object (`{status, exit, claims, errors}`), matching `check`/`drift`.
fn list_claims(output: &[u8]) -> Vec<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_slice(output).expect("list --json is one object");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["exit"], 0, "a clean list exits 0");
    v["claims"].as_array().expect("claims is an array").clone()
}

/// The ids in a `list --json` result, for set assertions.
fn ids(claims: &[serde_json::Value]) -> Vec<String> {
    claims
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

fn run_list(repo: &TestRepo, args: &[&str]) -> Vec<serde_json::Value> {
    let mut full = vec!["--json", "list"];
    full.extend_from_slice(args);
    let output = repo
        .claim()
        .args(full)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    list_claims(&output)
}

#[test]
fn lists_every_claim_in_the_store() {
    let repo = seeded_store();
    let claims = run_list(&repo, &[]);
    let mut got = ids(&claims);
    got.sort();
    assert_eq!(got, vec!["fresh", "gone", "old"]);
}

#[test]
fn path_filter_matches_repo_relative_paths() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("payments/pin", &claim_file("payments/pin"));
    repo.write_claim("infra/db", &claim_file("infra/db"));

    // `--path payments` matches `.claims/payments/pin.md` — the user thinks in repo
    // paths, so the `.claims/` prefix is stripped before matching.
    let claims = run_list(&repo, &["--path", "payments"]);
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0]["id"], "payments/pin");
}

#[test]
fn supports_filter_matches_a_declared_target() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim(
        "with-support",
        "---\nid: with-support\nchecks:\n  - kind: cmd\n    run: \"true\"\nsupports:\n  - requirements.txt#libfoo\n---\nSupports the pin.\n",
    );
    repo.write_claim("plain", &claim_file("plain"));

    let claims = run_list(&repo, &["--supports", "requirements.txt#libfoo"]);
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0]["id"], "with-support");
}

#[test]
fn text_term_searches_id_and_statement() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim(
        "libfoo-pin",
        "---\nid: libfoo-pin\nchecks:\n  - kind: cmd\n    run: \"true\"\n---\nWe pin the CJK-safe version.\n",
    );
    repo.write_claim("unrelated", &claim_file("unrelated"));

    // Match on statement text.
    let claims = run_list(&repo, &["CJK"]);
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0]["id"], "libfoo-pin");
}

#[test]
fn filters_combine_with_and() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim(
        "payments/pin",
        "---\nid: payments/pin\nchecks:\n  - kind: cmd\n    run: \"true\"\nsupports:\n  - requirements.txt#libfoo\n---\nWe pin libfoo.\n",
    );
    repo.write_claim("payments/other", &claim_file("payments/other"));

    // `--path payments --supports requirements.txt#libfoo` → only the pin.
    let claims = run_list(
        &repo,
        &[
            "--path",
            "payments",
            "--supports",
            "requirements.txt#libfoo",
        ],
    );
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0]["id"], "payments/pin");

    // A contradictory combination → nothing.
    let claims = run_list(
        &repo,
        &["--path", "infra", "--supports", "requirements.txt#libfoo"],
    );
    assert_eq!(claims.len(), 0);
}

#[test]
fn json_row_shape_is_stable() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim(
        "pin",
        "---\nid: pin\nchecks:\n  - kind: cmd\n    run: \"true\"\nsupports:\n  - a\n  - b\n---\nWe pin it.\n",
    );
    let claims = run_list(&repo, &[]);
    let row = &claims[0];
    assert_eq!(row["id"], "pin");
    assert_eq!(row["statement"], "We pin it.");
    assert!(row["file"].as_str().unwrap().ends_with("pin.md"));
    assert_eq!(row["supports"], 2);
    // No status is reported: the CLI stores no verdicts.
    assert!(row.get("status").is_none(), "list reports no status");
}

#[test]
fn human_output_is_an_aligned_table() {
    let repo = seeded_store();
    repo.claim()
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("ID"))
        .stdout(predicate::str::contains("SUPPORTS"))
        .stdout(predicate::str::contains("FILE"))
        .stdout(predicate::str::contains("fresh"))
        .stdout(predicate::str::contains("gone"))
        .stdout(predicate::str::contains("old"));
}

#[test]
fn a_malformed_claim_file_does_not_hide_the_good_ones() {
    // One bad file must not brick the whole listing. The good claims still list, the
    // bad one is reported, and the command exits 2 (loud AND useful).
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("good", &claim_file("good"));
    // A file that opens with a fence but has malformed YAML: it declared itself a
    // claim, so it is a loud error (a fenceless doc would be skipped as a non-claim).
    repo.write_claim("bad", "---\nchecks: [unterminated\n---\nS.\n");

    let output = repo
        .claim()
        .args(["--json", "list"])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(v["exit"], 2, "the envelope's exit matches the process code");
    // The good claim still listed.
    let got = ids(v["claims"].as_array().unwrap());
    assert!(
        got.contains(&"good".to_owned()),
        "the good claim still lists"
    );
    // The bad file is reported, naming it.
    let errors = v["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert!(errors[0]["file"].as_str().unwrap().ends_with("bad.md"));
}
