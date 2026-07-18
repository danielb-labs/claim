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
//!   total count of fired `Drifted` verdicts (the false-alarm denominator, measured
//!   over individual drift events per PRODUCT.md section 9) and the total claim
//!   count (the authoring-time denominator) — and states plainly that the numerators
//!   need external input. A later item may add a `--false-alarm` marker or capture
//!   authoring time; until then, inventing either number would be exactly the
//!   confident-but-wrong answer this tool exists to prevent.
//!
//! Like `list` and `drift`, this runs no checks: it derives every status from the
//! verdict logs via [`claim_core::compute_status`] against `now`. It is purely
//! informational and always exits `0`. Every per-file fault degrades gracefully —
//! an unparseable claim file *and* a corrupt verdict log are both reported (so a
//! broken file still nags) without aborting the rollup or changing the exit: the
//! numbers describe the claims that loaded and read cleanly, and each error names a
//! file that did not.

use anyhow::{Context, Result};
use claim_core::{
    compute_status, read_entries, Event, Grace, LogEntry, Status, Timestamp, Verdict,
};
use serde::Serialize;

use crate::output::{emit, Format};
use claim_store::{discover, LoadError};

/// Run `claim stats`. Always returns `Ok(())` (exit 0); it is informational.
///
/// # Errors
///
/// Fails only when no store is found — the one fault that makes the whole corpus
/// unreadable. Every *per-claim* fault degrades gracefully instead: a malformed or
/// duplicate-id claim *file* is a reported [`LoadError`] from `load_all`, and a
/// corrupt or unreadable *verdict log* is folded into the same error channel here
/// (the claim is skipped from the tallies, its log file named), so one bad file
/// never takes down the stats surface. The rollup then describes the claims that
/// loaded and read cleanly, and reports the ones that did not.
pub fn run(_args: &crate::cli::StatsArgs, format: Format) -> Result<()> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let store = discover(&cwd)?;
    let load = store.load_all()?;
    let now = crate::clock::now()?;

    // Claim-file load errors carry through; verdict-log read errors are appended to
    // the same channel so a corrupt log degrades exactly like a corrupt claim file
    // (invariant #6: a broken file nags, it does not silence the store).
    let mut errors = load.errors.clone();
    let mut acc = Accumulator::default();
    for loaded in &load.claims {
        match read_entries(&store.log_dir(), &loaded.claim.id) {
            Ok(history) => {
                let report = compute_status(loaded.claim.max_age, &history, now, Grace::DEFAULT);
                acc.observe(report.status, report.last_verified, &history);
            }
            Err(e) => errors.push(LoadError {
                file: format!(".claims/log/{}/", loaded.claim.id),
                message: format!("verdict log could not be read: {e}"),
            }),
        }
    }
    errors.sort_by(|a, b| a.file.cmp(&b.file));

    let stats = acc.finish(&errors, now);
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
    /// claim is never overdue (it is terminal).
    ///
    /// "Never passed" is derived from `last_verified`, not from a raw `Held` scan of
    /// the history. That distinction is load-bearing: `last_verified` is
    /// `compute_status`'s *future-excluding* signal — a `Held` timestamped after
    /// `now` (clock skew or forgery) does not certify present freshness and does not
    /// set it. Counting never-passed from a raw "any Held in history" flag would let
    /// exactly that future-dated Held drop a claim out of `never_passed` while it
    /// reads `stale` everywhere else — leaking the forgery the status logic was
    /// hardened against into the honesty instrument. So a claim counts as
    /// never-passed precisely when it has no *past-or-present* `Held`.
    fn observe(&mut self, status: Status, last_verified: Option<Timestamp>, history: &[LogEntry]) {
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

        // The raw verdict tallies count every recorded verdict (they describe the
        // log's contents, not present freshness), so a future-dated Held is still a
        // Held here — that is honest about what was written.
        let mut ever_drifted = false;
        for entry in history {
            if let Event::Verification { verdict, .. } = &entry.event {
                match verdict {
                    Verdict::Held => self.held += 1,
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
        if last_verified.is_none() {
            self.never_passed += 1;
        }

        // The oldest last-verified instant, the staleness frontier: the claim most
        // in need of a fresh look among those still open. A never-verified claim
        // contributes nothing (it has no last-verified instant; it is counted in
        // `never_passed`). A retired claim is skipped too — it is closed, not due, so
        // its last-verified must not pull the frontier back into settled history.
        if let (Some(at), false) = (last_verified, status == Status::Retired) {
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
            // The false-alarm denominator is *fired drift events*, not claims that
            // ever drifted: PRODUCT.md section 9 measures the rate over individual
            // drifts (a flappy claim firing ten drifts is ten chances to be a false
            // alarm), and the note asks a human to classify "each fired drift".
            needs_human_input: KillMetrics::describe(self.verdict_drifted, self.total),
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
    ///
    /// `fired_drifts` is the total count of `Drifted` verdicts recorded across the
    /// corpus (individual drift events), the denominator PRODUCT.md section 9's
    /// false-alarm rate is measured over — distinct from `drifts_caught`, the count
    /// of unique claims that ever drifted, which the report carries separately.
    fn describe(fired_drifts: usize, total: usize) -> Self {
        KillMetrics {
            false_alarm_rate: UnavailableMetric {
                available: false,
                denominator: fired_drifts,
                denominator_is: "total Drifted verdicts recorded",
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
