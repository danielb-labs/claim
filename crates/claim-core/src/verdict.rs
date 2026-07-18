//! The outcome of checking a claim: the verdict a check reports right now.
//!
//! This enum encodes the honesty contract of the whole system, so it lives in
//! the foundation rather than in any one feature. The rules stated in the doc
//! comments are binding: later work implements them, it does not revisit them.
//!
//! A verdict is a *check result* — what a check said the instant it ran — never
//! a claim's *status* over time. The CLI reports verdicts and does not persist
//! them; any longer-lived notion of freshness, staleness, or due-ness belongs to
//! the hub that ingests the reported stream, not to this stateless verifier. See
//! `docs/design/CLI-HUB-BOUNDARY.md`.

use serde::{Deserialize, Serialize};

/// The result of running one check against one claim at one moment.
///
/// The critical rule is the boundary of [`Held`](Verdict::Held): a check earns
/// `Held` only by signalling success deliberately. A check that fails to run —
/// a missing interpreter, a deleted directory, a typo in a command — must never
/// land here. It is [`Broken`](Verdict::Broken), which is loud and counts
/// against the claim's freshness, exactly like a check that has never run.
///
/// This is why the tool owns the mapping from a process's exit code to a
/// verdict, and why negation is a property of the claim rather than a shell `!`:
/// under `sh -c "! rg pattern dir"`, a missing `rg` or a deleted `dir` inverts
/// to success and reports a false `Held`. A green light that cannot turn red is
/// the single failure this tool exists to prevent.
///
/// The canonical exit-code mapping that check execution must implement:
///
/// | process outcome        | verdict                    |
/// |------------------------|----------------------------|
/// | exit 0                 | `Held`                     |
/// | exit 1                 | `Drifted`                  |
/// | any other exit code    | `Broken`                   |
/// | failed to spawn / signal | `Broken`                 |
///
/// A claim's declared `negate` inverts `Held` and `Drifted` only. It never
/// rewrites `Broken` into a pass: a broken check is broken regardless of
/// negation. `Unverifiable` is reserved for checks that run to completion but
/// cannot reach a conclusion (an agent finding conflicting evidence, a probe
/// that times out); a plain `cmd` check never returns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// The claim's stated fact is still true.
    Held,
    /// The claim's stated fact is no longer true.
    Drifted,
    /// The check ran but could not determine an answer. Distinct from `Drifted`,
    /// and never produced by a plain command check.
    Unverifiable,
    /// The check itself could not run or answer. Never counts as `Held`; treated
    /// as a freshness failure, like a check that has never run.
    Broken,
}

impl Verdict {
    /// Whether this verdict is a pass — the check confirmed the fact holds right
    /// now. Only [`Held`](Verdict::Held) is; everything else — including
    /// `Unverifiable` and `Broken` — is a signal to a reader, and to the hub, that
    /// the fact needs attention.
    #[must_use]
    pub fn is_held(self) -> bool {
        matches!(self, Verdict::Held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_held_is_a_pass() {
        assert!(Verdict::Held.is_held());
        assert!(!Verdict::Drifted.is_held());
        assert!(!Verdict::Unverifiable.is_held());
        assert!(!Verdict::Broken.is_held());
    }

    #[test]
    fn verdict_serializes_as_kebab_case() {
        assert_eq!(
            serde_json::to_string(&Verdict::Unverifiable).unwrap(),
            "\"unverifiable\""
        );
    }
}
