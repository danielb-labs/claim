//! `claim log <id>`: one claim's full definition and its verdict history.
//!
//! The join docs/design/PRODUCT.md section 3 describes: the definition (statement, checks,
//! max-age, supports) plus every log entry in chronological order — each with its
//! timestamp, verdict or adjudication, actor, commit, and evidence. A thin wrapper
//! over [`claim_core::read_entries`] and the parsed claim, so the CLI never
//! re-derives history semantics that core already owns.

use anyhow::{Context, Result};
use claim_core::{read_entries, Adjudication, Check, CheckKind, Claim, Event, LogEntry};
use serde::Serialize;

use crate::apperror::{app, ErrorKind};
use crate::cli::LogArgs;
use crate::output::{emit, trigger_label, verdict_label, Format};
use claim_store::discover;
use claim_store::git::short_commit;

/// Run `claim log`.
///
/// # Errors
///
/// Fails when no store is found, the id does not exist in the store, or a claim
/// file or verdict log cannot be read. An unknown id is a clear
/// [`ErrorKind::InvalidInput`] naming the id, not a silent empty history — a typo
/// must not read as "a claim with no verdicts". A malformed *other* claim file does
/// not block `log <good-id>`: per-file load errors are tolerated (the good claims
/// still load), but if the requested id's *own* file is the one that failed to
/// parse, that file's error is surfaced so the id resolves to a clear "could not
/// load" rather than "not found".
pub fn run(args: &LogArgs, format: Format) -> Result<()> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let store = discover(&cwd)?;
    let load = store.load_all()?;

    let Some(loaded) = load.claims.iter().find(|c| c.claim.id.as_str() == args.id) else {
        // Distinguish "the file this id lives in failed to parse" from "no such
        // id": a claim file named after the id, or reporting the id, that could not
        // load should say *why*, not that the claim does not exist.
        if let Some(err) = load
            .errors
            .iter()
            .find(|e| file_stem_matches_id(&e.file, &args.id))
        {
            return Err(app(
                ErrorKind::InvalidInput,
                format!("claim '{}' could not be loaded: {}", args.id, err.message),
            ));
        }
        return Err(app(
            ErrorKind::InvalidInput,
            format!(
                "no claim with id '{}' in this store; run `claim list` to see the ids that exist",
                args.id
            ),
        ));
    };

    let history = read_entries(&store.log_dir(), &loaded.claim.id)?;
    let report = History::new(&loaded.claim, &loaded.path, &history);

    emit(format, &report, || human(&report))
}

/// Whether a load-errored file's path could be the file for `id`: its `.md` stem,
/// relative to `.claims/`, equals the id. A best-effort match so an unparseable
/// file named after the requested id reports *that* file's error, not a bare "not
/// found". A claim whose id differs from its filename that fails to parse still
/// falls through to "not found", which is honest — the tool cannot know the id of a
/// file it could not parse.
fn file_stem_matches_id(file: &str, id: &str) -> bool {
    file.strip_prefix(".claims/")
        .and_then(|rest| rest.strip_suffix(".md"))
        .is_some_and(|stem| stem == id)
}

/// The machine form of `claim log`: the definition and the ordered history.
#[derive(Debug, Serialize)]
struct History<'a> {
    /// The claim's id.
    id: &'a str,
    /// The claim file's path relative to the store root.
    file: &'a str,
    /// The definition: statement, checks, max-age, supports.
    definition: Definition<'a>,
    /// Every log entry in chronological order.
    entries: Vec<Entry>,
}

/// The claim's definition, the half of the join that comes from the file.
#[derive(Debug, Serialize)]
struct Definition<'a> {
    /// The plain-language statement.
    statement: &'a str,
    /// The freshness window, e.g. `120d`.
    max_age: String,
    /// The checks, each as `kind` + trigger + payload summary.
    checks: Vec<CheckView>,
    /// The `supports` targets.
    supports: Vec<String>,
}

/// One check, flattened for display.
#[derive(Debug, Serialize)]
struct CheckView {
    /// `cmd`, `agent`, or `human`.
    kind: &'static str,
    /// The trigger, e.g. `on-change` or `every 30d`.
    when: String,
    /// A one-line summary of the check's payload: the command, the instruction, or
    /// the prompt.
    detail: String,
}

/// One history entry, flattened so a verification and an adjudication share a
/// shape a table can print.
#[derive(Debug, Serialize)]
struct Entry {
    /// When the observation was made, RFC 3339.
    at: String,
    /// `verification` or `adjudication`.
    event: &'static str,
    /// The verdict for a verification, or the adjudication name (`retire`).
    verdict: String,
    /// Who or what made the observation.
    actor: String,
    /// The full commit sha the observation was made against.
    commit: String,
    /// The evidence (a verification's evidence, or a retirement's note).
    evidence: Option<String>,
}

impl<'a> History<'a> {
    fn new(claim: &'a Claim, file: &'a str, history: &[LogEntry]) -> Self {
        History {
            id: claim.id.as_str(),
            file,
            definition: Definition {
                statement: claim.statement.trim(),
                max_age: claim.max_age.to_string(),
                checks: claim.checks.iter().map(CheckView::from).collect(),
                supports: claim.supports.iter().map(ToString::to_string).collect(),
            },
            // `read_entries` already returns chronological order; keep it.
            entries: history.iter().map(Entry::from).collect(),
        }
    }
}

impl From<&Check> for CheckView {
    fn from(check: &Check) -> Self {
        let (kind, detail) = match &check.kind {
            CheckKind::Cmd { run, negate } => (
                "cmd",
                if *negate {
                    format!("{run}  (negated)")
                } else {
                    run.clone()
                },
            ),
            CheckKind::Agent { instruction } => ("agent", instruction.clone()),
            CheckKind::Human { prompt } => (
                "human",
                prompt.clone().unwrap_or_else(|| "(no prompt)".to_owned()),
            ),
        };
        CheckView {
            kind,
            when: trigger_label(check.when),
            detail,
        }
    }
}

impl From<&LogEntry> for Entry {
    fn from(entry: &LogEntry) -> Self {
        let (event, verdict, evidence) = match &entry.event {
            Event::Verification { verdict, evidence } => (
                "verification",
                verdict_label(*verdict).to_owned(),
                evidence.clone(),
            ),
            Event::Adjudication { action } => match action {
                Adjudication::Retire { note } => {
                    ("adjudication", "retire".to_owned(), Some(note.clone()))
                }
            },
        };
        Entry {
            at: entry.at.to_string(),
            event,
            verdict,
            actor: entry.actor.clone(),
            commit: entry.commit.clone(),
            evidence,
        }
    }
}

/// Print the claim's definition, then its history in order.
fn human(history: &History) {
    println!("{}  ({})", history.id, history.file);
    println!();
    println!("{}", history.definition.statement);
    println!();
    println!("max-age: {}", history.definition.max_age);
    println!("checks:");
    for check in &history.definition.checks {
        println!("  - {} [{}]  {}", check.kind, check.when, check.detail);
    }
    if history.definition.supports.is_empty() {
        println!("supports: (none)");
    } else {
        println!("supports:");
        for target in &history.definition.supports {
            println!("  - {target}");
        }
    }

    println!();
    if history.entries.is_empty() {
        println!("History: (no verdicts yet — this claim is stale and due immediately)");
        return;
    }
    println!("History ({} entries, oldest first):", history.entries.len());
    for entry in &history.entries {
        println!(
            "  {}  {:<12}  {}  {}",
            entry.at,
            entry.verdict,
            short_commit(&entry.commit),
            entry.actor,
        );
        if let Some(ev) = &entry.evidence {
            for line in ev.lines() {
                println!("        | {line}");
            }
        }
    }
}
