//! The router / nag engine: notice a transition, route it, fire it exactly once.
//!
//! When a fact stops holding — a claim drifts, ages into stale by the clock, or a skip's
//! `until` lapses — the hub must route it to whoever owns the decision, **exactly once**,
//! and survive a restart without re-nagging (HUB.md §3, HUB-IMPLEMENTATION.md §1.9). This
//! module is that engine, over the pure core in [`claim_hub_core::nag`] and the storage
//! helpers in [`claim_hub_store::nag`].
//!
//! ## Fire-once is derived from the ledger
//!
//! There is **no mutable fired flag and no fired table** (invariant #3). The router
//! derives "already nagged" by diffing the current live transitions against the `nag`
//! events already on the ledger: a transition whose [`FireKey`](claim_hub_core::FireKey)
//! is absent from the ledger's fired set is new and fires (a `nag` event is appended); one
//! already present is not re-fired. Because the fired set is rebuilt by *re-scanning the
//! ledger*, a restart reaches the identical conclusion — the ledger is the only memory, so
//! `run_once` after a restart never double-fires (proven in `tests/router.rs`). A stale
//! transition fires from the clock crossing a window with no new verdict, which is why the
//! clock is a parameter here, never a hidden `now()`.
//!
//! ## Rendering vs. delivery
//!
//! The hub **renders** nag content and **serves** it; the CI glue (hub-12b) delivers it
//! (HUB-IMPLEMENTATION.md §4.5 decision 4). The hub holds no forge write token. So
//! [`Router::run_once`] appends the fire mark and returns a [`RouterView`] — the rendered,
//! owner-resolved groups and the dead-letter queue — which the `GET /api/nags` endpoint
//! serves for the glue to pull.
//!
//! ## Owners at fire time, dead-letter when none
//!
//! Owners resolve from CODEOWNERS in the synced git mirror at fire time
//! ([`claim_hub_store::resolve_owners`]) — never a stored owner field (invariant #3). A
//! group with no resolvable owner is a **dead-letter** queue item, first-class in the view
//! (invariant #6 — a nag with nobody to route to is visible, never silently dropped). One
//! commit breaking N claims is one grouped item (grouping by envelope commit).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use claim_core::Timestamp;
use claim_hub_core::{
    fire_key_of, group_transitions, nag_producer, pending_transitions, DeriverConfig, DriftedClaim,
    Event, NagGroup, PendingTransition, Transition,
};
use claim_hub_store::{
    fired_keys, ledger_events, registry_snapshot, resolve_owners, Ledger, Registry, SqliteStore,
    StoreError,
};

/// The router's dependencies: the store it reads and appends to, the mirror root it reads
/// CODEOWNERS from, and the freshness config the derivation runs under.
///
/// Cheap to clone (the store is a pooled handle, the paths are `Arc`-shared), so the tick
/// and the served endpoint share one. Deliberately *not* part of [`crate::AppState`]: the
/// endpoint reads it from the shared state, but the tick owns its own, and keeping the
/// router's concerns here keeps `app.rs` minimal and additive (hub-13 wraps routes in auth
/// without fighting an outer `.layer()`).
#[derive(Clone)]
pub struct Router {
    store: SqliteStore,
    /// The directory the per-store bare mirrors live under, so owner resolution reads
    /// CODEOWNERS locally. `None` when the hub syncs no stores yet (then every group with
    /// an owner rule dead-letters, honestly — there is no mirror to resolve against).
    mirror_root: Option<Arc<PathBuf>>,
    config: DeriverConfig,
}

impl Router {
    /// A router over `store`, resolving owners from mirrors under `mirror_root`, deriving
    /// under `config`.
    #[must_use]
    pub fn new(store: SqliteStore, mirror_root: Option<PathBuf>, config: DeriverConfig) -> Self {
        Self {
            store,
            mirror_root: mirror_root.map(Arc::new),
            config,
        }
    }

    /// Run one router pass as of `now`: notice the live transitions, fire the new ones once
    /// each, and return the rendered, owner-resolved view.
    ///
    /// The pass is idempotent and restart-safe by construction (see the module docs): it
    /// re-derives the read model, computes the live transitions, resolves owners from the
    /// mirror's CODEOWNERS, groups by commit, then diffs the groups' fire keys against the
    /// ledger's `nag` events — appending a `nag` event only for a group whose key is *not*
    /// already on the ledger. Two passes with no intervening change fire nothing the second
    /// time; a pass after a restart (which rebuilds the fired set from the ledger) fires
    /// nothing already fired.
    ///
    /// `now` is the derivation *and* the fire clock — injected, never `Timestamp::now()`
    /// inside — so a test drives a clock-crossing stale by advancing it (CLAUDE.md's
    /// determinism rule).
    ///
    /// # Errors
    ///
    /// Propagates a store read/append fault or a git-spawn fault from owner resolution. A
    /// single unresolvable owner is **not** an error — it is a dead-letter in the returned
    /// view (invariant #6).
    pub async fn run_once(&self, now: Timestamp) -> Result<RouterView, StoreError> {
        let (groups, tips) = self.current_pass(now).await?;

        // The fired set: derived from the ledger's `nag` events, never a stored flag.
        let already_fired = fired_keys(&self.store).await?;

        let mut view = RouterView::default();
        for group in groups {
            let key = group.fire_key();
            let is_new = !already_fired.contains(&key);
            if is_new {
                // Append the fire mark: a `nag` event whose producer carries the fire key,
                // so the next pass (and a restart) derive this as already-fired.
                let event = self.nag_event(&group, now);
                self.store.append(&event).await?;
                view.fired_this_pass += 1;
            }
            self.record_into(&mut view, group, is_new, &tips);
        }
        Ok(view)
    }

    /// The current rendered nag view as of `now`, **without firing anything**.
    ///
    /// The read half `GET /api/nags` serves: the same derivation `run_once` fires from, so
    /// the served content and the fired marks can never disagree. Every live transition is
    /// grouped, owner-resolved, and classified into owned nags or the dead-letter queue;
    /// nothing is appended, so a poll never double-fires. `fired_this_pass` is `0` and each
    /// item's `fired_this_pass` is `false` — a read fires nothing.
    ///
    /// # Errors
    ///
    /// Propagates a store read fault or a git-spawn fault from owner resolution.
    pub async fn current_view(&self, now: Timestamp) -> Result<RouterView, StoreError> {
        let (groups, tips) = self.current_pass(now).await?;
        let mut view = RouterView::default();
        for group in groups {
            self.record_into(&mut view, group, false, &tips);
        }
        Ok(view)
    }

    /// The current owner-resolved nag groups as of `now`, without firing anything.
    ///
    /// The grouped, commit-anchored transitions the view and the fire logic both build on.
    /// Every live transition is grouped (drifts by commit, others per-claim), enriched with
    /// its statement and supports; nothing is appended.
    ///
    /// # Errors
    ///
    /// Propagates a store read fault or a git-spawn fault from owner resolution.
    pub async fn current_groups(&self, now: Timestamp) -> Result<Vec<NagGroup>, StoreError> {
        Ok(self.current_pass(now).await?.0)
    }

    /// The grouped transitions plus the per-store registry tip commits — the shared spine of
    /// the view and the fire logic.
    ///
    /// The tip commits are load-bearing for owner resolution: CODEOWNERS is read from the
    /// mirror at the store's **synced tip** (a commit the mirror actually holds), not at a
    /// verdict's drift commit (which the hub may never have synced). The two are different
    /// commits with different jobs — the drift commit *groups* the nag, the tip commit is
    /// where the hub's own CODEOWNERS lives.
    async fn current_pass(
        &self,
        now: Timestamp,
    ) -> Result<(Vec<NagGroup>, BTreeMap<String, String>), StoreError> {
        let registry = registry_snapshot(&self.store).await?;
        let events = ledger_events(&self.store).await?;
        let model = claim_hub_core::derive(&registry, &events, now, &self.config);

        // Enrich drift transitions with the real breaking commit from the ledger — the
        // deriver's standing does not carry it — so a drift group keys on the commit that
        // broke it (grouping by envelope commit). Stale/lapsed-skip stay per-claim.
        let drift_commits = latest_drift_commits(&events);
        let mut pending = pending_transitions(&model, now);
        for pt in &mut pending {
            if pt.transition == Transition::Drifted {
                if let Some(commit) = drift_commits.get(&(pt.store.clone(), pt.claim.clone())) {
                    pt.commit.clone_from(commit);
                }
            }
        }

        // Pre-fetch each claim's statement and supports (and the store's synced tip commit)
        // from the registry, so the grouping fold stays pure (a sync lookup, no IO).
        let (details, tips) = self.claim_details(&pending).await?;
        let groups = group_transitions(&pending, |pt| {
            details
                .get(&(pt.store.clone(), pt.claim.clone()))
                .cloned()
                .unwrap_or_else(|| DriftedClaim {
                    id: pt.claim.clone(),
                    commit: pt.commit.clone(),
                    statement: String::new(),
                    supports: Vec::new(),
                })
        });
        Ok((groups, tips))
    }

    /// The statement and supports for each pending transition's claim, plus the synced tip
    /// commit of each store, from the registry.
    ///
    /// The tip commit is where the hub's own CODEOWNERS lives in the mirror — read from any
    /// live claim's `commit` (all a store's claims share the synced tip). It is returned
    /// alongside the details so owner resolution reads CODEOWNERS at a commit the mirror
    /// actually holds, not at a verdict's drift commit the hub may never have synced.
    #[allow(clippy::type_complexity)]
    async fn claim_details(
        &self,
        pending: &[PendingTransition],
    ) -> Result<
        (
            BTreeMap<(String, String), DriftedClaim>,
            BTreeMap<String, String>,
        ),
        StoreError,
    > {
        let mut details = BTreeMap::new();
        let mut tips: BTreeMap<String, String> = BTreeMap::new();
        for pt in pending {
            let key = (pt.store.clone(), pt.claim.clone());
            if details.contains_key(&key) {
                continue;
            }
            let Ok(claim_id) = pt.claim.parse() else {
                continue;
            };
            if let Some(registered) = self.store.claim(&pt.store, &claim_id).await? {
                tips.entry(pt.store.clone())
                    .or_insert_with(|| registered.commit.clone());
                details.insert(
                    key,
                    DriftedClaim {
                        id: pt.claim.clone(),
                        commit: pt.commit.clone(),
                        statement: registered.statement,
                        supports: registered.supports,
                    },
                );
            }
        }
        Ok((details, tips))
    }

    /// Resolve a group's owners from CODEOWNERS in the mirror, at the group's commit.
    ///
    /// The claim file path CODEOWNERS matches is reconstructed as the canonical
    /// `.claims/<id>.md` (a store's standalone claim lives there — `claim-store`'s authoring
    /// rule), so a directory rule like `.claims/payments/` or `payments/` routes the claim.
    /// An embedded claim (declared inside a host file) would not match its host file's path
    /// this way; that is a v1 approximation, and an unmatched claim falls to the store's
    /// catch-all rule or dead-letters — never a wrong owner.
    ///
    /// With no mirror root configured, no owner resolves and the group dead-letters —
    /// honestly, since there is no mirror to read CODEOWNERS from.
    ///
    /// `tip_commit` is the store's synced registry tip — where the hub's own CODEOWNERS
    /// lives in the mirror — **not** the group's drift commit, which the hub may never have
    /// synced. Resolving at the tip reads the CODEOWNERS the hub actually holds.
    fn owners_of(
        &self,
        group: &NagGroup,
        tip_commit: Option<&str>,
    ) -> Result<Vec<String>, StoreError> {
        let Some(mirror_root) = &self.mirror_root else {
            return Ok(Vec::new());
        };
        let Some(tip) = tip_commit else {
            // No synced tip for this store: no CODEOWNERS to read, so the group dead-letters.
            return Ok(Vec::new());
        };
        // Resolve against the primary claim's path; a drift group's claims share a store, and
        // CODEOWNERS routing is per-file. v1 routes the group by its primary claim's owners.
        let claim_file = format!(".claims/{}.md", group.primary_claim());
        resolve_owners(mirror_root, &group.store, tip, &claim_file)
    }

    /// Build the `nag` event that marks a group as fired.
    ///
    /// The event carries no verdict and no check (invariant #4 — a nag is not a verdict);
    /// its producer block is the router principal plus the fire key (as both the marker and
    /// the dedup `run`), and its `commit` is the group's grouping commit. `claim` names the
    /// group's primary claim.
    fn nag_event(&self, group: &NagGroup, now: Timestamp) -> Event {
        let producer = nag_producer(group.transition, &group.fire_key());
        Event::nag(
            group.primary_claim().to_owned(),
            group.commit.clone(),
            group.store.clone(),
            producer,
            now,
        )
    }

    /// Record a group into the view, resolving its owners (at the store's synced tip) and
    /// classifying it as an owned nag or a dead-letter.
    fn record_into(
        &self,
        view: &mut RouterView,
        group: NagGroup,
        fired: bool,
        tips: &BTreeMap<String, String>,
    ) {
        let owners = self
            .owners_of(&group, tips.get(&group.store).map(String::as_str))
            .unwrap_or_default();
        let item = NagView::from_group(group, owners, fired);
        if item.owners.is_empty() {
            view.dead_letters.push(item);
        } else {
            view.nags.push(item);
        }
    }
}

/// The router's rendered output for one pass: the owned nags, the dead-letter queue, and
/// how many marks fired this pass.
///
/// This is what `GET /api/nags` serves — dated evidence the CI glue pulls to deliver
/// (HUB.md §3). Every group appears exactly once, in either [`nags`](RouterView::nags) (an
/// owner resolved) or [`dead_letters`](RouterView::dead_letters) (none did — visible, never
/// dropped, invariant #6).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct RouterView {
    /// The owner-resolved nag groups: each a transition routed to its owners.
    pub nags: Vec<NagView>,
    /// The dead-letter queue: groups with no resolvable owner — a nag about the inability
    /// to nag, surfaced so it is never silently dropped (invariant #6).
    pub dead_letters: Vec<NagView>,
    /// How many `nag` marks this pass newly appended — `0` on a pass that fired nothing new
    /// (idempotent), non-zero only when a genuinely new transition was noticed.
    pub fired_this_pass: usize,
}

/// One rendered nag item: a grouped transition, its owners, and whether this pass fired it.
///
/// The rendered content the CI glue delivers (HUB.md §3): the transition, the store and
/// commit, the claims (with statement and supports), the resolved owners, and the fire key.
/// Owners empty means a dead-letter.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NagView {
    /// Which transition this item is.
    pub transition: Transition,
    /// The store the group's claims live in.
    pub store: String,
    /// The commit that groups the item (the breaking commit for a drift).
    pub commit: String,
    /// The claims in the group, each with its statement and supports.
    pub claims: Vec<DriftedClaim>,
    /// The resolved CODEOWNERS owners, at fire time from the mirror. Empty for a dead-letter.
    pub owners: Vec<String>,
    /// The stable fire key this item is nagged once per (see
    /// [`FireKey`](claim_hub_core::FireKey)).
    pub fire_key: String,
    /// Whether *this pass* newly fired this item (appended a `nag` mark). `false` for an
    /// already-fired item still live, or a dead-letter (which fires no mark to deliver).
    pub fired_this_pass: bool,
}

impl NagView {
    fn from_group(group: NagGroup, owners: Vec<String>, fired: bool) -> Self {
        Self {
            transition: group.transition,
            store: group.store,
            commit: group.commit,
            claims: group.claims,
            owners,
            fire_key: group.fire_key_str,
            fired_this_pass: fired,
        }
    }
}

/// The commit of each claim's latest *drifted* verdict, keyed by (store, claim).
///
/// A drift group keys on the commit that broke the claim, but the deriver's standing does
/// not carry it (the standing is the conservative fold, not a per-verdict record). So the
/// router reads it from the ledger: the latest drifted verdict's commit for each claim. When
/// two checks of one claim drift at different commits, the later report's commit wins —
/// grouping by the most recent breaking commit is the honest "what broke it now".
fn latest_drift_commits(events: &[(u64, Event)]) -> BTreeMap<(String, String), String> {
    let mut latest: BTreeMap<(String, String), (Timestamp, String)> = BTreeMap::new();
    for (_, event) in events {
        // Only a drifted verdict names a breaking commit; a nag or a non-drift verdict does
        // not. `event.verdict` is `None` on a nag, so this skips nags naturally.
        if event.verdict != Some(claim_core::Verdict::Drifted) {
            continue;
        }
        let key = (event.store.clone(), event.claim.clone());
        let candidate = (event.reported_at, event.commit.clone());
        latest
            .entry(key)
            .and_modify(|current| {
                if candidate.0 >= current.0 {
                    *current = candidate.clone();
                }
            })
            .or_insert(candidate);
    }
    latest
        .into_iter()
        .map(|(k, (_, commit))| (k, commit))
        .collect()
}

/// Spawn the router tick: a background task that runs [`Router::run_once`] on a fixed
/// cadence, so a clock-crossing transition is noticed without a new verdict.
///
/// The v1 scheduler dispatches nothing (HUB-IMPLEMENTATION.md §1.8); this is the one
/// recurring task that wakes the router to notice clock-crossing staleness. It ticks every
/// `period`, running one pass each tick at wall-clock `now`. A per-tick error is reported
/// through `on_result` and does not stop the loop — a transient store or git fault fails
/// this tick and is retried next. The first tick fires immediately (tokio's interval yields
/// at once), so a freshly started hub routes without waiting a full period.
///
/// The returned [`JoinHandle`](tokio::task::JoinHandle) lets a caller abort the tick on
/// shutdown; dropping it detaches the task.
pub fn spawn_router_tick<F>(
    router: Router,
    period: std::time::Duration,
    mut on_result: F,
) -> tokio::task::JoinHandle<()>
where
    F: FnMut(&Result<RouterView, StoreError>) + Send + 'static,
{
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        loop {
            interval.tick().await;
            let result = router.run_once(Timestamp::now()).await;
            on_result(&result);
        }
    })
}

/// Whether an event is a nag mark (kind `nag` with a fire key). A thin re-export of the
/// core check, so a hub-side reader classifies a nag the same way the ledger does.
#[must_use]
pub fn is_nag(event: &Event) -> bool {
    fire_key_of(event).is_some()
}

/// The mirror-root path a router reads owners from, given the hub's data directory.
///
/// Registry sync stores mirrors under `<data>/_mirror` by convention; the router reads
/// CODEOWNERS from the same place. A free helper so boot and tests agree on the path.
#[must_use]
pub fn mirror_root_for(data_dir: &Path) -> PathBuf {
    data_dir.join("_mirror")
}

#[cfg(test)]
mod tests {
    use super::*;
    use claim_hub_core::{CheckRef, Producer};

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn drift(claim: &str, commit: &str, at: &str) -> Event {
        let mut producer = serde_json::Map::new();
        producer.insert("run".into(), serde_json::json!("r"));
        Event::verdict(
            claim,
            CheckRef {
                index: 0,
                digest: "d".repeat(64),
            },
            claim_core::Verdict::Drifted,
            commit,
            "s",
            Producer(producer),
            ts(at),
        )
    }

    #[test]
    fn latest_drift_commit_is_the_most_recent_breaking_commit() {
        // Two drifts of one claim at different commits: the later report's commit is the
        // grouping commit — "what broke it now".
        let events = vec![
            (1u64, drift("c", "old", "2026-07-01T00:00:00Z")),
            (2u64, drift("c", "new", "2026-07-02T00:00:00Z")),
        ];
        let commits = latest_drift_commits(&events);
        assert_eq!(
            commits.get(&("s".into(), "c".into())).map(String::as_str),
            Some("new")
        );
    }

    #[test]
    fn a_nag_event_contributes_no_drift_commit() {
        // A nag on the ledger carries no verdict, so it never contributes a breaking commit —
        // only a drifted verdict does. This guards the `event.verdict != Some(Drifted)` skip.
        let nag = Event::nag(
            "c",
            "naggish",
            "s",
            claim_hub_core::nag_producer(
                Transition::Drifted,
                &claim_hub_core::FireKey::from_stored("k".repeat(64)),
            ),
            ts("2026-07-03T00:00:00Z"),
        );
        let events = vec![
            (1u64, drift("c", "real", "2026-07-01T00:00:00Z")),
            (2u64, nag),
        ];
        let commits = latest_drift_commits(&events);
        // The commit is the real drift's, not the nag's — the nag was skipped.
        assert_eq!(
            commits.get(&("s".into(), "c".into())).map(String::as_str),
            Some("real")
        );
    }

    #[test]
    fn is_nag_distinguishes_a_nag_from_a_verdict() {
        assert!(!is_nag(&drift("c", "x", "2026-07-01T00:00:00Z")));
        let nag = Event::nag(
            "c",
            "x",
            "s",
            claim_hub_core::nag_producer(
                Transition::Stale,
                &claim_hub_core::FireKey::from_stored("k".repeat(64)),
            ),
            ts("2026-07-01T00:00:00Z"),
        );
        assert!(is_nag(&nag));
    }
}
