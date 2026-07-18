//! Core domain logic for `claim`.
//!
//! This crate holds the parts of the system that must be correct above all
//! else, kept free of process, network, and terminal concerns so they can be
//! tested in isolation. The two binaries — the `claim` CLI and the MCP server —
//! are thin shells over this crate.
//!
//! The invariants that define the product live here as types. See
//! [`verdict::Verdict`] for the one that matters most: a check can report that a
//! claim still holds *only* by succeeding outright. Every other outcome,
//! including a check that could not run, resolves to something the operator will
//! eventually be nagged about. The failure mode is a nag, never a lie.

pub mod claim;
pub mod error;
pub mod verdict;

pub use claim::{
    extract_embedded_claims, parse_claim_file, Check, CheckKind, Claim, ClaimId, Days, Source,
    SupportTarget, Trigger, WikiLink,
};
pub use error::{Error, Result};
pub use verdict::{Status, Verdict};
