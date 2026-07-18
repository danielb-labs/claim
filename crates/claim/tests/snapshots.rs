//! Snapshot tests for the four read verbs' human output (CLAUDE.md's insta
//! obligation), so a change to a table or a report is a deliberate, reviewable
//! diff.
//!
//! Every fixture is built from fixed bytes with `now` pinned via `CLAIM_NOW`, and
//! the store paths in the output are store-relative (`.claims/x.md`) not temp
//! paths, so the snapshots are stable across runs and machines. Verdicts are
//! seeded with the all-zero commit sha, which abbreviates to a fixed `0000000`.

mod common;

use common::TestRepo;

const NOW: &str = "2026-07-17T00:00:00Z";

/// A cmd claim; the check body is inert (`list`/`drift`/`log` never run it).
fn claim_file(id: &str, max_age: &str, supports: &str) -> String {
    let supports_block = if supports.is_empty() {
        String::new()
    } else {
        format!("supports:\n  - {supports}\n")
    };
    format!(
        "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: on-change\nmax-age: {max_age}\n{supports_block}---\nStatement for {id}.\n"
    )
}

/// A store with three claims of distinct status at NOW: verified, drifted, stale.
fn mixed_store() -> TestRepo {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();

    repo.write_claim("fresh", &claim_file("fresh", "120d", ""));
    repo.write_verdict("fresh", "2026-07-10T00:00:00Z", "held");

    repo.write_claim(
        "gone",
        &claim_file("gone", "120d", "requirements.txt#libfoo"),
    );
    repo.write_verdict("gone", "2026-07-10T00:00:00Z", "held");
    repo.write_verdict("gone", "2026-07-15T00:00:00Z", "drifted");

    repo.write_claim("old", &claim_file("old", "30d", ""));
    repo.write_verdict("old", "2026-01-01T00:00:00Z", "held");

    repo
}

/// The stdout of a `claim` invocation as a UTF-8 string, for snapshotting.
fn stdout(repo: &TestRepo, args: &[&str], code: i32) -> String {
    let out = repo
        .claim_at(NOW)
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
    let repo = mixed_store();
    insta::assert_snapshot!(stdout(&repo, &["list"], 0));
}

#[test]
fn drift_human_snapshot() {
    let repo = mixed_store();
    insta::assert_snapshot!(stdout(&repo, &["drift"], 1));
}

#[test]
fn log_human_snapshot() {
    let repo = mixed_store();
    insta::assert_snapshot!(stdout(&repo, &["log", "gone"], 0));
}

#[test]
fn check_human_snapshot() {
    // A store whose checks all hold, so the run is deterministic (a `true` cmd is
    // always exit 0) and no wall-clock durations leak into the output.
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("a", &claim_file("a", "120d", ""));
    repo.write_claim("b", &claim_file("b", "120d", "requirements.txt#libfoo"));
    // --report-only so the run writes nothing (keeps the fixture inspectable) and
    // the "(report-only: not recorded)" line is exercised.
    insta::assert_snapshot!(stdout(&repo, &["check", "--all", "--report-only"], 0));
}

/// Redact the non-deterministic fragments a write verb prints — the abbreviated
/// commit sha, the temp store root, and the verdict-log filename's timestamp+hash —
/// so the snapshot captures the stable wording, not a run's incidental paths. Done
/// with plain string rewriting (no regex/insta-filters feature): each volatile
/// fragment sits on a line with a fixed, recognizable prefix, so the redaction keys
/// off that prefix rather than a pattern.
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
                // "Recorded ... at commit <7hex>." — keep everything up to the sha.
                format!("{}at commit <sha>.", &line[..idx])
            } else if line.trim_start().starts_with("git -C ") {
                // "  git -C <root> add <paths>" — redact the temp root and any
                // verdict-log filename in the argument list.
                let redacted_root = line.replacen(&root, "<root>", 1);
                redact_log_filenames(&redacted_root)
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Replace any `.claims/log/<id>/<stamp>-<hash>.json` argument with a stable token,
/// so a verdict-log filename's timestamp and content hash do not churn the snapshot.
fn redact_log_filenames(line: &str) -> String {
    line.split(' ')
        .map(|tok| {
            if tok.starts_with(".claims/log/") && tok.ends_with(".json") {
                let id = tok
                    .strip_prefix(".claims/log/")
                    .and_then(|rest| rest.split('/').next())
                    .unwrap_or("<id>");
                format!(".claims/log/{id}/<entry>.json")
            } else {
                tok.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn retire_human_snapshot() {
    let repo = mixed_store();
    let body = stdout(
        &repo,
        &["retire", "gone", "--note", "libfoo 5.0 shipped"],
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
    repo.write_claim("pin", &claim_file("pin", "120d", ""));
    let body = stdout(
        &repo,
        &["amend", "pin", "--statement", "We now pin libfoo at 5.0."],
        0,
    );
    insta::assert_snapshot!(redact_write_output(&body, repo.path()));
}

#[test]
fn stats_human_snapshot() {
    // The mixed store gives a verified/drifted/stale spread and several verdict
    // kinds; `now` is pinned, so every number and the honesty note are stable — no
    // redaction needed.
    let repo = mixed_store();
    insta::assert_snapshot!(stdout(&repo, &["stats"], 0));
}
