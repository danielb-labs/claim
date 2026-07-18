//! M2: every read verb reports a missing store with the same machine-readable
//! `kind` (`no-store`) an item-7 agent branches on. The mapping lives in
//! `store::discover`, so no verb re-derives it or falls back to the generic
//! `"other"` kind.

mod common;

use common::TestRepo;

/// Run `claim --json <verb> …` in a repo with no `.claims/` store and assert the
/// error object's `kind` is `no-store`.
fn assert_no_store_kind(args: &[&str]) {
    // A git repo but no `claim init`, so `discover` finds no store.
    let repo = TestRepo::new();
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let output = repo
        .claim()
        .args(full)
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();
    let v: serde_json::Value =
        serde_json::from_slice(&output).expect("json error object on stderr");
    assert_eq!(v["status"], "error");
    assert_eq!(v["kind"], "no-store", "kind for `claim {}`", args.join(" "));
}

#[test]
fn check_reports_no_store_kind() {
    assert_no_store_kind(&["check", "--all"]);
}

#[test]
fn list_reports_no_store_kind() {
    assert_no_store_kind(&["list"]);
}

#[test]
fn log_reports_no_store_kind() {
    assert_no_store_kind(&["log", "some-id"]);
}

#[test]
fn drift_reports_no_store_kind() {
    assert_no_store_kind(&["drift"]);
}
