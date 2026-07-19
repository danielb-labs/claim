//! The [`Findings`] trait: sync findings, a nag owed for a malformed claim.
//!
//! A claim file that fails to parse at a synced tip is not silently skipped
//! (invariant #6 and HUB.md §3): it is recorded as a [`SyncFinding`] — a durable,
//! queryable record naming the file and the parse reason — so the malformed claim
//! becomes something a human is asked to look at, never a quiet gap in coverage
//! that ages nothing into stale. The well-formed claims at the same tip still index;
//! one broken file does not deny the whole store its snapshot, exactly as the CLI's
//! store loading refuses to let one bad file silence the rest.
//!
//! Findings are **derived data, replaced per sync**, the same discipline the
//! [`Registry`](crate::Registry) follows: each successful sync of a store *replaces*
//! that store's findings with the ones observed at the new tip. So a file fixed at
//! the new tip drops its finding automatically (the nag clears when the cause does),
//! and a newly-broken file gains one — the findings always describe the live tip, not
//! an accreted history. History of a finding, if ever wanted, is renderable from git;
//! the live set is what a queue reads.

use crate::error::Result;

/// One malformed claim file observed at a store's tip during registry sync.
///
/// Recorded rather than dropped so the malformed claim is a nag, not a silent skip
/// (invariant #6). It carries enough to route a human to the fix: the store it was
/// found in, the file's path relative to the store root (as the author sees it on
/// disk), and the parser's own reason, which already names the field to fix. There
/// is no claim id — the file did not parse, so no id was produced; the file path is
/// the handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncFinding {
    /// The connected store the malformed file lives in (e.g.
    /// `github.com/acme/payments`).
    pub store: String,
    /// The offending file's path relative to the store root, e.g.
    /// `.claims/payments/libfoo-pin.md` or `CLAUDE.md` — what a human sees on disk.
    pub file: String,
    /// The commit sha the file was read at, so the finding pins the tip that
    /// produced it and a reader can inspect the exact bytes.
    pub commit: String,
    /// The parser's reason the file could not be understood, phrased for the author
    /// to fix (it already names the field). This is the whole of the nag's content.
    pub reason: String,
}

/// The store of sync findings: malformed claim files observed at a tip, as a
/// replace-per-sync live set.
///
/// Modeled as its own trait beside [`Registry`](crate::Registry) and
/// [`Ledger`](crate::Ledger) so the same storage seam covers it and the hosted tier's
/// Postgres impl inherits it for free. Every method is a read except
/// [`replace_store_findings`], which is the one write and mirrors
/// [`Registry::replace_store`]: a store's findings are wiped and re-inserted per sync,
/// so a fixed file's finding disappears and the set always describes the current tip.
///
/// [`replace_store_findings`]: Findings::replace_store_findings
/// [`Registry::replace_store`]: crate::Registry::replace_store
pub trait Findings {
    /// Replace a store's entire finding set with `findings`, observed at the sync.
    ///
    /// A *replace*, not a merge: every finding previously recorded for `store` is
    /// dropped and only `findings` remains, so a file that parsed cleanly at the new
    /// tip (its author fixed it) no longer nags. Passing an empty slice clears a
    /// store's findings — the healthy state, where every claim file parsed. Atomic:
    /// old rows out, new rows in, in one transaction, so a reader never sees a
    /// half-replaced set.
    fn replace_store_findings(
        &self,
        store: &str,
        findings: &[SyncFinding],
    ) -> impl std::future::Future<Output = Result<()>> + Send;

    /// Every current finding across every store, in ascending (store, file) order.
    ///
    /// Deterministic order so a queue rendering and a test read the same set every
    /// run. An empty vector means every claim file at every synced tip parsed — the
    /// healthy state.
    fn findings(&self) -> impl std::future::Future<Output = Result<Vec<SyncFinding>>> + Send;

    /// The current findings for one store, in ascending file order.
    ///
    /// The per-store slice of [`findings`](Findings::findings), for a store-scoped
    /// view. Empty when the store's tip parsed cleanly.
    fn findings_of(
        &self,
        store: &str,
    ) -> impl std::future::Future<Output = Result<Vec<SyncFinding>>> + Send;
}
