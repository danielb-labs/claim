//! The router's pure core: transitions, fire keys, and the nag payload.
//!
//! The router (the hub crate's loop) must *notice* when a fact stops holding and route
//! it to whoever owns the decision — exactly once, surviving restarts. Everything that
//! can be pure lives here: what a transition *is*, how a fire is *identified*, and what a
//! nag event *carries*. The impure parts — reading CODEOWNERS from the git mirror,
//! appending the nag event, the tick — stay in the store and hub crates.
//!
//! ## The three transitions
//!
//! A [`Transition`] is one of the derived events the router routes (HUB.md §3): a claim
//! entering [`Drifted`](Transition::Drifted), crossing into [`Stale`](Transition::Stale),
//! or a skip's `until` lapsing ([`LapsedSkip`](Transition::LapsedSkip)). They come from
//! the deriver's read model — [`pending_transitions`] reads a [`ReadModel`] and a clock
//! and returns the transitions that are *live right now*, with no IO. A stale transition
//! fires from the clock crossing a window with no new verdict, which is why the clock is a
//! parameter (CLAUDE.md's determinism rule).
//!
//! ## Fire-once is derived, not stored
//!
//! "Already nagged" is **not** a mutable flag (invariant #3). The router derives it by
//! diffing the current live transitions against the `nag` events already on the ledger:
//! a transition whose [`FireKey`] is absent from the ledger's fired keys is *new* and
//! fires; one already present is not re-fired. The fire key is a deterministic function of
//! the transition and the derived state it fired against ([`FireKey::compute`]), so the
//! same transition always maps to the same key and a restart — which rebuilds the fired
//! set by re-scanning the ledger — reaches the identical conclusion. There is no in-memory
//! memory to lose; the ledger is the only memory.
//!
//! ## Grouping by envelope commit
//!
//! A drift transition groups on the **commit** that broke the claims (HUB.md §3): one
//! refactor breaking twelve claims is one nag item, not twelve. [`group_transitions`] folds
//! a set of drifted claims into one [`NagGroup`] per (store, commit). A stale or lapsed-skip
//! transition is per-claim — there is no single breaking commit — so each is its own group
//! keyed on the claim.

use std::collections::BTreeMap;

use claim_core::Timestamp;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::deriver::{ClaimStanding, ReadModel, SkipAge, Standing};

/// The producer-block key under which a nag event records its principal, so a reader can
/// tell the hub's own scheduling telemetry from a CI producer's verdict.
pub const NAG_PRINCIPAL_KEY: &str = "principal";

/// The value of [`NAG_PRINCIPAL_KEY`] the router stamps on every nag it appends. A
/// constant so every nag is attributable to the router and a scan can find them by
/// principal even before matching on kind.
pub const NAG_PRINCIPAL: &str = "hub-router";

/// The producer-block key under which a nag records its [`FireKey`]. The storage layer
/// reads this into the ledger's `check_digest` column so the dedup index gives fire-once a
/// second line of defense beneath the router's ledger-diff; a reader reads it back to
/// rebuild the fired set.
pub const NAG_FIRE_KEY: &str = "fire_key";

/// The producer-block key under which a nag records its [`Transition`] name, so the fired
/// set can be grouped by transition without re-deriving.
pub const NAG_TRANSITION_KEY: &str = "transition";

/// The producer-block key under which a nag records its `run` — the router principal's
/// per-fire run id (the fire key), so a nag event is attributable and dedups like any
/// event (the storage layer requires a non-empty `run`).
pub const NAG_RUN_KEY: &str = "run";

/// Build the producer block for a nag event: the router principal, the transition, and the
/// fire key (as both the `fire_key` marker and the `run` the storage layer dedups on).
///
/// A nag is attributable to the hub's own router principal (invariant #4: it is the hub's
/// scheduling telemetry, not a CI producer's verdict), and its `run` is the fire key so the
/// ledger's dedup index enforces fire-once beneath the router's derived diff. The returned
/// map is the [`Producer`](crate::Producer) a nag [`Event`](crate::Event) carries.
#[must_use]
pub fn nag_producer(transition: Transition, fire_key: &FireKey) -> crate::Producer {
    let mut map = serde_json::Map::new();
    map.insert(
        NAG_PRINCIPAL_KEY.to_owned(),
        serde_json::Value::String(NAG_PRINCIPAL.to_owned()),
    );
    map.insert(
        NAG_TRANSITION_KEY.to_owned(),
        serde_json::Value::String(transition.as_str().to_owned()),
    );
    map.insert(
        NAG_FIRE_KEY.to_owned(),
        serde_json::Value::String(fire_key.as_str().to_owned()),
    );
    // The `run` IS the fire key, so the ledger's (store, run, claim, digest) dedup index
    // gives fire-once a second line of defense beneath the router's ledger-diff.
    map.insert(
        NAG_RUN_KEY.to_owned(),
        serde_json::Value::String(fire_key.as_str().to_owned()),
    );
    crate::Producer(map)
}

/// Read the [`FireKey`] a nag event's producer block recorded, if it is a well-formed nag.
///
/// The router's fired-set diff reads this from every `nag` event on the ledger to learn
/// what has already fired. A nag missing its fire-key marker is ill-formed telemetry;
/// returning `None` lets the caller skip it rather than treat a malformed nag as a fire
/// (which would suppress a real one — a silent miss invariant #6 forbids).
#[must_use]
pub fn fire_key_of(event: &crate::Event) -> Option<FireKey> {
    if event.kind != crate::EventKind::Nag {
        return None;
    }
    match event.producer.0.get(NAG_FIRE_KEY) {
        Some(serde_json::Value::String(key)) if !key.is_empty() => {
            Some(FireKey::from_stored(key.clone()))
        }
        _ => None,
    }
}

/// One kind of derived transition the router routes.
///
/// The three v1 transitions (HUB.md §3). `#[non_exhaustive]` reserves the enum for later
/// route kinds (a spot-audit contest, a suspect propagation), so a new transition forces
/// every consumer to decide how to route it rather than defaulting to silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Transition {
    /// A claim's latest verdict says the fact is false now — it entered
    /// [`Standing::Drifted`]. Grouped by the commit that broke it.
    Drifted,
    /// A claim aged past its freshness window with no new verdict — it crossed into
    /// [`Standing::Stale`] by the clock (HUB.md §3). Per-claim.
    Stale,
    /// A check's skip declared an `until` that has now passed — the deferred check is due
    /// again. Per-claim-and-check.
    LapsedSkip,
}

impl Transition {
    /// The wire/log name of this transition (`drifted`, `stale`, `lapsed-skip`), used in
    /// the fire key and the nag payload so a reader and the diff agree on the spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Transition::Drifted => "drifted",
            Transition::Stale => "stale",
            Transition::LapsedSkip => "lapsed-skip",
        }
    }
}

/// The stable identity of one fire: the (store, transition, subject) a nag fired for.
///
/// The router fires a transition **once** per fire key. The key is content — a
/// deterministic hash over the store, the transition, and the transition's *subject*
/// (the grouping commit for a drift, the claim for a stale, the claim+check for a lapsed
/// skip) — so the same transition always yields the same key regardless of process,
/// restart, or ordering. Diffing the current transitions' keys against the ledger's fired
/// keys is the whole of "already nagged" (invariant #3): no mutable flag, only the log.
///
/// The subject deliberately does *not* include the *set of claims* in a drift group: a
/// group grows as more verdicts for the same breaking commit arrive, and re-firing every
/// time a claim joins would violate fire-once. The group's identity is its (store,
/// commit); the claims are rendered content, not part of the key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FireKey(String);

impl FireKey {
    /// Compute the fire key for a transition over a subject.
    ///
    /// The hash covers the store, the transition name, and the subject, joined with a
    /// separator that cannot appear in the parts' meaningful content ambiguously, so two
    /// different (store, transition, subject) triples cannot collide onto one key. The
    /// output is the hex SHA-256, a stable 64-char string suited to the ledger's
    /// `check_digest` column (which is how a nag row carries its fire key for dedup).
    #[must_use]
    pub fn compute(store: &str, transition: Transition, subject: &str) -> Self {
        let mut hasher = Sha256::new();
        // Length-prefix each part so no concatenation of parts can be forged into a
        // different (store, transition, subject) split — a\u{1f} b\u{1f} c is unambiguous
        // because \u{1f} (unit separator) is a control char absent from stores, shas, and
        // claim ids, but the length prefix is the belt-and-suspenders guarantee.
        for part in [store, transition.as_str(), subject] {
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
        FireKey(hex(&hasher.finalize()))
    }

    /// The key as its stable hex string — what the ledger stores and the diff compares.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Wrap a stored key string read back from the ledger, for the fired-set diff.
    #[must_use]
    pub fn from_stored(key: String) -> Self {
        FireKey(key)
    }
}

/// Lowercase-hex encode bytes, dependency-free (the workspace already pulls `sha2`, not a
/// hex crate). Used only for the fire key's digest, so it need not be constant-time.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    out
}

/// One drifted claim, as the router renders it into a grouped nag.
///
/// Carries what a human (or the CI glue) needs to act: the claim's id, the store, the
/// commit that broke it, and — from the registry — the statement and the decisions it
/// supports, so the nag reads without opening the file. Built by the router from the read
/// model plus a registry lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftedClaim {
    /// The drifted claim's id.
    pub id: String,
    /// The commit the drift was reported against — the grouping key.
    pub commit: String,
    /// The claim's statement, so the nag reads without opening the file. Empty when the
    /// registry did not hold it (a retired-but-still-drifted claim).
    pub statement: String,
    /// The decisions and claims that rest on this now-broken fact (its `supports`).
    pub supports: Vec<String>,
}

/// A group of transitions that route as one nag item.
///
/// Grouping is HUB.md §3's rule: one commit breaking N claims is **one** item, not N. A
/// [`NagGroup`] is the unit the router fires and serves. For a drift group, `claims` holds
/// every claim broken by `commit`; for a stale or lapsed-skip group, it holds the single
/// claim (there is no shared breaking commit). Every group carries its resolved owners and
/// its [`FireKey`], so the router can diff it against the ledger and route it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NagGroup {
    /// The store the group's claims live in.
    pub store: String,
    /// Which transition fired this group.
    pub transition: Transition,
    /// The commit that identifies the group: the breaking commit for a drift, or the
    /// claim's latest-verdict commit for a per-claim stale/lapsed-skip (kept for display,
    /// not part of the fire key for those).
    pub commit: String,
    /// The claims in the group, each with its statement and supports. One for a per-claim
    /// transition; N for a drift group.
    pub claims: Vec<DriftedClaim>,
    /// The stable identity the router fires this group once per (see [`FireKey`]).
    #[serde(rename = "fire_key")]
    pub fire_key_str: String,
}

impl NagGroup {
    /// This group's fire key, wrapped for the diff.
    #[must_use]
    pub fn fire_key(&self) -> FireKey {
        FireKey::from_stored(self.fire_key_str.clone())
    }

    /// The primary claim id the nag event's `claim` field names — the first claim in the
    /// group in sorted order, so a grouped nag has a stable representative claim.
    #[must_use]
    pub fn primary_claim(&self) -> &str {
        self.claims.first().map_or("", |claim| claim.id.as_str())
    }
}

/// A live transition awaiting grouping and routing, as [`pending_transitions`] returns it.
///
/// Each carries the store, the claim, the transition, and the commit that anchors it. The
/// router turns these into [`NagGroup`]s (drifts grouped by commit, others per-claim),
/// resolves owners, and diffs against the ledger. Kept a flat list so the pure detection
/// stays simple and the grouping is a separate, testable fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTransition {
    /// The store the claim lives in.
    pub store: String,
    /// The claim's id.
    pub claim: String,
    /// Which transition is live for it.
    pub transition: Transition,
    /// The commit that anchors the transition: the drift's commit, or (for stale/lapsed)
    /// the latest verdict's commit — a display anchor for a per-claim group.
    pub commit: String,
    /// For a [`Transition::LapsedSkip`], the check digest whose skip lapsed, so two skips
    /// on one claim are distinct transitions. Empty for drift and stale.
    pub check_digest: String,
}

impl PendingTransition {
    /// The subject the fire key hashes for this transition.
    ///
    /// A drift's subject is its **commit** (so a group of claims broken by one commit
    /// shares one key — grouping is fire-once). A stale's subject is the **claim** (each
    /// claim ages independently). A lapsed skip's subject is the **claim and the check's
    /// digest** (a claim may have several skips, each its own transition).
    #[must_use]
    fn subject(&self) -> String {
        match self.transition {
            Transition::Drifted => self.commit.clone(),
            Transition::Stale => self.claim.clone(),
            Transition::LapsedSkip => format!("{}\u{1f}{}", self.claim, self.check_digest),
        }
    }

    /// This transition's fire key.
    #[must_use]
    pub fn fire_key(&self) -> FireKey {
        FireKey::compute(&self.store, self.transition, &self.subject())
    }
}

/// Read the live transitions from a derived read model as of `now`.
///
/// Pure and deterministic (the clock is a parameter): the same `(model, now)` always
/// yields the same transitions, in a stable order. For every claim in the model:
///
/// - a [`Standing::Drifted`] claim yields a [`Transition::Drifted`],
/// - a [`Standing::Stale`] claim yields a [`Transition::Stale`], and
/// - every skip on the claim whose `until` has lapsed as of `now` yields a
///   [`Transition::LapsedSkip`], **regardless of the claim's standing** — a lapsed skip is
///   a due-again check even on an otherwise-verified claim.
///
/// A [`Standing::Verified`], [`Standing::Suspect`], or [`Standing::Retired`] claim yields
/// no drift/stale transition (suspect and retired are surfaced elsewhere; verified is good
/// news), but a lapsed skip on any of them is still a transition — the skip's debt came
/// due no matter the claim's overall standing.
#[must_use]
pub fn pending_transitions(model: &ReadModel, now: Timestamp) -> Vec<PendingTransition> {
    let mut out = Vec::new();
    // Iterate the model's sorted claim map so the output order is deterministic.
    for standing in model.claims.values() {
        match standing.standing {
            Standing::Drifted => out.push(PendingTransition {
                store: standing.store.clone(),
                claim: standing.id.clone(),
                transition: Transition::Drifted,
                commit: drift_commit(standing),
                check_digest: String::new(),
            }),
            Standing::Stale => out.push(PendingTransition {
                store: standing.store.clone(),
                claim: standing.id.clone(),
                transition: Transition::Stale,
                commit: String::new(),
                check_digest: String::new(),
            }),
            Standing::Verified | Standing::Suspect | Standing::Retired => {}
        }
        for skip in &standing.skips {
            if skip.has_lapsed(now) {
                out.push(PendingTransition {
                    store: standing.store.clone(),
                    claim: standing.id.clone(),
                    transition: Transition::LapsedSkip,
                    commit: String::new(),
                    check_digest: skip.check_digest.clone(),
                });
            }
        }
    }
    out
}

/// The commit a drifted claim's nag groups on.
///
/// The [`ClaimStanding`] does not carry the breaking commit directly (it is derived from
/// the ledger, which the router holds separately), so the router overrides this with the
/// real drift commit from the ledger before grouping; the standing-derived default here is
/// empty, which groups all of one store's un-attributed drifts together only when the
/// router cannot supply a commit — a conservative fallback that still fires, never silent.
fn drift_commit(_standing: &ClaimStanding) -> String {
    String::new()
}

/// Fold pending transitions into [`NagGroup`]s: drifts grouped by (store, commit), others
/// per-claim.
///
/// The grouping rule (HUB.md §3): one commit breaking many claims is one item. Drift
/// transitions sharing a (store, commit) collapse into one group whose `claims` lists them
/// all; a stale or lapsed-skip transition is its own group. `resolve` supplies each claim's
/// statement and supports (and, for a drift, is how the router injects the real breaking
/// commit — see the router). The result is sorted for determinism.
///
/// `resolve` returns the [`DriftedClaim`] detail for a (store, claim, commit), reading the
/// registry; it is a closure so this fold stays pure and the registry IO lives in the
/// caller.
#[must_use]
pub fn group_transitions(
    pending: &[PendingTransition],
    mut resolve: impl FnMut(&PendingTransition) -> DriftedClaim,
) -> Vec<NagGroup> {
    // Drifts keyed by (store, commit); others keyed by (store, claim, transition, digest)
    // so each is its own group. A BTreeMap keeps the output deterministic.
    let mut groups: BTreeMap<(String, Transition, String), Vec<DriftedClaim>> = BTreeMap::new();
    // Remember each group's anchoring commit for display (the same for a drift group; the
    // per-claim commit otherwise).
    let mut commits: BTreeMap<(String, Transition, String), String> = BTreeMap::new();

    for pt in pending {
        let detail = resolve(pt);
        let group_subject = match pt.transition {
            Transition::Drifted => pt.commit.clone(),
            Transition::Stale => pt.claim.clone(),
            Transition::LapsedSkip => format!("{}\u{1f}{}", pt.claim, pt.check_digest),
        };
        let key = (pt.store.clone(), pt.transition, group_subject);
        commits
            .entry(key.clone())
            .or_insert_with(|| pt.commit.clone());
        groups.entry(key).or_default().push(detail);
    }

    groups
        .into_iter()
        .map(|((store, transition, subject), mut claims)| {
            claims.sort_by(|a, b| a.id.cmp(&b.id));
            let commit = commits
                .get(&(store.clone(), transition, subject.clone()))
                .cloned()
                .unwrap_or_default();
            let fire_key = FireKey::compute(&store, transition, &subject);
            NagGroup {
                store,
                transition,
                commit,
                claims,
                fire_key_str: fire_key.as_str().to_owned(),
            }
        })
        .collect()
}

/// The set of skips that have lapsed on a standing as of `now`, for the router to render.
///
/// A thin filter over [`ClaimStanding::skips`], surfaced so the router can name each
/// lapsed skip's check without re-deriving. Returned in the standing's declared skip order.
#[must_use]
pub fn lapsed_skips(standing: &ClaimStanding, now: Timestamp) -> Vec<&SkipAge> {
    standing
        .skips
        .iter()
        .filter(|skip| skip.has_lapsed(now))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deriver::{ClaimStanding, Standing};

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn standing(id: &str, store: &str, standing: Standing) -> ClaimStanding {
        ClaimStanding {
            id: id.to_owned(),
            store: store.to_owned(),
            standing,
            verified_as_of: None,
            stale_at: None,
            due_at: None,
            skips: Vec::new(),
        }
    }

    fn model_of(standings: Vec<ClaimStanding>) -> ReadModel {
        use crate::deriver::AsOf;
        let mut claims = BTreeMap::new();
        for s in standings {
            claims.insert((s.store.clone(), s.id.clone()), s);
        }
        ReadModel {
            as_of: AsOf {
                ledger_head: 1,
                registry_version: 1,
                clock: ts("2026-07-18T00:00:00Z"),
            },
            claims,
            due: Vec::new(),
            horizon: None,
        }
    }

    #[test]
    fn a_fire_key_is_deterministic_and_distinguishes_its_parts() {
        let a = FireKey::compute("s", Transition::Drifted, "abc");
        let b = FireKey::compute("s", Transition::Drifted, "abc");
        assert_eq!(a, b, "same inputs, same key");
        // A different store, transition, or subject each yields a different key.
        assert_ne!(a, FireKey::compute("s2", Transition::Drifted, "abc"));
        assert_ne!(a, FireKey::compute("s", Transition::Stale, "abc"));
        assert_ne!(a, FireKey::compute("s", Transition::Drifted, "abd"));
        assert_eq!(a.as_str().len(), 64, "hex sha-256 is 64 chars");
    }

    #[test]
    fn the_length_prefix_stops_a_boundary_forgery() {
        // Without length-prefixing, ("ab","c") and ("a","bc") could hash equal. The prefix
        // makes the split unambiguous, so two genuinely different subjects never collide.
        let a = FireKey::compute("ab", Transition::Drifted, "c");
        let b = FireKey::compute("a", Transition::Drifted, "bc");
        assert_ne!(a, b, "a boundary shift is a different fire key");
    }

    #[test]
    fn a_drifted_claim_yields_a_drift_transition() {
        let model = model_of(vec![standing("t", "s", Standing::Drifted)]);
        let pending = pending_transitions(&model, ts("2026-07-18T00:00:00Z"));
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].transition, Transition::Drifted);
        assert_eq!(pending[0].claim, "t");
    }

    #[test]
    fn a_stale_claim_yields_a_stale_transition() {
        let model = model_of(vec![standing("t", "s", Standing::Stale)]);
        let pending = pending_transitions(&model, ts("2026-07-18T00:00:00Z"));
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].transition, Transition::Stale);
    }

    #[test]
    fn verified_suspect_and_retired_yield_no_drift_or_stale_transition() {
        let model = model_of(vec![
            standing("v", "s", Standing::Verified),
            standing("u", "s", Standing::Suspect),
            standing("r", "s", Standing::Retired),
        ]);
        let pending = pending_transitions(&model, ts("2026-07-18T00:00:00Z"));
        assert!(
            pending.is_empty(),
            "no transition for good/other news: {pending:?}"
        );
    }

    #[test]
    fn a_lapsed_skip_fires_even_on_a_verified_claim() {
        // A lapsed skip is a due-again check regardless of the claim's overall standing —
        // the debt came due (invariant #6). A verified claim with a lapsed skip still fires.
        let mut s = standing("t", "s", Standing::Verified);
        s.skips.push(SkipAge {
            check_digest: "d".repeat(64),
            reason: "parked".into(),
            until: Some(ts("2026-07-01T00:00:00Z")),
        });
        let model = model_of(vec![s]);
        // Before the until: no transition.
        let before = pending_transitions(&model, ts("2026-06-01T00:00:00Z"));
        assert!(before.is_empty(), "the skip has not lapsed yet");
        // After the until: a lapsed-skip transition.
        let after = pending_transitions(&model, ts("2026-07-02T00:00:00Z"));
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].transition, Transition::LapsedSkip);
        assert_eq!(after[0].check_digest, "d".repeat(64));
    }

    #[test]
    fn one_commit_breaking_n_claims_is_one_group() {
        // The grouping rule: three claims broken by one commit collapse into one nag item.
        let pending = vec![
            PendingTransition {
                store: "s".into(),
                claim: "a".into(),
                transition: Transition::Drifted,
                commit: "deadbeef".into(),
                check_digest: String::new(),
            },
            PendingTransition {
                store: "s".into(),
                claim: "b".into(),
                transition: Transition::Drifted,
                commit: "deadbeef".into(),
                check_digest: String::new(),
            },
            PendingTransition {
                store: "s".into(),
                claim: "c".into(),
                transition: Transition::Drifted,
                commit: "deadbeef".into(),
                check_digest: String::new(),
            },
        ];
        let groups = group_transitions(&pending, |pt| DriftedClaim {
            id: pt.claim.clone(),
            commit: pt.commit.clone(),
            statement: String::new(),
            supports: Vec::new(),
        });
        assert_eq!(groups.len(), 1, "one commit → one group, got {groups:?}");
        assert_eq!(
            groups[0].claims.len(),
            3,
            "all three claims in the one group"
        );
        // Claims are sorted, and the primary is the first.
        assert_eq!(groups[0].primary_claim(), "a");
    }

    #[test]
    fn two_commits_breaking_claims_are_two_groups() {
        let pending = vec![
            PendingTransition {
                store: "s".into(),
                claim: "a".into(),
                transition: Transition::Drifted,
                commit: "c1".into(),
                check_digest: String::new(),
            },
            PendingTransition {
                store: "s".into(),
                claim: "b".into(),
                transition: Transition::Drifted,
                commit: "c2".into(),
                check_digest: String::new(),
            },
        ];
        let groups = group_transitions(&pending, |pt| DriftedClaim {
            id: pt.claim.clone(),
            commit: pt.commit.clone(),
            statement: String::new(),
            supports: Vec::new(),
        });
        assert_eq!(groups.len(), 2, "two commits → two groups");
        // Their fire keys differ (different subjects).
        assert_ne!(groups[0].fire_key(), groups[1].fire_key());
    }

    #[test]
    fn stale_transitions_are_per_claim_not_grouped() {
        let pending = vec![
            PendingTransition {
                store: "s".into(),
                claim: "a".into(),
                transition: Transition::Stale,
                commit: String::new(),
                check_digest: String::new(),
            },
            PendingTransition {
                store: "s".into(),
                claim: "b".into(),
                transition: Transition::Stale,
                commit: String::new(),
                check_digest: String::new(),
            },
        ];
        let groups = group_transitions(&pending, |pt| DriftedClaim {
            id: pt.claim.clone(),
            commit: pt.commit.clone(),
            statement: String::new(),
            supports: Vec::new(),
        });
        assert_eq!(groups.len(), 2, "each stale claim is its own group");
    }

    #[test]
    fn a_lapsed_skips_subject_includes_the_check_so_two_skips_are_distinct() {
        // Two skips on one claim, each lapsed, are two distinct transitions with distinct
        // fire keys — one must not suppress the other.
        let a = PendingTransition {
            store: "s".into(),
            claim: "t".into(),
            transition: Transition::LapsedSkip,
            commit: String::new(),
            check_digest: "aaaa".into(),
        };
        let b = PendingTransition {
            store: "s".into(),
            claim: "t".into(),
            transition: Transition::LapsedSkip,
            commit: String::new(),
            check_digest: "bbbb".into(),
        };
        assert_ne!(a.fire_key(), b.fire_key());
    }
}
