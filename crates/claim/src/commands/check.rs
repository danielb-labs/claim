//! `claim check`: run claims' checks, record their verdicts, and report.
//!
//! This is the verify loop — the verb that turns a schedule and a pile of checks
//! into fresh verdicts. It selects claims (`--all` or the default `--due`), runs
//! each selected claim's checks through [`claim_core::run_check`], and — unless
//! `--report-only` — appends each verdict to the log with a git-resolved commit
//! and actor.
//!
//! Two honesty properties are load-bearing and tested:
//!
//! - **`--report-only` writes nothing.** It runs and reports and sets the exit
//!   code, but never touches the verdict log or resolves git provenance. This is
//!   the untrusted-runner mode (PRODUCT.md section 3: a fork PR's CI reports in its
//!   output; trusted runs persist). The write path is only ever entered when
//!   `--report-only` is absent.
//! - **Every non-passing outcome pushes the exit code up, never down.** A drifted
//!   or unverifiable verdict, or an unresolved support, is exit 1; a broken check
//!   is exit 2; the highest applicable code wins. An `agent`/`human` check returns
//!   [`claim_core::Verdict::Unverifiable`] in v1 (core does this), which is exit 1
//!   — it never fakes a pass.
//!
//! # Exit codes
//!
//! - `0` — every check held and every support resolved.
//! - `1` — at least one drifted or unverifiable verdict, or an unresolved support.
//! - `2` — at least one broken check (or a tool error, surfaced as an `Err`).
//!
//! Highest wins: a store with one held, one drifted, and one broken check exits
//! `2`. Callers script on this (CI gates on non-zero, a report-only PR run gates
//! on `2` for "broken" versus `1` for "review").

use anyhow::{Context, Result};
use claim_core::{
    append_entry, read_entries, resolve_supports, run_check, CheckContext, CheckOutcome, ClaimId,
    Event, LogEntry, SupportResolution, Timestamp, Trigger, Verdict,
};
use serde::Serialize;

use crate::cli::CheckArgs;
use crate::git;
use crate::output::{emit, verdict_label, Format};
use crate::scheduling::is_due;
use crate::store::{discover, LoadedClaim, Store};

/// The exit code when every check held and every support resolved.
const EXIT_OK: i32 = 0;
/// The exit code for a review-worthy condition: a drift, an unverifiable verdict,
/// or an unresolved support.
const EXIT_REVIEW: i32 = 1;
/// The exit code for a broken check (a tool error is reported as `Err`, which
/// `main` also maps to this value).
const EXIT_BROKEN: i32 = 2;

/// Run `claim check`. See the module docs for selection and the exit-code
/// contract.
///
/// # Errors
///
/// Fails (exit 2) when no store is found, a claim file cannot be parsed, git
/// provenance cannot be resolved for a persisting run, or a log entry cannot be
/// written. A drift, an unverifiable verdict, a broken check, or an unresolved
/// support is *not* an error — those are the verb's normal findings and set the
/// returned exit code.
pub fn run(args: &CheckArgs, format: Format) -> Result<i32> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let store = discover(&cwd)?;
    let claims = store.load_all()?;
    let known_ids: Vec<ClaimId> = claims.iter().map(|c| c.claim.id.clone()).collect();

    // One clock for the whole run: every due decision and every appended verdict
    // shares this instant, so a slow store cannot have its later claims judged
    // against a drifted `now`.
    let now = crate::clock::now()?;

    // Select up front so provenance is resolved only when something will actually
    // be written: a persisting run with nothing selected (an empty store, or
    // `--due` with nothing due) must not fail for a missing git identity it will
    // never use.
    let selected: Vec<&LoadedClaim> = claims
        .iter()
        .filter(|loaded| {
            if args.all {
                return true;
            }
            // A read failure here is a real fault, but the loop cannot return an
            // error; defer it by re-reading inside `check_one`. In practice
            // `load_all` already proved the store readable, so a per-claim log read
            // failing is an I/O race, surfaced when the claim is checked.
            match read_entries(&store.log_dir(), &loaded.claim.id) {
                Ok(history) => is_due(&loaded.claim, &history, now),
                Err(_) => true,
            }
        })
        .collect();

    // Provenance is resolved once, up front, and only for a persisting run that has
    // work to do. `Some` means "persist with this commit/actor"; `None` means
    // report-only (or nothing to write), and the write path is then unreachable.
    let provenance = if args.report_only || selected.is_empty() {
        None
    } else {
        Some(Provenance {
            commit: git::resolve_commit(store.root())?,
            actor: git::resolve_actor(store.root())?,
        })
    };

    let run = RunContext {
        store: &store,
        ctx: CheckContext::new(store.root()),
        known_ids: &known_ids,
        provenance,
        now,
    };

    let mut results = Vec::with_capacity(selected.len());
    for loaded in selected {
        results.push(check_one(&run, loaded)?);
    }

    let exit = overall_exit(&results);
    report(format, &results, args, exit);
    Ok(exit)
}

/// The run-wide inputs every claim's check shares, resolved once.
///
/// `provenance` carries the persistence decision *structurally*: `Some` means the
/// run persists (with this commit and actor), `None` means `--report-only`. The
/// only code that writes to the log matches on this being `Some`, so a report-only
/// run cannot write — there is no provenance to write with. This is the
/// no-side-effect guarantee expressed as a type, not a flag a later edit could
/// forget to check.
struct RunContext<'a> {
    store: &'a Store,
    ctx: CheckContext,
    known_ids: &'a [ClaimId],
    provenance: Option<Provenance>,
    now: Timestamp,
}

/// The git-derived provenance stamped on each persisted verdict, resolved once.
struct Provenance {
    commit: String,
    actor: String,
}

/// One check's verdict within a claim's result.
#[derive(Debug, Serialize)]
struct CheckResult {
    /// The verdict the check reported.
    verdict: Verdict,
    /// The human one-liner describing how the process ended (`exit 0`, `exit 127`,
    /// `timed out after 60s`), so a broken verdict says why it broke.
    detail: String,
    /// The evidence the check recorded, if any.
    evidence: Option<String>,
    /// The claim's trigger for this check, so the report shows the cadence.
    when: String,
}

/// One resolved (or unresolved) `supports` target within a claim's result.
#[derive(Debug, Serialize)]
struct SupportResult {
    /// The target as written in the claim's `supports` list.
    target: String,
    /// Whether it still resolves against the current tree and store.
    resolved: bool,
    /// When unresolved, why. `None` when resolved.
    reason: Option<String>,
}

/// The result of checking one claim: its checks' verdicts, its supports'
/// resolutions, and whether the verdicts were persisted.
#[derive(Debug, Serialize)]
struct ClaimResult {
    /// The claim's id.
    id: String,
    /// The claim file's path relative to the store root.
    file: String,
    /// Each check's verdict, in the claim's declared order.
    checks: Vec<CheckResult>,
    /// Each `supports` target's resolution.
    supports: Vec<SupportResult>,
    /// Whether this claim's verdicts were written to the log (`false` under
    /// `--report-only`).
    persisted: bool,
    /// The per-claim exit contribution: the highest code any of its checks or
    /// supports produced. Surfaced so a consumer can see which claim drove the
    /// overall code.
    exit: i32,
}

/// Check one claim: run each of its checks, resolve its supports, persist the
/// verdicts unless the run is report-only, and classify the outcome into an exit
/// contribution.
fn check_one(run: &RunContext, loaded: &LoadedClaim) -> Result<ClaimResult> {
    let claim = &loaded.claim;

    let mut checks = Vec::with_capacity(claim.checks.len());
    for check in &claim.checks {
        let outcome = run_check(check, &run.ctx);

        // Persist the verdict only on a persisting run. This is the ONLY place the
        // log is touched, and it is reached only when `provenance` is `Some` — which
        // it is exactly when the run is not `--report-only`. A report-only run has
        // no provenance, so it structurally cannot write here.
        if let Some(provenance) = &run.provenance {
            let entry = verification_entry(run.now, provenance, &outcome);
            append_entry(&run.store.log_dir(), &claim.id, &entry)
                .with_context(|| format!("failed to record the verdict for '{}'", claim.id))?;
        }

        checks.push(CheckResult {
            verdict: outcome.verdict,
            detail: outcome.status(),
            evidence: outcome.evidence.clone(),
            when: trigger_label(check.when),
        });
    }

    let supports = resolve_supports(&claim.supports, run.store.root(), run.known_ids)
        .into_iter()
        .map(SupportResult::from)
        .collect::<Vec<_>>();

    let exit = claim_exit(&checks, &supports);

    Ok(ClaimResult {
        id: claim.id.to_string(),
        file: loaded.path.clone(),
        checks,
        supports,
        persisted: run.provenance.is_some(),
        exit,
    })
}

impl From<SupportResolution> for SupportResult {
    fn from(r: SupportResolution) -> Self {
        SupportResult {
            target: r.target,
            resolved: r.resolved,
            reason: r.reason,
        }
    }
}

/// Build a verification log entry from a check outcome and resolved provenance.
fn verification_entry(at: Timestamp, provenance: &Provenance, outcome: &CheckOutcome) -> LogEntry {
    LogEntry {
        at,
        commit: provenance.commit.clone(),
        actor: provenance.actor.clone(),
        event: Event::Verification {
            verdict: outcome.verdict,
            evidence: outcome.evidence.clone(),
        },
    }
}

/// The exit contribution of one verdict, under the check exit-code contract.
///
/// The single mapping from a verdict to a code, so every consumer agrees. A total
/// `match` (no wildcard) so a future verdict variant forces a decision here rather
/// than defaulting to a pass — the same discipline `classify_exit` uses in core.
fn verdict_exit(verdict: Verdict) -> i32 {
    match verdict {
        Verdict::Held => EXIT_OK,
        // A drift or an inconclusive answer is review-worthy but not a tooling
        // failure. Unverifiable (agent/human checks in v1) contributes exit 1 — it
        // never fakes a pass.
        Verdict::Drifted | Verdict::Unverifiable => EXIT_REVIEW,
        // A broken check is the loudest condition: the check could not run.
        Verdict::Broken => EXIT_BROKEN,
    }
}

/// The exit contribution of one claim: the highest code across its checks and its
/// supports. An unresolved support is exit 1 (review-worthy), like a drift.
fn claim_exit(checks: &[CheckResult], supports: &[SupportResult]) -> i32 {
    let from_checks = checks.iter().map(|c| verdict_exit(c.verdict)).max();
    let from_supports = supports
        .iter()
        .filter(|s| !s.resolved)
        .map(|_| EXIT_REVIEW)
        .max();
    from_checks
        .into_iter()
        .chain(from_supports)
        .max()
        .unwrap_or(EXIT_OK)
}

/// The overall exit code: the highest any claim produced (2 > 1 > 0). A run that
/// checked nothing exits 0 — there was nothing review-worthy to find.
fn overall_exit(results: &[ClaimResult]) -> i32 {
    results.iter().map(|r| r.exit).max().unwrap_or(EXIT_OK)
}

/// A short label for a trigger, for the report.
fn trigger_label(when: Trigger) -> String {
    match when {
        Trigger::OnChange => "on-change".to_owned(),
        Trigger::Every { days } => format!("every {days}d"),
    }
}

/// Emit the check report: a JSON object in `--json` mode, an aligned human summary
/// otherwise.
fn report(format: Format, results: &[ClaimResult], args: &CheckArgs, exit: i32) {
    let selection = if args.all { "all" } else { "due" };
    let report = CheckReport {
        status: "ok",
        selection,
        report_only: args.report_only,
        exit,
        checked: results.len(),
        claims: results,
    };

    emit(format, &report, || human(results, args, exit)).unwrap_or_else(|e| {
        // A failure to *write output* is a real fault, but the checks already ran
        // and (if persisting) were recorded; surface it on stderr rather than
        // discarding the exit code the caller scripts on.
        eprintln!("error: failed to write the check report: {e}");
    });
}

/// The machine form of `claim check`.
#[derive(Debug, Serialize)]
struct CheckReport<'a> {
    /// Always `"ok"`: the verb ran. The findings are in `exit` and the per-claim
    /// results, not this field — a drift is a successful run that found a drift.
    status: &'static str,
    /// `"all"` or `"due"`, the selection that produced these results.
    selection: &'static str,
    /// Whether this run persisted nothing (`--report-only`).
    report_only: bool,
    /// The overall exit code (0/1/2), duplicated in the process exit so a consumer
    /// that captured stdout need not also inspect `$?`.
    exit: i32,
    /// How many claims were checked (after selection).
    checked: usize,
    /// The per-claim results.
    claims: &'a [ClaimResult],
}

/// Print the human summary: one block per claim, then a one-line tally.
fn human(results: &[ClaimResult], args: &CheckArgs, exit: i32) {
    if results.is_empty() {
        let scope = if args.all { "" } else { " due" };
        println!("No{scope} claims to check.");
        return;
    }

    for result in results {
        println!("{}  ({})", result.id, result.file);
        for check in &result.checks {
            println!(
                "  {:<12} {} [{}]",
                verdict_label(check.verdict),
                check.detail,
                check.when
            );
            if let Some(ev) = &check.evidence {
                if let Some(line) = first_evidence_line(ev) {
                    println!("      | {line}");
                }
            }
        }
        for support in &result.supports {
            if !support.resolved {
                println!(
                    "  UNRESOLVED support {}: {}",
                    support.target,
                    support.reason.as_deref().unwrap_or("no longer resolves")
                );
            }
        }
        if !result.persisted {
            println!("  (report-only: not recorded)");
        }
    }

    println!();
    println!(
        "Checked {} claim(s). Exit {}: {}.",
        results.len(),
        exit,
        exit_meaning(exit)
    );
}

/// The check's evidence one-liner: the first non-empty line, so a human summary
/// stays one line per check while the full evidence is preserved in the log and
/// the `--json` output.
fn first_evidence_line(evidence: &str) -> Option<String> {
    evidence
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

/// A one-clause gloss of an exit code for the human tally.
fn exit_meaning(exit: i32) -> &'static str {
    match exit {
        EXIT_OK => "all held, all supports resolved",
        EXIT_REVIEW => "review needed (drift, unverifiable, or unresolved support)",
        _ => "broken check",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held() -> CheckResult {
        CheckResult {
            verdict: Verdict::Held,
            detail: "exit 0".to_owned(),
            evidence: None,
            when: "on-change".to_owned(),
        }
    }

    fn with(verdict: Verdict) -> CheckResult {
        CheckResult {
            verdict,
            detail: "x".to_owned(),
            evidence: None,
            when: "on-change".to_owned(),
        }
    }

    fn resolved() -> SupportResult {
        SupportResult {
            target: "t".to_owned(),
            resolved: true,
            reason: None,
        }
    }

    fn unresolved() -> SupportResult {
        SupportResult {
            target: "t".to_owned(),
            resolved: false,
            reason: Some("gone".to_owned()),
        }
    }

    #[test]
    fn all_held_and_all_resolved_is_zero() {
        assert_eq!(claim_exit(&[held(), held()], &[resolved()]), EXIT_OK);
    }

    #[test]
    fn a_drift_is_exit_one() {
        assert_eq!(
            claim_exit(&[held(), with(Verdict::Drifted)], &[]),
            EXIT_REVIEW
        );
    }

    #[test]
    fn an_unverifiable_is_exit_one_never_a_pass() {
        assert_eq!(claim_exit(&[with(Verdict::Unverifiable)], &[]), EXIT_REVIEW);
    }

    #[test]
    fn a_broken_check_is_exit_two() {
        assert_eq!(
            claim_exit(&[held(), with(Verdict::Broken)], &[]),
            EXIT_BROKEN
        );
    }

    #[test]
    fn an_unresolved_support_is_exit_one_even_when_checks_hold() {
        assert_eq!(claim_exit(&[held()], &[unresolved()]), EXIT_REVIEW);
    }

    #[test]
    fn broken_beats_drift_beats_hold_within_a_claim() {
        // A claim with a held, a drifted, and a broken check exits 2: highest wins.
        assert_eq!(
            claim_exit(
                &[held(), with(Verdict::Drifted), with(Verdict::Broken)],
                &[unresolved()]
            ),
            EXIT_BROKEN
        );
    }

    #[test]
    fn overall_is_the_highest_across_claims() {
        let mk = |exit: i32| ClaimResult {
            id: "c".to_owned(),
            file: "f".to_owned(),
            checks: vec![],
            supports: vec![],
            persisted: true,
            exit,
        };
        // A mixed store: 0, 1, 2 -> 2.
        assert_eq!(overall_exit(&[mk(0), mk(1), mk(2)]), EXIT_BROKEN);
        // 0 and 1 -> 1.
        assert_eq!(overall_exit(&[mk(0), mk(1)]), EXIT_REVIEW);
        // All clean -> 0.
        assert_eq!(overall_exit(&[mk(0), mk(0)]), EXIT_OK);
        // Nothing checked -> 0.
        assert_eq!(overall_exit(&[]), EXIT_OK);
    }
}
