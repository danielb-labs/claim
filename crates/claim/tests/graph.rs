//! Integration tests for `claim graph`: the `supports` graph, ASCII and JSON.
//!
//! The graph is a pure read over `supports` — no checks run, nothing is resolved — so
//! the tests seed a store with known edges and assert the grouping, the `[claim]`
//! tagging of claim-to-claim edges, node classification, and the support-nothing
//! footer. The default view groups by claim; `--backers` flips to grouping by target.
//! Output is deterministic (sorted), so it also carries an insta snapshot.

mod common;

use common::TestRepo;
use predicates::prelude::*;

/// A valid claim with a trivial holding check and the given `supports` targets.
fn claim_with_supports(id: &str, supports: &[&str]) -> String {
    let block = if supports.is_empty() {
        String::new()
    } else {
        let items = supports
            .iter()
            .map(|s| format!("  - {s}\n"))
            .collect::<String>();
        format!("supports:\n{items}")
    };
    format!(
        "---\nid: {id}\nchecks:\n  - kind: cmd\n    run: \"true\"\n{block}hub:\n  max-age: 30d\n---\nClaim {id}.\n"
    )
}

/// Three claims: `a` backs a decision and claim `b`; `b` backs a decision; `c` is
/// wired into nothing.
fn seeded() -> TestRepo {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("a", &claim_with_supports("a", &["DECISION.md#x", "b"]));
    repo.write_claim("b", &claim_with_supports("b", &["DECISION.md#y"]));
    repo.write_claim("c", &claim_with_supports("c", &[]));
    repo
}

#[test]
fn human_groups_targets_under_each_claim() {
    let repo = seeded();
    repo.claim()
        .arg("graph")
        .assert()
        .code(0)
        // `a` heads its group and lists the targets it supports, sorted.
        .stdout(predicate::str::contains(
            "a\n  ├─ DECISION.md#x\n  └─ b [claim]",
        ))
        .stdout(predicate::str::contains("b\n  └─ DECISION.md#y"))
        // `c` supports nothing, so it heads no group; it lands in the footer instead.
        .stdout(predicate::str::contains("1 claim(s) support nothing: c"));
}

#[test]
fn claim_targets_are_tagged_and_decision_targets_are_not() {
    let repo = seeded();
    let out = repo
        .claim()
        .arg("graph")
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    // `b` is a known claim id, so the edge `a -> b` carries the `[claim]` tag.
    assert!(
        text.contains("└─ b [claim]"),
        "a claim-to-claim edge is tagged: {text}"
    );
    // A decision ref is not a claim, so its child line carries no tag.
    assert!(
        text.contains("├─ DECISION.md#x\n"),
        "a decision target is plain: {text}"
    );
    assert!(
        !text.contains("DECISION.md#x [claim]"),
        "a decision target is never tagged as a claim: {text}"
    );
}

#[test]
fn backers_flag_groups_claims_under_each_target() {
    let repo = seeded();
    repo.claim()
        .args(["graph", "--backers"])
        .assert()
        .code(0)
        // The inverse view: each target, then the claims backing it.
        .stdout(predicate::str::contains("DECISION.md#x\n  └─ a"))
        .stdout(predicate::str::contains("DECISION.md#y\n  └─ b"))
        // A target that is itself a claim keeps the `[claim]` tag on its header line.
        .stdout(predicate::str::contains("b [claim]\n  └─ a"))
        // The support-nothing footer belongs to the by-claim view; the backers view
        // does not carry it, since a non-backer is not a target.
        .stdout(predicate::str::contains("support nothing").not());
}

#[test]
fn json_shape_classifies_nodes_and_lists_edges() {
    let repo = seeded();
    let out = repo
        .claim()
        .args(["--json", "graph"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
    assert_eq!(v["status"], "ok");

    let nodes = v["nodes"].as_array().unwrap();
    let kind_of = |id: &str| {
        nodes
            .iter()
            .find(|n| n["id"] == id)
            .map(|n| n["kind"].as_str().unwrap().to_owned())
    };
    // An isolated claim is still a node; a known claim id stays `claim` even when it is
    // the *target* of an edge; a decision ref is `decision`.
    assert_eq!(kind_of("c").as_deref(), Some("claim"));
    assert_eq!(kind_of("b").as_deref(), Some("claim"));
    assert_eq!(kind_of("DECISION.md#x").as_deref(), Some("decision"));

    let edges = v["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 3, "three supports edges");
    assert!(
        edges.iter().any(|e| e["from"] == "a" && e["to"] == "b"),
        "the claim-to-claim edge a -> b is present"
    );
}

#[test]
fn json_is_the_same_regardless_of_backers() {
    // `--backers` only regroups the human view; the machine node/edge list is
    // direction-agnostic, so the two must serialize identically.
    let repo = seeded();
    let plain = repo
        .claim()
        .args(["--json", "graph"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let flipped = repo
        .claim()
        .args(["--json", "graph", "--backers"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    assert_eq!(plain, flipped);
}

#[test]
fn a_store_with_no_supports_edges_says_so() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    repo.write_claim("lonely", &claim_with_supports("lonely", &[]));
    repo.claim()
        .arg("graph")
        .assert()
        .code(0)
        // No claim declares a `supports`, so there is no group to head — but the lone
        // claim is still surfaced as supporting nothing, never silently dropped.
        .stdout(predicate::str::contains("No supports edges"))
        .stdout(predicate::str::contains(
            "1 claim(s) support nothing: lonely",
        ));
}

#[test]
fn human_output_snapshot() {
    let repo = seeded();
    let out = repo
        .claim()
        .arg("graph")
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    insta::assert_snapshot!(String::from_utf8(out).unwrap());
}

#[test]
fn a_load_error_is_exit_2_and_surfaced_in_json() {
    let repo = seeded();
    // A file that opens with a `---` fence but is malformed: a real-but-broken claim
    // the scanner must report loudly, never drop.
    repo.write(".claims/bad.md", "---\nchecks: [unterminated\n---\nS.\n");

    // Human: exit 2, the broken file named on stdout.
    repo.claim()
        .arg("graph")
        .assert()
        .code(2)
        .stdout(predicate::str::contains(".claims/bad.md"));

    // JSON: the fault is in the payload — `exit: 2` and a non-empty `errors` — not a
    // silent green an agent inspecting the object would miss.
    let out = repo
        .claim()
        .args(["--json", "graph"])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
    assert_eq!(v["exit"], 2);
    let errors = v["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "the one broken file is reported");
    assert!(errors[0]["file"].as_str().unwrap().ends_with("bad.md"));
}

#[test]
fn duplicate_supports_collapse_to_one_edge() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    // `a` lists `b` twice; the grouped view and the JSON edges must both show it once.
    repo.write_claim("a", &claim_with_supports("a", &["b", "b"]));

    let out = repo
        .claim()
        .args(["--json", "graph"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let a_to_b = v["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["from"] == "a" && e["to"] == "b")
        .count();
    assert_eq!(
        a_to_b, 1,
        "a duplicated supports target is one edge, not two"
    );

    // The by-claim human view collapses the duplicate too: `b` appears once under `a`.
    let human = repo
        .claim()
        .arg("graph")
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        String::from_utf8(human).unwrap().matches("─ b").count(),
        1,
        "the duplicated target renders once, not twice"
    );
}

#[test]
fn a_self_loop_renders_and_heads_a_group() {
    let repo = TestRepo::new();
    repo.claim().arg("init").assert().success();
    // `a` supports itself; `z` supports nothing.
    repo.write_claim("a", &claim_with_supports("a", &["a"]));
    repo.write_claim("z", &claim_with_supports("z", &[]));

    repo.claim()
        .arg("graph")
        .assert()
        .code(0)
        // The self-edge renders under `a`, tagged as a claim-to-claim edge; only `z`
        // supports nothing.
        .stdout(predicate::str::contains("a\n  └─ a [claim]"))
        .stdout(predicate::str::contains("1 claim(s) support nothing: z"));
}
