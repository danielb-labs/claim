//! `claim drift`: run the checks and show the claims whose check reports drifted.
//!
//! A human scanning this sees, for each broken fact, the decision(s) that rested
//! on it (its `supports`) and the file to open (docs/design/PRODUCT.md section 5: "drifted
//! claims, each with what it supports"). Owner routing via CODEOWNERS and
//! cause-grouping are the CI/hub lane and out of scope here.
//!
//! Unlike v1, this *runs the checks* rather than reading a stored status: there is
//! no committed verdict log to read (see `docs/design/CLI-HUB-BOUNDARY.md`). A claim
//! is "drifted" here when running its check right now reports
//! [`claim_core::Verdict::Drifted`] — the check's own answer that the fact is no
//! longer true. Agent execution is opt-in via `CLAIM_AGENT_CMD`, exactly as `check`.
//!
//! # Exit codes
//!
//! - `0` — no claim's check reported drifted.
//! - `1` — at least one claim drifted (a review queue with items is a non-clean
//!   state a CI job or a human can gate on).
//! - `2` — a broken check, or a claim file that could not be loaded (a malformed or
//!   duplicate-id file is reported, not silently skipped, and floors the exit at 2
//!   while the well-formed claims are still triaged).

use anyhow::{Context, Result};
use claim_core::{run_check, CheckContext, Verdict};
use serde::Serialize;

use crate::cli::DriftArgs;
use crate::output::{emit, Format};
use claim_store::{agent_runner_from_env, discover, LoadError};

/// The exit code when nothing has drifted and every file loaded.
const EXIT_NO_DRIFT: i32 = 0;
/// The exit code when the review queue is non-empty.
const EXIT_DRIFT: i32 = 1;
/// The exit code when a check broke or a claim file could not be loaded (loud, but
/// the good claims are still triaged).
const EXIT_FAULT: i32 = 2;

/// Run `claim drift`, returning the exit code (0 clean, 1 drift present, 2 a broken
/// check or a load error).
///
/// # Errors
///
/// Fails (and `main` exits 2) when no store is found. A drifted claim is a
/// *finding*, a broken check is a *fault*, and a per-file load error is *reported* —
/// each sets the returned exit code without failing the run.
pub fn run(_args: &DriftArgs, format: Format) -> Result<i32> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let store = discover(&cwd)?;
    let load = store.load_all()?;

    // Agent execution is opt-in, exactly as `check`: unset CLAIM_AGENT_CMD leaves
    // agent checks Unverifiable and spawns nothing.
    let agent_runner = agent_runner_from_env().map_err(anyhow::Error::new)?;
    let ctx = CheckContext::new(store.root()).with_agent_runner(agent_runner);
    let now = claim_core::Timestamp::now();
    let _ = now; // no scheduling here; kept explicit so a future need is obvious

    let mut drifted = Vec::new();
    let mut any_broken = false;
    for loaded in &load.claims {
        // A claim is drifted when *any* of its checks reports Drifted right now. A
        // Broken check is a fault (it floors the exit at 2) but does not make the
        // claim "drifted" — the two are distinct conditions a reader must not
        // conflate.
        let mut this_drifted = false;
        for check in &loaded.claim.checks {
            match run_check(check, &ctx).verdict {
                Verdict::Drifted => this_drifted = true,
                Verdict::Broken => any_broken = true,
                Verdict::Held | Verdict::Unverifiable => {}
            }
        }
        if this_drifted {
            drifted.push(DriftRow {
                id: loaded.claim.id.to_string(),
                file: loaded.path.clone(),
                statement: loaded.claim.statement.trim().to_owned(),
                supports: loaded
                    .claim
                    .supports
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            });
        }
    }

    // A broken check or a load error is the loudest condition (it floors the exit at
    // 2); otherwise drift present is 1, clean is 0.
    let exit = if any_broken || !load.errors.is_empty() {
        EXIT_FAULT
    } else if drifted.is_empty() {
        EXIT_NO_DRIFT
    } else {
        EXIT_DRIFT
    };
    let report = DriftReport {
        status: "ok",
        exit,
        drifted_count: drifted.len(),
        drifted: &drifted,
        errors: &load.errors,
    };
    emit(format, &report, || human(&drifted, &load.errors))?;
    Ok(exit)
}

/// One drifted claim in the review queue.
#[derive(Debug, Serialize)]
struct DriftRow {
    /// The claim's id.
    id: String,
    /// The claim file's path relative to the store root — the file to open.
    file: String,
    /// The statement, so the queue reads without opening each file.
    statement: String,
    /// The decisions and claims that rest on this now-broken fact.
    supports: Vec<String>,
}

/// The machine form of `claim drift`.
#[derive(Debug, Serialize)]
struct DriftReport<'a> {
    /// Always `"ok"`: the verb ran. Drift is reported in the array, not here.
    status: &'static str,
    /// The exit code (0/1/2), matching `check`'s envelope so a consumer need not
    /// also inspect `$?`.
    exit: i32,
    /// How many claims have drifted.
    drifted_count: usize,
    /// The drifted claims.
    drifted: &'a [DriftRow],
    /// Per-file load errors (malformed or duplicate-id files); reported, not fatal.
    /// A non-empty list floors `exit` at 2.
    errors: &'a [LoadError],
}

/// Print the review queue (and any load errors), or a clean note.
fn human(drifted: &[DriftRow], errors: &[LoadError]) {
    if drifted.is_empty() {
        println!("No drifted claims. The store is clean.");
    } else {
        println!(
            "{} drifted claim(s) — each fact below is no longer true:",
            drifted.len()
        );
        for row in drifted {
            println!();
            println!("{}  ({})", row.id, row.file);
            println!("  {}", row.statement);
            if row.supports.is_empty() {
                println!("  supports: (nothing declared)");
            } else {
                println!("  supports:");
                for target in &row.supports {
                    println!("    - {target}");
                }
            }
        }
    }

    for err in errors {
        println!("error: {}: {}", err.file, err.message);
    }
}
