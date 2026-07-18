//! `claim drift`: the review queue — every claim whose computed status is
//! [`claim_core::Status::Drifted`], with what it supports and where it lives.
//!
//! A human scanning this sees, for each broken fact, the decision(s) that rested
//! on it (its `supports`) and the file to open (PRODUCT.md section 5: "drifted +
//! due claims, each with what it supports"). Owner routing via CODEOWNERS and
//! cause-grouping are the CI/hub lane and out of scope here.
//!
//! Like `list`, this runs no checks: it derives status from each claim's verdict
//! log via [`claim_core::compute_status`]. A drifted status means the claim's *own*
//! recorded check said the fact is no longer true — a `Drifted` in the log is the
//! latest conclusive verdict.
//!
//! # Exit codes
//!
//! - `0` — no claim has drifted.
//! - `1` — at least one claim has drifted (a review queue with items is a
//!   non-clean state a CI job or a human can gate on).

use anyhow::{Context, Result};
use claim_core::{compute_status, read_entries, Grace, Status};
use serde::Serialize;

use crate::cli::DriftArgs;
use crate::output::{emit, Format};
use crate::store::discover;

/// The exit code when nothing has drifted.
const EXIT_NO_DRIFT: i32 = 0;
/// The exit code when the review queue is non-empty.
const EXIT_DRIFT: i32 = 1;

/// Run `claim drift`, returning the exit code (0 clean, 1 drift present).
///
/// # Errors
///
/// Fails (and `main` exits 2) when no store is found, a claim file cannot be
/// parsed, or a verdict log cannot be read. A drifted claim is a *finding*, not an
/// error: it sets the returned exit code, it does not fail the run.
pub fn run(_args: &DriftArgs, format: Format) -> Result<i32> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let store = discover(&cwd)?;
    let claims = store.load_all()?;

    let now = crate::clock::now()?;
    let mut drifted = Vec::new();
    for loaded in &claims {
        let history = read_entries(&store.log_dir(), &loaded.claim.id)?;
        let report = compute_status(loaded.claim.max_age, &history, now, Grace::DEFAULT);
        if report.status == Status::Drifted {
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

    let exit = if drifted.is_empty() {
        EXIT_NO_DRIFT
    } else {
        EXIT_DRIFT
    };
    let report = DriftReport {
        status: "ok",
        drifted_count: drifted.len(),
        drifted: &drifted,
    };
    emit(format, &report, || human(&drifted))?;
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
    /// How many claims have drifted.
    drifted_count: usize,
    /// The drifted claims.
    drifted: &'a [DriftRow],
}

/// Print the review queue, or a clean note.
fn human(drifted: &[DriftRow]) {
    if drifted.is_empty() {
        println!("No drifted claims. The store is clean.");
        return;
    }

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
