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
use crate::store::{discover, LoadError, LoadedClaim};

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
        if !path_matches(loaded, prefix) {
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

/// Whether a claim has no passing verdict on record, or was explicitly recorded
/// unwitnessed — the soft-debt view.
///
/// "No passing verdict" is the never-witnessed / never-verified case: a claim
/// hand-committed with no log, or one whose checks have only ever come back
/// broken/drifted/unverifiable. An `--unwitnessed` add writes a `Held` but with an
/// evidence note saying the red was never seen; that note is the acknowledged
/// debt, so a claim carrying it counts as unverified even though it has a `Held`.
fn is_unverified(history: &[LogEntry]) -> bool {
    let has_passing = history.iter().any(|e| {
        matches!(
            &e.event,
            claim_core::Event::Verification {
                verdict: claim_core::Verdict::Held,
                ..
            }
        )
    });
    if !has_passing {
        return true;
    }
    history.iter().any(evidence_admits_unwitnessed)
}

/// Whether an entry's evidence is the `unwitnessed:` debt marker.
///
/// `claim add --unwitnessed` records the birth `Held` with an evidence note whose
/// first line is exactly `unwitnessed: ...` (see `commands::add::unwitnessed_note`).
/// Matching the `unwitnessed:` *prefix* — not a bare `contains("unwitnessed")` —
/// avoids flagging any claim whose evidence merely mentions the word (a human note
/// saying "this was previously unwitnessed", say). The debt lives in the log, where
/// it is committed and auditable, not in the definition file.
fn evidence_admits_unwitnessed(entry: &LogEntry) -> bool {
    matches!(
        &entry.event,
        claim_core::Event::Verification {
            evidence: Some(note),
            ..
        } if note.starts_with("unwitnessed:")
    )
}

/// Whether a claim's file path or any watched path lies under `prefix`.
///
/// The claim file's path is matched *inside* the `.claims/` store, not with the
/// store prefix: a claim at `.claims/src/a.md` matches `--path src`, because the
/// user thinks in repo paths (`src/…`), not in the store's internal layout. The
/// `.claims/` prefix is stripped before matching. A `supports` decision ref, by
/// contrast, already names a repo-relative path (`requirements.txt#libfoo`) and is
/// matched as-is.
///
/// "Watched paths" are best-effort: v1 does not trace a check's read-set, so the
/// paths a claim is *about* are approximated by its `supports` targets plus the
/// claim file's own location. This catches the common "claims under src/payments/"
/// query without read-set tracing, which is deferred.
fn path_matches(loaded: &LoadedClaim, prefix: &str) -> bool {
    let claim_path = loaded.path.strip_prefix(".claims/").unwrap_or(&loaded.path);
    if under_prefix(claim_path, prefix) {
        return true;
    }
    loaded.claim.supports.iter().any(|s| {
        // A decision ref `path#anchor` names a file in its path part; a bare claim
        // id has no path meaning, but comparing it as a path is harmless (it will
        // not match a real prefix a user would type).
        let path_part = s.as_str().split('#').next().unwrap_or(s.as_str());
        under_prefix(path_part, prefix)
    })
}

/// Whether `path` is under the directory/prefix `prefix`, by path segments.
///
/// Segment-wise (not raw substring) so `--path src` matches `src/a.md` but not
/// `srcfoo/a.md`: a prefix names a directory boundary, and a substring match would
/// wrongly pull in a sibling whose name merely starts with the same letters. A
/// prefix that exactly equals the path also matches (a claim can be named directly).
fn under_prefix(path: &str, prefix: &str) -> bool {
    let path = path.trim_start_matches("./");
    let prefix = prefix.trim_start_matches("./").trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }
    if path == prefix {
        return true;
    }
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with('/'))
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
    [
        row.id.clone(),
        status_label(row.status).to_owned(),
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
    fn under_prefix_matches_on_segment_boundaries() {
        assert!(under_prefix(".claims/src/a.md", ".claims/src"));
        assert!(under_prefix("src/a.md", "src"));
        assert!(under_prefix("src/a.md", "src/"));
        // A prefix that equals the path matches (a claim named directly).
        assert!(under_prefix("src/a.md", "src/a.md"));
        // An empty prefix matches everything.
        assert!(under_prefix("anything", ""));
        // A leading ./ on either side is normalized away.
        assert!(under_prefix("./src/a.md", "src"));
    }

    #[test]
    fn under_prefix_rejects_a_sibling_with_a_shared_name_start() {
        // The bug a raw substring match would introduce: `src` must not match
        // `srcfoo/`.
        assert!(!under_prefix("srcfoo/a.md", "src"));
        assert!(!under_prefix("other/a.md", "src"));
    }

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
    fn is_unverified_false_for_a_witnessed_hold() {
        assert!(!is_unverified(&[held(None)]));
    }

    #[test]
    fn is_unverified_true_for_an_unwitnessed_hold() {
        // An --unwitnessed add writes a Held carrying the debt marker; it is still
        // unverified debt despite the pass.
        assert!(is_unverified(&[held(Some(
            "unwitnessed: this claim was added with --unwitnessed"
        ))]));
    }

    #[test]
    fn is_unverified_ignores_evidence_that_merely_mentions_the_word() {
        // A witnessed Held whose evidence just mentions "unwitnessed" in prose is
        // NOT debt: only the `unwitnessed:` prefix marker counts (m2).
        assert!(!is_unverified(&[held(Some(
            "this claim was previously unwitnessed but is now witnessed"
        ))]));
    }

    #[test]
    fn path_matches_strips_the_claims_prefix() {
        // A claim at `.claims/src/a.md` matches `--path src`, because the user
        // thinks in repo paths, not the store's internal layout (m1).
        let claim = claim_core::parse_claim_file(
            ".claims/src/a.md",
            "---\nid: a\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: on-change\nmax-age: 30d\n---\nS.\n",
        )
        .unwrap();
        let loaded = LoadedClaim {
            claim,
            path: ".claims/src/a.md".to_owned(),
        };
        assert!(path_matches(&loaded, "src"));
        assert!(path_matches(&loaded, "src/a.md"));
        assert!(!path_matches(&loaded, "other"));
    }
}
