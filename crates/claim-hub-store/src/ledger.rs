//! The [`Ledger`] trait: the hub's append-only event log, as a storage seam.
//!
//! The ledger is the hub's one piece of primary state (HUB.md §2): an append-only
//! log of attested observations. The trait has **no `update` and no `delete`** —
//! not as a convention but structurally, so append-only discipline is
//! unrepresentable to break from Rust (HUB-IMPLEMENTATION.md §1.4). A caller
//! holding a `&dyn Ledger` has no method that could rewrite history; the SQLite
//! implementation backs that up with triggers that RAISE on any raw UPDATE or
//! DELETE, so a bug reaching past the trait cannot rewrite it either.
//!
//! The deriver consumes plain [`StoredEvent`]s from this trait and never sees SQL,
//! which is what lets the hosted tier swap in a Postgres implementation of the same
//! two traits with nothing above the trait changing.

use crate::error::Result;
use claim_hub_core::Event;

/// A monotonic position in the ledger — its cursor.
///
/// Every appended event lands at a strictly greater position than every event
/// before it, and a scan resumes from a caller-held position, so an intermittent
/// consumer catches up deterministically from where it left off (HUB.md §5's cursor
/// feed). `Position(0)` is *before the first event*: a fresh ledger has
/// [`Ledger::head`] `Position(0)`, and [`Ledger::scan_from`] `Position(0)` returns
/// every event. It is a newtype, not a bare integer, so a cursor cannot be confused
/// with a count, an index, or any other number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position(pub i64);

/// An event as it sits on the ledger: the appended [`Event`] plus the [`Position`]
/// it was assigned.
///
/// The position is assigned by the ledger at append time (it is the storage
/// cursor, not part of the attested envelope), so it is carried alongside the
/// `Event` rather than inside it. A scan returns these in strictly increasing
/// position order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEvent {
    /// Where this event sits in the ledger; strictly increasing across events.
    pub position: Position,
    /// The attested observation, exactly as appended.
    pub event: Event,
}

/// The outcome of an [`Ledger::append`].
///
/// Append is idempotent on the dedup key (HUB.md §2): a redelivered push carrying
/// an observation already on the ledger returns [`Appended::Duplicate`] with the
/// *original* event's position, never a second row. The caller distinguishes the
/// two so an ingest path can report "accepted" versus "already had this" without a
/// second query, but both are success — a retried push must never be an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appended {
    /// The event was new and now sits at this position.
    New(Position),
    /// The event's dedup key already existed; this is the position of the original,
    /// which is left untouched. The append was absorbed, not applied.
    Duplicate(Position),
}

impl Appended {
    /// The position of the event — the new one, or the pre-existing original a
    /// duplicate collapsed onto. Either way, the ledger position this observation
    /// occupies.
    #[must_use]
    pub fn position(self) -> Position {
        match self {
            Appended::New(p) | Appended::Duplicate(p) => p,
        }
    }
}

/// The hub's append-only event ledger.
///
/// Deliberately no `update` and no `delete`: the only mutation is [`append`], so
/// append-only discipline is a property of the type, not of caller discipline
/// (HUB-IMPLEMENTATION.md §1.4). Reads are a cursor scan and a head position, which
/// together give the deterministic, resumable feed HUB.md §5 requires.
///
/// [`append`]: Ledger::append
pub trait Ledger {
    /// Append one attested observation, returning where it landed.
    ///
    /// Idempotent on the dedup key (producer run, claim, check identity, HUB.md
    /// §2): appending an observation already on the ledger returns
    /// [`Appended::Duplicate`] with the original's position and adds no row, so a
    /// retried CI push cannot double-count. Appending a genuinely new observation
    /// returns [`Appended::New`] with its freshly assigned, strictly-greater
    /// position. The event is stored verbatim — the producer block and evidence are
    /// not reshaped — so the standing derived from it rests on exactly what was
    /// attested.
    fn append(&self, event: &Event) -> impl std::future::Future<Output = Result<Appended>> + Send;

    /// Return every event strictly after `cursor`, in increasing position order.
    ///
    /// `scan_from(Position(0))` returns the whole ledger; `scan_from(head)` returns
    /// nothing. The exclusive lower bound is what makes the cursor resumable: a
    /// consumer stores the position of the last event it processed and passes it
    /// back to receive only what is new, with no overlap and no gap.
    fn scan_from(
        &self,
        cursor: Position,
    ) -> impl std::future::Future<Output = Result<Vec<StoredEvent>>> + Send;

    /// The position of the most recent event, or `Position(0)` on an empty ledger.
    ///
    /// This is the head cursor: `scan_from(head())` is always empty, and the value
    /// is one of the deriver's memo keys (HUB-IMPLEMENTATION.md §1.5). It never
    /// decreases across appends.
    fn head(&self) -> impl std::future::Future<Output = Result<Position>> + Send;
}
