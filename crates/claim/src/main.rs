//! The `claim` command-line tool.
//!
//! A thin shell over `claim-core`: parsing and check execution live in the core
//! crate; this binary wires them to a CLI. The tool is a stateless runtime verifier
//! — it reads claim files, runs their checks, and reports the current verdict; it
//! stores nothing (see `docs/design/CLI-HUB-BOUNDARY.md`). The entry point stays a
//! small dispatcher — argument grammar in [`cli`], each verb's logic in
//! [`commands`], shared concerns (store discovery, git provenance, output) in their
//! own modules — so a later build item slots a verb in without touching the shape.
//!
//! # Exit codes
//!
//! Scriptable and stable. A usage error, a missing store, an I/O or git fault, or
//! any other failure to run exits `2` ([`EXIT_ERROR`]) with an error on stderr.
//! When a verb *runs* but finds a review-worthy condition, it returns its own
//! code instead of erroring:
//!
//! - **`claim check`** — `0` when every check held and every support resolved;
//!   `1` when at least one check drifted or was unverifiable, or a support did not
//!   resolve; `2` when a check was broken, a claim file could not be loaded, or a
//!   tool error occurred. Highest code wins over a mixed store.
//! - **`claim drift`** — `0` when no claim's check drifted, `1` when any did, `2`
//!   when a check was broken or a claim file could not be loaded.
//! - **`claim list`** — `0` normally, `2` when a claim file could not be loaded
//!   (the well-formed claims are still listed — a broken file nags, it does not
//!   silence the store).
//!
//! Every other verb is binary: `0` on success, `2` on error. A command signals a
//! non-error exit code by returning it from [`dispatch`]; an `Err` is always
//! `EXIT_ERROR`, so a bug that returns `Err` can never masquerade as a specific
//! low code.

mod apperror;
mod claimfile;
mod cli;
mod commands;
mod output;

use clap::Parser;

use apperror::kind_of;
use cli::{Cli, Command};
use output::Format;

/// The exit code for a usage, I/O, git, or other failure to run — distinct from a
/// verb's own review-worthy codes, which it returns as `Ok(code)`.
const EXIT_ERROR: i32 = 2;

fn main() {
    let cli = Cli::parse();
    let format = Format::from_json_flag(cli.json);

    match dispatch(&cli.command, format) {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            report_error(&err, format);
            std::process::exit(EXIT_ERROR);
        }
    }
}

/// Route a parsed command to its implementation, returning the process exit code.
///
/// Most verbs return `0` on success; `check`, `list`, and `drift` return a richer
/// code (see the module docs) computed from what they found. An `Err` is reported
/// and mapped to [`EXIT_ERROR`] by `main`, so a failure to run is always `2` and
/// never a verb's low code.
///
/// `amend`, `retire`, and `docs` are binary: `0` on success (via `.map(|()| 0)`),
/// `2` on any error — they have no review-worthy middle code the way `check`/`drift`
/// do.
fn dispatch(command: &Command, format: Format) -> anyhow::Result<i32> {
    match command {
        Command::Init(args) => commands::init::run(args, format).map(|()| 0),
        Command::Add(args) => commands::add::run(args, format).map(|()| 0),
        Command::Check(args) => commands::check::run(args, format),
        Command::List(args) => commands::list::run(args, format),
        Command::Drift(args) => commands::drift::run(args, format),
        Command::Amend(args) => commands::amend::run(args, format).map(|()| 0),
        Command::Retire(args) => commands::retire::run(args, format).map(|()| 0),
        Command::Docs(args) => commands::docs::run(args, format).map(|()| 0),
    }
}

/// Print an error to the user: a JSON object in `--json` mode, a plain line
/// otherwise. Errors go to stderr in both modes so stdout carries only a command's
/// real output.
///
/// The whole cause chain is rendered, not just the outermost context, so the
/// specific fix a lower layer named (a parser's "max-age: ... write '120d'") is not
/// swallowed by a broad wrapper ("the claim you described is not valid"). The JSON
/// form also carries a stable `kind` discriminator ([`kind_of`]) so a scripting
/// consumer branches on the machine value, not the English prose.
fn report_error(err: &anyhow::Error, format: Format) {
    let message = full_chain(err);
    match format {
        Format::Json => {
            let body = serde_json::json!({
                "status": "error",
                "kind": kind_of(err).as_str(),
                "error": message,
            });
            // A serialization failure on this tiny object is impossible in practice;
            // fall back to the plain message rather than panicking.
            let rendered = serde_json::to_string_pretty(&body)
                .unwrap_or_else(|_| format!("{{\"status\":\"error\",\"error\":\"{message}\"}}"));
            eprintln!("{rendered}");
        }
        Format::Human => eprintln!("error: {message}"),
    }
}

/// Flatten an error and its causes into one `: `-joined line.
///
/// Anyhow's default `Display` shows only the top context; a caller needs the leaf
/// cause too, because that is where the actionable detail (the field, the fix)
/// lives. Consecutive identical messages are collapsed so a context that merely
/// re-states its source does not read as a stutter.
fn full_chain(err: &anyhow::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    for cause in err.chain() {
        let text = cause.to_string();
        if parts.last().map(String::as_str) != Some(text.as_str()) {
            parts.push(text);
        }
    }
    parts.join(": ")
}
