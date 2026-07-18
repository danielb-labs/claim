//! When a claim's checks are due to run — the scheduling decision behind
//! `claim check --due`.
//!
//! `claim check` runs either every claim (`--all`) or only those *currently due*
//! (`--due`, the default). "Due" is a pure function of a claim's triggers and its
//! verdict history against `now`, isolated here so the decision can be unit-tested
//! with pinned timestamps rather than the wall clock, and so `check` stays a thin
//! shell over one clear rule.
//!
//! The rule, per the item's design and PRODUCT.md section 5's two lanes:
//!
//! - An **`on-change`** check is *always* due. v1 runs repo-triggered checks on
//!   every invocation (the on-change lane runs every cmd check on every PR):
//!   read-set tracing that would let the tool know *which* change touched a
//!   check's files is deferred, and greps are cheap, so running them every time
//!   is both correct and simpler than guessing.
//! - An **`every Nd`** check is due iff `now - last_run >= N days`, or the claim
//!   has never been checked. `last_run` is the timestamp of the claim's most
//!   recent *past* verdict of any kind — a scheduling clock ("when did we last
//!   run"), deliberately not the last *passing* verdict: a broken or drifted check
//!   should be retried on its own cadence, not hammered every invocation, and a
//!   claim whose check keeps failing must not become permanently "due" and drown
//!   the due list.
//!
//! A claim carries several checks; it is due when *any* of them is due, because a
//! due `every 30d` agent check must not be skipped just because a sibling
//! `on-change` grep is not (though in practice any `on-change` check makes the
//! whole claim due). This is a whole-claim decision, since `check` runs a claim's
//! checks together.

use claim_core::{Claim, LogEntry, SignedDuration, Timestamp, Trigger};

/// Whether any of `claim`'s checks are due to run at `now`, given its `history`.
///
/// Pure and total: it reads no clock (the caller passes `now`) and consults only
/// the claim's triggers and its verdict log, so a test pins every input and gets a
/// deterministic answer. See the module docs for the rule.
///
/// `history` is the claim's full verdict log (as from [`claim_core::read_entries`]);
/// order does not matter — the most recent *past* verdict is found by timestamp.
#[must_use]
pub fn is_due(claim: &Claim, history: &[LogEntry], now: Timestamp) -> bool {
    let last_run = last_verdict_at(history, now);
    claim
        .checks
        .iter()
        .any(|check| trigger_is_due(check.when, last_run, now))
}

/// Whether a single trigger is due at `now`, given the claim's last run.
///
/// Separated from [`is_due`] so the two trigger arms are each obvious and the
/// `every Nd` arithmetic is testable in isolation.
fn trigger_is_due(when: Trigger, last_run: Option<Timestamp>, now: Timestamp) -> bool {
    match when {
        // v1 runs on-change checks on every invocation; always due.
        Trigger::OnChange => true,
        Trigger::Every { days } => match last_run {
            // Never run: due immediately, exactly like a never-verified claim is
            // stale immediately.
            None => true,
            Some(last) => {
                let interval = SignedDuration::from_hours(i64::from(days.get()) * 24);
                // Due once at least `interval` has elapsed since the last run. The
                // boundary is inclusive (`>=`): a claim last run exactly `interval`
                // ago is due now, matching the inclusive fresh-window boundary in
                // `compute_status`. `duration_since` is `now - last`, non-negative
                // because `last` is a past verdict.
                now.duration_since(last) >= interval
            }
        },
    }
}

/// The instant of the claim's most recent *past* verdict of any kind, the
/// scheduling clock for `every Nd`.
///
/// "Any kind" — `Held`, `Drifted`, `Broken`, `Unverifiable`, even an adjudication —
/// because this answers "when did we last *run* this", not "when did it last
/// pass". A future-dated entry (clock skew or forgery) is excluded: a run that has
/// not happened yet cannot reset the cadence, and honoring it could suppress a
/// genuinely due check. Mirrors `compute_status`'s exclusion of future `Held`s
/// from `last_verified`.
fn last_verdict_at(history: &[LogEntry], now: Timestamp) -> Option<Timestamp> {
    history.iter().map(|e| e.at).filter(|at| *at <= now).max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use claim_core::{Event, Verdict};
    use std::num::NonZeroU32;

    fn ts(s: &str) -> Timestamp {
        s.parse().expect("valid RFC 3339 timestamp")
    }

    fn every(days: u32) -> Trigger {
        Trigger::Every {
            days: NonZeroU32::new(days).unwrap(),
        }
    }

    /// A claim with the given trigger on a single cmd check. The rest of the fields
    /// are inert for scheduling — only `checks[*].when` and the history matter.
    fn claim_with(trigger: Trigger) -> Claim {
        parse(trigger)
    }

    fn parse(trigger: Trigger) -> Claim {
        let when = match trigger {
            Trigger::OnChange => "on-change".to_owned(),
            Trigger::Every { days } => format!("every {days}d"),
        };
        let text = format!(
            "---\nid: c\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: {when}\nmax-age: 30d\n---\nS.\n"
        );
        claim_core::parse_claim_file(".claims/c.md", &text).expect("valid claim")
    }

    /// A claim with two checks: an on-change and an every-Nd, to prove any-due.
    fn two_check_claim(days: u32) -> Claim {
        let text = format!(
            "---\nid: c\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: on-change\n  - kind: agent\n    instruction: look\n    when: every {days}d\nmax-age: 30d\n---\nS.\n"
        );
        claim_core::parse_claim_file(".claims/c.md", &text).expect("valid claim")
    }

    fn verdict_at(at: &str, verdict: Verdict) -> LogEntry {
        LogEntry {
            at: ts(at),
            commit: "c".to_owned(),
            actor: "a".to_owned(),
            event: Event::Verification {
                verdict,
                evidence: None,
            },
        }
    }

    #[test]
    fn on_change_is_always_due_regardless_of_history() {
        let claim = claim_with(Trigger::OnChange);
        let now = ts("2026-07-17T00:00:00Z");
        // No history: due.
        assert!(is_due(&claim, &[], now));
        // A verdict one second ago: still due — on-change ignores the clock.
        let recent = verdict_at("2026-07-16T23:59:59Z", Verdict::Held);
        assert!(is_due(&claim, std::slice::from_ref(&recent), now));
    }

    #[test]
    fn every_nd_with_no_history_is_due() {
        let claim = claim_with(every(30));
        assert!(is_due(&claim, &[], ts("2026-07-17T00:00:00Z")));
    }

    #[test]
    fn every_nd_is_not_due_before_the_interval_elapses() {
        let claim = claim_with(every(30));
        // Last run 10 days ago; 30-day cadence: not due.
        let history = [verdict_at("2026-07-07T00:00:00Z", Verdict::Held)];
        assert!(!is_due(&claim, &history, ts("2026-07-17T00:00:00Z")));
    }

    #[test]
    fn every_nd_is_due_exactly_at_the_interval_boundary() {
        let claim = claim_with(every(30));
        // Last run exactly 30 days ago: inclusive boundary, due.
        let history = [verdict_at("2026-06-17T00:00:00Z", Verdict::Held)];
        assert!(is_due(&claim, &history, ts("2026-07-17T00:00:00Z")));
    }

    #[test]
    fn every_nd_is_due_well_past_the_interval() {
        let claim = claim_with(every(30));
        let history = [verdict_at("2026-01-01T00:00:00Z", Verdict::Held)];
        assert!(is_due(&claim, &history, ts("2026-07-17T00:00:00Z")));
    }

    #[test]
    fn cadence_counts_any_verdict_not_only_a_pass() {
        // A Broken verdict 5 days ago resets the 30-day cadence exactly like a
        // Held would: this is a scheduling clock, not a freshness clock. So the
        // claim is NOT due, even though it is not passing.
        let claim = claim_with(every(30));
        let history = [verdict_at("2026-07-12T00:00:00Z", Verdict::Broken)];
        assert!(!is_due(&claim, &history, ts("2026-07-17T00:00:00Z")));
    }

    #[test]
    fn a_future_dated_verdict_does_not_suppress_a_due_check() {
        // A verdict timestamped in the future (clock skew or forgery) must not
        // reset the cadence: the claim was last genuinely run long ago and is due.
        let claim = claim_with(every(30));
        let history = [
            verdict_at("2026-01-01T00:00:00Z", Verdict::Held),
            verdict_at("2027-01-01T00:00:00Z", Verdict::Held),
        ];
        assert!(is_due(&claim, &history, ts("2026-07-17T00:00:00Z")));
    }

    #[test]
    fn latest_past_verdict_sets_the_cadence() {
        // Two past verdicts: the most recent one is the clock. Last run 5 days ago
        // (not 200), so a 30-day claim is not due.
        let claim = claim_with(every(30));
        let history = [
            verdict_at("2026-01-01T00:00:00Z", Verdict::Held),
            verdict_at("2026-07-12T00:00:00Z", Verdict::Held),
        ];
        assert!(!is_due(&claim, &history, ts("2026-07-17T00:00:00Z")));
    }

    #[test]
    fn a_claim_is_due_if_any_check_is_due() {
        // on-change + every-30d, last run 1 day ago. The every-30d is not due, but
        // the on-change always is, so the claim is due.
        let claim = two_check_claim(30);
        let history = [verdict_at("2026-07-16T00:00:00Z", Verdict::Held)];
        assert!(is_due(&claim, &history, ts("2026-07-17T00:00:00Z")));
    }

    #[test]
    fn multi_check_all_slow_and_none_due_is_not_due() {
        // Two every-Nd checks, both recently run: the claim is not due. Confirms
        // any-due does not spuriously fire when every trigger is a slow cadence.
        let text = "---\nid: c\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: every 30d\n  - kind: agent\n    instruction: look\n    when: every 60d\nmax-age: 90d\n---\nS.\n";
        let claim = claim_core::parse_claim_file(".claims/c.md", text).expect("valid");
        let history = [verdict_at("2026-07-15T00:00:00Z", Verdict::Held)];
        assert!(!is_due(&claim, &history, ts("2026-07-17T00:00:00Z")));
    }
}
