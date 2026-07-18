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
//! - `2` — a tool error, or a claim file that could not be loaded (a malformed or
//!   duplicate-id file is reported, not silently skipped, and floors the exit at 2
//!   while the well-formed claims are still triaged).

use anyhow::{Context, Result};
use claim_core::{compute_status, read_entries, Grace, Status};
use serde::Serialize;

use crate::cli::DriftArgs;
use crate::output::{emit, Format};
use claim_store::{discover, LoadError};

/// The exit code when nothing has drifted and every file loaded.
const EXIT_NO_DRIFT: i32 = 0;
/// The exit code when the review queue is non-empty.
const EXIT_DRIFT: i32 = 1;
/// The exit code when a claim file could not be loaded (loud, but the good claims
/// are still triaged).
const EXIT_LOAD_ERROR: i32 = 2;

/// Run `claim drift`, returning the exit code (0 clean, 1 drift present, 2 a load
/// error).
///
/// # Errors
///
/// Fails (and `main` exits 2) when no store is found or a verdict log cannot be
/// read. A drifted claim is a *finding*, and a per-file load error is *reported*,
/// not an `Err`: both set the returned exit code without failing the run.
pub fn run(_args: &DriftArgs, format: Format) -> Result<i32> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let store = discover(&cwd)?;
    let load = store.load_all()?;

    let now = crate::clock::now()?;
    let mut drifted = Vec::new();
    for loaded in &load.claims {
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

    // A load error is the loudest condition (it floors the exit at 2); otherwise
    // drift present is 1, clean is 0.
    let exit = if !load.errors.is_empty() {
        EXIT_LOAD_ERROR
    } else if drifted.is_empty() {
        EXIT_NO_DRIFT
    } else {
        EXIT_DRIFT
    };
    let report = DriftReport {
        status: "ok",
        now: now.to_string(),
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
    /// The instant statuses were computed against, RFC 3339, matching `check`'s
    /// envelope so a consumer can reproduce the derivation.
    now: String,
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
