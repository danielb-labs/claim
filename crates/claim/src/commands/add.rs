//! `claim add`: author a claim, prove its check passes against reality now, then
//! write the definition file and its establishing log entry.
//!
//! This verb is where two invariants meet the user. Invariant #4 (a write is a
//! commit): `add` writes the claim file and appends the log entry to the working
//! tree, and stops — it never runs `git commit`; the user commits. Invariant #5
//! (a passing check verifies the fact): the default path runs the check once,
//! requires `Held`, and records it. `Drifted` (the fact is already false) and
//! `Broken` (the check cannot run) are refused — nothing is written.
//!
//! # The default path never touches the working tree
//!
//! `add` runs the check against the current tree exactly once and writes the claim
//! and its establishing verdict. It does not perturb the tree, restore it, or
//! require it to be clean. An agent working in a dirty tree can author a claim with
//! no ceremony, and there is no path on which uncommitted work can be lost.
//!
//! # Optional witnessed-red confidence (`--witness-cmd`)
//!
//! Witnessing a check go red is no longer required — a passing check is enough. But
//! an author who *can* stage the red may still ask for the extra confidence that the
//! check discriminates (a `grep` for the wrong string passes forever while proving
//! nothing). `--witness-cmd` supplies a command that makes the fact false; the tool
//! then:
//!
//! 1. Creates a throwaway `git worktree` detached at `HEAD`, *outside* the
//!    repository, so nothing below runs against the user's tree.
//! 2. Runs the witness command there, perturbing that isolated checkout.
//! 3. Runs the check there and requires `Drifted` — an *observed* red. A check that
//!    still `Held` does not detect this fact going false and is refused; a `Broken`
//!    means the perturbation broke execution and is refused.
//! 4. Tears the worktree down (even on failure).
//!
//! The observed red is recorded as an evidence note on the establishing entry. The
//! user's working tree is never mutated, so `--witness-cmd` needs no clean-tree
//! requirement and can never lose uncommitted work. Witnessing needs a born `HEAD`
//! (the worktree checks out a commit); on an unborn repository `--witness-cmd` is
//! refused with the fix, while the default no-witness path still works.

use std::io::{IsTerminal, Write};

use anyhow::{bail, Context, Result};
use claim_core::{
    append_entry, run_check, Check, CheckContext, CheckOutcome, Claim, ClaimId, Event, LogEntry,
    Timestamp, Verdict,
};
use serde::Serialize;

use crate::apperror::{app, ErrorKind};
use crate::claimfile::{
    primary_cmd_check, render_and_validate, CheckDraft, CheckDraftKind, ClaimDraft,
};
use crate::cli::AddArgs;
use crate::output::{emit, note, Format};
use claim_store::{discover, git, Store};

/// The machine form of `claim add`.
#[derive(Debug, Serialize)]
struct AddReport {
    /// Always `"ok"` on success.
    status: &'static str,
    /// The created claim's id.
    id: String,
    /// The store root (repository root), so an agent invoked from a subdirectory can
    /// resolve `file` and `to_commit`, which are root-relative.
    root: String,
    /// The path of the written claim file, relative to the store root.
    file: String,
    /// The full 40-char commit sha the establishing verdict was recorded against (the
    /// unborn-HEAD sentinel when the repo has no commit yet). Full, not abbreviated,
    /// so the recorded provenance does not vary with `core.abbrev`.
    commit: String,
    /// The actor the establishing verdict was attributed to.
    actor: String,
    /// The check's verdict on the establishing run — always `held`.
    verdict: Verdict,
    /// Whether the check was additionally witnessed failing in isolation. Optional
    /// confidence, not a gate: `false` is a fully verified claim, not a penalized one.
    witnessed_red: bool,
    /// The paths the caller must `git add` and commit, relative to `root` (invariant
    /// #4: the tool does not commit). Anchor them with `root`, or use the printed
    /// `git -C <root> add ...` line, so they resolve from any working directory.
    to_commit: Vec<String>,
}

/// Run `claim add`. See the module docs for the default path and the optional
/// witnessed-red confidence.
///
/// # Errors
///
/// Fails, with a message naming the fix, on: no store found; a missing required
/// field with no TTY to prompt for it; an invalid id, trigger, or `max-age`; a
/// duplicate id; a check that is `Drifted` or `Broken` against the current tree; a
/// `--witness-cmd` whose red is not observed (or that is requested on an unborn
/// HEAD); a git provenance failure; or an I/O failure writing the file or log.
pub fn run(args: &AddArgs, format: Format) -> Result<()> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    // `discover` returns `StoreError::NoStore` for a missing store, which
    // `apperror::kind_of` maps to the `no-store` kind, so no per-verb remapping is
    // needed — every verb agrees on the kind.
    let store = discover(&cwd)?;

    let draft = gather_draft(args, format)?;

    // The single validation path: render the file and parse it back, reusing
    // claim-core's schema. The parsed claim and the exact bytes come back together,
    // so what is validated is what is written.
    let file_path = store.claim_file_relative(&parse_id(&draft.id)?);
    let (claim, file_text) = render_and_validate(&draft, &file_path).map_err(|e| {
        app(
            ErrorKind::InvalidInput,
            format!("the claim you described is not valid: {e}"),
        )
    })?;

    reject_duplicate(&store, &claim)?;

    // Provenance is resolved up front, before the check runs: a missing git identity
    // or absent repository should fail while nothing has been written, not after.
    let provenance = Provenance {
        commit: git::resolve_commit(store.root())?,
        actor: git::resolve_actor(store.root())?,
    };

    let check = primary_cmd_check(&claim)
        .context("claim add authors a single cmd check; this claim has none to run")?;

    let ctx = CheckContext::new(store.root());

    // The establishing run: the fact must be true against the current tree. This is
    // the whole of verification — a passing check verifies the fact (invariant #5).
    let establishing = establishing_run(check, &ctx, format)?;

    // Optional: witness the check going red in an isolated worktree, for extra
    // confidence that it discriminates. Never touches the caller's tree.
    let witnessed_red = if let Some(witness_cmd) = &args.witness_cmd {
        witness_in_isolation(&store, &provenance.commit, check, witness_cmd, format)?;
        true
    } else {
        false
    };

    write_claim_and_log(
        &store,
        &claim,
        &file_text,
        &establishing,
        witnessed_red,
        &provenance,
        format,
    )
}

/// The git-derived provenance stamped on the establishing verdict: the commit the
/// check was observed against and the actor who observed it. Resolved once, before
/// anything is written, so a missing identity fails fast.
struct Provenance {
    commit: String,
    actor: String,
}

/// Gather every claim field from flags, falling back to interactive prompts when a
/// TTY is present and a field is absent. In JSON/non-TTY mode a missing required
/// field is a loud error, never a silent default (except `when`, which sensibly
/// defaults to `on-change`).
fn gather_draft(args: &AddArgs, format: Format) -> Result<ClaimDraft> {
    let interactive = !format.is_json() && std::io::stdin().is_terminal();

    let id = require_field(args.id.clone(), "id", "--id", interactive, || {
        prompt("Claim id (kebab-case, e.g. payments/libfoo-pin): ")
    })?;
    let statement = require_field(
        args.statement.clone(),
        "statement",
        "--statement",
        interactive,
        || prompt("Statement (the fact this records): "),
    )?;
    let run = require_field(args.run.clone(), "run", "--run", interactive, || {
        prompt("Check command (exit 0 = holds, exit 1 = drifted): ")
    })?;
    let max_age = require_field(
        args.max_age.clone(),
        "max-age",
        "--max-age",
        interactive,
        || prompt("Max age (e.g. 120d): "),
    )?;

    // `when` has a sensible default, so it is never prompted for or required.
    let when = args.when.clone().unwrap_or_else(|| "on-change".to_owned());

    Ok(ClaimDraft {
        id,
        max_age,
        checks: vec![CheckDraft {
            kind: CheckDraftKind::Cmd {
                run,
                negate: args.negate,
            },
            when,
        }],
        supports: args.supports.clone(),
        statement,
    })
}

/// Resolve a field from its flag, prompting when interactive, else erroring with
/// the flag name to set.
fn require_field(
    from_flag: Option<String>,
    field: &str,
    flag: &str,
    interactive: bool,
    prompt: impl FnOnce() -> Result<String>,
) -> Result<String> {
    if let Some(value) = from_flag {
        return Ok(value);
    }
    if interactive {
        return prompt();
    }
    Err(app(
        ErrorKind::MissingInput,
        format!("missing {field}; pass {flag} (no terminal is attached to prompt for it)"),
    ))
}

/// Prompt on stderr and read one trimmed line from stdin.
fn prompt(message: &str) -> Result<String> {
    eprint!("{message}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    let n = std::io::stdin()
        .read_line(&mut line)
        .context("failed to read from stdin")?;
    if n == 0 {
        bail!("input ended before the prompt was answered");
    }
    Ok(line.trim().to_owned())
}

/// Parse a raw id through claim-core's validator (reused, never reimplemented).
fn parse_id(raw: &str) -> Result<ClaimId> {
    raw.parse::<ClaimId>()
        .map_err(|e| app(ErrorKind::InvalidInput, e.to_string()))
}

/// Refuse a claim whose id already exists anywhere in the store.
///
/// A duplicate id is a false-green hazard: two files sharing an id share one
/// verdict log (`.claims/log/<id>/`), so their histories interleave and a drifted
/// fact can read as verified. Checking only the canonical path `.claims/<id>.md`
/// misses a claim that declares the same id from a *differently named* file, so
/// this scans every parsed claim's id, not just the one path — the id, not the
/// filename, is what must be unique. A canonical-path collision is still checked
/// too, in case a file exists but does not parse (and so is absent from the id
/// scan).
fn reject_duplicate(store: &Store, claim: &Claim) -> Result<()> {
    let canonical = store.claim_file(&claim.id);
    if canonical.exists() {
        return Err(app(
            ErrorKind::DuplicateId,
            format!(
                "a claim with id '{}' already exists at {}; choose a different id or edit that file",
                claim.id,
                canonical.display()
            ),
        ));
    }

    let load = store.load_all()?;
    if let Some(existing) = load.claims.iter().find(|c| c.claim.id == claim.id) {
        return Err(app(
            ErrorKind::DuplicateId,
            format!(
                "a claim with id '{}' is already declared in {}; choose a different id or edit \
                 that file",
                claim.id, existing.path
            ),
        ));
    }
    Ok(())
}

/// Run the check against the current tree and require `Held`, showing the evidence.
///
/// This is the whole of verification: a passing check against reality establishes
/// the fact (invariant #5). `Drifted` means the fact is already false — recording it
/// would be a lie. `Broken` means the check cannot run — there is nothing to trust.
/// Both are refused with the evidence, so the author sees *why*.
fn establishing_run(check: &Check, ctx: &CheckContext, format: Format) -> Result<CheckOutcome> {
    note(format, "Running the check against the current tree...");
    let outcome = run_check(check, ctx);
    show_evidence(format, "check", &outcome);

    match outcome.verdict {
        Verdict::Held => Ok(outcome),
        Verdict::Drifted => Err(app(
            ErrorKind::DriftedGreen,
            format!(
                "the check reports Drifted against the current tree ({}): the fact is already \
                 false, so there is nothing true to record. Fix the fact or the check first.",
                outcome.status()
            ),
        )),
        Verdict::Broken => Err(app(
            ErrorKind::BrokenGreen,
            format!(
                "the check is Broken against the current tree ({}): it cannot run, so it cannot be \
                 trusted. Fix the command first.",
                outcome.status()
            ),
        )),
        Verdict::Unverifiable => Err(app(
            ErrorKind::BrokenGreen,
            format!(
                "the check is Unverifiable ({}): claim add authors cmd checks, which never return \
                 this; this indicates an agent/human check, not supported by add in v1.",
                outcome.status()
            ),
        )),
    }
}

/// Optionally prove the check discriminates by observing it go red in an *isolated*
/// checkout — never the caller's tree.
///
/// A throwaway worktree detached at `HEAD` is created outside the repository, the
/// `--witness-cmd` perturbs it, and the check runs there and must report `Drifted`.
/// Because everything runs against the temp checkout, the user's working tree is
/// untouched no matter what the witness or check do, and no uncommitted work can be
/// lost. The worktree is torn down whether the observation succeeds or fails.
///
/// # Errors
///
/// Returns [`ErrorKind::MissingInput`] on an unborn HEAD (there is no commit to
/// check out; the fix is to commit first or drop `--witness-cmd`),
/// [`ErrorKind::NotWitnessed`] when the check does not go red, and any git or I/O
/// error from creating or tearing down the worktree.
fn witness_in_isolation(
    store: &Store,
    head_commit: &str,
    check: &Check,
    witness_cmd: &str,
    format: Format,
) -> Result<()> {
    // The isolated worktree checks out a commit, which an unborn HEAD does not have.
    // Reuse the already-resolved provenance commit rather than re-probing git.
    if head_commit == git::UNBORN_HEAD_SENTINEL {
        return Err(app(
            ErrorKind::MissingInput,
            "--witness-cmd needs a commit to check out in isolation, but this repository has no \
             commit yet (unborn HEAD). Commit something first, or drop --witness-cmd — a passing \
             check already verifies the fact. Nothing was written.",
        ));
    }

    note(
        format,
        "Witnessing the check go red in an isolated worktree...",
    );
    let worktree = git::Worktree::create_at_head(store.root())
        .context("failed to create the isolated worktree for --witness-cmd")?;

    // Everything runs against the worktree checkout, never the caller's tree.
    let observed = observe_red_in(worktree.path(), check, witness_cmd, format);

    // Tear down explicitly so a removal failure is surfaced — but only after the
    // observation, and let the more informative "check didn't go red" error win when
    // both would fire.
    let removed = worktree
        .remove()
        .context("failed to remove the isolated witness worktree");
    observed?;
    removed?;
    Ok(())
}

/// Perturb an isolated worktree with `witness_cmd` and require the check to report
/// `Drifted` there.
///
/// Both the perturbation and the check run with `root` set to the worktree, so the
/// caller's tree is never a factor. The check's working directory is the worktree
/// root, matching where the perturbation applied.
fn observe_red_in(
    worktree_root: &std::path::Path,
    check: &Check,
    witness_cmd: &str,
    format: Format,
) -> Result<()> {
    run_perturbation(worktree_root, witness_cmd)
        .context("the --witness-cmd failed to run; it must transform the tree, not error")?;

    let ctx = CheckContext::new(worktree_root);
    let outcome = run_check(check, &ctx);
    show_evidence(format, "witnessed red", &outcome);
    require_drift(&outcome)
}

/// Require an observed `Drifted`, with a message that names what actually happened.
///
/// This is the optional-confidence check: only a genuine `Drifted` proves the check
/// discriminates. `Held` means the perturbation did not make the fact false (or the
/// check ignores it) — the check is decoration. `Broken`/`Unverifiable` mean the
/// perturbation broke execution.
fn require_drift(outcome: &CheckOutcome) -> Result<()> {
    match outcome.verdict {
        Verdict::Drifted => Ok(()),
        Verdict::Held => Err(app(
            ErrorKind::NotWitnessed,
            format!(
                "the check still reports Held after --witness-cmd made the fact false ({}): it \
                 does not detect this drift, so witnessing cannot confirm it. Nothing was written. \
                 Fix the check to actually test the fact, or drop --witness-cmd.",
                outcome.status()
            ),
        )),
        Verdict::Broken => Err(app(
            ErrorKind::NotWitnessed,
            format!(
                "the check is Broken while witnessing the red ({}): the perturbation broke the \
                 check itself rather than making it report Drifted. Nothing was written.",
                outcome.status()
            ),
        )),
        Verdict::Unverifiable => Err(app(
            ErrorKind::NotWitnessed,
            format!(
                "the check is Unverifiable while witnessing the red ({}). Nothing was written.",
                outcome.status()
            ),
        )),
    }
}

/// Run the `--witness-cmd` through the shell, in the isolated worktree.
///
/// The child's stdout is redirected to *our stderr*: a witness command that prints
/// (most real ones do) must not leak onto stdout, or it would contaminate the single
/// JSON object a `--json` consumer parses. Its stderr is inherited as-is. A non-zero
/// exit is a failure — the command is meant to mutate the tree and succeed, and a
/// silent failure would leave the "red" unobserved.
fn run_perturbation(root: &std::path::Path, cmd: &str) -> Result<()> {
    use std::process::Stdio;
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(root)
        // Send the child's stdout to our stderr so it never mixes into the JSON on
        // stdout; stderr passes through inherited.
        .stdout(Stdio::from(std::io::stderr()))
        .stderr(Stdio::inherit())
        .status()
        .context("failed to spawn the command")?;
    if !status.success() {
        bail!("the command exited non-zero ({status})");
    }
    Ok(())
}

/// Write the claim file and append the establishing log entry. The last step, and
/// the only one that touches the store — everything before it is validation.
fn write_claim_and_log(
    store: &Store,
    claim: &Claim,
    file_text: &str,
    establishing: &CheckOutcome,
    witnessed_red: bool,
    provenance: &Provenance,
    format: Format,
) -> Result<()> {
    let commit = provenance.commit.as_str();
    let actor = provenance.actor.as_str();
    let now = Timestamp::now();

    let file = store.claim_file(&claim.id);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    // Guard against a race between the duplicate check and the write: create-new so
    // a claim file that appeared in between is never clobbered.
    write_new_file(&file, file_text)?;

    // A single birth entry: the establishing Held, which makes the claim born
    // verified. When the red was witnessed in isolation, a note records that observed
    // discrimination as evidence on the same entry.
    let log_root = store.log_dir();
    let held_note = if witnessed_red {
        Some(witness_note())
    } else {
        None
    };
    let held_entry = verification_entry(now, commit, actor, establishing, held_note);
    let held_path = append_entry(&log_root, &claim.id, &held_entry)?;

    let file_rel = relative_to(store.root(), &file);
    let to_commit = vec![file_rel.clone(), relative_to(store.root(), &held_path)];

    let root = store.root().display().to_string();
    let report = AddReport {
        status: "ok",
        id: claim.id.to_string(),
        root: root.clone(),
        file: file_rel,
        commit: commit.to_owned(),
        actor: actor.to_owned(),
        verdict: Verdict::Held,
        witnessed_red,
        to_commit,
    };

    emit(format, &report, || {
        println!("Created claim '{}' at {}", report.id, report.file);
        if report.witnessed_red {
            println!("The check was witnessed failing in an isolated worktree (extra confidence).");
        }
        // Abbreviate for the human line only; the recorded value stays the full sha.
        println!(
            "Recorded the establishing verdict at commit {}.",
            git::short_commit(&report.commit)
        );
        println!("\nNothing is committed yet. Review, then commit:");
        // Anchor the paths at the store root with `git -C`, so the printed command
        // works from any subdirectory the user ran `claim add` in.
        println!(
            "  git -C {} add {}",
            report.root,
            report.to_commit.join(" ")
        );
        println!(
            "  git -C {} commit -m \"Add claim {}\"",
            report.root, report.id
        );
    })
}

/// Build a verification log entry from a check outcome, folding in an optional
/// extra note ahead of the check's own evidence.
fn verification_entry(
    at: Timestamp,
    commit: &str,
    actor: &str,
    outcome: &CheckOutcome,
    extra_note: Option<String>,
) -> LogEntry {
    let evidence = match (extra_note, &outcome.evidence) {
        (Some(note), Some(ev)) => Some(format!("{note}\n{ev}")),
        (Some(note), None) => Some(note),
        (None, ev) => ev.clone(),
    };
    LogEntry {
        at,
        commit: commit.to_owned(),
        actor: actor.to_owned(),
        event: Event::Verification {
            verdict: outcome.verdict,
            evidence,
        },
    }
}

/// The evidence note recorded on the establishing entry when `--witness-cmd`
/// observed the check go red in isolation: proof, in the log, that the check
/// discriminates. Purely additive confidence — its absence never penalizes a claim.
fn witness_note() -> String {
    "witnessed-red: the check was observed reporting Drifted against a perturbed \
     isolated worktree at `claim add` time, proving it detects this fact going false"
        .to_owned()
}

/// Print a check's verdict and evidence as a narration block (human mode only).
fn show_evidence(format: Format, label: &str, outcome: &CheckOutcome) {
    note(
        format,
        &format!(
            "  [{label}] {} ({})",
            verdict_label(outcome.verdict),
            outcome.status()
        ),
    );
    if let Some(ev) = &outcome.evidence {
        for line in ev.lines() {
            note(format, &format!("    | {line}"));
        }
    }
}

/// A lowercase label for a verdict, for narration.
fn verdict_label(v: Verdict) -> &'static str {
    match v {
        Verdict::Held => "Held",
        Verdict::Drifted => "Drifted",
        Verdict::Broken => "Broken",
        Verdict::Unverifiable => "Unverifiable",
    }
}

/// Create a new file, failing loudly if one already exists (a race with the
/// duplicate check, or a concurrent `add`).
fn write_new_file(path: &std::path::Path, contents: &str) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("failed to create the claim file {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("failed to write the claim file {}", path.display()))?;
    Ok(())
}

/// Render `path` relative to `root` for display, falling back to the full path.
fn relative_to(root: &std::path::Path, path: &std::path::Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}
