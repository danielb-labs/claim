//! Snapshot tests for the verbs' human output (CLAUDE.md's insta obligation), so a
//! change to a table or a report is a deliberate, reviewable diff.
//!
//! Every fixture is built from fixed bytes, and the store paths in the output are
//! store-relative (`.claims/x.md`) not temp paths, so the snapshots are stable across
//! runs and machines. The checks are inert (`true`, or a `grep` against a committed
//! `requirements.txt`), so no wall-clock durations leak into the output.

mod common;

use common::TestRepo;

/// A cmd claim whose check is `true` (always holds). The check body is inert for
/// `list`; `check`/`drift` run it and it holds.
fn claim_file(id: &str, supports: &str) -> String {
    let supports_block = if supports.is_empty() {
        String::new()
    } else {
        format!("supports:\n  - {supports}\n")
    };
    format!(
        "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n{supports_block}---\nStatement for {id}.\n"
    )
}

/// A store with three claims: two plain, one with a supports edge.
fn simple_store() -> TestRepo {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("a-fact", &claim_file("a-fact", ""));
    repo.write_claim("b-fact", &claim_file("b-fact", "requirements.txt#libfoo"));
    repo.write_claim("c-fact", &claim_file("c-fact", ""));
    repo
}

/// The stdout of a `claim` invocation as a UTF-8 string, for snapshotting.
fn stdout(repo: &TestRepo, args: &[&str], code: i32) -> String {
    let out = repo
        .claim()
        .args(args)
        .assert()
        .code(code)
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).expect("stdout is UTF-8")
}

#[test]
fn list_human_table_snapshot() {
    let repo = simple_store();
    insta::assert_snapshot!(stdout(&repo, &["list"], 0));
}

#[test]
fn show_human_snapshot() {
    // A single rich claim: a negated cmd check with a skip (reason + both guards, a
    // fixed `until` date, not wall-clock), a second agent check, two supports, and
    // both hub hints — so the snapshot exercises the whole layout deterministically.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim(
        "payments/libfoo-pin",
        "---\nid: payments/libfoo-pin\nchecks:\n  - kind: cmd\n    run: \"grep -q 'libfoo==4.2' requirements.txt\"\n    negate: true\n    skip:\n      reason: windows CI has no grep\n      unless: \"test -x /usr/bin/grep\"\n      until: 2027-01-01\n  - kind: agent\n    instruction: Check the changelog since 5.0 for a CJK fix.\nsupports:\n  - requirements.txt#libfoo\n  - other-claim\nhub:\n  recheck: 30d\n  max-age: 120d\n---\nWe pin libfoo at 4.2.\n",
    );
    insta::assert_snapshot!(stdout(&repo, &["show", "payments/libfoo-pin"], 0));
}

#[test]
fn check_human_snapshot() {
    // A store whose checks all hold, so the run is deterministic (a `true` cmd is
    // always exit 0) and no wall-clock durations leak into the output.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("a", &claim_file("a", ""));
    repo.write_claim("b", &claim_file("b", "requirements.txt#libfoo"));
    insta::assert_snapshot!(stdout(&repo, &["check"], 0));
}

#[test]
fn check_all_skipped_human_snapshot() {
    // A claim whose only check is skipped: the run verified nothing, so the summary must
    // say "no checks ran (all skipped)" and never read as "all held". Captured as a
    // snapshot so the honest wording is a deliberate, reviewable diff.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim(
        "parked",
        "---\nid: parked\nchecks:\n  - kind: cmd\n    run: \"false\"\n    skip:\n      reason: no runner in this environment\n---\nWould drift, but is skipped.\n",
    );
    insta::assert_snapshot!(stdout(&repo, &["check"], 0));
}

#[test]
fn drift_clean_human_snapshot() {
    // Every check holds, so the drift queue is clean (exit 0).
    let repo = simple_store();
    insta::assert_snapshot!(stdout(&repo, &["drift"], 0));
}

#[test]
fn drift_with_a_drifted_claim_snapshot() {
    // One claim whose check reports drifted (a grep that fails), so it appears in the
    // queue (exit 1).
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("held", &claim_file("held", ""));
    repo.write_claim(
        "gone",
        "---\nid: gone\nchecks:\n  - kind: cmd\n    run: \"grep -q 'not-in-file' requirements.txt\"\nsupports:\n  - requirements.txt#libfoo\n---\nStatement for gone.\n",
    );
    insta::assert_snapshot!(stdout(&repo, &["drift"], 1));
}

/// Redact the non-deterministic fragments a write verb prints — the abbreviated
/// commit sha and the temp store root — so the snapshot captures the stable wording,
/// not a run's incidental paths. Done with plain string rewriting (no regex): each
/// volatile fragment sits on a line with a fixed, recognizable prefix.
fn redact_write_output(body: &str, root: &std::path::Path) -> String {
    // The tool prints the store root as git discovered it, which on macOS is the
    // canonicalized `/private/...` form, not the `/var/folders/...` symlink `TempDir`
    // hands back. Canonicalize so the redaction matches what the output contains.
    let root = std::fs::canonicalize(root)
        .unwrap_or_else(|_| root.to_path_buf())
        .display()
        .to_string();
    body.lines()
        .map(|line| {
            if let Some(idx) = line.find("at commit ") {
                // "The check held ... at commit <7hex>." — keep everything up to the sha.
                format!("{}at commit <sha>.", &line[..idx])
            } else if line.trim_start().starts_with("git -C ") {
                // "  git -C <root> add/rm/commit ..." — redact the temp root.
                line.replacen(&root, "<root>", 1)
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn retire_human_snapshot() {
    let repo = simple_store();
    let body = stdout(
        &repo,
        &["retire", "b-fact", "--note", "superseded by a real test"],
        0,
    );
    insta::assert_snapshot!(redact_write_output(&body, repo.path()));
}

#[test]
fn amend_human_snapshot() {
    // A store whose seeded claim's amended check holds against the tree, so the
    // amend succeeds deterministically (a `true` cmd is always exit 0).
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("pin", &claim_file("pin", ""));
    let body = stdout(
        &repo,
        &["amend", "pin", "--statement", "We now pin libfoo at 5.0."],
        0,
    );
    insta::assert_snapshot!(redact_write_output(&body, repo.path()));
}
