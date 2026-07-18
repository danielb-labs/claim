//! Store discovery, claim loading, and git provenance for the `claim` CLI.
//!
//! Where the store lives, what claims it holds, and who a claim is attributed to
//! are the store-and-git questions the `claim` CLI must answer the same way from
//! every verb — a trust tool that answered "which store am I reading?" or "which
//! commit does this claim belong to?" inconsistently would invite exactly the drift
//! it exists to prevent. So the answers live here, once, layered on [`claim_core`]
//! and deliberately kept free of CLI concerns so this logic could back another front
//! door unchanged:
//!
//! - [`Store`] and [`discover`] — locating a `.claims/` store and reading its
//!   whole corpus ([`Store::load_all`]), with a malformed or duplicate-id file
//!   surfaced as a [`LoadError`] rather than silencing the store.
//! - [`git::resolve_commit`] and [`git::resolve_actor`] — the git-derived
//!   provenance the authoring gate resolves (invariant #3), plus [`git::Worktree`],
//!   the isolated checkout `claim add --witness-cmd` uses to witness a red without
//!   touching the caller's tree.
//! - [`author_claim`] — the establish-then-write authoring core `claim add` calls:
//!   run the check, require `Held`, write the file, never commit, never write a
//!   verdict. Kept here as one gate so nothing can record a claim whose check did
//!   not hold.
//! - [`render_claim`] — the one renderer that turns a claim's fields into the
//!   `.claims/*.md` text, so the injection-hardening of the frontmatter lives in
//!   exactly one place.
//! - [`claim_matches_path`] — the "claims about these paths" prefix match `claim
//!   list` uses to answer which claims are about a given repo path.
//!
//! Errors are typed ([`StoreError`], [`GitError`]) so the CLI maps them to its own
//! surface — a `--json` error `kind` — without matching on prose. Everything
//! terminal-, argument-, and output-shaped stays in the binary; this crate is pure
//! store-and-git logic.

mod agent;
mod author;
mod error;
pub mod git;
mod path;
mod render;
mod store;

pub use agent::{agent_runner_from_env, AgentCmdError, CLAIM_AGENT_CMD_ENV};
pub use author::{author_claim, AuthorError, Authored, Provenance};
pub use error::{GitError, StoreError};
pub use path::{claim_matches_path, under_prefix};
pub use render::{render_claim, CheckRender, ClaimRender, RenderError};
pub use store::{discover, LoadError, LoadedClaim, Store, StoreLoad, CLAIMS_DIR};
