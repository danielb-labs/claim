//! The [`Registry`] trait: the hub's mirror of git, as derived, rebuildable data.
//!
//! The registry is every claim file in every connected store, indexed at the tip of
//! its default branch with the commit it was read at (HUB.md §2). It is derived data
//! in the strictest sense: delete it and a re-scan rebuilds it. This crate stores
//! what it is *given* — the git mirror and `claim-core` parsing that produce a
//! snapshot are hub-05; hub-02 owns the storage seam only.
//!
//! Two properties are load-bearing and tested:
//!
//! - **Replace, don't merge.** [`Registry::replace_store`] wipes a store's rows and
//!   re-inserts the snapshot, so a claim absent at the new tip is *dropped* — a
//!   retirement (HUB.md §3), not a stale row surviving forever. Wipe-plus-resnapshot
//!   of the whole registry reproduces it identically.
//! - **A version counter marks each sync.** [`Registry::version`] advances by one on
//!   every [`replace_store`], so a reader can tell the registry changed under it and
//!   the deriver's memo can key on it (HUB-IMPLEMENTATION.md §1.5).
//!
//! [`replace_store`]: Registry::replace_store

use crate::error::Result;
use crate::findings::SyncFinding;
use claim_core::ClaimId;

/// A monotonic version stamp for the registry, advanced once per store sync.
///
/// It is a newtype over the counter so it cannot be confused with a ledger
/// [`Position`](crate::Position) or a count. A fresh registry is
/// `RegistryVersion(0)`; the first [`Registry::replace_store`] makes it
/// `RegistryVersion(1)`, and so on. It is one of the deriver's three memo keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegistryVersion(pub i64);

/// One claim at a store's default-branch tip, as the registry holds it.
///
/// The `commit` is the sha the claim was read at, so the registry can never present
/// a claim more current than the tip it snapshotted (HUB.md §2). The `supports`
/// edges are the cross-store index the router keys on. This is the parsed claim's
/// *stored* shape — statement and identity and edges — not the full
/// [`claim_core::Claim`]: the check definitions live with the verdicts they produce,
/// on the ledger's `check_digest`, not duplicated here.
///
/// Supports targets are held as plain strings, not `claim_core::SupportTarget`: that
/// newtype is deliberately constructable only through the claim parser (its
/// validation belongs to authoring), so the storage seam persists and returns the
/// target's canonical string — which is exactly its transparent serialized form — and
/// leaves re-validation to the parse path that produced the snapshot (hub-05). The
/// store's job is durable bytes, not re-litigating grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredClaim {
    /// The claim's id, unique within its store.
    pub id: ClaimId,
    /// The human-and-agent-readable statement — the real source of truth a check
    /// only approximates.
    pub statement: String,
    /// The `supports` edges: decision refs or claim ids this claim justifies, each
    /// as the target's canonical string (see the type doc).
    pub supports: Vec<String>,
    /// The commit sha this claim was read at.
    pub commit: String,
}

/// One `supports` edge in the cross-store index: a claim and a target it justifies.
///
/// Returned in the reverse direction by [`Registry::claims_supporting`] — the shape
/// cross-repo routing (#10) reads to find the owner of a *decision*, wherever it
/// lives, from a drifted claim. The `target` is the canonical target string (see
/// [`RegisteredClaim`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupportsEdge {
    /// The store the supporting claim lives in.
    pub store: String,
    /// The supporting claim's id within that store.
    pub claim_id: ClaimId,
    /// The target the claim justifies (a decision ref or a claim id), as its
    /// canonical string.
    pub target: String,
}

/// The hub's mirror of git: claims at tip and the supports index, as derived data.
///
/// Every method is a read except [`replace_store`], which is the one write and is
/// atomic: within it the store's old rows are wiped, the snapshot is inserted, and
/// the version counter advances, so a reader never sees a half-replaced store. The
/// registry holds no status and no verdict — those are the ledger's and the
/// deriver's — only the claims-at-tip and the edges between them.
///
/// [`replace_store`]: Registry::replace_store
pub trait Registry {
    /// Replace a store's entire snapshot with `claims`, read at the sync, and
    /// advance the version counter by one.
    ///
    /// This is a *replace*, not a merge: every claim previously recorded for `store`
    /// that is not in `claims` is dropped (a retirement, HUB.md §3), and its
    /// supports edges go with it. The operation is atomic — old rows out, new rows
    /// in, version bumped, all in one transaction — so a concurrent reader sees
    /// either the whole old snapshot or the whole new one, never a mix. Passing an
    /// empty `claims` retires every claim in the store while keeping the store
    /// connected. Re-running the identical snapshot still advances the version (a
    /// sync happened); the *contents* are idempotent even though the counter is not.
    fn replace_store(
        &self,
        store: &str,
        claims: &[RegisteredClaim],
    ) -> impl std::future::Future<Output = Result<RegistryVersion>> + Send;

    /// Replace a store's claims **and** its sync findings in one atomic step, and
    /// advance the version counter by one.
    ///
    /// This is the single write registry sync (hub-05) uses, and its atomicity is
    /// **load-bearing for invariant #6**. A malformed claim file must never leave the
    /// registry (indexed away) while its [`SyncFinding`] is lost — that is a silent
    /// coverage gap, a nag dropped in the unsafe direction. Writing the claims, their
    /// supports edges, the findings, and the version bump inside one transaction makes
    /// that skew unrepresentable: a reader sees either the whole old snapshot (claims
    /// *and* findings) or the whole new one, never a claim retired-away with no finding
    /// to explain it. A crash or fault anywhere in the write rolls the entire snapshot
    /// back, so the previous, self-consistent snapshot survives and the next sync
    /// re-derives from the tip.
    ///
    /// Semantics otherwise match [`replace_store`] for claims (a claim absent from
    /// `claims` is retired; edges cascade) and [`Findings::replace_store_findings`]
    /// for findings (a file that parses cleanly at the new tip drops its finding). The
    /// version advances once per call even when the contents are identical, marking
    /// that a sync happened.
    ///
    /// [`replace_store`]: Registry::replace_store
    /// [`Findings::replace_store_findings`]: crate::Findings::replace_store_findings
    fn replace_store_snapshot(
        &self,
        store: &str,
        claims: &[RegisteredClaim],
        findings: &[SyncFinding],
    ) -> impl std::future::Future<Output = Result<RegistryVersion>> + Send;

    /// The current registry version — the number of syncs applied. `0` on a fresh
    /// registry. One of the deriver's memo keys.
    fn version(&self) -> impl std::future::Future<Output = Result<RegistryVersion>> + Send;

    /// Every claim currently at tip in `store`, in ascending id order.
    ///
    /// Ascending id order makes the result deterministic, so a
    /// wipe-plus-resnapshot comparison and the deriver's reads are reproducible.
    /// An empty vector means the store is connected but has no live claims.
    fn claims_of(
        &self,
        store: &str,
    ) -> impl std::future::Future<Output = Result<Vec<RegisteredClaim>>> + Send;

    /// The single claim at `id` in `store`, or `None` if it is not at tip (never
    /// registered, or retired at the last sync).
    fn claim(
        &self,
        store: &str,
        id: &ClaimId,
    ) -> impl std::future::Future<Output = Result<Option<RegisteredClaim>>> + Send;

    /// The targets a given claim supports, in ascending target order.
    ///
    /// The forward direction of the supports index. An empty vector means the claim
    /// stands alone (or is not at tip). Deterministic order for reproducibility.
    fn supports_targets_of(
        &self,
        store: &str,
        id: &ClaimId,
    ) -> impl std::future::Future<Output = Result<Vec<String>>> + Send;

    /// Every claim across every store that supports `target`, in ascending
    /// (store, id) order.
    ///
    /// The reverse direction of the supports index — the query cross-repo routing
    /// (#10) uses to route a drifted claim to the owner of the decision it supports,
    /// wherever that decision lives. `target` is matched by its canonical string.
    fn claims_supporting(
        &self,
        target: &str,
    ) -> impl std::future::Future<Output = Result<Vec<SupportsEdge>>> + Send;
}
