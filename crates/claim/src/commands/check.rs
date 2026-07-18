//! `claim check`: run every claim's checks and report their verdicts.
//!
//! This is the runtime verifier — the verb that turns a pile of checks into
//! `held`/`drifted`/`broken` right now. It runs every claim's checks through
//! [`claim_core::run_check`] and reports the outcome (human, or `--json`). It
//! stores nothing: a verdict is telemetry a hub ingests, not source (see
//! `docs/design/CLI-HUB-BOUNDARY.md`). The `--json` output *is* the interface a
//! hub, a CI lane, or a person consumes.
//!
//! Two honesty properties are load-bearing and tested:
//!
//! - **Every non-passing outcome pushes the exit code up, never down.** A drifted
//!   or unverifiable verdict, or an unresolved support, is exit 1; a broken check
//!   is exit 2; the highest applicable code wins. A `human` check, and an `agent`
//!   check with no runner configured, returns
//!   [`claim_core::Verdict::Unverifiable`] (core does this), which is exit 1 — it
//!   never fakes a pass.
//! - **Agent execution is opt-in.** An `agent` check runs only when
//!   [`claim_store::CLAIM_AGENT_CMD_ENV`] is set to a runner command (resolved by
//!   [`claim_store::agent_runner_from_env`]); the runner's structured output maps to a
//!   verdict under the same broken-never-passes contract as `cmd`. With the variable
//!   unset
//!   (the default) no runner is attached, so agent checks stay `Unverifiable` and no
//!   subprocess — and no model — is ever invoked.
//!
//! # Exit codes
//!
//! - `0` — every check held and every support resolved.
//! - `1` — at least one drifted or unverifiable verdict, or an unresolved support.
//! - `2` — at least one broken check (or a tool error, surfaced as an `Err`).
//!
//! Highest wins: a store with one held, one drifted, and one broken check exits
//! `2`. Callers script on this (CI gates on non-zero, a report run gates on `2` for
//! "broken" versus `1` for "review").

use anyhow::{Context, Result};
use claim_core::{
    evaluate_skip, resolve_supports, run_check, CheckContext, ClaimId, ProcessEnd, SkipDecision,
    SupportResolution, Verdict,
};
use serde::Serialize;

use crate::cli::CheckArgs;
use crate::output::{emit, verdict_label, Format};
use claim_store::{agent_runner_from_env, discover, LoadError, LoadedClaim, Store};

/// The exit code when every check held and every support resolved.
const EXIT_OK: i32 = 0;
/// The exit code for a review-worthy condition: a drift, an unverifiable verdict,
/// or an unresolved support.
const EXIT_REVIEW: i32 = 1;
/// The exit code for a broken check (a tool error is reported as `Err`, which
/// `main` also maps to this value).
const EXIT_BROKEN: i32 = 2;

/// Run `claim check`: run every claim's checks and report. See the module docs for
/// the exit-code contract.
///
/// # Errors
///
/// Fails (exit 2) when no store is found or a claim file cannot be parsed. A drift,
/// an unverifiable verdict, a broken check, or an unresolved support is *not* an
/// error — those are the verb's normal findings and set the returned exit code.
pub fn run(_args: &CheckArgs, format: Format) -> Result<i32> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let store = discover(&cwd)?;
    let load = store.load_all()?;
    let known_ids: Vec<ClaimId> = load.claims.iter().map(|c| c.claim.id.clone()).collect();

    // One clock for the whole run, read once, so every skip's `until` is evaluated
    // against the same instant.
    let now = claim_core::Timestamp::now();

    // Agent execution is strictly opt-in: only a set CLAIM_AGENT_CMD attaches a
    // runner. With it unset, every agent check is Unverifiable and nothing is
    // spawned, so a default `claim check` never reaches a model.
    let agent_runner = agent_runner_from_env().map_err(anyhow::Error::new)?;
    let run = RunContext {
        store: &store,
        ctx: CheckContext::new(store.root()).with_agent_runner(agent_runner),
        known_ids: &known_ids,
        now,
    };

    let mut results = Vec::with_capacity(load.claims.len());
    for loaded in &load.claims {
        results.push(check_one(&run, loaded));
    }

    // A load error (a malformed sibling, a duplicate id) is a review-worthy fault
    // that must not be masked by otherwise-clean checks: it floors the exit at 2 and
    // is reported alongside the results.
    let has_faults = !load.errors.is_empty();
    let exit = overall_exit(&results).max(if has_faults { EXIT_BROKEN } else { EXIT_OK });
    report(format, &results, &load.errors, exit);
    Ok(exit)
}

/// The run-wide inputs every claim's check shares, resolved once.
struct RunContext<'a> {
    store: &'a Store,
    ctx: CheckContext,
    known_ids: &'a [ClaimId],
    now: claim_core::Timestamp,
}

/// One check's verdict within a claim's result.
#[derive(Debug, Serialize)]
struct CheckResult {
    /// The verdict the check reported.
    verdict: Verdict,
    /// The structured process end: `exited` with a code, `timed-out`, `signalled`,
    /// `spawn-failed`, `not-executed`. An agent branches on this tagged structure
    /// rather than parsing the English `detail` string — e.g. "every timeout" is
    /// `end.kind == "timed-out"`, not a substring hunt.
    end: ProcessEnd,
    /// The human one-liner describing how the process ended (`exit 0`, `exit 127`,
    /// `timed out after 60s`), so a broken verdict reads plainly. Derived from
    /// `end`; the structured form is authoritative.
    detail: String,
    /// The evidence the check recorded, if any.
    evidence: Option<String>,
    /// Why a declared skip did *not* apply this run, when that is worth reporting: a
    /// lapsed `until`, or an `unless` condition that could not be evaluated (so the
    /// check ran rather than being silently muted). `None` on an ordinary run.
    note: Option<String>,
}

/// A check whose declared skip suppressed this run.
///
/// Reported so a skip is never silent, and carries no verdict: a skip is not a pass.
#[derive(Debug, Serialize)]
struct SkippedCheck {
    /// The author's justification, from the claim's `skip.reason`.
    reason: String,
    /// The skip's expiry, if it declared one (RFC 3339). `None` for an indefinite
    /// skip — surfaced plainly so an unbounded mute cannot hide.
    until: Option<String>,
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

/// The result of checking one claim: its checks' verdicts and its supports'
/// resolutions.
#[derive(Debug, Serialize)]
struct ClaimResult {
    /// The claim's id.
    id: String,
    /// The claim file's path relative to the store root.
    file: String,
    /// Each check's verdict, in the claim's declared order. A check whose skip was in
    /// force this run is *not* here — it is in [`skipped`](ClaimResult::skipped)
    /// instead, with no verdict, so it contributes nothing to the exit code.
    checks: Vec<CheckResult>,
    /// Checks whose declared skip suppressed this run: reported (never silent), never
    /// a pass, and recording no verdict.
    skipped: Vec<SkippedCheck>,
    /// Each `supports` target's resolution.
    supports: Vec<SupportResult>,
    /// The per-claim exit contribution: the highest code any of its checks or
    /// supports produced. Surfaced so a consumer can see which claim drove the
    /// overall code.
    exit: i32,
}

/// Check one claim: run each of its checks, resolve its supports, and classify the
/// outcome into an exit contribution. Reports the results; persists nothing.
fn check_one(run: &RunContext, loaded: &LoadedClaim) -> ClaimResult {
    let claim = &loaded.claim;

    let mut checks = Vec::new();
    let mut skipped = Vec::new();
    for check in &claim.checks {
        // A declared skip is evaluated *before* the check runs. When it holds, the
        // check does not run and records no verdict — a skip is never a pass.
        // `Run(note)` carries why a skip did not apply (a lapsed `until`, or an
        // `unless` that could not be evaluated), so the report can say the debt was
        // called rather than silently running.
        let note = match &check.skip {
            Some(skip) => match evaluate_skip(skip, &run.ctx, run.now) {
                SkipDecision::Skip => {
                    skipped.push(SkippedCheck {
                        reason: skip.reason.clone(),
                        until: skip.until.map(|t| t.to_string()),
                    });
                    continue;
                }
                SkipDecision::Run(note) => note,
            },
            None => None,
        };

        let outcome = run_check(check, &run.ctx);
        checks.push(CheckResult {
            verdict: outcome.verdict,
            detail: outcome.status(),
            end: outcome.end.clone(),
            evidence: outcome.evidence.clone(),
            note,
        });
    }

    let supports = resolve_supports(&claim.supports, run.store.root(), run.known_ids)
        .into_iter()
        .map(SupportResult::from)
        .collect::<Vec<_>>();

    let exit = claim_exit(&checks, &supports);

    ClaimResult {
        id: claim.id.to_string(),
        file: loaded.path.clone(),
        checks,
        skipped,
        supports,
        exit,
    }
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

/// Emit the check report: a JSON object in `--json` mode, an aligned human summary
/// otherwise. `load_errors` are per-file load faults (malformed sibling, duplicate
/// id) that floor the exit at 2 and are surfaced so a broken file nags without
/// denying the store's whole report.
fn report(format: Format, results: &[ClaimResult], load_errors: &[LoadError], exit: i32) {
    let report = CheckReport {
        status: "ok",
        exit,
        checked: results.len(),
        claims: results,
        errors: load_errors,
    };

    emit(format, &report, || {
        human(results, load_errors, exit);
    })
    .unwrap_or_else(|e| {
        // A failure to *write output* is a real fault, but the checks already ran;
        // surface it on stderr rather than discarding the exit code the caller
        // scripts on.
        eprintln!("error: failed to write the check report: {e}");
    });
}

/// The machine form of `claim check`.
#[derive(Debug, Serialize)]
struct CheckReport<'a> {
    /// Always `"ok"`: the verb ran. The findings are in `exit` and the per-claim
    /// results, not this field — a drift is a successful run that found a drift.
    status: &'static str,
    /// The overall exit code (0/1/2), duplicated in the process exit so a consumer
    /// that captured stdout need not also inspect `$?`.
    exit: i32,
    /// How many claims were checked.
    checked: usize,
    /// The per-claim results.
    claims: &'a [ClaimResult],
    /// Per-file load errors (a malformed claim file, a duplicate id): reported, not
    /// fatal, so the good claims above still ran. A non-empty list floors `exit` at
    /// 2.
    errors: &'a [LoadError],
}

/// Print the human summary: one block per claim, the load faults, then a one-line
/// tally.
fn human(results: &[ClaimResult], load_errors: &[LoadError], exit: i32) {
    if results.is_empty() && load_errors.is_empty() {
        println!("No claims to check.");
        return;
    }

    for result in results {
        println!("{}  ({})", result.id, result.file);
        for check in &result.checks {
            println!("  {:<12} {}", verdict_label(check.verdict), check.detail);
            if let Some(note) = &check.note {
                println!("      | {note}");
            }
            if let Some(ev) = &check.evidence {
                if let Some(line) = first_evidence_line(ev) {
                    println!("      | {line}");
                }
            }
        }
        for skip in &result.skipped {
            let bound = match &skip.until {
                Some(until) => format!("until {until}"),
                None => "no expiry".to_owned(),
            };
            println!("  {:<12} ({bound})", "skipped");
            println!("      | {}", skip.reason);
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
    }

    for err in load_errors {
        println!("error: {}: {}", err.file, err.message);
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
/// stays one line per check while the full evidence is preserved in the `--json`
/// output.
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
        _ => "a broken check or an unloadable claim file",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held() -> CheckResult {
        CheckResult {
            verdict: Verdict::Held,
            end: ProcessEnd::Exited { code: 0 },
            detail: "exit 0".to_owned(),
            evidence: None,
            note: None,
        }
    }

    fn with(verdict: Verdict) -> CheckResult {
        CheckResult {
            verdict,
            end: ProcessEnd::Exited { code: 0 },
            detail: "x".to_owned(),
            evidence: None,
            note: None,
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
            skipped: vec![],
            supports: vec![],
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
