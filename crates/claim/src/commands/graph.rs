//! `claim graph`: the store's `supports` graph — each claim's `supports` edges,
//! grouped by the decision or claim they back.
//!
//! A read-only view over the store: nodes are claims and the decision refs (or other
//! claims) they support; edges are `claim -> target`. The CLI keeps this deliberately
//! light — the hub is where richer graph analysis and real visualization live. Here
//! it is an ASCII grouping for a person and, under `--json`, a node/edge list an agent
//! can traverse. Everything is sorted, so the output is deterministic and diffs cleanly.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::cli::GraphArgs;
use crate::output::{emit, Format};
use claim_store::{discover, LoadError};

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
pub fn run(_args: &GraphArgs, format: Format) -> Result<i32> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let store = discover(&cwd)?;
    let load = store.load_all()?;

    // Known claim ids, so a target can be classified as another claim vs a decision ref.
    let claim_ids: BTreeSet<String> = load.claims.iter().map(|c| c.claim.id.to_string()).collect();

    // Backers grouped by target (deduped by the BTreeSet). Both the human view and the
    // machine edges are derived from this one grouping, so a target listed twice on one
    // claim is one edge in both — the two views never disagree.
    let mut backers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for loaded in &load.claims {
        let from = loaded.claim.id.to_string();
        for target in &loaded.claim.supports {
            backers
                .entry(target.as_str().to_owned())
                .or_default()
                .insert(from.clone());
        }
    }

    // Edges from the deduped grouping, ordered by (target, backing claim) via the
    // BTree ordering — distinct and sorted with no separate pass.
    let edges: Vec<Edge> = backers
        .iter()
        .flat_map(|(to, froms)| {
            froms.iter().map(move |from| Edge {
                from: from.clone(),
                to: to.clone(),
            })
        })
        .collect();

    // A claim is connected if it appears as a backer or as a target of any edge.
    let connected: BTreeSet<&str> = backers
        .keys()
        .map(String::as_str)
        .chain(backers.values().flatten().map(String::as_str))
        .collect();
    let isolated: Vec<String> = claim_ids
        .iter()
        .filter(|id| !connected.contains(id.as_str()))
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
    for target in backers.keys() {
        if !claim_ids.contains(target) {
            nodes.push(Node {
                id: target.clone(),
                kind: "decision",
            });
        }
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

    emit(format, &report, || human(&backers, &isolated, &load.errors))?;

    Ok(exit)
}

/// Print the human view: each target, then the claims backing it, then a footer for
/// claims wired into no edge and any load errors.
fn human(backers: &BTreeMap<String, BTreeSet<String>>, isolated: &[String], errors: &[LoadError]) {
    if backers.is_empty() {
        println!("No supports edges.");
    }
    for (target, claims) in backers {
        println!("{target}");
        let last = claims.len().saturating_sub(1);
        for (i, claim) in claims.iter().enumerate() {
            let marker = if i == last { "└─" } else { "├─" };
            println!("  {marker} {claim}");
        }
    }

    if !isolated.is_empty() {
        println!();
        println!(
            "Not connected ({}): {}",
            isolated.len(),
            isolated.join(", ")
        );
    }

    for err in errors {
        println!("error: {}: {}", err.file, err.message);
    }
}
