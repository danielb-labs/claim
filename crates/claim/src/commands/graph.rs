//! `claim graph`: the store's `supports` graph — each claim's `supports` edges,
//! grouped by the claim that declares them (the direction `supports` reads).
//!
//! A read-only view over the store: nodes are claims and the decision refs (or other
//! claims) they support; edges are `claim -> target`. The CLI keeps this deliberately
//! light — the hub is where richer graph analysis and real visualization live. Here
//! it is an ASCII grouping for a person and, under `--json`, a node/edge list an agent
//! can traverse. Everything is sorted, so the output is deterministic and diffs cleanly.
//!
//! The default human view groups by *claim*: each claim heads a group and lists the
//! targets it backs, with a target that is itself a known claim id tagged `[claim]` so
//! a claim-to-claim edge is visible at a glance. `--backers` flips to the inverse view
//! (each target, then the claims backing it) for the "who backs this decision?"
//! question. Both are rendered from the same deduped edge set, so they can never
//! disagree; only the grouping key differs. The `--json` output is direction-agnostic
//! (`{status, nodes, edges, exit, errors}`) and unaffected by `--backers`.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::cli::GraphArgs;
use crate::output::{emit, Format};
use claim_store::{discover, LoadError};

/// The tag appended to a target that is itself a known claim id, so a claim-to-claim
/// edge stands out from an edge to a decision ref or file in the grouped-by-claim view.
const CLAIM_TAG: &str = " [claim]";

/// The machine form of `claim graph`: a node/edge list.
#[derive(Debug, Serialize)]
struct GraphReport<'a> {
    /// Always `"ok"`: the verb ran. Findings are in `exit`/`errors`, not here — an
    /// unloadable file is a successful run that reports the fault.
    status: &'static str,
    /// Every claim (`kind: "claim"`) and every decision ref a claim supports that is
    /// not itself a claim id (`kind: "decision"`), sorted by id.
    nodes: Vec<Node>,
    /// One distinct `claim -> target` edge per supported target, sorted by target then
    /// backing claim; duplicate `supports` entries collapse to one edge (matching the
    /// grouped human view).
    edges: Vec<Edge>,
    /// The overall exit code (0, or 2 if any claim file failed to load), duplicated in
    /// the process exit so a `--json` consumer need not also inspect `$?`.
    exit: i32,
    /// Per-file load errors (a malformed claim file): surfaced in the payload so a
    /// `--json` agent sees a broken file rather than a silent green, matching the other
    /// read verbs. A non-empty list floors `exit` at 2.
    errors: &'a [LoadError],
}

#[derive(Debug, Serialize)]
struct Node {
    id: String,
    kind: &'static str,
}

#[derive(Debug, Serialize)]
struct Edge {
    /// The backing claim's id.
    from: String,
    /// The target it supports — a decision ref or another claim's id.
    to: String,
}

/// Render the `supports` graph. Exit 2 if any claim file failed to load (matching
/// `list`/`check`); a well-formed store with no edges is exit 0.
pub fn run(args: &GraphArgs, format: Format) -> Result<i32> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let store = discover(&cwd)?;
    let load = store.load_all()?;

    // Known claim ids, so a target can be classified as another claim vs a decision ref.
    let claim_ids: BTreeSet<String> = load.claims.iter().map(|c| c.claim.id.to_string()).collect();

    // Targets grouped by the claim that backs them (deduped by the BTreeSet), the
    // natural direction `supports` reads. Both the grouped-by-claim human view and the
    // machine edges derive from this one grouping, so a target listed twice on one claim
    // is one edge everywhere.
    let mut supports: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for loaded in &load.claims {
        let from = loaded.claim.id.to_string();
        for target in &loaded.claim.supports {
            supports
                .entry(from.clone())
                .or_default()
                .insert(target.as_str().to_owned());
        }
    }

    // Edges from the deduped grouping, ordered by (target, backing claim) so the machine
    // order is stable and independent of the human grouping direction.
    let mut edges: Vec<Edge> = supports
        .iter()
        .flat_map(|(from, tos)| {
            tos.iter().map(move |to| Edge {
                from: from.clone(),
                to: to.clone(),
            })
        })
        .collect();
    edges.sort_by(|a, b| (&a.to, &a.from).cmp(&(&b.to, &b.from)));

    // A claim that declares no `supports` heads no group; it is surfaced in a footer
    // rather than dropped, so a claim wired into nothing stays visible (invariant #6).
    let unsupported: Vec<String> = claim_ids
        .iter()
        .filter(|id| !supports.contains_key(id.as_str()))
        .cloned()
        .collect();

    let mut nodes: Vec<Node> = load
        .claims
        .iter()
        .map(|c| Node {
            id: c.claim.id.to_string(),
            kind: "claim",
        })
        .collect();
    // A decision target is any supported target that is not itself a claim id. Collected
    // into a set first so a ref backed by several claims yields one node, not one per
    // backer.
    let mut decision_targets: BTreeSet<&str> = BTreeSet::new();
    for tos in supports.values() {
        for to in tos {
            if !claim_ids.contains(to) {
                decision_targets.insert(to);
            }
        }
    }
    for target in decision_targets {
        nodes.push(Node {
            id: target.to_owned(),
            kind: "decision",
        });
    }
    nodes.sort_by(|a, b| a.id.cmp(&b.id));

    let exit = if load.errors.is_empty() { 0 } else { 2 };
    let report = GraphReport {
        status: "ok",
        nodes,
        edges,
        exit,
        errors: &load.errors,
    };

    emit(format, &report, || {
        if args.backers {
            human_by_target(&supports, &claim_ids, &load.errors);
        } else {
            human_by_claim(&supports, &claim_ids, &unsupported, &load.errors);
        }
    })?;

    Ok(exit)
}

/// The default view: group by claim, each claim then the targets it supports, a target
/// that is a known claim id tagged `[claim]`, and a footer for claims wired into
/// nothing and any load errors.
fn human_by_claim(
    supports: &BTreeMap<String, BTreeSet<String>>,
    claim_ids: &BTreeSet<String>,
    unsupported: &[String],
    errors: &[LoadError],
) {
    if supports.is_empty() {
        println!("No supports edges.");
    }
    for (claim, targets) in supports {
        println!("{claim}");
        let last = targets.len().saturating_sub(1);
        for (i, target) in targets.iter().enumerate() {
            let marker = if i == last { "└─" } else { "├─" };
            let tag = if claim_ids.contains(target) {
                CLAIM_TAG
            } else {
                ""
            };
            println!("  {marker} {target}{tag}");
        }
    }

    if !unsupported.is_empty() {
        println!();
        println!(
            "{} claim(s) support nothing: {}",
            unsupported.len(),
            unsupported.join(", ")
        );
    }

    print_errors(errors);
}

/// The `--backers` view: group by target, each target then the claims backing it, a
/// target that is itself a known claim id tagged `[claim]`. Answers "who backs this
/// decision?". Only load errors follow in the footer — every claim that backs anything
/// appears as a backer here, and a claim that backs nothing is not a backer of any
/// target, which is the by-claim view's concern, not this one's.
fn human_by_target(
    supports: &BTreeMap<String, BTreeSet<String>>,
    claim_ids: &BTreeSet<String>,
    errors: &[LoadError],
) {
    // Invert the by-claim grouping into targets, deduped and sorted by the BTree.
    let mut backers: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (claim, targets) in supports {
        for target in targets {
            backers.entry(target).or_default().insert(claim);
        }
    }

    if backers.is_empty() {
        println!("No supports edges.");
    }
    for (target, claims) in &backers {
        let tag = if claim_ids.contains(*target) {
            CLAIM_TAG
        } else {
            ""
        };
        println!("{target}{tag}");
        let last = claims.len().saturating_sub(1);
        for (i, claim) in claims.iter().enumerate() {
            let marker = if i == last { "└─" } else { "├─" };
            println!("  {marker} {claim}");
        }
    }

    print_errors(errors);
}

/// Print the per-file load errors that floor the exit at 2, one `error: <file>: <msg>`
/// line each. Shared by both views so a broken file is reported identically.
fn print_errors(errors: &[LoadError]) {
    for err in errors {
        println!("error: {}: {}", err.file, err.message);
    }
}
