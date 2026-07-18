//! The outcome of checking a claim, and a claim's status derived from its
//! history.
//!
//! These two enums encode the honesty contract of the whole system, so they
//! live in the foundation rather than in any one feature. The rules stated in
//! the doc comments are binding: later work implements them, it does not revisit
//! them.

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
    /// Whether this verdict lets a claim be considered fresh. Only [`Held`](Verdict::Held)
    /// does. Everything else — including `Unverifiable` and `Broken` — leaves the
    /// claim to age toward `stale` and, eventually, a human.
    #[must_use]
    pub fn keeps_fresh(self) -> bool {
        matches!(self, Verdict::Held)
    }
}

/// A claim's status. Never written to the claim file: always computed from the
/// verdict history and the claim's `max_age` at the moment it is read, the same
/// way a certificate's validity is read from its dates rather than a stored
/// flag.
///
/// A claim exists once its file is merged into a store, and not before; there is
/// no draft state inside the system, because an unmerged claim is a pull
/// request. A merged claim with no passing verdict on record is simply `Stale`
/// and due immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// The latest check passed and the claim is within `max_age`.
    Verified,
    /// The claim's own check reports its fact is no longer true.
    Drifted,
    /// Overdue: never verified, past `max_age`, or its checks have been broken or
    /// unverifiable past the configured grace window.
    Stale,
    /// Closed on purpose: the world changed and the decision was re-reviewed, or
    /// the fact became a real test and the closing note says where.
    Retired,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_held_keeps_a_claim_fresh() {
        assert!(Verdict::Held.keeps_fresh());
        assert!(!Verdict::Drifted.keeps_fresh());
        assert!(!Verdict::Unverifiable.keeps_fresh());
        assert!(!Verdict::Broken.keeps_fresh());
    }

    #[test]
    fn verdict_serializes_as_kebab_case() {
        assert_eq!(
            serde_json::to_string(&Verdict::Unverifiable).unwrap(),
            "\"unverifiable\""
        );
    }

    #[test]
    fn status_serializes_as_kebab_case() {
        assert_eq!(
            serde_json::to_string(&Status::Verified).unwrap(),
            "\"verified\""
        );
    }
}
