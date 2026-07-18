//! Integration tests for `claim check`, against real temp git stores.
//!
//! `check` is a stateless runtime verifier: it runs every claim's checks and
//! reports `held`/`drifted`/`broken` now, storing nothing. The exit-code contract
//! (0 held, 1 review, 2 broken, highest wins) and the never-persists guarantee are
//! the adversarial targets, so each has a direct test.

mod common;

use common::TestRepo;
use predicates::prelude::*;

/// A claim file whose cmd check holds iff `requirements.txt` pins libfoo at 4.2.
fn pin_claim(id: &str) -> String {
    format!(
        "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"grep -q 'libfoo==4.2' requirements.txt\"\n---\nWe pin libfoo at 4.2.\n"
    )
}

/// A store with a git identity and a committed `requirements.txt` pinning 4.2.
fn ready_repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo
}

#[test]
fn a_holding_check_is_held_and_exit_zero() {
    let repo = ready_repo();
    repo.write_claim("pin", &pin_claim("pin"));

    repo.claim()
        .arg("check")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("held"));

    // The CLI stores nothing: no verdict log tree is created.
    assert!(
        !repo.exists(".claims/log"),
        "check must not create a verdict log"
    );
}

#[test]
fn a_failing_check_is_drifted_and_exit_one() {
    let repo = ready_repo();
    repo.write_claim("pin", &pin_claim("pin"));
    // Break the fact.
    repo.write("requirements.txt", "libfoo==5.0\n");

    repo.claim()
        .arg("check")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("drifted"));
}

#[test]
fn a_broken_check_is_broken_and_exit_two() {
    let repo = ready_repo();
    // A command that cannot run: it exits non-0/1 (127 for not-found) → Broken.
    repo.write_claim(
        "broken",
        "---\nid: broken\nchecks:\n  - kind: cmd\n    run: \"this-binary-does-not-exist-xyz\"\n---\nA claim with a broken check.\n",
    );

    repo.claim()
        .arg("check")
        .assert()
        .code(2)
        .stdout(predicate::str::contains("broken"));
}

#[test]
fn a_plain_readme_in_the_store_is_ignored_and_check_succeeds() {
    // A plain `.claims/README.md` (no frontmatter fence) is a document, not a claim,
    // and must not be parsed. Here it coexists with a real claim: the claim is
    // checked, the README is skipped, and the run is a clean exit 0.
    let repo = ready_repo();
    repo.write_claim("pin", &pin_claim("pin"));
    repo.write(
        ".claims/README.md",
        "# Our claim store\n\nThis directory holds the repo's claims.\n",
    );

    repo.claim()
        .arg("check")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("held"))
        .stdout(predicate::str::contains("README").not());
}

#[test]
fn a_frontmatter_fenced_but_malformed_claim_stays_a_loud_error() {
    // A file that *opens with a `---` fence* declared its intent to be a claim, so
    // malformed YAML under it must stay a loud exit-2 error naming the file — never
    // silently skipped. Invariant #6: a real-but-broken claim is never dropped.
    let repo = ready_repo();
    repo.write_claim("pin", &pin_claim("pin"));
    repo.write(".claims/broken.md", "---\nchecks: [unclosed\n---\nS.\n");

    repo.claim()
        .arg("check")
        .assert()
        .code(2)
        .stdout(predicate::str::contains(".claims/broken.md"));
}

#[test]
fn an_unresolved_support_is_exit_one_even_when_the_check_holds() {
    let repo = ready_repo();
    // A claim that holds but supports a decision ref whose file does not exist.
    repo.write_claim(
        "orphan",
        "---\nid: orphan\nchecks:\n  - kind: cmd\n    run: \"grep -q 'libfoo==4.2' requirements.txt\"\nsupports:\n  - deleted-decision.md#anchor\n---\nSupports a deleted decision.\n",
    );

    repo.claim()
        .arg("check")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("held"))
        .stdout(predicate::str::contains("UNRESOLVED support"));
}

#[test]
fn highest_code_wins_across_a_mixed_store() {
    let repo = ready_repo();
    // Held, drifted, and broken claims together → overall exit 2.
    repo.write_claim("held", &pin_claim("held"));
    repo.write_claim(
        "broken",
        "---\nid: broken\nchecks:\n  - kind: cmd\n    run: \"nonexistent-cmd-abc\"\n---\nBroken.\n",
    );
    // A drifted claim: grep for a string not present.
    repo.write_claim(
        "drifted",
        "---\nid: drifted\nchecks:\n  - kind: cmd\n    run: \"grep -q 'not-in-file' requirements.txt\"\n---\nDrifted.\n",
    );

    repo.claim().arg("check").assert().code(2);
}

#[test]
fn an_agent_check_with_no_runner_is_unverifiable_exit_one_never_a_pass() {
    // The default: with CLAIM_AGENT_CMD unset, an agent check is Unverifiable and no
    // subprocess is spawned. `claim()` inherits the ambient environment, so this
    // asserts the real default a user gets — never a fabricated pass.
    let repo = ready_repo();
    repo.write_claim(
        "agentic",
        "---\nid: agentic\nchecks:\n  - kind: agent\n    instruction: investigate the changelog\n---\nNeeds an agent to verify.\n",
    );

    repo.claim()
        .env_remove("CLAIM_AGENT_CMD")
        .arg("check")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("unverifiable"));
}

/// A claim whose only check is an `agent` check. Its verdict comes entirely from
/// whatever runner `CLAIM_AGENT_CMD` names (or, unset, Unverifiable).
fn agent_claim(id: &str) -> String {
    format!(
        "---\nid: {id}\nchecks:\n  - kind: agent\n    instruction: is the CJK corruption still unfixed in libfoo 5.x?\n---\nWe pin libfoo at 4.2 because 5.x corrupts CJK PDFs.\n"
    )
}

/// Write an executable mock agent runner into the repo and return the shell
/// command string to point `CLAIM_AGENT_CMD` at it. The script reads and discards
/// stdin, then prints `stdout_line`. No real model is ever involved.
fn write_mock_runner(repo: &TestRepo, stdout_line: &str) -> String {
    let script =
        format!("#!/bin/sh\ncat >/dev/null\ncat <<'RUNNER_EOF'\n{stdout_line}\nRUNNER_EOF\n");
    repo.write("mock-agent.sh", &script);
    let path = repo.path().join("mock-agent.sh");
    std::fs::set_permissions(
        &path,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();
    // Run via the absolute path; CLAIM_AGENT_CMD is a shell command, so no argv split.
    path.to_string_lossy().into_owned()
}

#[test]
fn agent_check_with_runner_reports_held_and_its_evidence() {
    let repo = ready_repo();
    repo.write_claim("agentic", &agent_claim("agentic"));
    let runner = write_mock_runner(
        &repo,
        r#"{"verdict":"held","evidence":"no CJK fix in the 5.x changelog","citations":["CHANGELOG.md"]}"#,
    );

    let output = repo
        .claim()
        .env("CLAIM_AGENT_CMD", &runner)
        .args(["--json", "check"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(v["claims"][0]["checks"][0]["verdict"], "held");
    let evidence = v["claims"][0]["checks"][0]["evidence"].as_str().unwrap();
    assert!(evidence.contains("no CJK fix"), "evidence: {evidence}");
    assert!(evidence.contains("CHANGELOG.md"), "citations: {evidence}");
}

#[test]
fn agent_check_with_runner_reports_drifted() {
    let repo = ready_repo();
    repo.write_claim("agentic", &agent_claim("agentic"));
    let runner = write_mock_runner(
        &repo,
        r#"{"verdict":"drifted","evidence":"libfoo 5.3 shipped the CJK fix"}"#,
    );

    repo.claim()
        .env("CLAIM_AGENT_CMD", &runner)
        .arg("check")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("drifted"));
}

#[test]
fn agent_runner_malformed_output_is_broken_never_held() {
    // A runner that exits 0 but prints prose instead of the verdict JSON must be
    // Broken (exit 2), never a fabricated pass. This is the honesty guard against a
    // misbehaving runner.
    let repo = ready_repo();
    repo.write_claim("agentic", &agent_claim("agentic"));
    let runner = write_mock_runner(&repo, "I looked into it but could not decide.");

    repo.claim()
        .env("CLAIM_AGENT_CMD", &runner)
        .arg("check")
        .assert()
        .code(2)
        .stdout(predicate::str::contains("broken"));
}

#[test]
fn agent_runner_conflicting_verdicts_is_broken_never_a_chosen_pass() {
    // C1 end-to-end: a narrating runner emits a tentative held then a corrected
    // drifted. Neither wins — the run did not cleanly conclude, so it is Broken
    // (exit 2), and no false Held is reported.
    let repo = ready_repo();
    repo.write_claim("agentic", &agent_claim("agentic"));
    let runner = write_mock_runner(
        &repo,
        "Thinking out loud: {\"verdict\":\"held\"}\nOn reflection: {\"verdict\":\"drifted\",\"evidence\":\"5.3 fixed it\"}",
    );

    repo.claim()
        .env("CLAIM_AGENT_CMD", &runner)
        .arg("check")
        .assert()
        .code(2)
        .stdout(predicate::str::contains("broken"));
}

#[test]
fn agent_runner_duplicate_verdict_key_is_broken() {
    // M1 end-to-end: a single object with two `verdict` keys is ambiguous (serde
    // would silently keep the last). Broken, never resolved toward held.
    let repo = ready_repo();
    repo.write_claim("agentic", &agent_claim("agentic"));
    let runner = write_mock_runner(&repo, r#"{"verdict":"drifted","verdict":"held"}"#);

    repo.claim()
        .env("CLAIM_AGENT_CMD", &runner)
        .arg("check")
        .assert()
        .code(2)
        .stdout(predicate::str::contains("broken"));
}

#[test]
fn agent_runner_stderr_decoy_is_not_a_verdict() {
    // A well-formed held written to STDERR, with nothing parseable on stdout, must be
    // Broken — stderr is diagnostics, never a verdict source.
    let repo = ready_repo();
    repo.write_claim("agentic", &agent_claim("agentic"));
    repo.write(
        "stderr-decoy.sh",
        "#!/bin/sh\ncat >/dev/null\necho '{\"verdict\":\"held\",\"evidence\":\"decoy\"}' >&2\necho 'working...'\n",
    );
    let path = repo.path().join("stderr-decoy.sh");
    std::fs::set_permissions(
        &path,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();

    repo.claim()
        .env("CLAIM_AGENT_CMD", path.to_string_lossy().as_ref())
        .arg("check")
        .assert()
        .code(2)
        .stdout(predicate::str::contains("broken"));
}

#[test]
fn agent_runner_nonzero_exit_is_broken() {
    // A runner that fails (non-zero exit) is Broken even if it printed a held.
    let repo = ready_repo();
    repo.write_claim("agentic", &agent_claim("agentic"));
    repo.write(
        "failing-agent.sh",
        "#!/bin/sh\ncat >/dev/null\necho '{\"verdict\":\"held\"}'\nexit 4\n",
    );
    let path = repo.path().join("failing-agent.sh");
    std::fs::set_permissions(
        &path,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .unwrap();

    repo.claim()
        .env("CLAIM_AGENT_CMD", path.to_string_lossy().as_ref())
        .arg("check")
        .assert()
        .code(2)
        .stdout(predicate::str::contains("broken"));
}

#[test]
fn same_agent_claim_is_unverifiable_without_the_runner() {
    // The A/B on one claim: with CLAIM_AGENT_CMD pointing at the mock it is held;
    // with it unset the identical claim is Unverifiable. Proves the runner is what
    // executes the check, and its absence never fakes a pass.
    let repo = ready_repo();
    repo.write_claim("agentic", &agent_claim("agentic"));
    let runner = write_mock_runner(&repo, r#"{"verdict":"held","evidence":"ok"}"#);

    repo.claim()
        .env("CLAIM_AGENT_CMD", &runner)
        .arg("check")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("held"));

    repo.claim()
        .env_remove("CLAIM_AGENT_CMD")
        .arg("check")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("unverifiable"));
}

#[test]
fn empty_agent_cmd_is_a_loud_error_not_a_silent_fallback() {
    // A set-but-blank CLAIM_AGENT_CMD is a configuration mistake and must fail
    // loudly, never quietly fall back to leaving agent checks unverifiable.
    let repo = ready_repo();
    repo.write_claim("agentic", &agent_claim("agentic"));

    repo.claim()
        .env("CLAIM_AGENT_CMD", "   ")
        .arg("check")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("CLAIM_AGENT_CMD"));
}

#[test]
fn check_json_shape_is_stable() {
    let repo = ready_repo();
    repo.write_claim("pin", &pin_claim("pin"));

    let output = repo
        .claim()
        .args(["--json", "check"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();

    let v: serde_json::Value =
        serde_json::from_slice(&output).expect("check --json is one JSON object");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["exit"], 0);
    assert_eq!(v["checked"], 1);
    assert_eq!(v["errors"].as_array().unwrap().len(), 0);
    let claims = v["claims"].as_array().unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0]["id"], "pin");
    assert_eq!(claims[0]["checks"][0]["verdict"], "held");
    // The structured ProcessEnd is present alongside the human `detail`, so an
    // agent branches on structure, not prose.
    assert_eq!(claims[0]["checks"][0]["end"]["kind"], "exited");
    assert_eq!(claims[0]["checks"][0]["end"]["code"], 0);
    assert_eq!(claims[0]["checks"][0]["detail"], "exit 0");
    assert_eq!(claims[0]["exit"], 0);
}

#[test]
fn a_broken_check_json_carries_the_structured_end() {
    // A broken check's structured `end` lets an agent see 'not found' (exit 127)
    // without parsing English. A missing binary spawns then exits 127.
    let repo = ready_repo();
    repo.write_claim(
        "broken",
        "---\nid: broken\nchecks:\n  - kind: cmd\n    run: \"this-binary-does-not-exist-xyz\"\n---\nBroken.\n",
    );
    let output = repo
        .claim()
        .args(["--json", "check"])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(v["claims"][0]["checks"][0]["verdict"], "broken");
    // The shell ran the command and it exited non-0/1; the structured end is an
    // `exited` with a non-zero code (127 for not-found on a typical shell).
    assert_eq!(v["claims"][0]["checks"][0]["end"]["kind"], "exited");
}

#[test]
fn check_with_no_claims_reports_the_full_phrase() {
    // An empty store verified nothing: the human output says so plainly and, critically,
    // never "all held" — a false green over zero checks is exactly what this tool must
    // not emit (invariant #6).
    let repo = ready_repo();
    repo.claim()
        .arg("check")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("No claims matched."))
        .stdout(predicate::str::contains("all held").not());
}

#[test]
fn a_duplicate_id_across_two_files_is_a_loud_error_naming_both() {
    // Two files sharing an id conflate the recorded fact. `check` must error loudly,
    // naming both files, and not run the ambiguous claim.
    let repo = ready_repo();
    repo.write_claim("dup-a", &pin_claim("shared"));
    repo.write_claim("dup-b", &pin_claim("shared"));

    let output = repo
        .claim()
        .args(["--json", "check"])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let errors = v["errors"].as_array().unwrap();
    // Both files reported, each naming the other.
    assert_eq!(errors.len(), 2, "both duplicate files are reported");
    let files: Vec<&str> = errors.iter().map(|e| e["file"].as_str().unwrap()).collect();
    assert!(files.iter().any(|f| f.ends_with("dup-a.md")));
    assert!(files.iter().any(|f| f.ends_with("dup-b.md")));
    for e in errors {
        assert!(
            e["message"]
                .as_str()
                .unwrap()
                .contains("duplicate claim id 'shared'"),
            "the message names the shared id"
        );
    }
}

#[test]
fn a_malformed_sibling_does_not_stop_checking_the_good_claims() {
    // One bad file must not brick the run. The good claim still checks; the bad one
    // is reported; the exit is floored at 2. The bad sibling must *open with a
    // frontmatter fence* to count as a broken claim (a fenceless doc is a skipped
    // non-claim); its YAML is then malformed.
    let repo = ready_repo();
    repo.write_claim("good", &pin_claim("good"));
    repo.write_claim("bad", "---\nchecks: [unterminated\n---\nS.\n");

    let output = repo
        .claim()
        .args(["--json", "check"])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&output).unwrap();
    // The good claim ran.
    assert_eq!(v["claims"][0]["id"], "good");
    // The bad file is reported.
    let errors = v["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert!(errors[0]["file"].as_str().unwrap().ends_with("bad.md"));
}

// --- Skip: a skipped check is reported, green, and records no verdict. ---

/// A cmd claim whose check *would drift* (`run: false`), carrying the given skip
/// block, so a green result can come only from the skip suppressing the run — never
/// from the check itself passing.
fn drifting_claim_with_skip(id: &str, skip_yaml: &str) -> String {
    format!(
        "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"false\"\n{skip_yaml}---\nWould drift, but is skipped.\n"
    )
}

#[test]
fn an_unconditional_skip_is_green_and_records_no_verdict() {
    let repo = ready_repo();
    repo.write_claim(
        "parked",
        &drifting_claim_with_skip(
            "parked",
            "    skip:\n      reason: no runner in this environment\n",
        ),
    );

    // The check would drift, but the skip suppresses it, so the run is green and
    // reports the reason. A skip is not a pass.
    repo.claim()
        .arg("check")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("skipped"))
        .stdout(predicate::str::contains("no runner in this environment"));
}

#[test]
fn unless_true_cancels_the_skip_and_the_check_runs() {
    let repo = ready_repo();
    repo.write_claim(
        "parked",
        &drifting_claim_with_skip(
            "parked",
            "    skip:\n      reason: skip unless the runner is present\n      unless: \"true\"\n",
        ),
    );

    // `unless` succeeds (exit 0), so the skip is cancelled and the failing check runs
    // and drifts — proving an environment that can verify does.
    repo.claim()
        .arg("check")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("drifted"));
}

#[test]
fn unless_false_leaves_the_skip_in_force() {
    let repo = ready_repo();
    repo.write_claim(
        "parked",
        &drifting_claim_with_skip(
            "parked",
            "    skip:\n      reason: parked while the condition is false\n      unless: \"false\"\n",
        ),
    );
    repo.claim()
        .arg("check")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("skipped"));
}

#[test]
fn a_skip_json_shape_is_stable_and_carries_no_verdict() {
    let repo = ready_repo();
    repo.write_claim(
        "parked",
        &drifting_claim_with_skip("parked", "    skip:\n      reason: no runner here\n"),
    );
    let out = repo
        .claim()
        .args(["--json", "check"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
    let claim = &v["claims"][0];
    assert!(
        claim["checks"].as_array().unwrap().is_empty(),
        "a skipped check produces no verdict-bearing check result"
    );
    let skipped = claim["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1, "the skip is reported in the skipped list");
    assert_eq!(skipped[0]["reason"], "no runner here");
    assert!(
        skipped[0]["until"].is_null(),
        "an indefinite skip has no expiry"
    );
}

#[test]
fn a_skip_does_not_mask_a_sibling_checks_drift() {
    // The load-bearing honesty property: a skip suppresses only its own check. A
    // second check on the same claim that drifts must still surface (exit 1, in the
    // verdict-bearing `checks` list) — a skip that swallowed a sibling's drift would
    // be exactly the stale-green this tool exists to prevent.
    let repo = ready_repo();
    repo.write_claim(
        "mixed",
        "---\nid: mixed\nchecks:\n  - kind: cmd\n    run: \"true\"\n    skip:\n      reason: parked\n  - kind: cmd\n    run: \"false\"\n---\nOne check skipped, one drifts.\n",
    );

    let out = repo
        .claim()
        .args(["--json", "check"])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let claim = &v["claims"][0];
    let checks = claim["checks"].as_array().unwrap();
    assert_eq!(
        checks.len(),
        1,
        "the drifting check produced a verdict result"
    );
    assert_eq!(
        checks[0]["verdict"], "drifted",
        "the sibling drift surfaced"
    );
    assert_eq!(
        claim["skipped"].as_array().unwrap().len(),
        1,
        "the skipped check is reported separately"
    );
    assert_eq!(v["exit"], 1, "the overall exit reflects the sibling drift");
}

#[test]
fn a_broken_unless_runs_the_check_and_never_mutes() {
    // An `unless` that cannot be evaluated (exit 3) must run the failing check and
    // surface the drift, never silently skip it.
    let repo = ready_repo();
    repo.write_claim(
        "parked",
        &drifting_claim_with_skip(
            "parked",
            "    skip:\n      reason: parked\n      unless: \"exit 3\"\n",
        ),
    );
    repo.claim()
        .arg("check")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("drifted"));
}

#[test]
fn an_expired_until_runs_the_check_and_reports_the_lapse() {
    // `until` is long past, so the debt is called: the check runs (and drifts), and
    // the lapse is reported both in the JSON `note` and the human output.
    let repo = ready_repo();
    repo.write_claim(
        "parked",
        &drifting_claim_with_skip(
            "parked",
            "    skip:\n      reason: parked\n      until: 2020-01-01\n",
        ),
    );

    let out = repo
        .claim()
        .args(["--json", "check"])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let check = &v["claims"][0]["checks"][0];
    assert_eq!(check["verdict"], "drifted");
    assert!(
        check["note"]
            .as_str()
            .unwrap_or_default()
            .contains("skip expired"),
        "the lapse is reported in the note: {check}"
    );
}

// --- Selection: positional ids and --path narrow which claims run (issue #19). ---

/// Seed a store with three holding claims in distinct namespaces, so selection by id
/// and by path can be told apart.
fn selectable_store() -> TestRepo {
    let repo = ready_repo();
    repo.write_claim("auth/no-cycles", &pin_claim("auth/no-cycles"));
    repo.write_claim("billing/tax", &pin_claim("billing/tax"));
    repo.write_claim("infra/db", &pin_claim("infra/db"));
    repo
}

/// The ids in a `check --json` result, sorted, for set assertions.
fn checked_ids(output: &[u8]) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_slice(output).expect("check --json is one object");
    let mut ids: Vec<String> = v["claims"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

#[test]
fn a_single_positional_id_checks_only_that_claim() {
    let repo = selectable_store();
    let out = repo
        .claim()
        .args(["--json", "check", "billing/tax"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    assert_eq!(checked_ids(&out), vec!["billing/tax"]);
}

#[test]
fn multiple_positional_ids_check_exactly_those_claims() {
    let repo = selectable_store();
    let out = repo
        .claim()
        .args(["--json", "check", "auth/no-cycles", "billing/tax"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    assert_eq!(checked_ids(&out), vec!["auth/no-cycles", "billing/tax"]);
}

#[test]
fn an_unknown_positional_id_is_a_usage_error_naming_it() {
    // A named id asserts "this claim exists," so a typo must be a loud exit 2 naming
    // the unresolved id — never a silent pass over nothing.
    let repo = selectable_store();
    repo.claim()
        .args(["check", "auth/nocycles"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("no claim with id 'auth/nocycles'"));
}

#[test]
fn one_unknown_id_among_several_fails_the_whole_run() {
    // If any named id is unresolved the run is a usage error, even when the others
    // resolve — the caller asked for a claim that does not exist.
    let repo = selectable_store();
    repo.claim()
        .args(["check", "billing/tax", "does/not-exist"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("'does/not-exist'"));
}

#[test]
fn path_selects_the_matching_subset_and_agrees_with_list() {
    // `check --path` and `list --path` must select the same claims by construction —
    // both call the shared `claim_matches_path`. Here `auth/` matches exactly one.
    let repo = selectable_store();

    let checked = repo
        .claim()
        .args(["--json", "check", "--path", "auth/"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    assert_eq!(checked_ids(&checked), vec!["auth/no-cycles"]);

    let listed_out = repo
        .claim()
        .args(["--json", "list", "--path", "auth/"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let listed: serde_json::Value = serde_json::from_slice(&listed_out).unwrap();
    let listed_ids: Vec<&str> = listed["claims"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(listed_ids, vec!["auth/no-cycles"], "list agrees with check");
}

#[test]
fn ids_and_path_together_select_the_union() {
    // Named `infra/db` OR under `auth/` → both, never the intersection.
    let repo = selectable_store();
    let out = repo
        .claim()
        .args(["--json", "check", "infra/db", "--path", "auth/"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    assert_eq!(checked_ids(&out), vec!["auth/no-cycles", "infra/db"]);
}

#[test]
fn a_path_matching_zero_claims_is_exit_zero_and_says_no_claims_matched() {
    // A path glob may legitimately be empty: exit 0, and the report says plainly that
    // no claims matched — critically NOT "all held", which over zero checks would be a
    // false green (invariant #6).
    let repo = selectable_store();
    repo.claim()
        .args(["check", "--path", "nowhere/"])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("No claims matched."))
        .stdout(predicate::str::contains("all held").not());
}

#[test]
fn a_zero_match_path_reports_run_level_zero_counts_in_json() {
    let repo = selectable_store();
    let out = repo
        .claim()
        .args(["--json", "check", "--path", "nowhere/"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["checked"], 0);
    assert_eq!(v["ran"], 0, "a zero-match run verified nothing");
    assert_eq!(v["skipped"], 0);
    assert!(v["claims"].as_array().unwrap().is_empty());
}

#[test]
fn selection_does_not_lower_the_load_error_floor() {
    // A malformed sibling still floors the exit at 2 even when selection narrows to a
    // holding claim: selection is orthogonal to load faults.
    let repo = ready_repo();
    repo.write_claim("good", &pin_claim("good"));
    repo.write_claim("bad", "---\nchecks: [unterminated\n---\nS.\n");

    repo.claim()
        .args(["check", "good"])
        .assert()
        .code(2)
        .stdout(predicate::str::contains(".claims/bad.md"));
}

// --- Honest reporting when checks were skipped or nothing ran (issue #17). ---

#[test]
fn a_run_where_every_check_is_skipped_says_no_checks_ran_not_all_held() {
    // The bug this fixes: with every selected check skipped, exit 0 is correct (a skip
    // is an honest deferral), but the summary must say "no checks ran (all skipped)" —
    // never "all held", which would be a false green over zero verifications.
    let repo = ready_repo();
    repo.write_claim(
        "parked",
        &drifting_claim_with_skip("parked", "    skip:\n      reason: no runner here\n"),
    );

    repo.claim()
        .arg("check")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("no checks ran (all skipped)"))
        .stdout(predicate::str::contains("all held").not());
}

#[test]
fn a_mixed_held_and_skipped_run_names_both_in_the_summary() {
    // One claim holds, another is skipped: the summary must name both the hold and the
    // skip — reporting only the hold would hide the deferred check.
    let repo = ready_repo();
    repo.write_claim("held", &pin_claim("held"));
    repo.write_claim(
        "parked",
        &drifting_claim_with_skip("parked", "    skip:\n      reason: no runner here\n"),
    );

    repo.claim()
        .arg("check")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("held; 1 check(s) skipped"));
}

#[test]
fn the_all_held_summary_is_unchanged_when_nothing_is_skipped() {
    // The baseline: every check ran and held, none skipped → the wording is exactly the
    // long-standing "all held, all supports resolved".
    let repo = ready_repo();
    repo.write_claim("pin", &pin_claim("pin"));

    repo.claim()
        .arg("check")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("all held, all supports resolved"));
}

#[test]
fn run_level_counts_are_present_and_honest_in_json() {
    // A hub reads `ran`/`skipped` off the envelope to see "this run verified nothing"
    // without re-deriving it from the per-claim results.
    let repo = ready_repo();
    repo.write_claim("held", &pin_claim("held"));
    repo.write_claim(
        "parked",
        &drifting_claim_with_skip("parked", "    skip:\n      reason: no runner here\n"),
    );

    let out = repo
        .claim()
        .args(["--json", "check"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["checked"], 2, "two claims were evaluated");
    assert_eq!(v["ran"], 1, "one check produced a verdict");
    assert_eq!(v["skipped"], 1, "one check was skipped");
}
