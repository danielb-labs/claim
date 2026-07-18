//! `claim add`: author a claim, prove its check both passes now and can go red,
//! then write the definition file and its birth log entries.
//!
//! This verb is where two invariants meet the user. Invariant #4 (a write is a
//! commit): `add` writes the claim file and appends log entries to the working
//! tree, and stops — it never runs `git commit`; the user commits. Invariant #5
//! (witnessed-red): a check is trusted only after it is *observed* failing, not
//! merely asserted to.
//!
//! # The witnessed-red workflow
//!
//! A check that has never been seen failing is decoration: a `grep` for the wrong
//! string, a command that exits 0 no matter what, a `negate` with inverted sense —
//! all pass against today's tree while proving nothing, and would sit green forever.
//! So before recording a claim, `add` proves the check discriminates:
//!
//! 1. **Green run.** Run the check against the current tree. It must be `Held` —
//!    the fact must be true *now*. `Drifted` is refused (recording an
//!    already-false fact); `Broken` is refused (the check cannot even run). The
//!    evidence is shown.
//! 2. **Witnessed red.** The tree is perturbed so the fact becomes false, and the
//!    check is re-run. It must come back `Drifted` — an *actually observed* red. If
//!    it still `Held`, the check does not discriminate and is refused; if `Broken`,
//!    the perturbation broke execution and is refused. This observed `Drifted` is
//!    recorded as the claim's first log entry: the evidence that the check works.
//!    - **Interactive** (a TTY, no `--witness-cmd`): the author is prompted to make
//!      the fact false by hand, then continue; then to restore the tree, then
//!      continue.
//!    - **Scripted** (`--witness-cmd`): the supplied command perturbs the tree,
//!      the tool observes the red, then restores it — with `--restore-cmd` if
//!      given, else by reverting tracked changes with git (never `git clean`, so
//!      the untracked store is never at risk).
//! 3. **Restore and confirm.** After the red, the tree is restored and the check
//!    re-run, which must be `Held` again — confirming the recorded fact is true and
//!    the perturbation left nothing behind. This second `Held` is the birth
//!    verdict that makes the claim born-verified.
//!
//! The default path *requires* an observed `Drifted`. The only way to skip it is
//! `--unwitnessed`, the visible escape hatch for a fact whose red genuinely cannot
//! be staged: the claim is still recorded, but with an evidence note that its check
//! was never witnessed failing, so a later `claim list --unverified` can surface it.
//! It is never silently trusted.

use std::io::{IsTerminal, Write};

use anyhow::{bail, Context, Result};
use claim_core::{
    append_entry, run_check, Check, CheckContext, CheckOutcome, Claim, ClaimId, Event, LogEntry,
    SignedDuration, Timestamp, Verdict,
};
use serde::Serialize;

use crate::claimfile::{
    primary_cmd_check, render_and_validate, CheckDraft, CheckDraftKind, ClaimDraft,
};
use crate::cli::AddArgs;
use crate::git;
use crate::output::{emit, note, Format};
use crate::store::{discover, Store};

/// The machine form of `claim add`.
#[derive(Debug, Serialize)]
struct AddReport {
    /// Always `"ok"` on success.
    status: &'static str,
    /// The created claim's id.
    id: String,
    /// The path of the written claim file, relative to the store root.
    file: String,
    /// The commit sha the birth verdict was recorded against.
    commit: String,
    /// The actor the birth verdict was attributed to.
    actor: String,
    /// The check's verdict on the establishing (final) green run — always `held`.
    verdict: Verdict,
    /// Whether the check was witnessed failing. `false` only with `--unwitnessed`.
    witnessed_red: bool,
    /// The paths the caller must `git add` and commit (invariant #4: the tool does
    /// not commit).
    to_commit: Vec<String>,
}

/// Run `claim add`. See the module docs for the witnessed-red workflow.
///
/// # Errors
///
/// Fails, with a message naming the fix, on: no store found; a missing required
/// field with no TTY to prompt for it; an invalid id, trigger, or `max-age`; a
/// duplicate id; a green run that is not `Held`; a witnessed-red run that is not
/// `Drifted`; a git provenance failure; or an I/O failure writing the file or log.
pub fn run(args: &AddArgs, format: Format) -> Result<()> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let store = discover(&cwd)?;

    let draft = gather_draft(args, format)?;

    // Validate by rendering the file and parsing it — the single validation path,
    // reusing claim-core's schema. The parsed claim and the exact bytes come back
    // together, so what we validate is what we write.
    let file_path = store.claim_file_relative(&parse_id(&draft.id)?);
    let (claim, file_text) =
        render_and_validate(&draft, &file_path).context("the claim you described is not valid")?;

    reject_duplicate(&store, &claim)?;

    // Resolve provenance up front, before the check runs or the tree is perturbed:
    // a missing git identity or absent repository is a misconfiguration that should
    // fail while the tree is still untouched, not after the witness dance.
    let provenance = Provenance {
        commit: git::resolve_commit(store.root())?,
        actor: git::resolve_actor(store.root())?,
    };

    let check = primary_cmd_check(&claim)
        .context("claim add authors a single cmd check; this claim has none to run")?;

    let ctx = CheckContext::new(store.root());

    // Step 1: the green run. The fact must be true now.
    let green = green_run(check, &ctx, format)?;

    // Step 2 + 3: witness the red (unless the escape hatch is used), then restore
    // and confirm green. Returns the log entries to record and whether red was
    // witnessed.
    let birth = witness(args, &store, check, &ctx, &green, format)?;

    write_claim_and_log(&store, &claim, &file_text, &birth, &provenance, format)
}

/// The git-derived provenance stamped on a birth verdict: the commit the check was
/// observed against and the actor who observed it. Resolved once, before the tree
/// is touched, so a missing identity fails fast.
struct Provenance {
    commit: String,
    actor: String,
}

/// The verdicts and evidence gathered while proving the check, plus whether the
/// red was witnessed — everything needed to write the birth log entries.
struct BirthEvidence {
    /// The observed `Drifted` outcome, present when the red was witnessed.
    witnessed_drift: Option<CheckOutcome>,
    /// The final `Held` outcome that makes the claim born-verified.
    establishing_held: CheckOutcome,
    /// `false` only under `--unwitnessed`.
    witnessed_red: bool,
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
    bail!("missing {field}; pass {flag} (no terminal is attached to prompt for it)")
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
    raw.parse::<ClaimId>().map_err(|e| anyhow::anyhow!("{e}"))
}

/// Refuse a claim whose id already exists in the store: a duplicate would either
/// clobber a real claim's file or collide its log. The check is on the file path;
/// the id is unique per store by design.
fn reject_duplicate(store: &Store, claim: &Claim) -> Result<()> {
    let path = store.claim_file(&claim.id);
    if path.exists() {
        bail!(
            "a claim with id '{}' already exists at {}; choose a different id or edit that file",
            claim.id,
            path.display()
        );
    }
    Ok(())
}

/// Run the check against the current tree and require `Held`, showing the evidence.
///
/// `Drifted` means the fact is already false — recording it would be a lie. `Broken`
/// means the check cannot run — there is nothing to trust. Both are refused with the
/// evidence, so the author sees *why*.
fn green_run(check: &Check, ctx: &CheckContext, format: Format) -> Result<CheckOutcome> {
    note(format, "Running the check against the current tree...");
    let outcome = run_check(check, ctx);
    show_evidence(format, "green run", &outcome);

    match outcome.verdict {
        Verdict::Held => Ok(outcome),
        Verdict::Drifted => bail!(
            "the check reports Drifted against the current tree ({}): the fact is already false, \
             so there is nothing true to record. Fix the fact or the check first.",
            outcome.status()
        ),
        Verdict::Broken => bail!(
            "the check is Broken against the current tree ({}): it cannot run, so it cannot be \
             trusted. Fix the command first.",
            outcome.status()
        ),
        Verdict::Unverifiable => bail!(
            "the check is Unverifiable ({}): claim add authors cmd checks, which never return this; \
             this indicates an agent/human check, not supported by add in v1.",
            outcome.status()
        ),
    }
}

/// Prove the check can go red (or record the escape hatch), then restore and
/// confirm the green establishing verdict.
fn witness(
    args: &AddArgs,
    store: &Store,
    check: &Check,
    ctx: &CheckContext,
    green: &CheckOutcome,
    format: Format,
) -> Result<BirthEvidence> {
    if args.unwitnessed {
        return Ok(BirthEvidence {
            witnessed_drift: None,
            establishing_held: green.clone(),
            witnessed_red: false,
        });
    }

    let witnessed_drift = if let Some(cmd) = &args.witness_cmd {
        witness_scripted(args, store, check, ctx, cmd, format)?
    } else {
        witness_interactive(check, ctx, format)?
    };

    // Restore-and-confirm: after the perturbation the tree must be back to a state
    // where the fact holds. The scripted path already restored via git; the
    // interactive path asked the author to restore. Either way, re-run and require
    // Held — the establishing verdict, and proof the perturbation left nothing.
    let establishing = run_check(check, ctx);
    show_evidence(format, "confirm green", &establishing);
    if establishing.verdict != Verdict::Held {
        bail!(
            "after restoring the tree the check is {} ({}), not Held: the tree was not restored to \
             a state where the fact is true. Nothing was written.",
            verdict_label(establishing.verdict),
            establishing.status()
        );
    }

    Ok(BirthEvidence {
        witnessed_drift: Some(witnessed_drift),
        establishing_held: establishing,
        witnessed_red: true,
    })
}

/// Scripted witnessed-red: run `--witness-cmd` to perturb the tree, observe the
/// `Drifted`, then restore.
///
/// Restoration is explicit and narrow, never a blunt `git clean` (which would
/// delete the untracked `.claims/` store, and any other untracked file, along with
/// the perturbation). Two restore paths:
///
/// - `--restore-cmd` supplied: run it. This undoes exactly what `--witness-cmd`
///   did, works on an unborn HEAD (no commit to restore from), and is the author's
///   own inverse operation.
/// - `--restore-cmd` omitted: revert *tracked* modifications with `git checkout --
///   .`, which requires the perturbation to only edit tracked files and the repo to
///   have a commit. Untracked files the perturbation created are left for the
///   confirm-green run to catch (the check will not hold if they matter).
///
/// The restore runs *before* the drift is judged, so even a non-`Drifted` outcome
/// leaves the tree restored rather than perturbed.
fn witness_scripted(
    args: &AddArgs,
    store: &Store,
    check: &Check,
    ctx: &CheckContext,
    witness_cmd: &str,
    format: Format,
) -> Result<CheckOutcome> {
    note(format, "Perturbing the tree to witness the check fail...");
    run_perturbation(store.root(), witness_cmd)
        .context("the --witness-cmd failed to run; it must transform the tree, not error")?;

    let outcome = run_check(check, ctx);
    show_evidence(format, "witnessed red", &outcome);

    // Restore before judging, so a non-Drifted result still leaves the tree usable.
    let restore = restore_tree(args, store);
    require_drift(&outcome)?;
    // A restore failure is only fatal once we know the red was genuine; surface it
    // after the drift check so the more informative "check didn't go red" error
    // wins when both would fire.
    restore.context("failed to restore the tree after witnessing the red")?;

    Ok(outcome)
}

/// Undo the perturbation: run `--restore-cmd` if given, else revert tracked
/// changes with git. See [`witness_scripted`] for why `git clean` is never used.
fn restore_tree(args: &AddArgs, store: &Store) -> Result<()> {
    match &args.restore_cmd {
        Some(cmd) => run_perturbation(store.root(), cmd).context("the --restore-cmd failed to run"),
        None => git::revert_tracked_changes(store.root()),
    }
}

/// Interactive witnessed-red: ask the author to make the fact false, observe the
/// `Drifted`, then ask them to restore. The confirm-green run (in [`witness`])
/// verifies the restore.
fn witness_interactive(check: &Check, ctx: &CheckContext, format: Format) -> Result<CheckOutcome> {
    if format.is_json() {
        bail!(
            "witnessing the red needs interaction, but --json implies a script. Pass --witness-cmd \
             to supply the failing state, or --unwitnessed to record the claim unverified."
        );
    }

    prompt(
        "\nNow make the fact FALSE in the working tree (edit a file, remove a pin, ...), then \
         press Enter to re-run the check. It must report Drifted.\n> ",
    )?;

    let outcome = run_check(check, ctx);
    show_evidence(format, "witnessed red", &outcome);
    require_drift(&outcome)?;

    prompt(
        "\nGood — the check went red. Now RESTORE the tree so the fact is true again, then press \
         Enter to confirm.\n> ",
    )?;

    Ok(outcome)
}

/// Require an observed `Drifted`, with a message that names what actually happened.
///
/// This is the heart of invariant #5: only a genuine `Drifted` proves the check
/// discriminates. `Held` means the perturbation did not make the fact false (or the
/// check ignores it) — the check is decoration. `Broken`/`Unverifiable` mean the
/// perturbation broke execution.
fn require_drift(outcome: &CheckOutcome) -> Result<()> {
    match outcome.verdict {
        Verdict::Drifted => Ok(()),
        Verdict::Held => bail!(
            "the check still reports Held after the fact was made false ({}): it does not detect \
             this drift, so it cannot be trusted. Nothing was written. Fix the check to actually \
             test the fact.",
            outcome.status()
        ),
        Verdict::Broken => bail!(
            "the check is Broken while witnessing the red ({}): the perturbation broke the check \
             itself rather than making it report Drifted. Nothing was written.",
            outcome.status()
        ),
        Verdict::Unverifiable => bail!(
            "the check is Unverifiable while witnessing the red ({}). Nothing was written.",
            outcome.status()
        ),
    }
}

/// Run the perturbation command through the shell, in the store root. A non-zero
/// exit is a failure — the command is meant to mutate the tree and succeed, and a
/// silent failure would leave the "red" unwitnessed.
fn run_perturbation(root: &std::path::Path, cmd: &str) -> Result<()> {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(root)
        .status()
        .context("failed to spawn the --witness-cmd")?;
    if !status.success() {
        bail!("the --witness-cmd exited non-zero ({status})");
    }
    Ok(())
}

/// Write the claim file and append the birth log entries. The last step, and the
/// only one that touches the store — everything before it is validation.
fn write_claim_and_log(
    store: &Store,
    claim: &Claim,
    file_text: &str,
    birth: &BirthEvidence,
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

    // The birth entries, in chronological order: the witnessed drift (if any) first,
    // then the establishing hold. Both share the resolved commit and actor.
    //
    // The hold must be strictly later than the drift, or `compute_status` — which
    // orders by each entry's own `at` — could read the drift as the latest
    // conclusive verdict and report the newborn claim as Drifted. A single tick is
    // enough and is the true order (the confirm-green run ran after the red).
    let log_root = store.log_dir();
    let mut written_log = Vec::new();

    let held_at = if birth.witnessed_drift.is_some() {
        now.checked_add(SignedDuration::from_nanos(1))
            .unwrap_or(now)
    } else {
        now
    };

    if let Some(drift) = &birth.witnessed_drift {
        let entry = verification_entry(now, commit, actor, drift, witness_note());
        let path = append_entry(&log_root, &claim.id, &entry)?;
        written_log.push(path);
    }

    let held_note = if birth.witnessed_red {
        None
    } else {
        Some(unwitnessed_note())
    };
    let held_entry =
        verification_entry(held_at, commit, actor, &birth.establishing_held, held_note);
    let held_path = append_entry(&log_root, &claim.id, &held_entry)?;
    written_log.push(held_path);

    let file_rel = relative_to(store.root(), &file);
    let mut to_commit = vec![file_rel.clone()];
    to_commit.extend(written_log.iter().map(|p| relative_to(store.root(), p)));

    let report = AddReport {
        status: "ok",
        id: claim.id.to_string(),
        file: file_rel,
        commit: commit.to_owned(),
        actor: actor.to_owned(),
        verdict: Verdict::Held,
        witnessed_red: birth.witnessed_red,
        to_commit,
    };

    emit(format, &report, || {
        println!("Created claim '{}' at {}", report.id, report.file);
        if report.witnessed_red {
            println!("The check was witnessed failing, then confirmed passing (born verified).");
        } else {
            println!(
                "WARNING: recorded --unwitnessed. The check was never seen failing; \
                 `claim list --unverified` will surface it."
            );
        }
        println!("Recorded the birth verdict at commit {}.", report.commit);
        println!("\nNothing is committed yet. Review, then commit:");
        println!("  git add {}", report.to_commit.join(" "));
        println!("  git commit -m \"Add claim {}\"", report.id);
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

/// The evidence note recorded on the witnessed-drift entry: this red was observed
/// at creation, which is the proof the check discriminates.
fn witness_note() -> Option<String> {
    Some(
        "witnessed-red: the check was observed reporting Drifted against a perturbed tree at \
          `claim add` time, proving it detects this fact going false"
            .to_owned(),
    )
}

/// The evidence note recorded on an `--unwitnessed` claim's establishing hold, so
/// the log itself says the check was never seen failing.
fn unwitnessed_note() -> String {
    "unwitnessed: this claim was added with --unwitnessed; its check was NEVER observed failing, \
     so it is not yet trusted. `claim list --unverified` surfaces it until a red is witnessed."
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
