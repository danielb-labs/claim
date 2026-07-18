//! Output plumbing: the one place a verb chooses between human text and `--json`.
//!
//! Every command owes both a readable human form and a stable machine form
//! (PRODUCT.md section 5, "everything has `--json`"). Rather than let each verb
//! re-derive that choice — and risk one command printing JSON to stderr or
//! forgetting the flag — this module carries the selected [`Format`] and offers the
//! two emit paths a verb uses. `init` and `add` are the first users; the plumbing
//! is shared so later verbs inherit it.
//!
//! The contract: machine output is one JSON object on stdout, nothing else on
//! stdout. Human progress and prompts go to stderr, so `claim --json add … | jq`
//! sees only the JSON even while a human watching the terminal still sees the
//! narration.

use std::io::Write;

use anyhow::Result;
use serde::Serialize;

/// Which form a command should print in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Readable text for a person at a terminal.
    Human,
    /// One JSON object on stdout, for agents and scripts.
    Json,
}

impl Format {
    /// The format selected by the global `--json` flag.
    #[must_use]
    pub fn from_json_flag(json: bool) -> Self {
        if json {
            Format::Json
        } else {
            Format::Human
        }
    }

    /// Whether machine output was requested. Verbs consult this to suppress
    /// human-only narration and interactive prompts.
    #[must_use]
    pub fn is_json(self) -> bool {
        matches!(self, Format::Json)
    }
}

/// Emit the final result of a command.
///
/// In [`Format::Json`] the `value` is written to stdout as a single pretty JSON
/// object and `human` is ignored. In [`Format::Human`] the `human` closure runs to
/// print readable text and `value` is ignored. Taking the human side as a closure
/// keeps a verb from building strings it will not use in JSON mode.
///
/// # Errors
///
/// Fails only if serialization or writing to stdout fails.
pub fn emit<T: Serialize>(format: Format, value: &T, human: impl FnOnce()) -> Result<()> {
    match format {
        Format::Json => {
            let json = serde_json::to_string_pretty(value)?;
            let mut out = std::io::stdout().lock();
            writeln!(out, "{json}")?;
            Ok(())
        }
        Format::Human => {
            human();
            Ok(())
        }
    }
}

/// Print a progress or narration line that must never contaminate stdout.
///
/// Always goes to stderr, so it is visible to a human in both modes yet never
/// mixes into the JSON object a `--json` consumer parses from stdout. Suppressed
/// entirely in JSON mode to keep even stderr quiet for scripted runs.
pub fn note(format: Format, message: &str) {
    if !format.is_json() {
        eprintln!("{message}");
    }
}
