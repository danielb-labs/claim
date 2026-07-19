//! Core domain logic for `claim`.
//!
//! This crate holds the parts of the system that must be correct above all
//! else, kept free of process, network, and terminal concerns so they can be
//! tested in isolation. The `claim` CLI is a thin shell over this crate.
//!
//! The invariants that define the product live here as types. See
//! [`verdict::Verdict`] for the one that matters most: a check can report that a
//! claim still holds *only* by succeeding outright. Every other outcome,
//! including a check that could not run, is loud — the failure mode is a nag,
//! never a lie.
//!
//! A [`claim::Claim`] is a statement, the checks that re-verify it, its
//! `supports` graph edges, and an optional [`claim::Hub`] block of scheduling
//! hints the CLI validates but never acts on. There is no committed verdict log
//! and no derived status here: the CLI reports a check's *current* verdict and
//! stores nothing. Any longer-lived ledger — freshness, staleness, due-dates —
//! belongs to the hub that ingests the reported stream (see
//! `docs/design/CLI-HUB-BOUNDARY.md`).
//!
//! Running a check and turning the result into a verdict is [`check`]:
//! [`check::run_check`] executes a command through the shell and maps its exit
//! code to a [`verdict::Verdict`] under the honesty contract — a check that
//! cannot run is `Broken`, never a pass. An `agent` check runs the same way when
//! the context carries an [`check::AgentRunner`], mapping the runner's structured
//! output to a verdict under the same contract; with no runner it is
//! `Unverifiable` and spawns nothing. [`check::resolve_supports`] reports,
//! separately from the verdict, whether a claim's `supports` targets still
//! resolve, so a deleted decision goes loud instead of leaving a claim green.

pub mod check;
pub mod claim;
pub mod error;
pub mod verdict;

pub use check::{
    build_agent_prompt, evaluate_skip, resolve_supports, run_check, AgentRunner, CheckContext,
    CheckOutcome, ProcessEnd, SkipDecision, SupportResolution, DEFAULT_OUTPUT_CAP, DEFAULT_TIMEOUT,
};
pub use claim::{
    extract_embedded_claims, has_frontmatter_fence, parse_claim_file, Check, CheckKind, Claim,
    ClaimId, Days, Hub, Skip, Source, SupportTarget, WikiLink,
};
pub use error::{Error, Result};
pub use verdict::Verdict;

/// The UTC instant type the tool records and reasons about, re-exported so the
/// CLI and the store crate name one `Timestamp` and cannot disagree about its
/// semantics. From `jiff`, for correctness-first instant arithmetic and
/// lossless RFC 3339 round-trips.
pub use jiff::Timestamp;
