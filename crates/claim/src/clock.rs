//! The one place the CLI reads "now".
//!
//! The read/verify verbs (`check`, `list`, `drift`) all need the current instant:
//! `check` to decide what is due and to stamp the verdicts it appends, `list` and
//! `drift` to compute status against `max_age`. Core keeps `now` a parameter so its
//! logic is deterministic (see [`claim_core::compute_status`]); the binary is where
//! a real clock must finally be read, and this module is the single seam that reads
//! it.
//!
//! # The `CLAIM_NOW` test seam, and why it is debug-only
//!
//! Centralizing the clock buys the tests a pinned instant: when [`CLAIM_NOW_ENV`]
//! holds an RFC 3339 timestamp, [`now`] returns it instead of the wall clock, so an
//! integration test measures the due/stale arithmetic against a fixed moment rather
//! than racing the real clock.
//!
//! That override is honored **only in debug builds** (`#[cfg(debug_assertions)]`).
//! A release or installed binary ignores the variable entirely and always reads the
//! wall clock. This is a trust boundary, not a convenience: a freshness tool whose
//! notion of "now" can be moved by an environment variable is a tool that can be
//! made to lie — set `CLAIM_NOW` to a past date and every claim reads `verified`,
//! and a persisting `claim check` would stamp the forged instant into the
//! append-only verdict log. The `assert_cmd` tests run the *debug* binary, so the
//! seam keeps working under test while never existing in a shipped binary.
//!
//! Even in debug, honoring the override is announced: [`now`] emits a loud
//! `warning:` line on stderr whenever it returns an overridden instant, so a debug
//! run can never quietly use a fake clock. A malformed value is always a hard error,
//! never a silent fall-back to the wall clock.

use anyhow::Result;
use claim_core::Timestamp;

/// The environment variable that, in a **debug build only**, overrides the wall
/// clock in [`now`] when set to an RFC 3339 timestamp. Ignored by release builds.
/// For deterministic tests.
///
/// Only read in a debug build (the override is `#[cfg(debug_assertions)]`), so a
/// release build never references it; the `allow(dead_code)` keeps the constant —
/// and its documentation — present in every build without a release-only warning.
#[cfg_attr(not(debug_assertions), allow(dead_code))]
pub const CLAIM_NOW_ENV: &str = "CLAIM_NOW";

/// The current instant.
///
/// In a release build this is always the wall clock. In a debug build it is the
/// wall clock unless [`CLAIM_NOW_ENV`] is set, in which case it is that instant —
/// and honoring the override prints a loud `warning:` to stderr so it is never
/// silent.
///
/// # Errors
///
/// In a debug build, fails when [`CLAIM_NOW_ENV`] is set but does not parse as an
/// RFC 3339 timestamp (or is non-UTF-8). A bad override is loud rather than silently
/// ignored: a test or script that pins `now` and mistypes it must not fall through
/// to the real clock and pass by accident. A release build never reads the variable,
/// so it never fails here.
pub fn now() -> Result<Timestamp> {
    #[cfg(debug_assertions)]
    {
        if let Some(overridden) = overridden_now()? {
            crate::output::warn(&format!(
                "using an overridden clock ({CLAIM_NOW_ENV}={overridden}); this is a debug-only \
                 test seam and is ignored by release builds"
            ));
            return Ok(overridden);
        }
    }
    Ok(Timestamp::now())
}

/// Read and parse the [`CLAIM_NOW_ENV`] override, if set. Debug builds only.
///
/// Returns `Ok(None)` when unset (the ordinary case), `Ok(Some(_))` with the pinned
/// instant when set to a valid timestamp, and an `Err` when set to something that is
/// not a valid RFC 3339 timestamp — the loud-not-silent contract.
#[cfg(debug_assertions)]
fn overridden_now() -> Result<Option<Timestamp>> {
    use anyhow::Context;
    match std::env::var(CLAIM_NOW_ENV) {
        Ok(raw) => raw.parse::<Timestamp>().map(Some).with_context(|| {
            format!("{CLAIM_NOW_ENV} is set to '{raw}', which is not an RFC 3339 timestamp")
        }),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("{CLAIM_NOW_ENV} is set to a non-UTF-8 value; use an RFC 3339 timestamp")
        }
    }
}
