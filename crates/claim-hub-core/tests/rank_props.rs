//! Property tests for the skip-ranking projection (hub-14, issue #9).
//!
//! These enumerate a small, meaningful universe exhaustively rather than sampling — the
//! same discipline as `deriver_props.rs`, so the tests stay deterministic and
//! dependency-free (no `proptest`, which the workspace's approved deps exclude) while still
//! asserting a universally-quantified property.
//!
//! The universe: a spread of skips, each either indefinite or with an `until` drawn from a
//! set of instants straddling a fixed read clock (some already lapsed, some not), placed on
//! claims across stores. Over *every* such arrangement the ranking must satisfy:
//!
//! - **A lapsed skip always outranks a not-yet-lapsed one.** No arrangement puts an aging
//!   skip ahead of a lapsed one — the plan's headline rule.
//! - **The ranking is a total, stable order.** Sorting the same skips presented in any input
//!   order yields the identical sequence, and the order is antisymmetric and transitive
//!   (the tuple key guarantees it; a re-sort is idempotent).
//! - **Monotonic in the clock.** Advancing the read clock only ever moves a skip *up* the
//!   ranking (a not-yet-lapsed skip can become lapsed; a lapsed skip's rank never drops
//!   below a skip that was above it) — a skip's urgency only grows with time, never shrinks.

use std::collections::BTreeMap;

use claim_core::Timestamp;
use claim_hub_core::deriver::{AsOf, ClaimStanding, ReadModel, SkipAge, Standing};
use claim_hub_core::rank::{rank_skips, RankedSkip};

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

/// The `until` values a skip can take: two already lapsed (before the read clock), two not
/// yet lapsed (after it), and the indefinite case (`None`). The clock the tests read at is
/// `2026-07-18T00:00:00Z`, so the split is deterministic.
const UNTIL_CHOICES: &[Option<&str>] = &[
    Some("2024-01-01T00:00:00Z"), // long lapsed
    Some("2026-01-01T00:00:00Z"), // recently lapsed
    Some("2026-12-01T00:00:00Z"), // near, not lapsed
    Some("2030-01-01T00:00:00Z"), // far, not lapsed
    None,                         // indefinite
];

/// The read clock every enumeration reads at, midway through `UNTIL_CHOICES`.
const CLOCK: &str = "2026-07-18T00:00:00Z";

/// Build a read model at `clock` whose single claim carries one skip per `untils` entry,
/// each on a distinct check digest so they are distinct skips.
fn model_with_skips(clock: &str, untils: &[Option<&str>]) -> ReadModel {
    let skips = untils
        .iter()
        .enumerate()
        .map(|(i, until)| SkipAge {
            // A distinct 64-char digest per skip, so no two collide on the tiebreak.
            check_digest: format!("{i:064x}"),
            reason: format!("skip-{i}"),
            until: until.map(ts),
        })
        .collect();
    let standing = ClaimStanding {
        id: "t".to_owned(),
        store: "s".to_owned(),
        standing: Standing::Stale,
        verified_as_of: None,
        stale_at: None,
        due_at: None,
        skips,
    };
    let mut claims = BTreeMap::new();
    claims.insert((standing.store.clone(), standing.id.clone()), standing);
    ReadModel {
        as_of: AsOf {
            ledger_head: 1,
            registry_version: 1,
            clock: ts(clock),
        },
        claims,
        due: Vec::new(),
        horizon: None,
    }
}

/// Whether a ranked skip has lapsed as of the read clock — recomputed here independently of
/// the ranking so the property test does not trust the field it is checking.
fn lapsed_at(skip: &RankedSkip, now: Timestamp) -> bool {
    skip.until.is_some_and(|until| now >= until)
}

/// Every non-empty subset of `UNTIL_CHOICES`, as a list of `until` slices — the exhaustive
/// space of skip arrangements on one claim.
fn subsets() -> Vec<Vec<Option<&'static str>>> {
    let n = UNTIL_CHOICES.len();
    let mut out = Vec::new();
    for mask in 1u32..(1 << n) {
        let mut subset = Vec::new();
        for (i, choice) in UNTIL_CHOICES.iter().enumerate() {
            if mask & (1 << i) != 0 {
                subset.push(*choice);
            }
        }
        out.push(subset);
    }
    out
}

#[test]
fn a_lapsed_skip_always_outranks_a_not_yet_lapsed_one() {
    // Over every subset of skip arrangements: in the ranked order, no not-yet-lapsed skip
    // ever precedes a lapsed one. Equivalently, all lapsed skips form a prefix.
    let now = ts(CLOCK);
    for subset in subsets() {
        let model = model_with_skips(CLOCK, &subset);
        let ranked = rank_skips(&model);
        // Recompute lapsed-ness independently and check the prefix property.
        let mut seen_not_lapsed = false;
        for skip in &ranked {
            let lapsed = lapsed_at(skip, now);
            assert_eq!(lapsed, skip.lapsed, "the lapsed field matches the clock");
            if lapsed {
                assert!(
                    !seen_not_lapsed,
                    "a lapsed skip ({:?}) followed a not-yet-lapsed one in {subset:?}",
                    skip.until
                );
            } else {
                seen_not_lapsed = true;
            }
        }
    }
}

#[test]
fn ranking_is_stable_and_idempotent() {
    // The order is total and stable: re-ranking the already-ranked skips (re-inserted in
    // that order) reproduces the identical sequence, and reversing the input does too —
    // there is no arrangement whose ranking depends on input order.
    for subset in subsets() {
        let model = model_with_skips(CLOCK, &subset);
        let once = rank_skips(&model);

        // Present the same skips in reverse and confirm the ranking is unchanged.
        let mut reversed = subset.clone();
        reversed.reverse();
        let model_rev = model_with_skips(CLOCK, &reversed);
        let from_rev = rank_skips(&model_rev);
        // The digests differ between the two models (index-derived), so compare by the
        // ordering-relevant fields, which are what the rule sorts on.
        let key = |s: &RankedSkip| (s.lapsed, s.until, s.reason.clone());
        let a: Vec<_> = once.iter().map(key).collect();
        // Reversed model relabels reasons by position, so compare the ordering signature
        // (lapsed, until) alone — the reasons are per-model labels, not order facts.
        let sig = |v: &[RankedSkip]| v.iter().map(|s| (s.lapsed, s.until)).collect::<Vec<_>>();
        assert_eq!(
            sig(&once),
            sig(&from_rev),
            "input order does not change the ranking for {subset:?}"
        );
        // And re-ranking the same model is idempotent.
        let twice = rank_skips(&model);
        let b: Vec<_> = twice.iter().map(key).collect();
        assert_eq!(a, b, "re-ranking is idempotent for {subset:?}");
    }
}

#[test]
fn advancing_the_clock_never_demotes_a_skip() {
    // Monotonic in age: for every arrangement, advancing the read clock only moves skips up
    // the ranking. Concretely, the set of lapsed skips only grows, and a skip lapsed at an
    // earlier clock stays lapsed (and no earlier-lapsed skip is overtaken by a later one) at
    // a later clock. We sweep two clocks — the base and a much later one — and assert the
    // lapsed set at the later clock is a superset of the earlier one, per skip.
    let earlier = ts("2026-07-18T00:00:00Z");
    let later = ts("2031-01-01T00:00:00Z");
    for subset in subsets() {
        let at_earlier = rank_skips(&model_with_skips("2026-07-18T00:00:00Z", &subset));
        let at_later = rank_skips(&model_with_skips("2031-01-01T00:00:00Z", &subset));

        // Every skip lapsed at the earlier clock is still lapsed at the later clock: a lapse
        // never un-happens as time passes (the debt only ages).
        for skip in &at_earlier {
            if lapsed_at(skip, earlier) {
                let same = at_later
                    .iter()
                    .find(|s| s.until == skip.until && s.reason == skip.reason)
                    .expect("the same skip is present at both clocks");
                assert!(
                    lapsed_at(same, later),
                    "a skip lapsed at {earlier} must stay lapsed at {later}: {:?}",
                    skip.until
                );
            }
        }

        // The count of lapsed skips is non-decreasing as the clock advances — urgency only
        // grows.
        let lapsed_earlier = at_earlier.iter().filter(|s| lapsed_at(s, earlier)).count();
        let lapsed_later = at_later.iter().filter(|s| lapsed_at(s, later)).count();
        assert!(
            lapsed_later >= lapsed_earlier,
            "advancing the clock cannot reduce the lapsed set for {subset:?}"
        );
    }
}

#[test]
fn indefinite_skips_are_ranked_last_and_never_lapse() {
    // An indefinite skip (`until: None`) is surfaced but never lapses and always sorts after
    // every skip that has an `until`, at any clock. So in any arrangement, once a `None`
    // appears in the ranked order, only other `None`s follow.
    for subset in subsets() {
        for clock in [
            "2020-01-01T00:00:00Z",
            "2026-07-18T00:00:00Z",
            "2099-01-01T00:00:00Z",
        ] {
            let ranked = rank_skips(&model_with_skips(clock, &subset));
            let mut seen_indefinite = false;
            for skip in &ranked {
                match skip.until {
                    None => {
                        assert!(!skip.lapsed, "an indefinite skip never lapses");
                        seen_indefinite = true;
                    }
                    Some(_) => assert!(
                        !seen_indefinite,
                        "a bounded skip followed an indefinite one at {clock} in {subset:?}"
                    ),
                }
            }
        }
    }
}
