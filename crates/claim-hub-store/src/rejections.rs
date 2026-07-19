//! The [`Rejections`] trait: a durable count of ingests the hub refused.
//!
//! A rejected push — a forged signature, an expired token, a wrong audience, an
//! unconnected repository, a malformed envelope — writes **no event** (invariant #4:
//! a rejected push never becomes telemetry). But it must not vanish silently either:
//! a hub that quietly drops telemetry ages the affected claims into staleness with
//! nobody told why (invariant #6, HUB.md §3). So every rejection is counted here and
//! surfaced at `/status`, where a monitor watching the count rise sees that a
//! producer's pushes are being turned away while the claims they would refresh go
//! stale.
//!
//! The count is a small mutable counter, deliberately **not** on the append-only
//! [`Ledger`](crate::Ledger): a rejection is the *absence* of an event, operational
//! health the hub owns, not an attested observation on the log — and the events table
//! forbids mutation by trigger. It lives beside the ledger and registry in the same
//! SQLite file, and like every other storage concern it is a trait so the hosted
//! tier's Postgres impl inherits it.

use crate::error::Result;

/// The store of the ingest rejection count.
///
/// Two methods: [`record_rejection`](Rejections::record_rejection) increments the
/// count durably (called by the ingest gate on every refused push, before it returns
/// the 4xx, so the count reflects the rejection even if the response never reaches the
/// producer), and [`rejection_count`](Rejections::rejection_count) reads it (for
/// `/status`). There is deliberately no reset: the count is monotonic operational
/// history, and a hub that could zero its own rejection count could hide a flood of
/// turned-away telemetry.
pub trait Rejections {
    /// Increment the rejection count by one and return the new total.
    ///
    /// Durable and atomic: the increment is its own committed statement, so a
    /// concurrent reader sees a consistent count and a rejection is recorded before
    /// the gate answers the producer. Returns the post-increment value so a caller can
    /// log or surface it without a follow-up read.
    fn record_rejection(&self) -> impl std::future::Future<Output = Result<i64>> + Send;

    /// The current rejection count — how many ingests the hub has refused since the
    /// database was created. `0` on a fresh hub.
    fn rejection_count(&self) -> impl std::future::Future<Output = Result<i64>> + Send;
}
