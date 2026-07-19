//! The deriver: the hub's read model as a pure function of its inputs.
//!
//! Everything the hub *appears* to hold — a claim's standing, its freshness, the
//! due set, skip ages, the review queue — is not stored anywhere. It is derived,
//! at read time, from three inputs and a clock:
//!
//! ```text
//! read model = derive(registry snapshot, ledger events, clock, config)
//! ```
//!
//! This is invariant #3 (derive, don't store) made load-bearing (HUB.md §2): there
//! is no status column anything must remember to update, so there is no path to a
//! stored status quietly disagreeing with the evidence. A wrong cache is discarded
//! and recomputed from the log; wrong *truth* would be forever.
//!
//! The whole module is pure: no IO, no async, no network, no wall clock. The clock
//! is always a parameter ([`derive()`] takes a `now: Timestamp`), so every answer is
//! reproducible and every test sets time explicitly (CLAUDE.md's determinism rule).
//!
//! Three properties are load-bearing and proven by the property tests
//! (`tests/deriver_props.rs`):
//!
//! - **No combination of events manufactures a green** (invariant #1). The join
//!   across a claim's checks is conservative — *bad news dominates* — so a drifted,
//!   broken, unverifiable, or overdue check can never be out-voted into a pass by a
//!   held one.
//! - **A shallow check's pass never clears a deep check's drift** (issue #18). The
//!   join keys each check's history on its content digest ([`crate::check_digest`]),
//!   so one check's `held` satisfies only *its own* ledger position, never another's.
//! - **`broken` counts against freshness exactly like never-checked** (invariant
//!   #1). A `Broken` or `Unverifiable` latest verdict is not a passing verdict, so
//!   it neither freshens the claim nor is it treated as a drift; the claim ages into
//!   [`Standing::Stale`] the same as one that was never verified.
//!
//! Freshness is *arithmetic over the clock*, not an event: a claim crosses into
//! stale when `now` passes `latest_pass + max_age`, with no new event required — the
//! next read reports it, the way a certificate expires (HUB.md §3). Because that
//! transition is silent, [`ReadModel`] records the [`horizon`](ReadModel::horizon):
//! the earliest future instant at which any claim's answer changes, so the memo
//! ([`crate::memo`]) knows when a cached read must recompute even though no input
//! changed.

use std::collections::BTreeMap;

use claim_core::{Claim, Days, Timestamp, Verdict};
use serde::{Deserialize, Serialize};

use crate::check_digest;
use crate::envelope::{Event, EventKind};

/// One claim as the deriver joins over it at a store's tip: its id, the store it
/// lives in, its per-check identities and skips, and its `hub:` freshness hints.
///
/// The registry ([`RegistrySnapshot`]) is the hub's mirror of git — every claim file
/// at the tip of its default branch — so a claim's derivation-relevant shape reaches
/// the deriver through this type. It deliberately carries the **already-computed check
/// digests** (the join's keys) rather than the checks' full definitions, so the type is
/// buildable from what the storage layer holds — the registry stores each check's
/// content digest, not its source. This is the integration seam hub-07 wires: a store
/// builds a `ClaimEntry` from its `RegisteredClaim` with [`new`](ClaimEntry::new), and a
/// caller holding a parsed [`Claim`] builds one with [`from_claim`](ClaimEntry::from_claim),
/// which computes each digest with the one canonical [`crate::check_digest`] the registry
/// and ingest also use — so the deriver's per-check join keys match the ledger events'
/// digests by construction, never by a recomputation that could diverge.
///
/// The `store` is the connected store the claim lives in (e.g.
/// `github.com/acme/payments`), matched against an [`Event`]'s `store`+`claim` to
/// attribute a verdict to this entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimEntry {
    /// The connected store this claim lives in, as it appears on an event's
    /// `store` field.
    pub store: String,
    /// The claim's id, as events reference it (`claim` on the envelope).
    pub id: String,
    /// Each check's derivation-relevant facts, in the claim's declared check order:
    /// the content digest the join keys verdict history on, and the declared skip.
    /// The order matters only for surfacing; identity is the digest.
    pub checks: Vec<DerivedCheck>,
    /// The claim's `hub:` freshness hints (`recheck`, `max-age`), used to compute
    /// due-ness and staleness. A hub config override wins over these (see
    /// [`DeriverConfig`]); absent both, the config default (if any) governs.
    pub hub: HubHints,
}

/// One check's facts the deriver folds: its content identity and any declared skip.
///
/// The digest is [`crate::check_digest`] of the check's canonical definition — the
/// ledger's join key (issue #18), so a verdict lands only on the check whose definition
/// it was reported against. The skip is carried for the review queue (it is never a
/// pass); a skip's presence does not freshen a claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedCheck {
    /// The check's canonical content digest — the identity the ledger keys on.
    pub digest: String,
    /// The declared skip on this check, if any. `None` — the common case — means the
    /// check always runs.
    pub skip: Option<DerivedSkip>,
}

/// The skip data the deriver surfaces on a standing for the review queue.
///
/// A skip is an acknowledged, bounded debt, never a pass ([`SkipAge`] is what the read
/// model exposes it as). This carries the two fields the queue reads — the reason and
/// the optional expiry — decoupled from `claim-core`'s `Skip` so the deriver's input
/// stays buildable from stored data (the store persists these fields, not a `Skip`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedSkip {
    /// The author's justification (`skip.reason`).
    pub reason: String,
    /// The skip's expiry, if it declared one. `None` is an indefinite skip.
    pub until: Option<Timestamp>,
}

/// A claim's `hub:` freshness hints, as the deriver reads them.
///
/// The frozen v1 hint set (HUB-IMPLEMENTATION.md §4.5): `recheck` cadence and `max-age`
/// freshness window, both optional day counts. A plain struct so the store can build it
/// from stored day counts and the parser-produced [`claim_core::Hub`] maps onto it
/// directly (see [`ClaimEntry::from_claim`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HubHints {
    /// The recheck cadence hint: how often the hub should re-check this claim. `None`
    /// when the claim declares none.
    pub recheck: Option<Days>,
    /// The freshness-window hint: how long a passing check keeps the claim fresh. `None`
    /// when the claim declares none; the config default (if any) then governs.
    pub max_age: Option<Days>,
}

impl ClaimEntry {
    /// A claim entry from already-derived facts, as the storage layer builds it.
    ///
    /// The `id` and `store` are the claim's identity; `checks` carries each check's
    /// stored digest and skip in declared order; `hub` its freshness hints. The digests
    /// must be the canonical [`crate::check_digest`] of the checks — the storage layer
    /// gets them from registry sync, which computed them with that same function, so the
    /// join keys match the ledger by construction.
    #[must_use]
    pub fn new(
        store: impl Into<String>,
        id: impl Into<String>,
        checks: Vec<DerivedCheck>,
        hub: HubHints,
    ) -> Self {
        Self {
            store: store.into(),
            id: id.into(),
            checks,
            hub,
        }
    }

    /// A claim entry from a parsed [`Claim`], computing each check's digest.
    ///
    /// The convenience for a caller that holds the parsed claim (the deriver's own
    /// tests, and any path that reads a store through `claim-core`): it computes each
    /// check's [`crate::check_digest`] — the identical function the registry and ingest
    /// use — extracts each check's skip, and maps the claim's `hub:` hints onto
    /// [`HubHints`]. Using one digest function everywhere is what keeps a check's
    /// identity the same across the registry, the ingest gate, and the deriver.
    #[must_use]
    pub fn from_claim(store: impl Into<String>, claim: &Claim) -> Self {
        let checks = claim
            .checks
            .iter()
            .map(|check| DerivedCheck {
                digest: check_digest(check),
                skip: check.skip.as_ref().map(|skip| DerivedSkip {
                    reason: skip.reason.clone(),
                    until: skip.until,
                }),
            })
            .collect();
        Self::new(
            store,
            claim.id.as_str().to_owned(),
            checks,
            HubHints {
                recheck: claim.hub.recheck,
                max_age: claim.hub.max_age,
            },
        )
    }

    /// The claim's id string, as events reference it (`claim` on the envelope).
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// The registry snapshot: the hub's live set of claims plus a version counter.
///
/// The registry is derived data (HUB.md §2): delete it and a re-scan rebuilds it.
/// Its `version` is the deriver's second memo-invalidation cause — every sync that
/// changes the live set (a new claim, an edited check, a retirement) bumps it, so a
/// registry change invalidates a cached read the same as a new event does. A claim
/// *absent* from `claims` but present in the ledger is a retirement: its standing
/// is [`Standing::Retired`], its history still renderable from the ledger.
///
/// The `version` is supplied by the storage layer (hub-05's sync), not derived
/// here; this crate only reads it to key the memo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrySnapshot {
    /// A monotonic counter the storage layer bumps on every sync that changes the
    /// live set. Two snapshots with the same `version` are the same registry.
    pub version: u64,
    /// The live set: every claim at its store's tip. Order is not significant —
    /// the deriver keys claims by (store, id).
    pub claims: Vec<ClaimEntry>,
}

/// The deriver-facing configuration: the defaults and overrides the freshness and
/// due-ness arithmetic needs.
///
/// This is deliberately minimal — only what the deriver consumes. The real TOML
/// config is hub-03's; it will map onto this. The `hub:` hint set is frozen at
/// `recheck` + `max-age` (HUB-IMPLEMENTATION.md §4.5), and the deriver honors a
/// claim's own hints, falling back to these defaults when a claim omits one.
///
/// Config is an input to `derive`, so its [`hash`](DeriverConfig::hash) is the
/// third memo-invalidation cause: a config change (a different default max-age)
/// invalidates cached answers like any other input change.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeriverConfig {
    /// The freshness window applied to a claim that declares no `hub.max-age`. When
    /// `None`, a claim with no `max-age` (neither its own nor this default) is never
    /// aged into stale *by the clock* — it can still be [`Standing::Stale`] for
    /// never having verified. This mirrors the CLI's stance: absent a window, the
    /// hub does not invent one.
    pub default_max_age: Option<Days>,
    /// Per-hub override of a claim's `hub.max-age`, applied to every claim
    /// regardless of what it declares. When `Some`, this wins over both the claim's
    /// own hint and [`default_max_age`](DeriverConfig::default_max_age) — the hub
    /// operator's word on cadence is final (HUB.md §3: "applies per-hub config
    /// overrides"). When `None`, the claim's own hint (then the default) governs.
    pub max_age_override: Option<Days>,
}

impl DeriverConfig {
    /// A stable content hash of this config, for the memo key.
    ///
    /// Two configs that would produce different derived answers must hash
    /// differently, so the memo recomputes when config changes. The hash covers
    /// every field the derivation reads; adding a field the derivation consumes
    /// obliges extending this, or a config change could silently serve a stale
    /// cached answer.
    #[must_use]
    pub fn hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        // `Days` is `Hash`; `Option<Days>` composes. Hashing the day count keeps
        // the key stable across processes on the same std version, which is all the
        // memo (an in-process cache) needs — it is never persisted or compared
        // across builds.
        self.default_max_age.map(Days::get).hash(&mut h);
        self.max_age_override.map(Days::get).hash(&mut h);
        h.finish()
    }

    /// The effective freshness window for a claim: the override if set, else the
    /// claim's own `hub.max-age`, else the configured default.
    ///
    /// `None` means no window applies — the claim never ages into stale by the
    /// clock (though it is still stale until it has ever passed).
    fn effective_max_age(&self, hub: &HubHints) -> Option<Days> {
        self.max_age_override
            .or(hub.max_age)
            .or(self.default_max_age)
    }
}

/// The effective recheck cadence for a claim: its own `hub.recheck`, and nothing
/// else.
///
/// A free function, not a `DeriverConfig` method, because v1 config deliberately
/// does *not* override the recheck cadence — recheck and max-age are distinct hints
/// (HUB-IMPLEMENTATION.md §4.5), and the config's `max_age_override`/`default_max_age`
/// have no recheck counterpart. So this reads only the claim, and taking `&config`
/// it would ignore would be a false parallel with [`DeriverConfig::effective_max_age`]
/// (which genuinely needs three sources). `None` means the claim declares no cadence
/// and the deriver computes no due instant for it.
fn effective_recheck(hub: &HubHints) -> Option<Days> {
    hub.recheck
}

/// The derivation's provenance: the exact inputs a read model was computed from.
///
/// Every displayed standing carries its as-of (HUB.md §4), so the hub can never
/// show a green older than its evidence and an agent can cache, diff, and resume.
/// Two derivations with the same `AsOf` produce the same answers — reads are
/// deterministic in (ledger position, registry version, clock).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AsOf {
    /// The ledger head sequence the derivation saw: the seq of the last event in
    /// the input slice, or `0` for an empty ledger. Ledger positions start at 1, so
    /// `0` unambiguously means "no events", matching what `/status` and `/api/feed`
    /// report — one integer contract across every surface, never a `null` on one and
    /// a number on another. Pagination and the memo key on this, never on a wall-clock
    /// time.
    pub ledger_head: u64,
    /// The registry version the derivation read.
    pub registry_version: u64,
    /// The clock instant the derivation used. A standing's freshness is arithmetic
    /// against this instant, so it is part of the answer's identity.
    pub clock: Timestamp,
}

/// A claim's standing: the conservative verdict over all its checks and their
/// freshness.
///
/// The variants are ordered by *severity of the news* only for readers; the join
/// does not rely on a numeric order but on an explicit "bad news dominates" rule
/// (the crate-internal `join`). Every variant is a distinct answer a surface renders
/// and a router may act on.
///
/// `#[non_exhaustive]` reserves the enum for growth: a later deriver rule (windowed
/// claims, spot-audit) may add a variant, and forcing consumers to match
/// exhaustively means a new standing can never default to a silent green.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Standing {
    /// Every check's latest verdict holds and the claim is within its freshness
    /// window. The only standing that is "good news"; reachable only when no check
    /// is drifted, broken, unverifiable, overdue, or never-verified.
    Verified,
    /// The claim has no drifted check, but at least one check is overdue for
    /// re-verification, was never verified, or last reported `broken`/`unverifiable`
    /// — freshness has lapsed. Counts for a nag exactly like a drift would
    /// (invariant #6), without asserting the fact is false.
    Stale,
    /// At least one check's latest verdict is `drifted`: the fact is known false
    /// right now. Bad news dominates, so one drift makes the whole claim drifted
    /// regardless of how many other checks hold.
    Drifted,
    /// A dependent of a drifted claim, flagged for a look because the decision it
    /// rested on may no longer hold. The *propagation rule* (which claims become
    /// suspect, over the supports graph) is a later item; this variant is defined
    /// now so the standing model and every surface already carry it, and the rule
    /// slots in without a schema change.
    Suspect,
    /// The claim is absent from the registry's live set — deleted from git — but has
    /// history in the ledger. A retirement, not a failure: rendered from history,
    /// never counted as a pass.
    Retired,
}

/// A single check's freshness input to the join: its latest verdict and when it was
/// reported, or the absence of any verdict.
///
/// This is the per-check state the conservative join folds. It is keyed by the
/// check's digest in [`ClaimStanding`]'s computation, so a verdict only ever lands
/// on the check whose definition it was reported against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CheckState {
    /// The check's latest verdict, or `None` if the ledger holds none for this
    /// check's digest — the "never checked" case, which counts against freshness
    /// exactly like `Broken`.
    latest: Option<(Verdict, Timestamp)>,
}

impl CheckState {
    /// The instant of this check's latest *passing* verdict, if its latest verdict
    /// is a pass. A non-pass latest (drifted, broken, unverifiable) yields `None`:
    /// freshness is measured from the last time the check actually confirmed the
    /// fact, and a later non-pass does not extend it.
    fn latest_pass(&self) -> Option<Timestamp> {
        match self.latest {
            Some((v, at)) if v.is_held() => Some(at),
            _ => None,
        }
    }
}

/// A skip's age data, carried on a claim's standing for the review queue.
///
/// Skip *ranking* (ordering skips by age and lapsed `until` in the queue) is a
/// later item (hub-14, issue #9); this type carries the raw data that ranking will
/// consume, so the standing model already surfaces every declared skip. A skip is
/// never a pass — it is an acknowledged, bounded debt (`claim-core`'s `Skip` doc) —
/// so its presence does not freshen a claim; it is reported alongside the standing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkipAge {
    /// The check's digest the skip is declared on, so a surface can name the exact
    /// check being muted.
    pub check_digest: String,
    /// The author's justification (`skip.reason`).
    pub reason: String,
    /// The skip's expiry, if it declared one. A skip whose `until` is at or before
    /// the derivation clock has *lapsed* — the queue ranks a lapsed skip above an
    /// aging one (hub-14). `None` is an indefinite skip, surfaced plainly so an
    /// unbounded mute cannot hide.
    pub until: Option<Timestamp>,
}

impl SkipAge {
    /// Whether this skip has lapsed as of `now`: it declared an `until` and that
    /// instant is at or before the clock. A lapsed skip is a louder queue signal
    /// than an aging one (hub-14's rule); the deriver computes the fact, ranking is
    /// later.
    #[must_use]
    pub fn has_lapsed(&self, now: Timestamp) -> bool {
        self.until.is_some_and(|until| now >= until)
    }
}

/// One claim's full derived standing: its verdict-over-checks, freshness, due-ness,
/// skips, and the inputs it was derived from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClaimStanding {
    /// The claim's id.
    pub id: String,
    /// The store the claim lives in.
    pub store: String,
    /// The conservative standing over all the claim's checks.
    pub standing: Standing,
    /// The instant the claim last passed *every* check within its freshness window —
    /// the "as-of" of the good news. `None` when the claim has never been fully
    /// verified (some check has no passing verdict). A [`Standing::Verified`] claim
    /// always carries `Some`; a stale or drifted one may or may not.
    pub verified_as_of: Option<Timestamp>,
    /// When the claim becomes (or became) stale by the clock: the earliest instant
    /// at which freshness lapses, `latest full-pass + max_age`. `None` when no
    /// freshness window applies, or when the claim is not currently on a
    /// pass-and-waiting track (already stale for a non-clock reason, or drifted).
    /// Used to compute the read model's horizon.
    pub stale_at: Option<Timestamp>,
    /// When the claim is next due for a re-check per its `hub.recheck` cadence:
    /// `latest full-pass + recheck`. `None` when the claim declares no cadence or
    /// has no passing baseline to count from. A claim whose `due_at` is at or before
    /// the clock is in the read model's [due set](ReadModel::due).
    pub due_at: Option<Timestamp>,
    /// The declared skips on this claim's checks, with their age data for the queue.
    /// Empty when the claim declares no skips.
    pub skips: Vec<SkipAge>,
}

/// The derived read model: every claim's standing, the due set, and the horizon.
///
/// This is the whole of what a read surface renders — one derivation, four
/// renderings (HUB.md §5). It carries its [`as_of`](ReadModel::as_of) so a caller
/// knows exactly which inputs produced it, and its [`horizon`](ReadModel::horizon)
/// so the memo knows when a cached copy must be recomputed even though no input
/// changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadModel {
    /// The inputs this model was derived from.
    pub as_of: AsOf,
    /// Every claim's standing, keyed by (store, id) into a sorted map so the read
    /// model is deterministic — the same inputs yield byte-identical output
    /// regardless of registry or ledger ordering.
    pub claims: BTreeMap<(String, String), ClaimStanding>,
    /// The claim keys currently due for a re-check (`due_at <= clock`), plus every
    /// drifted and stale claim — the review queue's membership. Sorted for
    /// determinism.
    pub due: Vec<(String, String)>,
    /// The earliest future instant at which *any* claim's standing changes with no
    /// new event — the soonest `stale_at` or `due_at` strictly after the clock.
    /// `None` when nothing is scheduled to change by the clock alone. The memo
    /// recomputes when a read arrives at or past this instant (HUB.md §2's third
    /// invalidation cause), so a claim aging into stale is reported without a timer.
    pub horizon: Option<Timestamp>,
}

impl ReadModel {
    /// The standing of one claim by (store, id), if the registry holds it.
    #[must_use]
    pub fn standing(&self, store: &str, id: &str) -> Option<&ClaimStanding> {
        self.claims.get(&(store.to_owned(), id.to_owned()))
    }
}

/// Derive the whole read model from the registry, the ledger, the clock, and config.
///
/// This is the hub's one derivation (HUB.md §2), pure and deterministic: the same
/// `(registry, events, now, config)` always produce the same [`ReadModel`], with no
/// IO and no wall clock. `events` is the ledger's events in ascending `seq` order,
/// paired with each event's `seq`; `seq` is only used to report the ledger head in
/// the [`AsOf`] and is otherwise opaque here (the deriver folds *content*, not
/// positions). Events out of `seq` order are still folded correctly — the join
/// takes each check's latest verdict by `reported_at`, not by slice position — but
/// callers pass ledger order by convention.
///
/// The contract the join upholds (proven in `tests/deriver_props.rs`):
/// no combination of events yields [`Standing::Verified`] unless every check's
/// latest verdict is a pass within its freshness window; a check's verdict lands
/// only on the check whose digest it names, so a shallow pass never clears a deep
/// drift; and `broken`/`unverifiable`/never-checked all count against freshness
/// identically.
#[must_use]
pub fn derive(
    registry: &RegistrySnapshot,
    events: &[(u64, Event)],
    now: Timestamp,
    config: &DeriverConfig,
) -> ReadModel {
    let as_of = AsOf {
        ledger_head: events.iter().map(|(seq, _)| *seq).max().unwrap_or(0),
        registry_version: registry.version,
        clock: now,
    };

    // Index the ledger's latest verdict per (store, claim id, check digest). The
    // digest is what stops a shallow check's pass from clearing a deep check's
    // drift: a verdict only ever updates the state of the check it names.
    let latest = index_latest_verdicts(events);

    // Which (store, id) pairs the ledger has *any* history for, so a claim deleted
    // from the registry but present in the ledger derives as Retired rather than
    // vanishing.
    let mut seen_in_ledger: BTreeMap<(String, String), ()> = BTreeMap::new();
    for (store, id, _) in latest.keys() {
        seen_in_ledger.insert((store.clone(), id.clone()), ());
    }

    let mut claims: BTreeMap<(String, String), ClaimStanding> = BTreeMap::new();
    let mut horizon: Option<Timestamp> = None;

    // Every live claim in the registry.
    for entry in &registry.claims {
        let key = (entry.store.clone(), entry.id().to_owned());
        seen_in_ledger.remove(&key);
        let standing = derive_claim(entry, &latest, now, config);
        advance_horizon(&mut horizon, standing.stale_at, now);
        advance_horizon(&mut horizon, standing.due_at, now);
        claims.insert(key, standing);
    }

    // Every claim the ledger knows but the registry no longer does: retired.
    for (store, id) in seen_in_ledger.into_keys() {
        claims.insert(
            (store.clone(), id.clone()),
            ClaimStanding {
                id,
                store,
                standing: Standing::Retired,
                verified_as_of: None,
                stale_at: None,
                due_at: None,
                skips: Vec::new(),
            },
        );
    }

    let due = claims
        .iter()
        .filter(|(_, s)| is_in_queue(s, now))
        .map(|(k, _)| k.clone())
        .collect();

    ReadModel {
        as_of,
        claims,
        due,
        horizon,
    }
}

/// The latest verdict per (store, claim id, check digest), by `reported_at`.
///
/// Only [`EventKind::Verdict`] events carry a verdict; other kinds (a `nag`, later)
/// are skipped here — they do not bear on a claim's standing. Ties in `reported_at`
/// resolve to the later position in the input slice (ledger order), so a redelivered
/// event does not flip a result.
///
/// Identity is *content*, not count: two byte-identical check definitions on one
/// claim share a digest ([`crate::check_digest`]), so a single verdict against that
/// digest verifies both copies, and a drift on either drifts the claim. A reader
/// expecting N checks to need N observations should note this — it is issue #18's
/// content-identity model working as designed, not a missed observation: two checks
/// that verify the *same* thing the same way are one identity to the ledger.
type LatestMap = BTreeMap<(String, String, String), (Verdict, Timestamp)>;

fn index_latest_verdicts(events: &[(u64, Event)]) -> LatestMap {
    let mut latest: LatestMap = BTreeMap::new();
    for (_, event) in events {
        // A match, not an `if let`, so a future `EventKind` forces a decision here
        // rather than silently counting toward — or against — a standing.
        match event.kind {
            EventKind::Verdict => {}
        }
        let key = (
            event.store.clone(),
            event.claim.clone(),
            event.check.digest.clone(),
        );
        let candidate = (event.verdict, event.reported_at);
        latest
            .entry(key)
            .and_modify(|current| {
                // Keep the later report. On an exact tie, the later-seen event wins,
                // matching ledger append order.
                if candidate.1 >= current.1 {
                    *current = candidate;
                }
            })
            .or_insert(candidate);
    }
    latest
}

/// Derive one live claim's standing from its definition and the ledger index.
fn derive_claim(
    entry: &ClaimEntry,
    latest: &LatestMap,
    now: Timestamp,
    config: &DeriverConfig,
) -> ClaimStanding {
    let store = &entry.store;

    // Each check's latest state, keyed by its content digest. The digest is the
    // registry's stored one (or, via `from_claim`, computed by the same
    // `check_digest`), so a verdict lands only on the check whose definition it names.
    let states: Vec<CheckState> = entry
        .checks
        .iter()
        .map(|check| CheckState {
            latest: latest
                .get(&(store.clone(), entry.id.clone(), check.digest.clone()))
                .copied(),
        })
        .collect();

    let max_age = config.effective_max_age(&entry.hub);
    let recheck = effective_recheck(&entry.hub);

    // The freshness baseline: the instant every check last passed. A claim is only
    // as fresh as its *stalest* passing check, and only if every check has passed at
    // all — a single never-passed check leaves the baseline absent (never fully
    // verified).
    let full_pass_baseline = full_pass_baseline(&states, now);

    let standing = join(&states, full_pass_baseline, max_age, now);

    let verified_as_of = match standing {
        Standing::Verified => full_pass_baseline,
        _ => None,
    };

    // `stale_at` and `due_at` are only meaningful while the claim is on a
    // pass-and-waiting track: it has a full-pass baseline and is not already drifted.
    // A drifted or never-fully-verified claim has no clock threshold to cross.
    let (stale_at, due_at) = match (standing, full_pass_baseline) {
        (Standing::Drifted | Standing::Suspect | Standing::Retired, _) | (_, None) => (None, None),
        (_, Some(baseline)) => (
            max_age.and_then(|d| add_days(baseline, d)),
            recheck.and_then(|d| add_days(baseline, d)),
        ),
    };

    let skips = entry
        .checks
        .iter()
        .filter_map(|check| {
            check.skip.as_ref().map(|skip| SkipAge {
                check_digest: check.digest.clone(),
                reason: skip.reason.clone(),
                until: skip.until,
            })
        })
        .collect();

    ClaimStanding {
        id: entry.id.clone(),
        store: store.clone(),
        standing,
        verified_as_of,
        stale_at,
        due_at,
        skips,
    }
}

/// The instant every check last passed — clamped to no later than `now` — or `None`
/// if any check has no passing latest verdict.
///
/// This is the freshness baseline: a claim is fresh from the moment its *stalest*
/// passing check passed, and only if every check has a passing latest at all. A
/// check whose latest verdict is not a pass (drifted, broken, unverifiable) or which
/// has never been checked yields `None` for the whole claim — there is no baseline
/// to age from, which is exactly why `broken` and never-checked count identically
/// against freshness (invariant #1).
///
/// Each passing instant is clamped to `min(reported_at, now)` before it feeds
/// freshness. A verdict's `reported_at` is producer-asserted, so a future-dated pass
/// (clock skew, or a forged timestamp) must never buy freshness a real observation
/// has not earned: clamping caps `verified_as_of`, `stale_at`, and the horizon at the
/// read clock, so a green can never be asserted *before* its own evidence timestamp
/// (invariant #6 — a wrong answer stays loud, never a lie into the future). This is
/// defense in depth: the deriver is pure and trust is the ingest gate's job, so the
/// gate (hub-04) should also reject or clamp a future `reported_at` at the door; the
/// deriver refusing to be fooled by one is the second line.
fn full_pass_baseline(states: &[CheckState], now: Timestamp) -> Option<Timestamp> {
    // The minimum passing instant across all checks; `None` the moment any check
    // lacks a passing latest.
    let mut baseline: Option<Timestamp> = None;
    for state in states {
        // Clamp before folding: a future-dated pass contributes `now`, not its
        // asserted instant, so it can never extend the window past the read clock.
        let pass = state.latest_pass()?.min(now);
        baseline = Some(match baseline {
            Some(current) => current.min(pass),
            None => pass,
        });
    }
    baseline
}

/// The conservative join across a claim's checks: bad news dominates.
///
/// The order of precedence is the whole honesty contract:
///
/// 1. **Any drifted latest → [`Standing::Drifted`].** A known-false check makes the
///    claim false, no matter how many others hold. This is checked first so a drift
///    can never be out-voted.
/// 2. **Otherwise, any non-passing or overdue check → [`Standing::Stale`].** A
///    `broken`/`unverifiable` latest, a never-checked check, or a claim past its
///    freshness window all age the claim into stale — freshness has lapsed but the
///    fact is not asserted false.
/// 3. **Otherwise → [`Standing::Verified`].** Reached only when every check's latest
///    verdict is a pass *and* the claim is within its freshness window.
///
/// [`Standing::Suspect`] and [`Standing::Retired`] are not produced here: suspect is
/// a later propagation rule over the supports graph (the variant is reserved), and
/// retired is decided by registry absence before a claim reaches the join.
///
/// A claim with *no checks* cannot occur — `claim-core` rejects it at parse — so the
/// empty-`states` case (which would fall through to `Verified`) is unreachable from
/// a real registry; it is left as `Verified` only because a vacuous "all checks
/// hold" is the mathematically consistent answer, never a manufactured green for a
/// real claim.
fn join(
    states: &[CheckState],
    full_pass_baseline: Option<Timestamp>,
    max_age: Option<Days>,
    now: Timestamp,
) -> Standing {
    // 1. A single drift dominates.
    if states
        .iter()
        .any(|s| matches!(s.latest, Some((Verdict::Drifted, _))))
    {
        return Standing::Drifted;
    }

    // 2. Any check that is not a pass — broken, unverifiable, or never checked —
    //    means freshness has lapsed. `broken`/`unverifiable`/absent are one class
    //    here (invariant #1: broken counts as never-checked).
    let all_checks_pass = states.iter().all(|s| s.latest_pass().is_some());
    if !all_checks_pass {
        return Standing::Stale;
    }

    // 3. Every check passes; the only remaining question is the clock. Past the
    //    freshness window, the claim is stale (a certificate expiring), even though
    //    no verdict is bad and no new event arrived.
    match freshness_lapsed(full_pass_baseline, max_age, now) {
        true => Standing::Stale,
        false => Standing::Verified,
    }
}

/// Whether the freshness window has lapsed as of `now`: there is a baseline, a
/// window, and `now` is at or past `baseline + window`.
///
/// Boundary: the claim is *fresh* strictly before the expiry instant and *stale* at
/// or after it, matching how a certificate's `notAfter` is inclusive of expiry.
/// With no window (`max_age` is `None`) freshness never lapses by the clock; with no
/// baseline the caller has already decided the claim is stale for another reason.
fn freshness_lapsed(baseline: Option<Timestamp>, max_age: Option<Days>, now: Timestamp) -> bool {
    match (baseline, max_age) {
        (Some(baseline), Some(window)) => match add_days(baseline, window) {
            // Past (or exactly at) the expiry instant is stale.
            Some(expiry) => now >= expiry,
            // Overflow past the representable range of time means the expiry is
            // unreachably far off, so freshness has not lapsed.
            None => false,
        },
        _ => false,
    }
}

/// Add a whole-day window to an instant, as a fixed 24-hour-per-day duration.
///
/// A `hub:` window is a count of whole days measured as fixed 86 400-second
/// intervals from the baseline instant — not a calendar operation, so it needs no
/// time zone and is unambiguous (jiff reserves calendar-day arithmetic for zoned
/// datetimes). Returns `None` only if the sum overflows the representable timestamp
/// range, an operational impossibility a hub tolerates by treating the window as
/// effectively infinite rather than panicking (checked arithmetic, never wrapping —
/// the `jiff` rationale in `claim-core`).
fn add_days(instant: Timestamp, days: Days) -> Option<Timestamp> {
    // `days` is a positive `u32`; `u32::MAX * 86_400` fits comfortably in `i64`, so
    // the multiplication cannot overflow before jiff's own checked add.
    let seconds = i64::from(days.get()) * 86_400;
    instant
        .checked_add(jiff::SignedDuration::from_secs(seconds))
        .ok()
}

/// Fold a candidate future threshold into the running horizon: the earliest instant
/// strictly after `now` at which some claim's answer changes.
///
/// A threshold at or before `now` has already fired (its effect is already in the
/// current standing), so only a *future* threshold advances the horizon. The horizon
/// is the soonest such instant across all claims; a read at or past it must recompute
/// (the memo's clock-crossing cause).
fn advance_horizon(horizon: &mut Option<Timestamp>, candidate: Option<Timestamp>, now: Timestamp) {
    if let Some(t) = candidate {
        if t > now {
            *horizon = Some(match *horizon {
                Some(current) => current.min(t),
                None => t,
            });
        }
    }
}

/// Whether a claim belongs in the review queue as of `now`: it is drifted, stale, or
/// due for a re-check.
///
/// The queue is the union of "needs attention now" states. A [`Standing::Verified`]
/// claim is in the queue only if it is *due* — its cadence says re-check even though
/// it currently holds — so a fresh, not-yet-due claim stays out. Suspect and retired
/// claims are surfaced elsewhere (suspect enters the queue with its later rule;
/// retired is history), so they are not queue members here.
fn is_in_queue(s: &ClaimStanding, now: Timestamp) -> bool {
    match s.standing {
        Standing::Drifted | Standing::Stale => true,
        Standing::Verified => s.due_at.is_some_and(|due| now >= due),
        Standing::Suspect | Standing::Retired => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claim_core::parse_claim_file;

    /// Parse a claim from frontmatter, so tests exercise real parser output.
    fn claim_of(yaml_body: &str) -> Claim {
        let text = format!("---\n{yaml_body}\n---\nStatement.\n");
        parse_claim_file(".claims/t.md", &text).expect("valid claim")
    }

    /// A one-cmd-check claim with the given id and optional hub hints.
    fn simple_claim(id: &str, hub: &str) -> Claim {
        let body = format!("id: {id}\n{hub}checks:\n  - kind: cmd\n    run: \"true\"");
        claim_of(&body)
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    /// A verdict event for the nth check of a claim in a store.
    fn verdict_event(
        store: &str,
        claim: &Claim,
        check_index: usize,
        verdict: Verdict,
        at: &str,
    ) -> Event {
        let check = &claim.checks[check_index];
        let mut producer = serde_json::Map::new();
        producer.insert("run".into(), serde_json::json!("1"));
        Event {
            kind: EventKind::Verdict,
            claim: claim.id.as_str().to_owned(),
            check: crate::CheckRef {
                index: check_index,
                digest: check_digest(check),
            },
            verdict,
            evidence: None,
            commit: "abc".into(),
            store: store.into(),
            producer: crate::Producer(producer),
            reported_at: ts(at),
        }
    }

    fn registry(claims: Vec<(&str, Claim)>) -> RegistrySnapshot {
        RegistrySnapshot {
            version: 1,
            claims: claims
                .into_iter()
                .map(|(store, claim)| ClaimEntry::from_claim(store, &claim))
                .collect(),
        }
    }

    #[test]
    fn a_never_checked_claim_is_stale_not_verified() {
        // No events at all: the claim has never been verified, which counts against
        // freshness exactly like broken (invariant #1). It must not read as verified.
        let claim = simple_claim("t", "");
        let reg = registry(vec![("s", claim)]);
        let model = derive(
            &reg,
            &[],
            ts("2026-07-18T00:00:00Z"),
            &DeriverConfig::default(),
        );
        assert_eq!(model.standing("s", "t").unwrap().standing, Standing::Stale);
    }

    #[test]
    fn a_held_check_within_window_is_verified() {
        let claim = simple_claim("t", "hub:\n  max-age: 30d\n");
        let event = verdict_event("s", &claim, 0, Verdict::Held, "2026-07-01T00:00:00Z");
        let reg = registry(vec![("s", claim)]);
        let model = derive(
            &reg,
            &[(1, event)],
            ts("2026-07-10T00:00:00Z"),
            &DeriverConfig::default(),
        );
        let s = model.standing("s", "t").unwrap();
        assert_eq!(s.standing, Standing::Verified);
        assert_eq!(s.verified_as_of, Some(ts("2026-07-01T00:00:00Z")));
    }

    #[test]
    fn a_claim_crosses_into_stale_by_the_clock_alone() {
        // One held verdict, a 30-day window: verified at day 10, stale at day 40,
        // with no new event — only the clock advanced.
        let claim = simple_claim("t", "hub:\n  max-age: 30d\n");
        let event = verdict_event("s", &claim, 0, Verdict::Held, "2026-07-01T00:00:00Z");
        let reg = registry(vec![("s", claim)]);
        let events = [(1, event)];

        let fresh = derive(
            &reg,
            &events,
            ts("2026-07-10T00:00:00Z"),
            &DeriverConfig::default(),
        );
        assert_eq!(
            fresh.standing("s", "t").unwrap().standing,
            Standing::Verified
        );

        // Exactly at expiry (2026-07-31T00:00:00Z = 2026-07-01 + 30d) it is stale:
        // the boundary is inclusive, like a certificate's notAfter.
        let at_expiry = derive(
            &reg,
            &events,
            ts("2026-07-31T00:00:00Z"),
            &DeriverConfig::default(),
        );
        assert_eq!(
            at_expiry.standing("s", "t").unwrap().standing,
            Standing::Stale
        );
    }

    #[test]
    fn a_drift_dominates_a_hold() {
        // Two checks; one holds, one drifts. Bad news dominates: drifted.
        let claim = claim_of(
            "id: t\nchecks:\n  - kind: cmd\n    run: \"a\"\n  - kind: cmd\n    run: \"b\"",
        );
        let hold = verdict_event("s", &claim, 0, Verdict::Held, "2026-07-01T00:00:00Z");
        let drift = verdict_event("s", &claim, 1, Verdict::Drifted, "2026-07-02T00:00:00Z");
        let reg = registry(vec![("s", claim)]);
        let model = derive(
            &reg,
            &[(1, hold), (2, drift)],
            ts("2026-07-03T00:00:00Z"),
            &DeriverConfig::default(),
        );
        assert_eq!(
            model.standing("s", "t").unwrap().standing,
            Standing::Drifted
        );
    }

    #[test]
    fn a_broken_latest_is_stale_not_verified() {
        // A check that held then broke is stale: broken counts as never-checked, so
        // its passing history does not keep the claim fresh.
        let claim = simple_claim("t", "");
        let hold = verdict_event("s", &claim, 0, Verdict::Held, "2026-07-01T00:00:00Z");
        let broke = verdict_event("s", &claim, 0, Verdict::Broken, "2026-07-02T00:00:00Z");
        let reg = registry(vec![("s", claim)]);
        let model = derive(
            &reg,
            &[(1, hold), (2, broke)],
            ts("2026-07-03T00:00:00Z"),
            &DeriverConfig::default(),
        );
        assert_eq!(model.standing("s", "t").unwrap().standing, Standing::Stale);
    }

    #[test]
    fn a_claim_deleted_from_the_registry_is_retired() {
        // The ledger has a verdict for a claim the registry no longer lists.
        let claim = simple_claim("t", "");
        let event = verdict_event("s", &claim, 0, Verdict::Held, "2026-07-01T00:00:00Z");
        let empty_reg = RegistrySnapshot {
            version: 2,
            claims: vec![],
        };
        let model = derive(
            &empty_reg,
            &[(1, event)],
            ts("2026-07-02T00:00:00Z"),
            &DeriverConfig::default(),
        );
        assert_eq!(
            model.standing("s", "t").unwrap().standing,
            Standing::Retired
        );
    }

    #[test]
    fn the_horizon_is_the_soonest_future_threshold() {
        // Two claims with different windows verified at the same instant: the horizon
        // is the earlier expiry.
        let a = simple_claim("a", "hub:\n  max-age: 10d\n");
        let b = simple_claim("b", "hub:\n  max-age: 30d\n");
        let ea = verdict_event("s", &a, 0, Verdict::Held, "2026-07-01T00:00:00Z");
        let eb = verdict_event("s", &b, 0, Verdict::Held, "2026-07-01T00:00:00Z");
        let reg = registry(vec![("s", a), ("s", b)]);
        let model = derive(
            &reg,
            &[(1, ea), (2, eb)],
            ts("2026-07-05T00:00:00Z"),
            &DeriverConfig::default(),
        );
        // a expires at 2026-07-11, b at 2026-07-31; the horizon is the sooner.
        assert_eq!(model.horizon, Some(ts("2026-07-11T00:00:00Z")));
    }

    #[test]
    fn a_config_max_age_override_wins_over_the_claims_own() {
        // The claim declares 30d; the hub overrides to 5d. At day 10 the override
        // makes it stale though its own window would keep it fresh.
        let claim = simple_claim("t", "hub:\n  max-age: 30d\n");
        let event = verdict_event("s", &claim, 0, Verdict::Held, "2026-07-01T00:00:00Z");
        let reg = registry(vec![("s", claim)]);
        let config = DeriverConfig {
            default_max_age: None,
            max_age_override: Some("5d".parse().unwrap()),
        };
        let model = derive(&reg, &[(1, event)], ts("2026-07-10T00:00:00Z"), &config);
        assert_eq!(model.standing("s", "t").unwrap().standing, Standing::Stale);
    }

    #[test]
    fn a_default_max_age_applies_when_a_claim_declares_none() {
        let claim = simple_claim("t", "");
        let event = verdict_event("s", &claim, 0, Verdict::Held, "2026-07-01T00:00:00Z");
        let reg = registry(vec![("s", claim)]);
        let config = DeriverConfig {
            default_max_age: Some("7d".parse().unwrap()),
            max_age_override: None,
        };
        // Within the default window: verified.
        let fresh = derive(
            &reg,
            &[(1, event.clone())],
            ts("2026-07-05T00:00:00Z"),
            &config,
        );
        assert_eq!(
            fresh.standing("s", "t").unwrap().standing,
            Standing::Verified
        );
        // Past it: stale by the clock.
        let stale = derive(&reg, &[(1, event)], ts("2026-07-09T00:00:00Z"), &config);
        assert_eq!(stale.standing("s", "t").unwrap().standing, Standing::Stale);
    }

    #[test]
    fn with_no_window_a_passing_claim_stays_verified_forever() {
        // No max-age anywhere: the claim never ages into stale by the clock. It is
        // verified as long as its check's latest verdict passes.
        let claim = simple_claim("t", "");
        let event = verdict_event("s", &claim, 0, Verdict::Held, "2020-01-01T00:00:00Z");
        let reg = registry(vec![("s", claim)]);
        let model = derive(
            &reg,
            &[(1, event)],
            ts("2099-01-01T00:00:00Z"),
            &DeriverConfig::default(),
        );
        assert_eq!(
            model.standing("s", "t").unwrap().standing,
            Standing::Verified
        );
        assert!(model.horizon.is_none(), "nothing expires, so no horizon");
    }

    #[test]
    fn the_due_set_holds_drifted_stale_and_due_claims() {
        // A drifted claim and a fresh-but-due claim are both in the queue; a fresh,
        // not-yet-due claim is not.
        let drifting = simple_claim("d", "");
        let recheck = simple_claim("r", "hub:\n  recheck: 7d\n");
        let fresh = simple_claim("f", "hub:\n  recheck: 30d\n");
        let ed = verdict_event("s", &drifting, 0, Verdict::Drifted, "2026-07-01T00:00:00Z");
        let er = verdict_event("s", &recheck, 0, Verdict::Held, "2026-07-01T00:00:00Z");
        let ef = verdict_event("s", &fresh, 0, Verdict::Held, "2026-07-01T00:00:00Z");
        let reg = registry(vec![("s", drifting), ("s", recheck), ("s", fresh)]);
        // Day 10: r is due (7d cadence lapsed), f is not (30d cadence).
        let model = derive(
            &reg,
            &[(1, ed), (2, er), (3, ef)],
            ts("2026-07-11T00:00:00Z"),
            &DeriverConfig::default(),
        );
        let due: Vec<_> = model.due.iter().map(|(_, id)| id.as_str()).collect();
        assert!(due.contains(&"d"), "drifted is queued");
        assert!(due.contains(&"r"), "due-for-recheck is queued");
        assert!(!due.contains(&"f"), "fresh not-yet-due is not queued");
    }

    #[test]
    fn skips_are_carried_on_the_standing() {
        let claim = claim_of(
            "id: t\nchecks:\n  - kind: cmd\n    run: \"x\"\n    skip:\n      reason: parked\n      until: 2030-01-01",
        );
        let reg = registry(vec![("s", claim)]);
        let model = derive(
            &reg,
            &[],
            ts("2026-07-18T00:00:00Z"),
            &DeriverConfig::default(),
        );
        let s = model.standing("s", "t").unwrap();
        assert_eq!(s.skips.len(), 1);
        assert_eq!(s.skips[0].reason, "parked");
        assert!(!s.skips[0].has_lapsed(ts("2026-07-18T00:00:00Z")));
        assert!(s.skips[0].has_lapsed(ts("2031-01-01T00:00:00Z")));
    }

    #[test]
    fn read_model_is_deterministic_regardless_of_input_order() {
        // The same claims and events in different orders derive to byte-identical
        // models (BTreeMap keys, sorted due set): determinism is structural.
        let a = simple_claim("a", "hub:\n  max-age: 30d\n");
        let b = simple_claim("b", "hub:\n  max-age: 30d\n");
        let ea = verdict_event("s", &a, 0, Verdict::Held, "2026-07-01T00:00:00Z");
        let eb = verdict_event("s", &b, 0, Verdict::Held, "2026-07-02T00:00:00Z");

        let reg1 = registry(vec![("s", a.clone()), ("s", b.clone())]);
        let reg2 = registry(vec![("s", b), ("s", a)]);
        let now = ts("2026-07-10T00:00:00Z");
        let m1 = derive(
            &reg1,
            &[(1, ea.clone()), (2, eb.clone())],
            now,
            &DeriverConfig::default(),
        );
        let m2 = derive(&reg2, &[(2, eb), (1, ea)], now, &DeriverConfig::default());

        // Same claim standings and due set; only the ledger_head max differs not at
        // all (both slices carry seqs 1 and 2).
        assert_eq!(m1.claims, m2.claims);
        assert_eq!(m1.due, m2.due);
        assert_eq!(m1.as_of.ledger_head, m2.as_of.ledger_head);
    }

    #[test]
    fn a_future_dated_pass_does_not_read_fresh_into_the_future() {
        // A producer asserts `reported_at: 2027-01-01` (clock skew or a forgery), read
        // at `now: 2026-07-01`. The pass is clamped to `now`, so freshness is measured
        // from the read clock, not the future timestamp: `verified_as_of`, `stale_at`,
        // and the horizon are all capped at `now`. A green is never asserted before its
        // own evidence timestamp.
        let claim = simple_claim("t", "hub:\n  max-age: 30d\n");
        let future = verdict_event("s", &claim, 0, Verdict::Held, "2027-01-01T00:00:00Z");
        let reg = registry(vec![("s", claim)]);
        let now = ts("2026-07-01T00:00:00Z");
        let model = derive(&reg, &[(1, future)], now, &DeriverConfig::default());
        let s = model.standing("s", "t").unwrap();
        // Still verified (the check does hold), but the good news is dated at the read
        // clock, not the future — no fresh-into-the-future.
        assert_eq!(s.standing, Standing::Verified);
        assert_eq!(
            s.verified_as_of,
            Some(now),
            "verified_as_of is clamped to the read clock, not the future timestamp"
        );
        // The window ages from `now`, so stale_at is now + 30d, never now + (future +
        // 30d): the future timestamp bought no extra freshness.
        assert_eq!(s.stale_at, Some(ts("2026-07-31T00:00:00Z")));
        // The horizon is a real future instant relative to the read clock, not one
        // pushed past it by the forged timestamp.
        assert_eq!(model.horizon, Some(ts("2026-07-31T00:00:00Z")));
    }

    #[test]
    fn an_all_skipped_claim_is_stale_not_an_accidental_green() {
        // A claim whose only check is skipped records no verdict — a skip is an
        // acknowledged debt, never a pass (invariant #6). With no passing verdict the
        // claim has no freshness baseline, so it derives Stale, never a green. The skip
        // is still surfaced on the standing for the queue.
        let claim = claim_of(
            "id: t\nchecks:\n  - kind: cmd\n    run: \"x\"\n    skip:\n      reason: parked",
        );
        let reg = registry(vec![("s", claim)]);
        // No events at all — the skip suppressed the only check, so nothing was
        // reported.
        let model = derive(
            &reg,
            &[],
            ts("2026-07-18T00:00:00Z"),
            &DeriverConfig::default(),
        );
        let s = model.standing("s", "t").unwrap();
        assert_eq!(
            s.standing,
            Standing::Stale,
            "an all-skipped claim is stale, never an accidental green"
        );
        assert_eq!(s.verified_as_of, None);
        assert_eq!(s.skips.len(), 1, "the skip is still surfaced for the queue");
    }
}
