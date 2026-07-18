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
use crate::store::{discover, LoadedClaim};

/// Run `claim list`.
///
/// # Errors
///
/// Fails when no store is found, a claim file cannot be parsed, or a verdict log
/// cannot be read.
pub fn run(args: &ListArgs, format: Format) -> Result<()> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let store = discover(&cwd)?;
    let claims = store.load_all()?;

    let now = crate::clock::now()?;
    let status_filter = parse_status_filter(args.status.as_deref())?;

    let mut rows = Vec::new();
    for loaded in &claims {
        // One read of the log per claim: the status computation and the
        // `--unverified` filter both derive from the same history.
        let history = read_entries(&store.log_dir(), &loaded.claim.id)?;
        let report = compute_status(loaded.claim.max_age, &history, now, Grace::DEFAULT);
        if keep(args, loaded, &report, status_filter, &history) {
            rows.push(Row::new(loaded, &report, now));
        }
    }

    emit(format, &rows, || human(&rows))
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

    // `--stale` is the "needs attention" shortcut: anything past its window and
    // wanting a look. `report.due` is true for both Stale and Drifted, which is
    // exactly the overdue set, and false for Verified and Retired.
    if args.stale && !report.due {
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

/// Whether an entry's evidence marks it as an unwitnessed establishment.
///
/// `claim add --unwitnessed` records the birth `Held` with an evidence note
/// beginning `unwitnessed:` (see `commands::add`). Matching that marker surfaces
/// the acknowledged debt without a new schema field — the debt lives in the log,
/// where it is committed and auditable, not in the definition file.
fn evidence_admits_unwitnessed(entry: &LogEntry) -> bool {
    matches!(
        &entry.event,
        claim_core::Event::Verification {
            evidence: Some(note),
            ..
        } if note.contains("unwitnessed")
    )
}

/// Whether a claim's file path or any watched path lies under `prefix`.
///
/// "Watched paths" are best-effort: v1 does not trace a check's read-set, so the
/// paths a claim is *about* are approximated by its `supports` targets (a decision
/// ref names a file) plus the claim file's own location. This catches the common
/// "claims under src/payments/" query without read-set tracing, which is deferred.
fn path_matches(loaded: &LoadedClaim, prefix: &str) -> bool {
    if under_prefix(&loaded.path, prefix) {
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

/// Print the inventory as an aligned table, or a friendly note when empty.
fn human(rows: &[Row]) {
    if rows.is_empty() {
        println!("No claims match.");
        return;
    }

    let id_w = rows
        .iter()
        .map(|r| r.id.len())
        .chain(std::iter::once("ID".len()))
        .max()
        .unwrap_or(2);
    let status_w = "unverifiable".len();

    println!(
        "{:<id_w$}  {:<status_w$}  {:<12}  {:>7}  SUPPORTS",
        "ID",
        "STATUS",
        "LAST-VERIFIED",
        "STALE-IN",
        id_w = id_w,
        status_w = status_w,
    );
    for row in rows {
        println!(
            "{:<id_w$}  {:<status_w$}  {:<12}  {:>7}  {}",
            row.id,
            status_label(row.status),
            last_verified_cell(row),
            stale_in_cell(row),
            row.supports,
            id_w = id_w,
            status_w = status_w,
        );
    }
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
}
