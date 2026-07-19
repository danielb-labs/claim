//! `claim show <id>`: print one claim's full definition.
//!
//! The static counterpart to `claim check <id>`. Where `check` runs a claim's
//! checks and reports a verdict, `show` runs *nothing*: it discovers the store,
//! finds the one claim whose id matches, and prints everything the file holds — the
//! id, the file path, the statement, each check (its mechanism, command or
//! instruction, `negate`, and any `skip`), the `supports` targets, and the `hub:`
//! hints. No check executes, no verdict is produced, nothing is stored.
//!
//! `show` is about a *single* claim, which shapes its error contract. Every miss is a
//! loud exit-2 error, never an empty "success" — printing nothing and exiting 0 for a
//! typo is exactly the quiet failure the tool exists to prevent (invariant #6) — but
//! the *message* is the honest one for the cause, via [`claim_store::StoreLoad::resolve`]:
//! an unknown id names the id; a *duplicate* id (declared in two files, so dropped as
//! ambiguous) says "declared more than once" rather than a false "not found"; and an
//! id whose file failed to parse surfaces that parse error, so the user sees *why* the
//! claim could not be shown. An unrelated malformed *sibling* — a different id in the
//! load errors — is not this command's concern: if the requested claim loaded cleanly,
//! it is shown and the command exits 0. Store-wide health is `list`/`check`.

use anyhow::{Context, Result};
use claim_core::{Check, CheckKind, Hub, Skip, SupportTarget};
use serde::Serialize;

use crate::apperror::{app, ErrorKind};
use crate::cli::ShowArgs;
use crate::output::{emit, Format};
use claim_store::{discover, LoadedClaim, Resolved};

/// The exit code when the claim was found and printed. `show` is binary: this or an
/// `Err` (exit 2). Carried on the `--json` envelope for symmetry with the read verbs
/// (`list`/`check`/`drift`/`graph` all report `exit`) so a consumer need not inspect
/// `$?`.
const EXIT_OK: i32 = 0;

/// Run `claim show`.
///
/// Returns `Ok(())` (exit 0) once the claim is printed. Every failure is an `Err`
/// (exit 2): no store found, an unknown id, a duplicate id declared twice, or a
/// target whose file could not be parsed. There is no review-worthy middle code —
/// `show` either found and printed the one claim or it did not.
///
/// # Errors
///
/// Fails when no store is found; when no claim in the store has the requested id
/// ([`ErrorKind::InvalidInput`], naming the id); when two files declare the id (so it
/// was dropped as ambiguous — reported as "declared twice", not "not found"); or when
/// the file that *is* the requested id failed to parse (surfacing that parse reason,
/// so a typo, a duplicate, and a broken file are three distinct messages).
pub fn run(args: &ShowArgs, format: Format) -> Result<()> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let store = discover(&cwd)?;
    let load = store.load_all()?;

    let loaded = match load.resolve(&args.id) {
        Resolved::Found(loaded) => loaded,
        // A duplicate exists twice, so "no such claim" would be a lie; name the
        // conflict (its message lists the colliding files) instead.
        Resolved::Duplicate(err) => {
            return Err(app(
                ErrorKind::InvalidInput,
                format!(
                    "claim '{}' is declared more than once: {}",
                    args.id, err.message
                ),
            ));
        }
        // A broken file named for the id: surface *why* it could not be shown, so a
        // broken file is distinguishable from a typo.
        Resolved::LoadFailed(err) => {
            return Err(app(
                ErrorKind::InvalidInput,
                format!("claim '{}' could not be loaded: {}", args.id, err.message),
            ));
        }
        Resolved::NotFound => {
            return Err(app(
                ErrorKind::InvalidInput,
                format!(
                    "no claim with id '{}' in this store; run `claim list` to see the ids that \
                     exist",
                    args.id
                ),
            ));
        }
    };

    let report = ClaimReport::new(loaded);
    emit(format, &report, || human(&report))
}

/// The machine form of `claim show`: a self-describing envelope carrying the one
/// claim's structured definition.
///
/// The load-bearing fields — `checks`, `supports`, `hub` — are the core model's own
/// `Serialize`, so the JSON reads a check the way the file writes it (a `kind`
/// discriminator, a `run`/`negate` or `instruction`, a `skip`) and the two cannot
/// drift. Only `id`, `file`, and `statement` are lifted onto the envelope, because
/// the id lives on the claim as a typed [`ClaimId`](claim_core::ClaimId) and the
/// file path lives on the [`LoadedClaim`], not the claim.
#[derive(Debug, Serialize)]
struct ClaimReport<'a> {
    /// Always `"ok"`: the claim was found and printed. A failure never reaches here
    /// — it is an `Err` mapped to an exit-2 error object.
    status: &'static str,
    /// The exit code (always [`EXIT_OK`] here, since a failure is an `Err`),
    /// duplicated in the process exit so a `--json` consumer need not inspect `$?` —
    /// matching `list`/`check`/`drift`/`graph`.
    exit: i32,
    /// The claim's id.
    id: String,
    /// The claim file's path relative to the store root, e.g.
    /// `.claims/payments/libfoo-pin.md`.
    file: String,
    /// The plain-language statement — the fact the claim records.
    statement: String,
    /// The checks that re-verify the statement, serialized by the core model.
    checks: &'a [Check],
    /// The `supports` targets this claim justifies, serialized by the core model.
    supports: &'a [SupportTarget],
    /// The `hub:` scheduling hints (`recheck`/`max-age`), serialized by the core
    /// model. An empty block serializes to `{}` — the CLI never invents a cadence.
    hub: Hub,
}

impl<'a> ClaimReport<'a> {
    fn new(loaded: &'a LoadedClaim) -> Self {
        let claim = &loaded.claim;
        ClaimReport {
            status: "ok",
            exit: EXIT_OK,
            id: claim.id.to_string(),
            file: loaded.path.clone(),
            statement: claim.statement.trim().to_owned(),
            checks: &claim.checks,
            supports: &claim.supports,
            hub: claim.hub,
        }
    }
}

/// Print the claim as an aligned, readable definition, so a person sees at a glance
/// what the claim asserts and how it is verified.
fn human(report: &ClaimReport) {
    println!("{}  ({})", report.id, report.file);
    println!();
    println!("{}", report.statement);

    println!();
    println!("checks:");
    for (i, check) in report.checks.iter().enumerate() {
        print_check(i, check);
    }

    if !report.supports.is_empty() {
        println!();
        println!("supports:");
        for target in report.supports {
            println!("  - {target}");
        }
    }

    print_hub(&report.hub);
}

/// Print one check: its kind and payload, then its skip if it declares one.
fn print_check(index: usize, check: &Check) {
    match &check.kind {
        CheckKind::Cmd { run, negate } => {
            let tag = if *negate { " (negated)" } else { "" };
            println!("  [{index}] cmd{tag}");
            println!("      run: {run}");
        }
        CheckKind::Agent { instruction } => {
            println!("  [{index}] agent");
            println!("      instruction: {instruction}");
        }
        CheckKind::Human { prompt } => {
            println!("  [{index}] human");
            if let Some(prompt) = prompt {
                println!("      prompt: {prompt}");
            }
        }
    }
    if let Some(skip) = &check.skip {
        print_skip(skip);
    }
}

/// Print a check's declared skip: its reason, and the guards (`unless`/`until`) that
/// keep it honest, so a reader sees the whole acknowledged debt, never just that a
/// check is muted.
fn print_skip(skip: &Skip) {
    println!("      skip: {}", skip.reason);
    if let Some(unless) = &skip.unless {
        println!("        unless: {unless}");
    }
    match &skip.until {
        Some(until) => println!("        until: {until}"),
        None => println!("        until: no expiry"),
    }
}

/// Print the `hub:` hints, if any. A claim with no hints prints nothing — the CLI
/// invents no cadence, and an absent block is honestly absent rather than a
/// misleading default.
fn print_hub(hub: &Hub) {
    if hub.recheck.is_none() && hub.max_age.is_none() {
        return;
    }
    println!();
    println!("hub:");
    if let Some(recheck) = hub.recheck {
        println!("  recheck: {recheck}");
    }
    if let Some(max_age) = hub.max_age {
        println!("  max-age: {max_age}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The report lifts id/file/statement onto the envelope and borrows the claim's
    /// typed checks/supports/hub, so the machine form carries the whole definition,
    /// with a binary `exit: 0` for cross-verb symmetry.
    #[test]
    fn report_carries_the_full_definition() {
        let text = "---\nid: pin\nchecks:\n  - kind: cmd\n    run: \"true\"\nsupports:\n  - a\nhub:\n  max-age: 30d\n---\nWe pin it.\n";
        let claim = claim_core::parse_claim_file(".claims/pin.md", text).unwrap();
        let loaded = LoadedClaim {
            claim,
            path: ".claims/pin.md".to_owned(),
        };
        let report = ClaimReport::new(&loaded);
        assert_eq!(report.id, "pin");
        assert_eq!(report.file, ".claims/pin.md");
        assert_eq!(report.statement, "We pin it.");
        assert_eq!(report.exit, 0);
        assert_eq!(report.checks.len(), 1);
        assert_eq!(report.supports.len(), 1);
        assert_eq!(report.hub.max_age.map(|d| d.get()), Some(30));
    }
}
