//! Property tests for the deriver's honesty invariants.
//!
//! These enumerate whole combinatorial spaces rather than sampling: for the small
//! input universes that matter (a claim's few checks, each with one of a handful of
//! latest verdicts, at a spread of instants), exhaustive enumeration *is* the
//! property proof — every case is checked, not a random subset. That keeps the tests
//! deterministic and dependency-free (no `proptest`), which the workspace's approved
//! deps require, while still asserting a universally-quantified property.
//!
//! The properties, one per golden concern:
//!
//! - **No combination of events manufactures a green** (invariant #1): over every
//!   assignment of latest verdicts to a claim's checks, [`Standing::Verified`] occurs
//!   *only* when every check's latest is a pass. A companion property adds the clock
//!   dimension — the same verdict-combos crossed with a sweep of clocks around a
//!   claim's freshness boundary — so an all-held-but-*expired* multi-check claim
//!   reading `Verified` would fail a test, not just the single-check case.
//! - **A shallow check's pass never clears a deep check's drift** (issue #18): a
//!   verdict reported against one check's digest never changes another check's state,
//!   so a held shallow check cannot rescue a drifted deep check.
//! - **`broken` counts as never-checked** (invariant #1): a `Broken` (or
//!   `Unverifiable`) latest yields exactly the standing an absent verdict would.
//! - **A future-dated pass buys no freshness** (invariant #6): the clock-boundary
//!   sweep and a targeted unit test pin that a producer-asserted `reported_at` past
//!   the read clock cannot extend a claim's window past `now`.

use claim_core::{parse_claim_file, Claim, Timestamp, Verdict};
use claim_hub_core::deriver::{derive, ClaimEntry, DeriverConfig, RegistrySnapshot, Standing};
use claim_hub_core::{check_digest, CheckRef, Event, EventKind, Producer};

/// The verdicts a check's latest can take, plus the "never checked" absence.
///
/// The one "good news" state is `Some(Held)`; everything else — including the
/// absence — is a non-pass, and the property tests assert none of them can be joined
/// into a verified claim.
const LATEST_CHOICES: &[Option<Verdict>] = &[
    Some(Verdict::Held),
    Some(Verdict::Drifted),
    Some(Verdict::Broken),
    Some(Verdict::Unverifiable),
    None,
];

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

/// An n-check claim, each check a distinct `cmd` so their digests differ.
fn n_check_claim(id: &str, n: usize) -> Claim {
    let mut body = format!("id: {id}\nchecks:\n");
    for i in 0..n {
        body.push_str(&format!("  - kind: cmd\n    run: \"check-{i}\"\n"));
    }
    let text = format!("---\n{body}---\nStatement.\n");
    parse_claim_file(".claims/t.md", &text).expect("valid claim")
}

/// A verdict event for a specific check of a claim.
fn event_for_check(claim: &Claim, check_index: usize, verdict: Verdict, at: &str) -> Event {
    let mut producer = serde_json::Map::new();
    producer.insert("run".into(), serde_json::json!("1"));
    Event {
        kind: EventKind::Verdict,
        claim: claim.id.as_str().to_owned(),
        check: CheckRef {
            index: check_index,
            digest: check_digest(&claim.checks[check_index]),
        },
        verdict,
        evidence: None,
        commit: "abc".into(),
        store: "s".into(),
        producer: Producer(producer),
        reported_at: ts(at),
    }
}

fn single_claim_registry(claim: Claim) -> RegistrySnapshot {
    RegistrySnapshot {
        version: 1,
        claims: vec![ClaimEntry {
            store: "s".into(),
            claim,
        }],
    }
}

/// Build the ledger for a per-check assignment of latest verdicts. Each present
/// choice is one event against that check's own digest; an absence contributes no
/// event (never checked).
fn events_for_assignment(claim: &Claim, assignment: &[Option<Verdict>]) -> Vec<(u64, Event)> {
    let mut events = Vec::new();
    let mut seq = 0u64;
    for (i, choice) in assignment.iter().enumerate() {
        if let Some(v) = choice {
            seq += 1;
            events.push((seq, event_for_check(claim, i, *v, "2026-07-01T00:00:00Z")));
        }
    }
    events
}

/// Enumerate every length-`n` assignment over [`LATEST_CHOICES`].
fn assignments(n: usize) -> Vec<Vec<Option<Verdict>>> {
    let mut out = vec![vec![]];
    for _ in 0..n {
        let mut next = Vec::new();
        for prefix in &out {
            for choice in LATEST_CHOICES {
                let mut extended = prefix.clone();
                extended.push(*choice);
                next.push(extended);
            }
        }
        out = next;
    }
    out
}

#[test]
fn no_combination_of_events_manufactures_a_green() {
    // For a 1-, 2-, and 3-check claim, over *every* assignment of latest verdicts,
    // Verified appears iff every check's latest is Held. No mix of good and bad news
    // can be joined into a pass (invariant #1). The window is wide open (no max-age)
    // and the clock is right after the verdicts, so freshness never independently
    // demotes a genuinely-all-held case — isolating the join's own behavior.
    let now = ts("2026-07-01T00:00:01Z");
    for n in 1..=3 {
        let claim = n_check_claim("t", n);
        let reg = single_claim_registry(claim.clone());
        for assignment in assignments(n) {
            let events = events_for_assignment(&claim, &assignment);
            let model = derive(&reg, &events, now, &DeriverConfig::default());
            let standing = model.standing("s", "t").unwrap().standing;

            let all_held = assignment.iter().all(|c| *c == Some(Verdict::Held));
            let any_drift = assignment.contains(&Some(Verdict::Drifted));

            if all_held {
                assert_eq!(
                    standing,
                    Standing::Verified,
                    "all checks held → verified: {assignment:?}"
                );
            } else {
                assert_ne!(
                    standing,
                    Standing::Verified,
                    "a non-held check must never verify: {assignment:?}"
                );
            }

            // And a drift, wherever it appears in the mix, dominates absolutely.
            if any_drift {
                assert_eq!(
                    standing,
                    Standing::Drifted,
                    "any drift dominates the join: {assignment:?}"
                );
            }
        }
    }
}

#[test]
fn no_verdict_combo_manufactures_a_green_across_the_freshness_boundary() {
    // The companion to the join property, adding the clock dimension: for a windowed
    // multi-check claim, over *every* assignment of latest verdicts *and* a sweep of
    // clock instants around the freshness boundary, an all-held claim is Verified only
    // strictly before expiry (Stale at or after), and a non-all-held claim is never
    // Verified at any clock — the multi-check-combos × lapsed-window interaction is
    // enumerated, so an all-held-but-expired multi-check claim reading Verified would
    // fail here. All verdicts are dated at a fixed instant; the 30-day window makes
    // expiry 2026-07-31T00:00:00Z.
    let passed_at = "2026-07-01T00:00:00Z";
    let expiry = ts("2026-07-31T00:00:00Z");
    // Clocks straddling the boundary: mid-window (fresh), one second before expiry
    // (fresh), exactly at expiry (stale), and past it (stale).
    let clocks = [
        (ts("2026-07-10T00:00:00Z"), true),
        (ts("2026-07-30T23:59:59Z"), true),
        (ts("2026-07-31T00:00:00Z"), false),
        (ts("2026-08-15T00:00:00Z"), false),
    ];

    for n in 1..=3 {
        let claim = with_max_age(n_check_claim("t", n), "30d");
        let reg = single_claim_registry(claim.clone());
        for assignment in assignments(n) {
            // All verdicts share `passed_at`, so the freshness baseline (the min
            // passing instant) is `passed_at` whenever every check passes.
            let events: Vec<(u64, Event)> = assignment
                .iter()
                .enumerate()
                .filter_map(|(i, choice)| {
                    choice.map(|v| (i as u64 + 1, event_for_check(&claim, i, v, passed_at)))
                })
                .collect();

            let all_held = assignment.iter().all(|c| *c == Some(Verdict::Held));
            let any_drift = assignment.contains(&Some(Verdict::Drifted));

            for (clock, within_window) in clocks {
                let model = derive(&reg, &events, clock, &DeriverConfig::default());
                let s = model.standing("s", "t").unwrap();

                if any_drift {
                    // A drift dominates at every clock, expired or not.
                    assert_eq!(
                        s.standing,
                        Standing::Drifted,
                        "drift dominates regardless of clock: {assignment:?} at {clock}"
                    );
                } else if all_held && within_window {
                    assert_eq!(
                        s.standing,
                        Standing::Verified,
                        "all held and within window → verified: {assignment:?} at {clock}"
                    );
                    assert_eq!(s.stale_at, Some(expiry));
                } else {
                    // Either a non-held check (never verifies at any clock) or an
                    // all-held claim past its window (stale by the clock alone). In
                    // neither case may it read Verified.
                    assert_ne!(
                        s.standing,
                        Standing::Verified,
                        "a non-held check or an expired window must never verify: \
                         {assignment:?} at {clock}"
                    );
                }
            }
        }
    }
}

#[test]
fn a_shallow_checks_pass_never_clears_a_deep_checks_drift() {
    // The #18 property: a two-check claim (a "shallow" check 0 and a "deep" check 1).
    // The deep check drifts; the shallow check later passes. Because the join keys on
    // each check's *digest*, the shallow pass lands only on check 0 and cannot satisfy
    // check 1's drifted position — the claim stays drifted no matter how much later or
    // how often the shallow check passes.
    let claim = n_check_claim("t", 2);
    let reg = single_claim_registry(claim.clone());

    let deep_drift = event_for_check(&claim, 1, Verdict::Drifted, "2026-07-01T00:00:00Z");
    // The shallow pass is reported *after* the deep drift, so a naive "latest verdict
    // wins across the whole claim" would wrongly clear it.
    let shallow_pass_later = event_for_check(&claim, 0, Verdict::Held, "2026-07-05T00:00:00Z");

    let model = derive(
        &reg,
        &[(1, deep_drift), (2, shallow_pass_later)],
        ts("2026-07-06T00:00:00Z"),
        &DeriverConfig::default(),
    );
    assert_eq!(
        model.standing("s", "t").unwrap().standing,
        Standing::Drifted,
        "a later shallow pass must not clear an earlier deep drift"
    );
}

#[test]
fn a_shallow_pass_reported_against_the_deep_digest_would_clear_it_only_by_identity() {
    // The mirror of the property above, proving the digest is what does the work: if
    // the *same* passing verdict is instead reported against the deep check's own
    // digest (index 1), it does clear that check — confirming the earlier test's
    // drift persistence is the digest keying, not an accident.
    let claim = n_check_claim("t", 2);
    let reg = single_claim_registry(claim.clone());

    let deep_drift = event_for_check(&claim, 1, Verdict::Drifted, "2026-07-01T00:00:00Z");
    let deep_pass_later = event_for_check(&claim, 1, Verdict::Held, "2026-07-05T00:00:00Z");
    // Check 0 also needs a pass for the claim to verify.
    let shallow_pass = event_for_check(&claim, 0, Verdict::Held, "2026-07-05T00:00:00Z");

    let model = derive(
        &reg,
        &[(1, deep_drift), (2, deep_pass_later), (3, shallow_pass)],
        ts("2026-07-06T00:00:00Z"),
        &DeriverConfig::default(),
    );
    assert_eq!(
        model.standing("s", "t").unwrap().standing,
        Standing::Verified,
        "a later pass on the deep check's own digest does clear its drift"
    );
}

#[test]
fn broken_counts_exactly_like_never_checked() {
    // For every check position in a 2-check claim, and for the whole claim, the
    // standing when a check's latest is Broken (or Unverifiable) equals the standing
    // when that check has no verdict at all. The other check is fixed at Held so the
    // difference isolates the broken-vs-absent check.
    let claim = n_check_claim("t", 2);
    let reg = single_claim_registry(claim.clone());
    let now = ts("2026-07-02T00:00:00Z");

    for inconclusive in [Verdict::Broken, Verdict::Unverifiable] {
        // Check 0 held; check 1 is either inconclusive or absent.
        let held0 = event_for_check(&claim, 0, Verdict::Held, "2026-07-01T00:00:00Z");
        let broken1 = event_for_check(&claim, 1, inconclusive, "2026-07-01T00:00:00Z");

        let with_broken = derive(
            &reg,
            &[(1, held0.clone()), (2, broken1)],
            now,
            &DeriverConfig::default(),
        );
        // Absent: no event for check 1 at all.
        let with_absent = derive(&reg, &[(1, held0)], now, &DeriverConfig::default());

        let broken_standing = with_broken.standing("s", "t").unwrap();
        let absent_standing = with_absent.standing("s", "t").unwrap();
        assert_eq!(
            broken_standing.standing, absent_standing.standing,
            "a {inconclusive:?} latest must derive the same standing as never-checked"
        );
        assert_eq!(
            broken_standing.standing,
            Standing::Stale,
            "and that shared standing is stale, never verified"
        );
        // Neither has a full-pass baseline, so neither carries a verified-as-of.
        assert_eq!(broken_standing.verified_as_of, None);
        assert_eq!(absent_standing.verified_as_of, None);
    }
}

#[test]
fn freshness_lapses_by_the_clock_across_the_whole_expiry_range() {
    // A property over the clock: for a single held check with a 30-day window, the
    // claim is Verified at every instant strictly before expiry and Stale at every
    // instant at or after it — the boundary is a single sharp inclusive edge, with no
    // event on either side. Sampling a spread of instants around the boundary proves
    // the arithmetic, not one lucky point.
    let claim = n_check_claim("t", 1);
    let claim = with_max_age(claim, "30d");
    let reg = single_claim_registry(claim.clone());
    let event = event_for_check(&claim, 0, Verdict::Held, "2026-07-01T00:00:00Z");
    let events = [(1, event)];
    // 2026-07-01 + 30d = 2026-07-31T00:00:00Z.
    let expiry = ts("2026-07-31T00:00:00Z");

    for (instant, expected_fresh) in [
        ("2026-07-01T00:00:00Z", true),  // the instant it passed
        ("2026-07-15T12:00:00Z", true),  // mid-window
        ("2026-07-30T23:59:59Z", true),  // one second before expiry
        ("2026-07-31T00:00:00Z", false), // exactly at expiry: stale
        ("2026-07-31T00:00:01Z", false), // just past
        ("2027-01-01T00:00:00Z", false), // long past
    ] {
        let model = derive(&reg, &events, ts(instant), &DeriverConfig::default());
        let s = model.standing("s", "t").unwrap();
        let want = if expected_fresh {
            Standing::Verified
        } else {
            Standing::Stale
        };
        assert_eq!(s.standing, want, "at {instant} (expiry {expiry})");
        if expected_fresh {
            // A verified claim's stale_at is exactly the expiry, feeding the horizon.
            assert_eq!(s.stale_at, Some(expiry));
        }
    }
}

/// Reparse a claim with a `max-age` hint added, since the parser is the only builder.
fn with_max_age(claim: Claim, days: &str) -> Claim {
    let n = claim.checks.len();
    let mut body = format!(
        "id: {}\nhub:\n  max-age: {days}\nchecks:\n",
        claim.id.as_str()
    );
    for i in 0..n {
        body.push_str(&format!("  - kind: cmd\n    run: \"check-{i}\"\n"));
    }
    let text = format!("---\n{body}---\nStatement.\n");
    parse_claim_file(".claims/t.md", &text).expect("valid claim")
}
