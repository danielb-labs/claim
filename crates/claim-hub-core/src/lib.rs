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
//! What is deliberately *not* here: any store, the deriver, the ingest route, or
//! anything async — those are later hub items. This crate is types and two pure
//! functions.

pub mod envelope;
pub mod evidence;
pub mod wire;

mod digest;

pub use digest::check_digest;
pub use envelope::{CheckRef, Event, EventKind, Producer};
pub use evidence::{cap_evidence, EVIDENCE_CAP};
