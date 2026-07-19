//! The hub's pure domain — the hub's answer to `claim-core`.
//!
//! `claim-hub-core` holds the parts of the hub that must be correct above all
//! else, kept free of IO, async, network, and storage so they can be tested in
//! isolation (`HUB-IMPLEMENTATION.md` §1.2). It is where the hub's correctness
//! lives; later hub crates (`claim-hub-store`, `claim-hub`) are thin shells over
//! it, exactly as the CLI is a thin shell over `claim-core`.
//!
//! This first item (hub-01) provides the wire and identity primitives the rest of
//! the hub is built on:
//!
//! - [`Event`] — the event envelope of HUB.md §2: the shape of one attested
//!   observation on the append-only ledger, reusing [`claim_core::Verdict`] and
//!   [`claim_core::Timestamp`] and round-tripping through JSON losslessly.
//! - [`wire`] — serde types that parse the CLI's `claim check --json` report *as a
//!   wire format*, independent of the CLI's own structs, rejecting a malformed or
//!   unknown-field report with the offending field named. The hub ingests many
//!   CLI versions; it parses what is on the wire, and the workspace contract test
//!   keeps that parse honest against the real binary.
//! - [`check_digest`] — the canonical, reorder-proof digest of a check's
//!   definition, so a shallow check's pass never clears a deep check's drift
//!   (issue #18).
//! - [`cap_evidence`] — capping an event's evidence at ingest, truncating with a
//!   recorded marker rather than dropping silently (invariant #6).
//!
//! On those primitives, hub-06 adds **the deriver** ([`deriver`]): the pure function
//! that turns a registry snapshot, the ledger's events, a clock, and config into the
//! read model — per-claim standing, freshness, the due set, skip ages — with the
//! conservative "bad news dominates" join that no combination of events can turn into
//! a manufactured green. [`memo`] caches the most recent derivation, invalidated by
//! exactly the three causes (a new event, a registry change, the clock crossing a
//! threshold) — a cache, never a store.
//!
//! hub-11 adds the router's pure core ([`nag`]): the [`Transition`]s the router routes,
//! the deterministic [`FireKey`] that makes a fire idempotent by diffing against the
//! ledger's `nag` events (invariant #3, no mutable flag), and the grouping that turns one
//! commit breaking N claims into one nag item.
//!
//! hub-14 adds the skip-ranking projection ([`rank`]): [`rank_skips`] orders a store's
//! skipped checks into the review queue by age and lapsed `until` — the one pure ranking
//! every read surface shares, so the API, MCP, twins, and UI cannot disagree.
//!
//! What is deliberately *not* here: any store, the ingest route, or anything async —
//! those are later hub items. This crate is types and pure functions.

pub mod deriver;
pub mod envelope;
pub mod evidence;
pub mod memo;
pub mod nag;
pub mod rank;
pub mod wire;

mod digest;

pub use deriver::{
    derive, AsOf, ClaimEntry, ClaimStanding, DerivedCheck, DerivedSkip, DeriverConfig, HubHints,
    ReadModel, RegistrySnapshot, SkipAge, Standing,
};
pub use digest::check_digest;
pub use envelope::{CheckRef, Event, EventKind, Producer};
pub use evidence::{cap_evidence, EVIDENCE_CAP};
pub use memo::Memo;
pub use nag::{
    fire_key_of, group_transitions, lapsed_skips, nag_producer, pending_transitions, DriftedClaim,
    FireKey, NagGroup, PendingTransition, Transition, NAG_FIRE_KEY, NAG_PRINCIPAL,
    NAG_PRINCIPAL_KEY, NAG_RUN_KEY, NAG_TRANSITION_KEY,
};
pub use rank::{rank_skips, RankedSkip};
