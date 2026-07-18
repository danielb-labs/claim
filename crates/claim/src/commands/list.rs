//! `claim list`: the store inventory, each claim with its *computed* status.
//!
//! Unlike `check`, this runs nothing: it reads each claim's verdict log and
//! derives the status with [`claim_core::compute_status`] against `now`, the same
//! read-time derivation the hub and MCP use (invariant #3, status is computed
//! never stored). The result is a filtered, aligned table (or a `--json` array) of
//! id, status, last-verified / stale-in, and supports count.
//!
//! Filters combine with AND — every one given must hold — so a claim survives to
//! the output only if it matches the status filter *and* the path prefix *and* the
//! text term, and so on. This makes `--status drifted --path src/` mean exactly
//! "drifted claims under src/", which is what a reader expects.

use anyhow::{Context, Result};
use claim_core::{
    compute_status, read_entries, Claim, Grace, LogEntry, Status, StatusReport, Timestamp,
};
use serde::Serialize;

use crate::cli::ListArgs;
use crate::output::{emit, status_label, Format};
use claim_store::{claim_matches_path, discover, LoadError, LoadedClaim};

/// Run `claim list`.
///
/// A malformed or duplicate-id claim file is *reported* (in the envelope's `errors`
/// array, or an error line in the human output) rather than aborting the listing —
/// the well-formed claims still show. Their presence then makes the command exit 2:
/// the good claims are emitted first, then an `Err` is returned so `main` reports
/// the fault and exits non-zero. Loud and useful, never a store silenced by one bad
/// file.
///
/// Returns the process exit code: `0` when every file loaded, `2` when any claim
/// file could not be loaded (the good claims are still listed — loud and useful,
/// never a store silenced by one bad file).
///
/// # Errors
///
/// Fails only when no store is found or a verdict log cannot be read. A per-file
/// load error is not an `Err` — it is reported in the output and reflected in the
/// returned exit code.
pub fn run(args: &ListArgs, format: Format) -> Result<i32> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let store = discover(&cwd)?;
    let load = store.load_all()?;

    let now = crate::clock::now()?;
    let status_filter = parse_status_filter(args.status.as_deref())?;

    let mut rows = Vec::new();
    for loaded in &load.claims {
        // One read of the log per claim: the status computation and the
        // `--unverified` filter both derive from the same history.
        let history = read_entries(&store.log_dir(), &loaded.claim.id)?;
        let report = compute_status(loaded.claim.max_age, &history, now, Grace::DEFAULT);
        if keep(args, loaded, &report, status_filter, &history) {
            rows.push(Row::new(loaded, &report, now));
        }
    }

    let exit = if load.errors.is_empty() { 0 } else { 2 };
    let inventory = Inventory {
        status: "ok",
        now: now.to_string(),
        exit,
        claims: &rows,
        errors: &load.errors,
    };
    emit(format, &inventory, || human(&rows, &load.errors))?;
    Ok(exit)
}

/// Whether a claim passes every active filter (AND semantics).
///
/// A filter that was not given is not a constraint. Kept as one predicate so the
/// AND-combination is in one place and a new filter is one clause, not a new pass
/// over the corpus.
fn keep(
    args: &ListArgs,
    loaded: &LoadedClaim,
    report: &StatusReport,
    status_filter: Option<Status>,
    history: &[LogEntry],
) -> bool {
    if let Some(want) = status_filter {
        if report.status != want {
            return false;
        }
    }

    // `--stale` means the `Status::Stale` status only, matching its name and
    // `--status stale`. Drift is a distinct status (surfaced by `claim drift`); a
    // `--stale` that also matched drifted would give the word two meanings.
    if args.stale && report.status != Status::Stale {
        return false;
    }

    if args.unverified && !is_unverified(history) {
        return false;
    }

    if let Some(prefix) = &args.path {
        if !claim_matches_path(&loaded.path, &loaded.claim.supports, prefix) {
            return false;
        }
    }

    if let Some(target) = &args.supports {
        if !loaded
            .claim
            .supports
            .iter()
            .any(|s| s.as_str() == target.as_str())
        {
            return false;
        }
    }

    if let Some(term) = &args.term {
        if !text_matches(&loaded.claim, term) {
            return false;
        }
    }

    true
}

/// Whether a claim has no passing verdict on record — never genuinely verified.
///
/// This is the never-verified case: a claim hand-committed with no log, or one whose
/// checks have only ever come back broken/drifted/unverifiable. A single `Held`
/// anywhere in the history clears it — a passing check verifies the fact (invariant
/// #5), so a claim with a pass is not epistemic debt, whatever else its log holds.
fn is_unverified(history: &[LogEntry]) -> bool {
    !history.iter().any(|e| {
        matches!(
            &e.event,
            claim_core::Event::Verification {
                verdict: claim_core::Verdict::Held,
                ..
            }
        )
    })
}

/// Whether the term occurs in the claim's id or statement (case-sensitive
/// substring). A plain positional argument to `list` is this quick find.
fn text_matches(claim: &Claim, term: &str) -> bool {
    claim.id.as_str().contains(term) || claim.statement.contains(term)
}

/// Parse the `--status` filter value into a [`Status`], erroring on an unknown
/// name with the valid set.
fn parse_status_filter(raw: Option<&str>) -> Result<Option<Status>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let status = match raw {
        "verified" => Status::Verified,
        "drifted" => Status::Drifted,
        "stale" => Status::Stale,
        "retired" => Status::Retired,
        other => anyhow::bail!(
            "unknown --status '{other}'; use one of: verified, drifted, stale, retired"
        ),
    };
    Ok(Some(status))
}

/// The machine form of `claim list`: a self-describing envelope, so it matches the
/// shape of `check`/`drift` (an object with `status`/`now`, not a bare array), and
/// carries the load errors and the instant statuses were computed against.
#[derive(Debug, Serialize)]
struct Inventory<'a> {
    /// Always `"ok"`: the verb ran.
    status: &'static str,
    /// The instant statuses were computed against, RFC 3339.
    now: String,
    /// The exit code (0 clean, 2 when a claim file could not be loaded), so a
    /// consumer that captured stdout need not also inspect `$?`. Matches `check`/`drift`.
    exit: i32,
    /// The matching claims.
    claims: &'a [Row],
    /// Per-file load errors (malformed or duplicate-id files); reported, not fatal.
    /// A non-empty list makes the command exit 2.
    errors: &'a [LoadError],
}

/// One inventory row: the machine form of a listed claim.
#[derive(Debug, Serialize)]
struct Row {
    /// The claim's id.
    id: String,
    /// The computed status.
    status: Status,
    /// The claim file's path relative to the store root.
    file: String,
    /// The last-verified instant as an RFC 3339 string, or `null` if never
    /// verified.
    last_verified: Option<String>,
    /// Whole days until the claim goes stale, for a still-fresh (`verified`)
    /// claim. `null` when there is no finite future deadline to report: an
    /// already-overdue claim (`stale`/`drifted`, whose window has passed), a
    /// retired claim, or one never verified. Core reports the deadline only while
    /// the claim is fresh, so this is non-negative or absent — an overdue claim is
    /// named by its `stale`/`drifted` status, not by a negative countdown.
    stale_in_days: Option<i64>,
    /// How many `supports` targets the claim declares.
    supports: usize,
    /// How many of the claim's checks declare a skip. Surfaced so a claim kept green
    /// only by a skip never *looks* silently healthy — a non-zero count marks it as
    /// deliberately parked, and lets a reviewer audit skips across the corpus.
    skips: usize,
    /// Whether the claim needs attention now (stale or drifted).
    due: bool,
}

impl Row {
    fn new(loaded: &LoadedClaim, report: &StatusReport, now: Timestamp) -> Self {
        Row {
            id: loaded.claim.id.to_string(),
            status: report.status,
            file: loaded.path.clone(),
            last_verified: report.last_verified.map(|t| t.to_string()),
            stale_in_days: report.stale_at.map(|at| whole_days_between(now, at)),
            supports: loaded.claim.supports.len(),
            skips: loaded
                .claim
                .checks
                .iter()
                .filter(|c| c.skip.is_some())
                .count(),
            due: report.due,
        }
    }
}

/// Whole days from `now` to `at`, truncated toward zero. Positive when `at` is in
/// the future (the "stale in N days" countdown); it is only ever called with a
/// future `stale_at`, since core reports no deadline once a claim is overdue.
fn whole_days_between(now: Timestamp, at: Timestamp) -> i64 {
    at.duration_since(now).as_secs() / 86_400
}

/// The table's fixed column headers, in order.
const HEADERS: [&str; 5] = ["ID", "STATUS", "LAST-VERIFIED", "STALE-IN", "SUPPORTS"];

/// Print the inventory as an aligned table (and any load errors), or a friendly
/// note when empty.
///
/// Every column's width is `max(header width, widest cell)`, so the header row and
/// the data rows always line up regardless of the longest id or a header longer
/// than its data — the earlier hardcoded widths drifted 1–2 chars under
/// `LAST-VERIFIED`/`STALE-IN`.
fn human(rows: &[Row], errors: &[LoadError]) {
    if rows.is_empty() {
        println!("No claims match.");
    } else {
        let cells: Vec<[String; 5]> = rows.iter().map(row_cells).collect();
        let widths = column_widths(&cells);
        print_row(&header_cells(), &widths);
        for cell in &cells {
            print_row(cell, &widths);
        }
    }

    for err in errors {
        println!("error: {}: {}", err.file, err.message);
    }
}

/// The header cells as owned strings, so they share the printing path with data.
fn header_cells() -> [String; 5] {
    HEADERS.map(ToOwned::to_owned)
}

/// The five display cells for one row, in header order.
fn row_cells(row: &Row) -> [String; 5] {
    // A claim carrying a skip is marked in its status cell so it reads as parked, not
    // silently healthy — the same fact the `skips` count carries in `--json`.
    let status = if row.skips > 0 {
        format!("{} +skip", status_label(row.status))
    } else {
        status_label(row.status).to_owned()
    };
    [
        row.id.clone(),
        status,
        last_verified_cell(row),
        stale_in_cell(row),
        row.supports.to_string(),
    ]
}

/// The width of each column: the widest of its header and every cell.
fn column_widths(cells: &[[String; 5]]) -> [usize; 5] {
    let mut widths = HEADERS.map(str::len);
    for row in cells {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    widths
}

/// Print one row left-aligned to `widths`, two spaces between columns. The last
/// column is not padded (nothing follows it).
fn print_row(cells: &[String; 5], widths: &[usize; 5]) {
    let mut line = String::new();
    for (i, (cell, w)) in cells.iter().zip(widths).enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        if i == cells.len() - 1 {
            line.push_str(cell);
        } else {
            line.push_str(&format!("{cell:<w$}"));
        }
    }
    println!("{line}");
}

/// The last-verified cell: a `YYYY-MM-DD` date, or `never`.
fn last_verified_cell(row: &Row) -> String {
    match &row.last_verified {
        // Show the date portion only; the full instant is in `--json`.
        Some(ts) => ts.split('T').next().unwrap_or(ts).to_owned(),
        None => "never".to_owned(),
    }
}

/// The stale-in cell: `Nd` for a fresh claim's countdown, `—` when there is no
/// future deadline (already overdue, retired, or never verified — the status
/// column carries that news).
fn stale_in_cell(row: &Row) -> String {
    match row.stale_in_days {
        Some(days) => format!("{days}d"),
        None => "—".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_filter_rejects_an_unknown_status() {
        assert!(parse_status_filter(Some("bogus")).is_err());
        assert_eq!(
            parse_status_filter(Some("drifted")).unwrap(),
            Some(Status::Drifted)
        );
        assert_eq!(parse_status_filter(None).unwrap(), None);
    }

    fn held(evidence: Option<&str>) -> LogEntry {
        LogEntry {
            at: "2026-07-17T00:00:00Z".parse().unwrap(),
            commit: "c".to_owned(),
            actor: "a".to_owned(),
            event: claim_core::Event::Verification {
                verdict: claim_core::Verdict::Held,
                evidence: evidence.map(ToOwned::to_owned),
            },
        }
    }

    fn broken() -> LogEntry {
        LogEntry {
            at: "2026-07-17T00:00:00Z".parse().unwrap(),
            commit: "c".to_owned(),
            actor: "a".to_owned(),
            event: claim_core::Event::Verification {
                verdict: claim_core::Verdict::Broken,
                evidence: None,
            },
        }
    }

    #[test]
    fn is_unverified_true_for_empty_history() {
        assert!(is_unverified(&[]));
    }

    #[test]
    fn is_unverified_true_when_only_broken() {
        assert!(is_unverified(&[broken()]));
    }

    #[test]
    fn is_unverified_false_for_any_passing_hold() {
        // A single Held clears the debt regardless of its evidence: a passing check
        // verifies the fact (invariant #5). There is no evidence marker that turns a
        // pass back into debt.
        assert!(!is_unverified(&[held(None)]));
        assert!(!is_unverified(&[held(Some(
            "witnessed-red: observed Drifted"
        ))]));
    }

    #[test]
    fn is_unverified_true_when_broken_precedes_no_hold() {
        // Broken then nothing else: still no pass on record, so still debt.
        assert!(is_unverified(&[broken()]));
    }
}
