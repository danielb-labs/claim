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
//! - [`git::resolve_commit`] and [`git::resolve_actor`] — the git-derived
//!   provenance a verdict-log entry needs (invariant #3), plus [`git::Worktree`],
//!   the isolated checkout `claim add --witness-cmd` uses to witness a red without
//!   touching the caller's tree.
//! - [`author_claim`] — the establish-then-write authoring core both `claim add`
//!   and the MCP `create` tool call, so the two front doors take the same steps to
//!   record a claim (run the check, require `Held`, write the file and birth
//!   verdict, never commit) and cannot disagree about what authoring means.
//! - [`render_claim`] — the one renderer that turns a claim's fields into the
//!   `.claims/*.md` text, so `claim add` and `create` emit byte-identical files and
//!   the injection-hardening of the frontmatter lives in exactly one place.
//! - [`claim_matches_path`] — the "claims about these paths" prefix match both
//!   `claim list` and the MCP `query` tool share, so the two cannot answer a path
//!   query differently.
//!
//! Errors are typed ([`StoreError`], [`GitError`]) so each binary maps them to
//! its own surface — the CLI to a `--json` error `kind`, the server to a protocol
//! error — without matching on prose. Everything terminal-, argument-, and
//! output-shaped stays in the binaries; this crate is pure store-and-git logic.

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
pub use store::{discover, LoadError, LoadedClaim, Store, StoreLoad, CLAIMS_DIR, LOG_DIR};
