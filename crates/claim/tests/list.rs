//! Integration tests for `claim list`: computed status and the filters.
//!
//! Status is computed from the verdict log at a pinned `now` (the `CLAIM_NOW`
//! seam), so a verified / drifted / stale mix is deterministic rather than racing
//! the wall clock.

mod common;

use common::TestRepo;
use predicates::prelude::*;

/// A cmd claim with the given id, trigger, and `max-age`. The check body is inert
/// here — `list` never runs it; only the log drives status.
fn claim_file(id: &str, max_age: &str) -> String {
    format!(
        "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: on-change\nmax-age: {max_age}\n---\nStatement for {id}.\n"
    )
}

/// A store seeded with three claims of distinct computed status at `NOW`:
/// - `fresh`: held recently, within max-age → verified.
/// - `gone`: latest verdict is drifted → drifted.
/// - `old`: held long ago, past max-age → stale.
const NOW: &str = "2026-07-17T00:00:00Z";

fn seeded_store() -> TestRepo {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();

    repo.write_claim("fresh", &claim_file("fresh", "120d"));
    repo.write_verdict("fresh", "2026-07-10T00:00:00Z", "held");

    repo.write_claim("gone", &claim_file("gone", "120d"));
    repo.write_verdict("gone", "2026-07-10T00:00:00Z", "held");
    repo.write_verdict("gone", "2026-07-15T00:00:00Z", "drifted");

    repo.write_claim("old", &claim_file("old", "30d"));
    repo.write_verdict("old", "2026-01-01T00:00:00Z", "held");

    repo
}

#[test]
fn statuses_are_computed_across_a_mixed_store() {
    let repo = seeded_store();
    let output = repo
        .claim_at(NOW)
        .args(["--json", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let by_id = |id: &str| -> String {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("row {id}"))["status"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(by_id("fresh"), "verified");
    assert_eq!(by_id("gone"), "drifted");
    assert_eq!(by_id("old"), "stale");
}

#[test]
fn status_filter_narrows_to_one_status() {
    let repo = seeded_store();
    let output = repo
        .claim_at(NOW)
        .args(["--json", "list", "--status", "drifted"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let arr = rows.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "gone");
}

#[test]
fn unknown_status_filter_errors() {
    let repo = seeded_store();
    repo.claim_at(NOW)
        .args(["list", "--status", "bogus"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unknown --status"));
}

#[test]
fn stale_shortcut_shows_overdue_claims() {
    let repo = seeded_store();
    // `--stale` = due = stale or drifted; so `gone` (drifted) and `old` (stale),
    // not `fresh`.
    let output = repo
        .claim_at(NOW)
        .args(["--json", "list", "--stale"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let ids: Vec<&str> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"gone"));
    assert!(ids.contains(&"old"));
    assert!(!ids.contains(&"fresh"));
}

#[test]
fn path_filter_matches_on_segment_boundaries() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("payments/pin", &claim_file("payments/pin", "120d"));
    repo.write_claim("infra/db", &claim_file("infra/db", "120d"));

    let output = repo
        .claim_at(NOW)
        .args(["--json", "list", "--path", ".claims/payments"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let arr = rows.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "payments/pin");
}

#[test]
fn supports_filter_matches_a_declared_target() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim(
        "with-support",
        "---\nid: with-support\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: on-change\nmax-age: 120d\nsupports:\n  - requirements.txt#libfoo\n---\nSupports the pin.\n",
    );
    repo.write_claim("plain", &claim_file("plain", "120d"));

    let output = repo
        .claim_at(NOW)
        .args(["--json", "list", "--supports", "requirements.txt#libfoo"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let arr = rows.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "with-support");
}

#[test]
fn text_term_searches_id_and_statement() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim(
        "libfoo-pin",
        "---\nid: libfoo-pin\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: on-change\nmax-age: 120d\n---\nWe pin the CJK-safe version.\n",
    );
    repo.write_claim("unrelated", &claim_file("unrelated", "120d"));

    // Match on statement text.
    let output = repo
        .claim_at(NOW)
        .args(["--json", "list", "CJK"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let arr = rows.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "libfoo-pin");
}

#[test]
fn unverified_surfaces_a_never_verified_and_an_unwitnessed_claim() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();

    // Never verified: a claim with no log at all.
    repo.write_claim("never", &claim_file("never", "120d"));

    // Unwitnessed: a Held carrying the debt marker.
    repo.write_claim("debt", &claim_file("debt", "120d"));
    let dir = repo.path().join(".claims/log/debt");
    std::fs::create_dir_all(&dir).unwrap();
    let entry = serde_json::json!({
        "at": "2026-07-10T00:00:00Z",
        "commit": "0".repeat(40),
        "actor": "Test User <test@example.com>",
        "event": {
            "type": "verification",
            "verdict": "held",
            "evidence": "unwitnessed: this claim was added with --unwitnessed",
        },
    });
    std::fs::write(
        dir.join("2026-07-10T00-00-00Z-0000.json"),
        serde_json::to_vec_pretty(&entry).unwrap(),
    )
    .unwrap();

    // A genuinely verified claim, which must NOT appear.
    repo.write_claim("solid", &claim_file("solid", "120d"));
    repo.write_verdict("solid", "2026-07-10T00:00:00Z", "held");

    let output = repo
        .claim_at(NOW)
        .args(["--json", "list", "--unverified"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let ids: Vec<&str> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&"never"),
        "a never-verified claim is unverified debt"
    );
    assert!(
        ids.contains(&"debt"),
        "an unwitnessed hold is unverified debt"
    );
    assert!(!ids.contains(&"solid"), "a witnessed hold is not debt");
}

#[test]
fn filters_combine_with_and() {
    let repo = seeded_store();
    // `--status stale --path .claims/old` → only `old` (stale AND under that path).
    let output = repo
        .claim_at(NOW)
        .args([
            "--json",
            "list",
            "--status",
            "stale",
            "--path",
            ".claims/old.md",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let arr = rows.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "old");

    // A contradictory combination (drifted AND that path) → nothing.
    let output = repo
        .claim_at(NOW)
        .args([
            "--json",
            "list",
            "--status",
            "drifted",
            "--path",
            ".claims/old.md",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 0);
}

#[test]
fn json_row_shape_is_stable() {
    let repo = seeded_store();
    let output = repo
        .claim_at(NOW)
        .args(["--json", "list", "--status", "verified"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let row = &rows.as_array().unwrap()[0];
    assert_eq!(row["id"], "fresh");
    assert_eq!(row["status"], "verified");
    assert!(row["file"].as_str().unwrap().ends_with("fresh.md"));
    assert_eq!(row["last_verified"], "2026-07-10T00:00:00Z");
    assert!(row["stale_in_days"].is_i64());
    assert_eq!(row["supports"], 0);
    assert_eq!(row["due"], false);
}

#[test]
fn human_output_is_an_aligned_table() {
    let repo = seeded_store();
    repo.claim_at(NOW)
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("ID"))
        .stdout(predicate::str::contains("STATUS"))
        .stdout(predicate::str::contains("LAST-VERIFIED"))
        .stdout(predicate::str::contains("verified"))
        .stdout(predicate::str::contains("drifted"))
        .stdout(predicate::str::contains("stale"));
}
