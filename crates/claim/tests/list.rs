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

/// Parse the `list --json` envelope and return its `claims` array. `list` emits a
/// self-describing object (`{status, now, claims, errors}`), matching `check`/`drift`.
fn list_claims(output: &[u8]) -> Vec<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_slice(output).expect("list --json is one object");
    assert_eq!(v["status"], "ok");
    assert!(
        v["now"].is_string(),
        "the envelope records the computed-at instant"
    );
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
        .claim_at(NOW)
        .args(full)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    list_claims(&output)
}

#[test]
fn statuses_are_computed_across_a_mixed_store() {
    let repo = seeded_store();
    let claims = run_list(&repo, &[]);
    let status_of = |id: &str| -> String {
        claims
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("row {id}"))["status"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(status_of("fresh"), "verified");
    assert_eq!(status_of("gone"), "drifted");
    assert_eq!(status_of("old"), "stale");
}

#[test]
fn status_filter_narrows_to_one_status() {
    let repo = seeded_store();
    let claims = run_list(&repo, &["--status", "drifted"]);
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0]["id"], "gone");
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
fn stale_shortcut_shows_only_stale_not_drifted() {
    let repo = seeded_store();
    // `--stale` is `Status::Stale` only (m8): `old` (stale), NOT `gone` (drifted)
    // and NOT `fresh` (verified). Drift has its own verb.
    let claims = run_list(&repo, &["--stale"]);
    let got = ids(&claims);
    assert_eq!(got, vec!["old"], "--stale is stale-only, not drifted");
}

#[test]
fn path_filter_matches_repo_relative_paths() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("payments/pin", &claim_file("payments/pin", "120d"));
    repo.write_claim("infra/db", &claim_file("infra/db", "120d"));

    // `--path payments` matches `.claims/payments/pin.md` — the user thinks in repo
    // paths, so the `.claims/` prefix is stripped before matching (m1).
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
        "---\nid: with-support\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: on-change\nmax-age: 120d\nsupports:\n  - requirements.txt#libfoo\n---\nSupports the pin.\n",
    );
    repo.write_claim("plain", &claim_file("plain", "120d"));

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
        "---\nid: libfoo-pin\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: on-change\nmax-age: 120d\n---\nWe pin the CJK-safe version.\n",
    );
    repo.write_claim("unrelated", &claim_file("unrelated", "120d"));

    // Match on statement text.
    let claims = run_list(&repo, &["CJK"]);
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0]["id"], "libfoo-pin");
}

#[test]
fn unverified_surfaces_claims_with_no_passing_verdict() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();

    // Never verified: a claim with no log at all.
    repo.write_claim("never", &claim_file("never", "120d"));

    // Only ever broken: still no pass on record, so still debt.
    repo.write_claim("broke", &claim_file("broke", "120d"));
    repo.write_verdict("broke", "2026-07-10T00:00:00Z", "broken");

    // Only ever drifted, with no initial Held: also no pass on record, so also debt.
    // (The seeded `gone` claim opens with a Held, so it does not cover this case.)
    repo.write_claim("driftonly", &claim_file("driftonly", "120d"));
    repo.write_verdict("driftonly", "2026-07-09T00:00:00Z", "drifted");
    repo.write_verdict("driftonly", "2026-07-10T00:00:00Z", "drifted");

    // A genuinely verified claim, which must NOT appear — a passing check verifies
    // the fact, full stop, whatever evidence it carries.
    repo.write_claim("solid", &claim_file("solid", "120d"));
    repo.write_verdict("solid", "2026-07-10T00:00:00Z", "held");

    let got = ids(&run_list(&repo, &["--unverified"]));
    assert!(got.contains(&"never".to_owned()), "never-verified is debt");
    assert!(
        got.contains(&"broke".to_owned()),
        "only-broken is debt (no pass on record)"
    );
    assert!(
        got.contains(&"driftonly".to_owned()),
        "only-drifted is debt (no pass on record)"
    );
    assert!(
        !got.contains(&"solid".to_owned()),
        "a passing hold is not debt"
    );
}

#[test]
fn filters_combine_with_and() {
    let repo = seeded_store();
    // `--status stale --path old` → only `old` (stale AND under that path).
    let claims = run_list(&repo, &["--status", "stale", "--path", "old.md"]);
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0]["id"], "old");

    // A contradictory combination (drifted AND that path) → nothing.
    let claims = run_list(&repo, &["--status", "drifted", "--path", "old.md"]);
    assert_eq!(claims.len(), 0);
}

#[test]
fn json_row_shape_is_stable() {
    let repo = seeded_store();
    let claims = run_list(&repo, &["--status", "verified"]);
    let row = &claims[0];
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

#[test]
fn a_malformed_claim_file_does_not_hide_the_good_ones() {
    // M1: one bad file must not brick the whole listing. The good claims still
    // list, the bad one is reported, and the command exits 2 (loud AND useful).
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("good", &claim_file("good", "120d"));
    repo.write_verdict("good", "2026-07-10T00:00:00Z", "held");
    // A file that opens with a fence but has malformed YAML: it declared itself a
    // claim, so it is a loud error (a fenceless doc would be skipped as a non-claim).
    repo.write_claim("bad", "---\nchecks: [unterminated\n---\nS.\n");

    let output = repo
        .claim_at(NOW)
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
