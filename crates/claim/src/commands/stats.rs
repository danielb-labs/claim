//! `claim stats`: the pilot instrumentation (PRODUCT.md section 9).
//!
//! The day-90 verdict on v1 rests on two kill metrics — a false-alarm rate above
//! one in three fired drifts, or an authoring cost well over five minutes a claim,
//! reshapes or kills the design. Neither is mechanically derivable: whether a fired
//! drift was a *false* alarm is a human judgement, and how long a claim took to
//! author is wall-clock time the tool never observed. So this verb is scrupulous
//! about the line between what the store *records* and what a human must still
//! label:
//!
//! - **Derived and reported as fact:** total claims; the breakdown by computed
//!   status; verdict-history totals (held/drifted/unverifiable/broken across every
//!   log); "drifts caught" (claims with at least one `Drifted` in history); how many
//!   claims have never passed a check; and staleness (count overdue, and the oldest
//!   last-verified instant).
//! - **Left to human input, never fabricated:** the false-alarm rate and
//!   minutes-per-claim. The verb surfaces the denominators it *does* have — the
//!   count of claims that ever fired a drift (the false-alarm denominator) and the
//!   total claim count (the authoring-time denominator) — and states plainly that
//!   the numerators need external input. A later item may add a `--false-alarm`
//!   marker or capture authoring time; until then, inventing either number would be
//!   exactly the confident-but-wrong answer this tool exists to prevent.
//!
//! Like `list` and `drift`, this runs no checks: it derives every status from the
//! verdict logs via [`claim_core::compute_status`] against `now`. It is purely
//! informational and always exits `0`. A per-file load error is *reported* (so a
//! broken file still nags) but does not abort the rollup or change the exit — the
//! numbers describe the claims that loaded, and the error names the one that did
//! not.

use anyhow::{Context, Result};
use claim_core::{
    compute_status, read_entries, Event, Grace, LogEntry, Status, Timestamp, Verdict,
};
use serde::Serialize;

use crate::output::{emit, Format};
use crate::store::{discover, LoadError};

/// Run `claim stats`. Always returns `Ok(())` (exit 0); it is informational.
///
/// # Errors
///
/// Fails only when no store is found or a verdict log cannot be read — the same
/// hard faults every read verb shares. A malformed or duplicate-id claim file is a
/// reported [`LoadError`], not an `Err`: the rollup still describes the claims that
/// loaded.
pub fn run(_args: &crate::cli::StatsArgs, format: Format) -> Result<()> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let store = discover(&cwd)?;
    let load = store.load_all()?;
    let now = crate::clock::now()?;

    let mut acc = Accumulator::default();
    for loaded in &load.claims {
        let history = read_entries(&store.log_dir(), &loaded.claim.id)?;
        let report = compute_status(loaded.claim.max_age, &history, now, Grace::DEFAULT);
        acc.observe(&report.status, report.last_verified, &history);
    }

    let stats = acc.finish(&load.errors, now);
    emit(format, &stats, || human(&stats))
}

/// The running tallies, folded over the store's claims. Kept as a distinct struct
/// (rather than mutating the output type) so the observation logic — what counts as
/// a drift caught, an unverified claim, an overdue claim — lives in one `observe`
/// call per claim and the output type is pure data.
#[derive(Default)]
struct Accumulator {
    total: usize,
    verified: usize,
    drifted: usize,
    stale: usize,
    retired: usize,
    held: usize,
    verdict_drifted: usize,
    unverifiable: usize,
    broken: usize,
    drifts_caught: usize,
    never_passed: usize,
    overdue: usize,
    oldest_last_verified: Option<Timestamp>,
}

impl Accumulator {
    /// Fold one claim's computed status and full history into the tallies.
    ///
    /// `overdue` counts claims wanting attention now — `Stale` or `Drifted` — which
    /// is exactly the set `compute_status` marks `due`, but derived from the status
    /// here so the count and the status breakdown can never disagree. A `Retired`
    /// claim is never overdue (it is terminal). "Never passed" is the honest
    /// unverified count: no `Held` anywhere in history, whether the claim is brand
    /// new, only ever broke, or drifted without a prior pass.
    fn observe(&mut self, status: &Status, last_verified: Option<Timestamp>, history: &[LogEntry]) {
        self.total += 1;
        match status {
            Status::Verified => self.verified += 1,
            Status::Drifted => {
                self.drifted += 1;
                self.overdue += 1;
            }
            Status::Stale => {
                self.stale += 1;
                self.overdue += 1;
            }
            Status::Retired => self.retired += 1,
        }

        let mut ever_drifted = false;
        let mut ever_held = false;
        for entry in history {
            if let Event::Verification { verdict, .. } = &entry.event {
                match verdict {
                    Verdict::Held => {
                        self.held += 1;
                        ever_held = true;
                    }
                    Verdict::Drifted => {
                        self.verdict_drifted += 1;
                        ever_drifted = true;
                    }
                    Verdict::Unverifiable => self.unverifiable += 1,
                    Verdict::Broken => self.broken += 1,
                }
            }
        }
        if ever_drifted {
            self.drifts_caught += 1;
        }
        if !ever_held {
            self.never_passed += 1;
        }

        // The oldest last-verified instant across the corpus: the claim most in need
        // of a fresh look among those that ever passed. A never-verified claim
        // contributes nothing here (it has no last-verified instant); it is already
        // counted in `never_passed`.
        if let Some(at) = last_verified {
            self.oldest_last_verified = Some(match self.oldest_last_verified {
                Some(current) => current.min(at),
                None => at,
            });
        }
    }

    /// Freeze the tallies into the reportable [`Stats`], attaching the load errors
    /// and the honesty note about the metrics that need human input.
    fn finish(self, errors: &[LoadError], now: Timestamp) -> Stats<'_> {
        Stats {
            status: "ok",
            now: now.to_string(),
            total: self.total,
            by_status: StatusBreakdown {
                verified: self.verified,
                drifted: self.drifted,
                stale: self.stale,
                retired: self.retired,
            },
            verdicts: VerdictBreakdown {
                held: self.held,
                drifted: self.verdict_drifted,
                unverifiable: self.unverifiable,
                broken: self.broken,
            },
            drifts_caught: self.drifts_caught,
            never_passed: self.never_passed,
            overdue: self.overdue,
            oldest_last_verified: self.oldest_last_verified.map(|t| t.to_string()),
            needs_human_input: KillMetrics::describe(self.drifts_caught, self.total),
            errors,
        }
    }
}

/// The machine form of `claim stats`: every mechanically-derived number, plus the
/// explicit accounting of what the store cannot supply on its own.
#[derive(Debug, Serialize)]
struct Stats<'a> {
    /// Always `"ok"`: the verb ran (it is informational, never a finding).
    status: &'static str,
    /// The instant statuses were computed against, RFC 3339.
    now: String,
    /// Total claims that loaded.
    total: usize,
    /// The breakdown by computed status. The four counts sum to `total`.
    by_status: StatusBreakdown,
    /// Verdict counts across every claim's whole log (not just the latest).
    verdicts: VerdictBreakdown,
    /// Claims with at least one `Drifted` verdict anywhere in history — the drifts
    /// the system actually caught over the corpus's life. The denominator for the
    /// false-alarm rate.
    drifts_caught: usize,
    /// Claims with no `Held` verdict anywhere in history: never verified. Includes
    /// brand-new claims, only-ever-broken checks, and claims that drifted without a
    /// prior pass.
    never_passed: usize,
    /// Claims wanting attention now (computed status `stale` or `drifted`).
    overdue: usize,
    /// The oldest last-verified instant across all claims that ever passed, RFC
    /// 3339, or `null` when no claim has ever passed. The staleness frontier.
    oldest_last_verified: Option<String>,
    /// The kill metrics the store cannot derive, with the denominators it can and a
    /// statement of what human input each numerator needs. Never a fabricated rate.
    needs_human_input: KillMetrics,
    /// Per-file load errors (malformed or duplicate-id files); reported so a broken
    /// file still nags, but they do not change the exit (stats is informational).
    errors: &'a [LoadError],
}

/// The status breakdown; the four counts partition `total`.
#[derive(Debug, Serialize)]
struct StatusBreakdown {
    verified: usize,
    drifted: usize,
    stale: usize,
    retired: usize,
}

/// Verdict totals across every log entry in the store.
#[derive(Debug, Serialize)]
struct VerdictBreakdown {
    held: usize,
    drifted: usize,
    unverifiable: usize,
    broken: usize,
}

/// The two PRODUCT.md section-9 kill metrics, reported honestly: the denominators
/// the store supplies, and the human input each numerator still needs. Serialized as
/// data (not prose the caller must parse) so an agent can see the rate is *absent*,
/// not zero.
#[derive(Debug, Serialize)]
struct KillMetrics {
    /// The false-alarm rate: fabricated numerator refused. `available` is `false`
    /// because classifying a fired drift as a false alarm is a human judgement the
    /// store does not record.
    false_alarm_rate: UnavailableMetric,
    /// Minutes per claim to author: fabricated numerator refused. `available` is
    /// `false` because the tool never observed authoring wall-clock time.
    minutes_per_claim: UnavailableMetric,
}

impl KillMetrics {
    /// Build the honest kill-metric block, wiring each metric to the denominator the
    /// store *can* supply and naming the input its numerator needs.
    fn describe(drifts_caught: usize, total: usize) -> Self {
        KillMetrics {
            false_alarm_rate: UnavailableMetric {
                available: false,
                denominator: drifts_caught,
                denominator_is: "claims that fired at least one drift",
                needs:
                    "a human (or a later `--false-alarm` marker) to classify each fired drift as a \
                     real drift or a false alarm; the store does not record that judgement",
            },
            minutes_per_claim: UnavailableMetric {
                available: false,
                denominator: total,
                denominator_is: "total claims",
                needs:
                    "authoring wall-clock time, which the tool never observed; a later item may \
                        capture it at `claim add` time",
            },
        }
    }
}

/// A metric the store cannot compute, reported with its available denominator and
/// the input its numerator needs — so the number is honestly *missing*, never
/// invented as zero or a placeholder.
#[derive(Debug, Serialize)]
struct UnavailableMetric {
    /// Always `false` in v1: the numerator needs input the store lacks.
    available: bool,
    /// The denominator the store *can* supply.
    denominator: usize,
    /// What that denominator counts, in words.
    denominator_is: &'static str,
    /// The external input the numerator requires.
    needs: &'static str,
}

/// Print the rollup as a compact human report, then the honest note on the metrics
/// that need human input, then any load errors.
fn human(stats: &Stats) {
    println!("Claim store stats (computed at {})", stats.now);
    println!();
    println!("Claims: {}", stats.total);
    println!("  verified:  {}", stats.by_status.verified);
    println!("  drifted:   {}", stats.by_status.drifted);
    println!("  stale:     {}", stats.by_status.stale);
    println!("  retired:   {}", stats.by_status.retired);
    println!();
    println!("Verdicts recorded (all history):");
    println!("  held:          {}", stats.verdicts.held);
    println!("  drifted:       {}", stats.verdicts.drifted);
    println!("  unverifiable:  {}", stats.verdicts.unverifiable);
    println!("  broken:        {}", stats.verdicts.broken);
    println!();
    println!(
        "Drifts caught (claims that ever drifted): {}",
        stats.drifts_caught
    );
    println!(
        "Never verified (no passing verdict):      {}",
        stats.never_passed
    );
    println!(
        "Overdue now (stale or drifted):           {}",
        stats.overdue
    );
    match &stats.oldest_last_verified {
        Some(at) => println!("Oldest last-verified:                     {at}"),
        None => println!("Oldest last-verified:                     (no claim has ever passed)"),
    }
    println!();
    println!("Kill metrics (PRODUCT.md section 9) — need human input, not fabricated:");
    print_unavailable(
        "false-alarm rate",
        &stats.needs_human_input.false_alarm_rate,
    );
    print_unavailable(
        "minutes per claim",
        &stats.needs_human_input.minutes_per_claim,
    );

    for err in stats.errors {
        println!("error: {}: {}", err.file, err.message);
    }
}

/// Print one unavailable kill metric: its denominator and the input it still needs,
/// so a human reads why the number is absent rather than assuming it is zero.
fn print_unavailable(label: &str, metric: &UnavailableMetric) {
    println!(
        "  {label}: not available — denominator {} ({}); needs {}",
        metric.denominator, metric.denominator_is, metric.needs
    );
}
