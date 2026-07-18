//! The one place the CLI reads "now".
//!
//! The read/verify verbs (`check`, `list`, `drift`) all need the current instant:
//! `check` to decide what is due and to stamp the verdicts it appends, `list` and
//! `drift` to compute status against `max_age`. Core keeps `now` a parameter so its
//! logic is deterministic (see [`claim_core::compute_status`]); the binary is where
//! a real clock must finally be read, and this module is the single seam that reads
//! it.
//!
//! Centralizing it buys one thing the tests need: a documented override. When the
//! environment variable [`CLAIM_NOW_ENV`] holds an RFC 3339 timestamp, [`now`]
//! returns it instead of the wall clock, so an integration test can pin the instant
//! the due/stale arithmetic is measured against and get a deterministic answer
//! without racing the real clock. The override is a *test and scripting* seam, not
//! a user feature — it is intentionally undocumented in `--help` — but it is honest
//! rather than hidden: a malformed value is a loud error, never a silent fall-back
//! to the wall clock that would make a pinned test quietly non-deterministic.

use anyhow::{Context, Result};
use claim_core::Timestamp;

/// The environment variable that, when set to an RFC 3339 timestamp, overrides the
/// wall clock in [`now`]. For deterministic tests and scripted runs.
pub const CLAIM_NOW_ENV: &str = "CLAIM_NOW";

/// The current instant: the wall clock, or the [`CLAIM_NOW_ENV`] override when set.
///
/// # Errors
///
/// Fails when [`CLAIM_NOW_ENV`] is set but does not parse as an RFC 3339 timestamp.
/// A bad override is loud rather than silently ignored: a test or script that pins
/// `now` and mistypes it must not fall through to the real clock and pass by
/// accident.
pub fn now() -> Result<Timestamp> {
    match std::env::var(CLAIM_NOW_ENV) {
        Ok(raw) => raw.parse::<Timestamp>().with_context(|| {
            format!("{CLAIM_NOW_ENV} is set to '{raw}', which is not an RFC 3339 timestamp")
        }),
        Err(std::env::VarError::NotPresent) => Ok(Timestamp::now()),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("{CLAIM_NOW_ENV} is set to a non-UTF-8 value; use an RFC 3339 timestamp")
        }
    }
}
