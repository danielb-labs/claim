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
//!
//! A claim's history and current standing live in [`log`]: an append-only
//! verdict log on disk, and [`log::compute_status`], the pure function that
//! derives a claim's [`verdict::Status`] from that history and its `max_age` at
//! read time. Status is computed, never stored.
//!
//! Running a check and turning the result into a verdict is [`check`]:
//! [`check::run_check`] executes a command through the shell and maps its exit
//! code to a [`verdict::Verdict`] under the honesty contract — a check that
//! cannot run is `Broken`, never a pass. [`check::resolve_supports`] reports,
//! separately from the verdict, whether a claim's `supports` targets still
//! resolve, so a deleted decision goes loud instead of leaving a claim green.

pub mod check;
pub mod claim;
pub mod error;
pub mod log;
pub mod verdict;

pub use check::{
    resolve_supports, run_check, CheckContext, CheckOutcome, SupportResolution, DEFAULT_OUTPUT_CAP,
    DEFAULT_TIMEOUT,
};
pub use claim::{
    extract_embedded_claims, parse_claim_file, Check, CheckKind, Claim, ClaimId, Days, Source,
    SupportTarget, Trigger, WikiLink,
};
pub use error::{Error, Result};
pub use log::{
    append_entry, compute_status, read_entries, Adjudication, Event, Grace, LogEntry,
    SignedDuration, StatusReport, Timestamp,
};
pub use verdict::{Status, Verdict};
