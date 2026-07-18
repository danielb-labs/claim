//! The `claim` command-line tool.
//!
//! A thin shell over `claim-core`: parsing, check execution, and the verdict log
//! all live in the core crate; this binary wires them to a CLI. The entry point
//! stays a small dispatcher — argument grammar in [`cli`], each verb's logic in
//! [`commands`], shared concerns (store discovery, git provenance, output) in their
//! own modules — so a later build item slots a verb in without touching the shape.
//!
//! # Exit codes
//!
//! Scriptable and stable: `0` on success, `2` on any usage or other error. The
//! richer check/drift codes (a `Drifted` claim failing CI, for instance) arrive
//! with those verbs; the two verbs built here are binary — they worked or they did
//! not.

mod claimfile;
mod cli;
mod commands;
mod git;
mod output;
mod store;

use clap::Parser;

use cli::{Cli, Command};
use output::Format;

/// The non-success exit code for a usage or other error, per the module docs.
const EXIT_ERROR: i32 = 2;

fn main() {
    let cli = Cli::parse();
    let format = Format::from_json_flag(cli.json);

    if let Err(err) = dispatch(&cli.command, format) {
        report_error(&err, format);
        std::process::exit(EXIT_ERROR);
    }
}

/// Route a parsed command to its implementation.
///
/// Unbuilt verbs return an error here rather than doing nothing, so an unfinished
/// command exits non-zero and never masquerades as a success.
fn dispatch(command: &Command, format: Format) -> anyhow::Result<()> {
    match command {
        Command::Init(args) => commands::init::run(args, format),
        Command::Add(args) => commands::add::run(args, format),
        Command::Check
        | Command::List
        | Command::Log
        | Command::Drift
        | Command::Amend
        | Command::Retire
        | Command::Stats => {
            anyhow::bail!("`claim {}` is not implemented yet", command.name())
        }
    }
}

/// Print an error to the user: a JSON object in `--json` mode, a plain line
/// otherwise. Errors go to stderr in both modes so stdout carries only a command's
/// real output.
///
/// The whole cause chain is rendered, not just the outermost context, so the
/// specific fix a lower layer named (a parser's "max-age: ... write '120d'") is not
/// swallowed by a broad wrapper ("the claim you described is not valid").
fn report_error(err: &anyhow::Error, format: Format) {
    let message = full_chain(err);
    match format {
        Format::Json => {
            let body = serde_json::json!({
                "status": "error",
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
