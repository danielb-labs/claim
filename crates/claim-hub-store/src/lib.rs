//! The hub's storage layer: the append-only ledger and the derived registry.
//!
//! `claim-hub-store` is the seam HUB-IMPLEMENTATION.md §1.4 defines: a pair of
//! traits and a SQLite implementation of them, so the deriver above consumes plain
//! data and never sees SQL, and the hosted tier can later swap in a Postgres
//! implementation of the same two traits with nothing above the seam changing.
//!
//! Two invariants are enforced *below* the trait, in the schema itself, so a bug
//! reaching around the trait cannot break them:
//!
//! - **Append-only.** [`Ledger`] has no `update` and no `delete` — the only mutation
//!   is [`Ledger::append`], so append-only discipline is unrepresentable to break
//!   from Rust. The SQLite schema backs that with triggers that RAISE on any raw
//!   `UPDATE` or `DELETE` against the events table, so even a foreign SQL path fails.
//! - **Dedup on redelivery.** Appending the same observation — keyed on
//!   (store, producer run, claim, check identity) — twice yields one row and an
//!   idempotent success (HUB.md §2), a UNIQUE index the append absorbs a conflict
//!   against. A verdict with no usable producer run is rejected, not bucketed, since
//!   a run-less observation is unattributable (invariant #6).
//!
//! The [`Registry`] is derived data: [`Registry::replace_store`] wipes a store's
//! snapshot and re-inserts it (a claim absent at the new tip is retired), and a
//! version counter advances per sync so the deriver's memo can key on it. The git
//! mirror and `claim-core` parsing that *feed* a snapshot live in [`sync`] (hub-05):
//! [`sync::sync_store`] mirrors a connected store, reads its tip through
//! `claim-store`'s loader plus the embedded-block grammar, snapshots the registry, and
//! records malformed files as [`SyncFinding`]s (invariant #6 — a nag, never a silent
//! skip). [`sync::spawn_interval_poll`] is the v1 interval-poll trigger over that.
//!
//! The [`Rejections`] counter records how many ingests the gate refused: a rejected
//! push writes no event (invariant #4) but is counted here and surfaced at `/status`,
//! so a hub silently turning telemetry away is visible rather than quietly aging
//! claims into stale (invariant #6, HUB.md §3).
//!
//! The one implementation is [`SqliteStore`], over a single WAL-mode SQLite file —
//! the data-ownership invariant made physical (export is `cp`, delete is `rm`) —
//! implementing [`Ledger`], [`Registry`], [`Findings`], and [`Rejections`].

pub mod error;
pub mod findings;
pub mod ledger;
pub mod registry;
pub mod rejections;
pub mod sqlite;
pub mod sync;

pub use error::{Result, StoreError};
pub use findings::{Findings, SyncFinding};
pub use ledger::{Appended, Ledger, Position, StoredEvent};
pub use registry::{RegisteredClaim, Registry, RegistryVersion, SupportsEdge};
pub use rejections::Rejections;
pub use sqlite::{SqliteStore, MIGRATOR};
pub use sync::{
    spawn_interval_poll, sync_store, ConnectedStore, SyncOutcome, DEFAULT_BRANCH,
    EMBEDDED_HOST_FILES,
};
