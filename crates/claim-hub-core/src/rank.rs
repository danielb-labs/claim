//! Ranking a store's skipped checks into the review queue (issue #9, hub-14).
//!
//! A skip is an acknowledged, bounded debt — a check deliberately not run — and never a
//! pass (invariant #4/#6). The deriver already surfaces every declared skip on a claim's
//! standing ([`SkipAge`](crate::SkipAge)) and never folds one into a `verified`/`stale`/
//! `drifted` standing. This module is the read-time *projection* that orders those skips
//! for a human to look at: it ranks them by age and lapsed `until` so the loudest debts
//! rise to the top of the review queue.
//!
//! It is a **pure function of the read model** ([`rank_skips`]): the clock is the read
//! model's own [`AsOf::clock`](crate::AsOf) — the exact instant the model derived at — so
//! ranking carries no clock of its own and cannot disagree with the standings it ranks.
//! No IO, no wall clock, no network; the same [`ReadModel`] always yields the same ranked
//! list (CLAUDE.md's determinism rule). This is the one place the four read surfaces (the
//! JSON API, the MCP, the markdown twins, and the UI) get their ranked skip set, so they
//! cannot disagree — they are the same ordering by construction, not four hand-kept copies.
//!
//! ## The ranking rule (the exact total order)
//!
//! A skip that ranks *higher* (needs a look sooner) sorts *earlier*. The order is total and
//! stable, so the projection is deterministic:
//!
//! 1. **A lapsed skip outranks a not-yet-lapsed one.** A skip whose `until` is at or before
//!    the clock has *lapsed* — the deferred check is due again (the router routes it as a
//!    [`Transition::LapsedSkip`](crate::Transition)) — so it is the louder queue signal and
//!    sorts ahead of every skip still within its window (the plan's headline rule).
//! 2. **Among lapsed skips, the one that lapsed *longer ago* outranks a more recent one.**
//!    "Longer ago" is a smaller `until`, so lapsed skips sort by `until` ascending: the
//!    oldest debt is loudest. This is the "by age" dimension for the skips that have come
//!    due.
//! 3. **Among not-yet-lapsed skips, the one *nearer its expiry* outranks a later one, and an
//!    indefinite skip (`until: None`) sorts last.** A bounded skip closer to lapsing is
//!    aging toward its deadline faster, so a smaller `until` sorts earlier; an indefinite
//!    skip has no deadline and will never lapse, so it is the least time-pressing — but it
//!    is still surfaced, plainly and last, so an *unbounded* mute cannot hide (invariant
//!    #6).
//! 4. **Ties break on `(store, claim, check_digest)` ascending**, so two skips with the same
//!    lapsed-ness and `until` still have one canonical order and the ranking is a total,
//!    stable order — the same inputs render byte-identically on every surface.
//!
//! The rule is *monotonic in age*: advancing the clock only ever moves a skip **up** the
//! ranking (a not-yet-lapsed skip can cross into lapsed, and a lapsed skip's lapse only
//! grows), never down — proven by the property tests (`tests/rank_props.rs`).

use claim_core::Timestamp;
use serde::Serialize;

use crate::deriver::ReadModel;

/// One skipped check, ranked into the review queue.
///
/// Carries the identity a surface names (the store, the claim, the check's digest), the
/// author's `reason`, the skip's `until` if it declared one, and the derived [`lapsed`]
/// fact as of the read model's clock. A surface renders these in the order [`rank_skips`]
/// returns them; the [`lapsed`] flag lets a surface mark the loudest ones without redoing
/// the arithmetic.
///
/// This is **queue/ranking data, not a verdict** (invariant #4): a `RankedSkip` records no
/// verdict and does not change any claim's standing — it surfaces a debt for a human to
/// look at. The type is [`Serialize`] so the JSON API and the MCP return it verbatim and
/// the UI/twin build their view rows from it, all from the one ranking.
///
/// [`lapsed`]: RankedSkip::lapsed
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RankedSkip {
    /// The connected store the skipped check's claim lives in.
    pub store: String,
    /// The claim's id the skip is declared on.
    pub claim: String,
    /// The digest of the check the skip mutes, so a surface names the exact check.
    pub check_digest: String,
    /// The author's justification for the skip (`skip.reason`).
    pub reason: String,
    /// The skip's expiry, if it declared one. `None` is an indefinite skip — an unbounded
    /// mute, surfaced plainly and ranked last among not-yet-lapsed skips so it cannot hide.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<Timestamp>,
    /// Whether the skip has lapsed as of the read model's clock: it declared an `until` and
    /// that instant is at or before the clock. A lapsed skip is the loudest queue signal —
    /// the deferred check is due again — and sorts ahead of every not-yet-lapsed skip.
    pub lapsed: bool,
}

/// The sort key that expresses the ranking rule as a comparable tuple.
///
/// Kept private and derived once per skip so the comparison is total, cheap, and stated in
/// one place. The fields are ordered so the tuple's natural `Ord` *is* the rule:
///
/// - `not_lapsed`: `false` for a lapsed skip, `true` otherwise — so lapsed skips (which
///   compare `false < true`) sort first (rule 1).
/// - `indefinite`: `false` for a skip with an `until`, `true` for one without — so an
///   indefinite skip sorts *after* every bounded skip within its lapsed-ness class (rule
///   3's "indefinite last"). This is needed because a bare `Option<Timestamp>` sorts
///   `None` *before* `Some(_)` (Rust's `Option` ordering), the opposite of what rule 3
///   wants; a lapsed skip always has an `until`, so this flag is always `false` for the
///   lapsed class and never perturbs rule 2.
/// - `until`: the `until` instant (`None` only for an indefinite skip, already forced last
///   by `indefinite`). Ascending, so an older lapse and a nearer expiry sort first (rules 2
///   and 3).
/// - `store`/`claim`/`check_digest`: the deterministic tiebreak (rule 4).
type RankKey<'a> = (bool, bool, Option<Timestamp>, &'a str, &'a str, &'a str);

/// Rank every skipped check across every claim in the read model into the review queue.
///
/// A pure projection of the [`ReadModel`] — the clock is the model's own
/// [`as_of.clock`](crate::AsOf), so the ranking cannot disagree with the standings it draws
/// from and needs no clock parameter of its own. Every declared skip on every claim's
/// standing becomes one [`RankedSkip`], ordered by the rule in the module docs: lapsed
/// before not-yet-lapsed, then by `until` (oldest lapse and nearest expiry first, indefinite
/// last), with a deterministic tiebreak. The result is a total, stable order, so the four
/// read surfaces render the identical ranked set.
///
/// A skip is **not** a verdict (invariant #4): this reads only the skips the deriver already
/// surfaced (a skipped check records no verdict, so it never bears on a standing), and
/// changes no standing — it orders debts for review, nothing more.
#[must_use]
pub fn rank_skips(model: &ReadModel) -> Vec<RankedSkip> {
    let now = model.as_of.clock;
    let mut ranked: Vec<RankedSkip> = model
        .claims
        .values()
        .flat_map(|standing| {
            standing.skips.iter().map(move |skip| RankedSkip {
                store: standing.store.clone(),
                claim: standing.id.clone(),
                check_digest: skip.check_digest.clone(),
                reason: skip.reason.clone(),
                until: skip.until,
                lapsed: skip.has_lapsed(now),
            })
        })
        .collect();
    ranked.sort_by(|a, b| rank_key(a).cmp(&rank_key(b)));
    ranked
}

/// The [`RankKey`] for one ranked skip — the tuple whose natural `Ord` is the ranking rule.
fn rank_key(skip: &RankedSkip) -> RankKey<'_> {
    (
        !skip.lapsed,
        skip.until.is_none(),
        skip.until,
        &skip.store,
        &skip.claim,
        &skip.check_digest,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deriver::{AsOf, ClaimStanding, SkipAge, Standing};
    use std::collections::BTreeMap;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    /// A standing carrying the given skips, for building a read model directly.
    fn standing_with_skips(id: &str, store: &str, skips: Vec<SkipAge>) -> ClaimStanding {
        ClaimStanding {
            id: id.to_owned(),
            store: store.to_owned(),
            standing: Standing::Stale,
            verified_as_of: None,
            stale_at: None,
            due_at: None,
            skips,
        }
    }

    fn skip(digest: &str, reason: &str, until: Option<&str>) -> SkipAge {
        SkipAge {
            check_digest: digest.to_owned(),
            reason: reason.to_owned(),
            until: until.map(ts),
        }
    }

    /// A read model at `clock` holding the given standings.
    fn model_of(clock: &str, standings: Vec<ClaimStanding>) -> ReadModel {
        let mut claims = BTreeMap::new();
        for s in standings {
            claims.insert((s.store.clone(), s.id.clone()), s);
        }
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

    #[test]
    fn a_claim_with_no_skips_contributes_nothing() {
        let model = model_of(
            "2026-07-18T00:00:00Z",
            vec![standing_with_skips("t", "s", Vec::new())],
        );
        assert!(rank_skips(&model).is_empty());
    }

    #[test]
    fn a_lapsed_skip_outranks_a_not_yet_lapsed_one() {
        // The headline rule: a skip whose `until` has passed sorts ahead of one still within
        // its window, even though the not-yet-lapsed skip expires sooner in wall-clock terms.
        let now = "2026-07-18T00:00:00Z";
        let model = model_of(
            now,
            vec![standing_with_skips(
                "t",
                "s",
                vec![
                    // Not lapsed: expires next year.
                    skip(
                        "a".repeat(64).as_str(),
                        "aging",
                        Some("2027-01-01T00:00:00Z"),
                    ),
                    // Lapsed: expired last year.
                    skip(
                        "b".repeat(64).as_str(),
                        "lapsed",
                        Some("2025-01-01T00:00:00Z"),
                    ),
                ],
            )],
        );
        let ranked = rank_skips(&model);
        assert_eq!(ranked.len(), 2);
        assert!(ranked[0].lapsed, "the lapsed skip ranks first");
        assert_eq!(ranked[0].reason, "lapsed");
        assert!(!ranked[1].lapsed, "the aging skip ranks second");
    }

    #[test]
    fn among_lapsed_skips_the_older_lapse_ranks_first() {
        // Rule 2: two lapsed skips sort by `until` ascending — the one that lapsed longer
        // ago is the louder, older debt.
        let now = "2026-07-18T00:00:00Z";
        let model = model_of(
            now,
            vec![standing_with_skips(
                "t",
                "s",
                vec![
                    skip(
                        "a".repeat(64).as_str(),
                        "recent",
                        Some("2026-06-01T00:00:00Z"),
                    ),
                    skip("b".repeat(64).as_str(), "old", Some("2025-01-01T00:00:00Z")),
                ],
            )],
        );
        let ranked = rank_skips(&model);
        assert_eq!(ranked[0].reason, "old", "the older lapse ranks first");
        assert_eq!(ranked[1].reason, "recent");
    }

    #[test]
    fn among_not_yet_lapsed_skips_the_nearer_expiry_ranks_first_and_indefinite_is_last() {
        // Rule 3: a bounded skip nearer its expiry outranks a later one, and an indefinite
        // skip (`until: None`) sorts last — surfaced, but least time-pressing.
        let now = "2026-07-18T00:00:00Z";
        let model = model_of(
            now,
            vec![standing_with_skips(
                "t",
                "s",
                vec![
                    skip("a".repeat(64).as_str(), "indefinite", None),
                    skip("b".repeat(64).as_str(), "far", Some("2028-01-01T00:00:00Z")),
                    skip(
                        "c".repeat(64).as_str(),
                        "near",
                        Some("2026-08-01T00:00:00Z"),
                    ),
                ],
            )],
        );
        let ranked = rank_skips(&model);
        assert_eq!(ranked[0].reason, "near", "nearest expiry first");
        assert_eq!(ranked[1].reason, "far");
        assert_eq!(ranked[2].reason, "indefinite", "indefinite sorts last");
    }

    #[test]
    fn ranking_spans_every_claim_and_store() {
        // The projection is over the whole read model, not one claim: skips from different
        // claims and stores are ranked into one queue.
        let now = "2026-07-18T00:00:00Z";
        let model = model_of(
            now,
            vec![
                standing_with_skips(
                    "billing/b",
                    "github.com/acme/billing",
                    vec![skip(
                        "a".repeat(64).as_str(),
                        "b-lapsed",
                        Some("2025-01-01T00:00:00Z"),
                    )],
                ),
                standing_with_skips(
                    "payments/p",
                    "github.com/acme/payments",
                    vec![skip("c".repeat(64).as_str(), "p-aging", None)],
                ),
            ],
        );
        let ranked = rank_skips(&model);
        assert_eq!(ranked.len(), 2, "both stores' skips are ranked");
        assert_eq!(ranked[0].reason, "b-lapsed", "the lapsed skip leads");
        assert_eq!(ranked[1].reason, "p-aging");
    }

    #[test]
    fn a_skip_never_reports_a_standing() {
        // Invariant #4: a RankedSkip carries no verdict and no standing field — it is queue
        // data a human looks at, never a fact the ranking asserts is true or false. The
        // serialized shape has exactly the queue fields, no `standing`/`verdict`.
        let now = "2026-07-18T00:00:00Z";
        let model = model_of(
            now,
            vec![standing_with_skips(
                "t",
                "s",
                vec![skip("a".repeat(64).as_str(), "parked", None)],
            )],
        );
        let ranked = rank_skips(&model);
        let json = serde_json::to_value(&ranked[0]).unwrap();
        assert!(json.get("standing").is_none(), "a skip has no standing");
        assert!(json.get("verdict").is_none(), "a skip has no verdict");
        assert_eq!(json["reason"], "parked");
        assert_eq!(json["lapsed"], false);
    }

    #[test]
    fn the_order_is_stable_regardless_of_input_claim_order() {
        // Determinism: the same skips inserted in a different order rank identically (the
        // BTreeMap-keyed model and the total-order sort make it structural).
        let now = "2026-07-18T00:00:00Z";
        let a = standing_with_skips(
            "a",
            "s",
            vec![skip(
                "d".repeat(64).as_str(),
                "a-skip",
                Some("2025-01-01T00:00:00Z"),
            )],
        );
        let b = standing_with_skips(
            "b",
            "s",
            vec![skip(
                "e".repeat(64).as_str(),
                "b-skip",
                Some("2025-01-01T00:00:00Z"),
            )],
        );
        let m1 = model_of(now, vec![a.clone(), b.clone()]);
        let m2 = model_of(now, vec![b, a]);
        assert_eq!(rank_skips(&m1), rank_skips(&m2));
    }
}
