//! Store discovery, claim loading, and git provenance for the `claim` tools.
//!
//! Where the store lives, what claims it holds, and who a verdict is attributed
//! to are questions both the `claim` CLI and the `claim-mcp` server must answer
//! identically — a trust tool that let its two front doors disagree about which
//! store they read, or which commit a verdict belongs to, would invite exactly
//! the drift it exists to prevent. So the answers live here, once, layered on
//! [`claim_core`]:
//!
//! - [`Store`] and [`discover`] — locating a `.claims/` store and reading its
//!   whole corpus ([`Store::load_all`]), with a malformed or duplicate-id file
//!   surfaced as a [`LoadError`] rather than silencing the store.
//! - [`resolve_commit`] and [`resolve_actor`] — the git-derived provenance a
//!   verdict-log entry needs (invariant #3), plus the working-tree helpers the
//!   witnessed-red flow uses.
//!
//! Errors are typed ([`StoreError`], [`GitError`]) so each binary maps them to
//! its own surface — the CLI to a `--json` error `kind`, the server to a protocol
//! error — without matching on prose. Everything terminal-, argument-, and
//! output-shaped stays in the binaries; this crate is pure store-and-git logic.

mod error;
mod git;
mod store;

pub use error::{GitError, StoreError};
pub use git::{
    is_inside_work_tree, resolve_actor, resolve_commit, revert_tracked_changes, short_commit,
    tracked_tree_is_dirty, UNBORN_HEAD_SENTINEL,
};
pub use store::{discover, LoadError, LoadedClaim, Store, StoreLoad, CLAIMS_DIR, LOG_DIR};
