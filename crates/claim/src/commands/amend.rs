//! `claim amend <id>`: fix a claim's statement and/or check in place, keeping its
//! verdict history.
//!
//! Amend is how a *drifted* claim is brought back to the truth without losing the
//! record of how it got there: the world moved (libfoo shipped 5.0), so the
//! statement and its check are updated to the new fact, the file is rewritten in
//! place, and the verdict log under `.claims/log/<id>/` is left entirely untouched.
//! The drift, and every verdict before it, stays on record; `claim log <id>` still
//! reads as a continuous story.
//!
//! # The core guarantee: an amend cannot green a claim whose new fact is false
//!
//! The load-bearing rule is [`run`]'s require-`Held`: after the overlay is applied
//! and validated, the amended check is *run against the current tree*, and the amend
//! is refused unless it reports `Held`. A `Drifted`, `Broken`, or `Unverifiable`
//! result writes nothing — you cannot amend a claim to a fact that is still false,
//! any more than you could `add` one. The passing verdict is then appended to the
//! (preserved) log with git provenance, so the amended claim is verified against the
//! tree the amendment was made on, not merely asserted.
//!
//! # Why there is no witnessed-red dance here (deliberate)
//!
//! `claim add` witnesses a check going red before trusting it (invariant #5). Amend
//! does **not**, on purpose. TODO.md ("Rethink witnessed-red: demote from mandatory
//! to optional") records that witnessed-red is being demoted from a mandatory gate
//! to an optional convenience — it is impossible for world-facts, too harsh a label
//! for a check that really did evaluate reality, and hostile to the dirty trees
//! agents work in. Forcing a perturb/restore dance in `amend` would compound exactly
//! the friction that decision removes, on the verb most likely to run mid-drift on a
//! dirty tree. So the amend path is: apply the overlay, run the amended check,
//! require `Held`, write. A passing check verifies the fact; that is enough. (If a
//! later item adds an optional `--witness-cmd` for parity with `add`, it belongs
//! here as a convenience, never a gate.)
//!
//! # The overlay, and what must change
//!
//! Every field is optional and overlays the claim's current value: an unspecified
//! field is kept. The id is not amendable — a different id is a different claim, with
//! its own history — so there is no `--id` flag. At least one field must be supplied
//! *and* actually differ from the current claim, or the amend is a
//! [`ErrorKind::NoChange`] no-op that writes nothing.
//!
//! Amend re-renders through the same render-then-parse path `add` uses, so the
//! rewritten file is validated byte-for-byte before it replaces the original. That
//! path renders a single `cmd` check (the v1 `add` shape); a claim whose checks it
//! cannot faithfully round-trip — more than one check, or an `agent`/`human` check —
//! is refused with a clear error rather than having checks silently dropped.

use anyhow::{Context, Result};
use claim_core::{
    append_entry, run_check, Check, CheckContext, CheckKind, Claim, Event, LogEntry, Verdict,
};
use serde::Serialize;

use crate::apperror::{app, ErrorKind};
use crate::claimfile::{render_and_validate, CheckDraft, CheckDraftKind, ClaimDraft};
use crate::cli::AmendArgs;
use crate::git;
use crate::output::{emit, note, trigger_label, Format};
use crate::store::{discover, LoadedClaim, Store};

/// The machine form of `claim amend`.
#[derive(Debug, Serialize)]
struct AmendReport {
    /// Always `"ok"` on success.
    status: &'static str,
    /// The amended claim's id (unchanged — the id is not amendable).
    id: String,
    /// The store root (repository root), so an agent invoked from a subdirectory can
    /// resolve `file` and `to_commit`, which are root-relative.
    root: String,
    /// The path of the rewritten claim file, relative to the store root.
    file: String,
    /// The full 40-char commit sha the confirming verdict was recorded against.
    commit: String,
    /// The actor the confirming verdict was attributed to.
    actor: String,
    /// The verdict of the amended check against the current tree — always `held`
    /// (an amend that did not hold is refused and writes nothing).
    verdict: Verdict,
    /// The claim fields that actually changed, for the human confirmation.
    changed: Vec<&'static str>,
    /// The paths the caller must `git add` and commit (the rewritten file and the
    /// new verdict), relative to `root` (invariant #4: the tool does not commit).
    to_commit: Vec<String>,
}

/// Run `claim amend`. See the module docs for the require-`Held` guarantee and the
/// deliberate absence of witnessed-red.
///
/// # Errors
///
/// Fails, with a message naming the fix, on: no store found; an unknown id; a claim
/// whose checks amend cannot faithfully re-render (multi-check or non-`cmd`); no
/// field given or every field unchanged ([`ErrorKind::NoChange`]); an amended claim
/// that fails schema validation; an amended check that does not report `Held`
/// (nothing is written); a git provenance failure; or an I/O failure. In every
/// refusal path the original file and log are left exactly as they were.
pub fn run(args: &AmendArgs, format: Format) -> Result<()> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let store = discover(&cwd)?;

    let existing = resolve_claim(&store, &args.id)?;
    let current_draft = draft_from_claim(&existing.claim)?;
    let (amended_draft, changed) = apply_overlay(&current_draft, args);

    if changed.is_empty() {
        return Err(app(
            ErrorKind::NoChange,
            format!(
                "`claim amend {}` changed nothing: every field given matches the claim's current \
                 value (or none was given). Pass at least one of --statement, --run, --when, \
                 --negate, --max-age, --supports with a new value.",
                args.id
            ),
        ));
    }

    // The single validation path, shared with `add`: render the amended file and
    // parse it back, so what will be written is proven to satisfy the schema. The id
    // is unchanged, so the file path is the same one the claim already lives at.
    let file_rel = existing.path.clone();
    let (amended_claim, file_text) =
        render_and_validate(&amended_draft, &file_rel).map_err(|e| {
            app(
                ErrorKind::InvalidInput,
                format!("the amended claim is not valid: {e}"),
            )
        })?;

    // Provenance up front, before running the check, so a missing repository or
    // identity fails while nothing has been written (`append_entry` rejects an empty
    // commit/actor).
    let commit = git::resolve_commit(store.root())?;
    let actor = git::resolve_actor(store.root())?;

    // The core guarantee: the amended fact must actually hold now. Run the amended
    // check against the current tree; a non-`Held` result is refused and writes
    // nothing, so an amend can never turn a drifted claim green on a false fact.
    let check = primary_cmd_check(&amended_claim)
        .context("the amended claim has no cmd check to run; amend authors a single cmd check")?;
    let ctx = CheckContext::new(store.root());
    let outcome = require_held(check, &ctx, format)?;

    // Only now, with the fact confirmed, is anything written. The file is rewritten
    // in place; the log is NOT touched beyond appending this confirming verdict, so
    // the drift and every prior verdict stay on record (history preserved).
    let file_abs = store.root().join(&file_rel);
    std::fs::write(&file_abs, &file_text)
        .with_context(|| format!("failed to rewrite the claim file {file_rel}"))?;

    let evidence = match &outcome.evidence {
        Some(ev) => Some(format!("{}\n{ev}", amend_note())),
        None => Some(amend_note()),
    };
    // Stamp through the clock seam (as `check` does), so the confirming verdict's
    // instant is governed by the same `now` a read verb uses — deterministic under
    // test, wall clock in a shipped binary.
    let entry = LogEntry {
        at: crate::clock::now()?,
        commit: commit.clone(),
        actor: actor.clone(),
        event: Event::Verification {
            verdict: Verdict::Held,
            evidence,
        },
    };
    let log_path = append_entry(&store.log_dir(), &amended_claim.id, &entry)
        .context("failed to record the amend's confirming verdict")?;

    let to_commit = vec![file_rel.clone(), relative_to(store.root(), &log_path)];
    let report = AmendReport {
        status: "ok",
        id: amended_claim.id.to_string(),
        root: store.root().display().to_string(),
        file: file_rel,
        commit,
        actor,
        verdict: Verdict::Held,
        changed,
        to_commit,
    };

    emit(format, &report, || human(&report))
}

/// The evidence note recorded on the amend's confirming verdict, so the log says why
/// this `Held` appears: the claim was amended and re-verified against the tree.
fn amend_note() -> String {
    "amended: the claim's statement and/or check were updated by `claim amend`, and the amended \
     check was confirmed Held against the tree at amend time"
        .to_owned()
}

/// Resolve the requested id to a claim that exists in the store, mirroring
/// `retire`/`log`: an unknown id is a loud error, and a file that *is* the id but
/// failed to parse reports that file's error rather than "not found".
fn resolve_claim(store: &Store, id: &str) -> Result<LoadedClaim> {
    let load = store.load_all()?;
    if let Some(loaded) = load.claims.iter().find(|c| c.claim.id.as_str() == id) {
        return Ok(loaded.clone());
    }
    if let Some(err) = load
        .errors
        .iter()
        .find(|e| file_stem_matches_id(&e.file, id))
    {
        return Err(app(
            ErrorKind::InvalidInput,
            format!("claim '{id}' could not be loaded: {}", err.message),
        ));
    }
    Err(app(
        ErrorKind::InvalidInput,
        format!(
            "no claim with id '{id}' in this store; run `claim list` to see the ids that exist"
        ),
    ))
}

/// Whether a load-errored file's path could be the file for `id`. Mirrors `log`.
fn file_stem_matches_id(file: &str, id: &str) -> bool {
    file.strip_prefix(".claims/")
        .and_then(|rest| rest.strip_suffix(".md"))
        .is_some_and(|stem| stem == id)
}

/// The primary `cmd` check of a parsed claim, for the amended require-`Held` run.
fn primary_cmd_check(claim: &Claim) -> Option<&Check> {
    claim
        .checks
        .first()
        .filter(|c| matches!(c.kind, CheckKind::Cmd { .. }))
}

/// Reconstruct an editable [`ClaimDraft`] from a parsed claim, so the overlay can be
/// applied and the whole thing re-rendered through `add`'s validation path.
///
/// Amend re-renders the file through [`render_and_validate`], which emits the v1
/// `add` shape: exactly one `cmd` check. A claim it cannot faithfully round-trip —
/// more than one check, or an `agent`/`human` check — is refused here rather than
/// having checks silently dropped on the rewrite. That refusal is honest: amend in
/// v1 edits `add`-shaped claims; a richer claim must be edited by hand until the
/// renderer grows.
fn draft_from_claim(claim: &Claim) -> Result<ClaimDraft> {
    let [check] = claim.checks.as_slice() else {
        return Err(app(
            ErrorKind::InvalidInput,
            format!(
                "claim '{}' has {} checks; `claim amend` re-renders a single cmd check in v1. Edit \
                 this claim's file by hand.",
                claim.id,
                claim.checks.len()
            ),
        ));
    };
    let CheckKind::Cmd { run, negate } = &check.kind else {
        return Err(app(
            ErrorKind::InvalidInput,
            format!(
                "claim '{}' has a non-cmd check; `claim amend` edits cmd checks in v1. Edit this \
                 claim's file by hand.",
                claim.id
            ),
        ));
    };
    Ok(ClaimDraft {
        id: claim.id.to_string(),
        max_age: claim.max_age.to_string(),
        checks: vec![CheckDraft {
            kind: CheckDraftKind::Cmd {
                run: run.clone(),
                negate: *negate,
            },
            when: trigger_label(check.when),
        }],
        supports: claim.supports.iter().map(ToString::to_string).collect(),
        statement: claim.statement.trim().to_owned(),
    })
}

/// Overlay the provided flags onto the current draft, returning the amended draft
/// and the list of fields that actually changed.
///
/// A field is "changed" only if the flag was given *and* its value differs from the
/// current one — passing `--max-age 30d` on a claim already at `30d` is not a change.
/// Negation is only overlaid when `--run` is also given (clap enforces
/// `--negate requires --run`), so an amend that does not touch the check can never
/// silently un-negate a negated one; when `--run` is present, the new check takes
/// exactly the `--negate` flag's value. Supports, when `--supports` is passed, are
/// *replaced* by the given set (order-insensitive comparison for "changed").
fn apply_overlay(current: &ClaimDraft, args: &AmendArgs) -> (ClaimDraft, Vec<&'static str>) {
    let mut changed = Vec::new();
    let mut draft = clone_draft(current);
    let (current_run, current_negate) = current_cmd(current);

    if let Some(statement) = &args.statement {
        let statement = statement.trim().to_owned();
        if statement != current.statement {
            draft.statement = statement;
            changed.push("statement");
        }
    }

    if let Some(run) = &args.run {
        // With --run present, the new check is (run, negate); --negate is only
        // meaningful here (it `requires` --run). Report "run" and "negate" changes
        // independently so the confirmation names exactly what moved.
        if run != current_run {
            changed.push("run");
        }
        if args.negate != current_negate {
            changed.push("negate");
        }
        draft.checks[0].kind = CheckDraftKind::Cmd {
            run: run.clone(),
            negate: args.negate,
        };
    }

    if let Some(when) = &args.when {
        if when != &current.checks[0].when {
            draft.checks[0].when = when.clone();
            changed.push("when");
        }
    }

    if let Some(max_age) = &args.max_age {
        if max_age != &current.max_age {
            draft.max_age = max_age.clone();
            changed.push("max-age");
        }
    }

    // `--supports` replaces the whole set. An empty `Vec` means the flag was not
    // given (clap requires a value per occurrence), so absent supports are kept, not
    // cleared — amend never silently drops edges it was not told to touch.
    if !args.supports.is_empty() && !same_set(&args.supports, &current.supports) {
        draft.supports = args.supports.clone();
        changed.push("supports");
    }

    (draft, changed)
}

/// The current draft's single cmd check as `(run, negate)`. The draft always holds
/// exactly one cmd check (built by [`draft_from_claim`], which refuses anything
/// else), so this indexing is total.
fn current_cmd(draft: &ClaimDraft) -> (&str, bool) {
    match &draft.checks[0].kind {
        CheckDraftKind::Cmd { run, negate } => (run.as_str(), *negate),
    }
}

/// A field-by-field clone of a draft. `ClaimDraft`/`CheckDraft` are not `Clone` (the
/// CLI never needed it before), so the overlay rebuilds one rather than widening a
/// core type's derives for one call site.
fn clone_draft(d: &ClaimDraft) -> ClaimDraft {
    ClaimDraft {
        id: d.id.clone(),
        max_age: d.max_age.clone(),
        checks: d
            .checks
            .iter()
            .map(|c| CheckDraft {
                kind: match &c.kind {
                    CheckDraftKind::Cmd { run, negate } => CheckDraftKind::Cmd {
                        run: run.clone(),
                        negate: *negate,
                    },
                },
                when: c.when.clone(),
            })
            .collect(),
        supports: d.supports.clone(),
        statement: d.statement.clone(),
    }
}

/// Whether two supports lists hold the same targets, order-insensitive, so
/// reordering the same set is not counted as a change (the on-disk order is not
/// semantically meaningful).
fn same_set(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a: Vec<&String> = a.iter().collect();
    let mut b: Vec<&String> = b.iter().collect();
    a.sort();
    b.sort();
    a == b
}

/// Run the amended check and require `Held`, showing the evidence.
///
/// This is the amend guarantee. A `Drifted` means the amended fact is still false —
/// the amend would be a lie. A `Broken`/`Unverifiable` means the check cannot answer.
/// All three are refused with the evidence and write nothing, so the original file
/// and log are left exactly as they were.
fn require_held(
    check: &Check,
    ctx: &CheckContext,
    format: Format,
) -> Result<claim_core::CheckOutcome> {
    note(
        format,
        "Running the amended check against the current tree...",
    );
    let outcome = run_check(check, ctx);
    show_evidence(format, &outcome);

    match outcome.verdict {
        Verdict::Held => Ok(outcome),
        Verdict::Drifted => Err(app(
            ErrorKind::DriftedGreen,
            format!(
                "the amended check reports Drifted against the current tree ({}): the new fact is \
                 still false, so there is nothing true to record. Nothing was written. Fix the \
                 fact or the statement/check first.",
                outcome.status()
            ),
        )),
        Verdict::Broken => Err(app(
            ErrorKind::BrokenGreen,
            format!(
                "the amended check is Broken against the current tree ({}): it cannot run, so it \
                 cannot be trusted. Nothing was written. Fix the command first.",
                outcome.status()
            ),
        )),
        Verdict::Unverifiable => Err(app(
            ErrorKind::BrokenGreen,
            format!(
                "the amended check is Unverifiable ({}): `claim amend` authors cmd checks, which \
                 never return this. Nothing was written.",
                outcome.status()
            ),
        )),
    }
}

/// Print the amended check's verdict and evidence as a narration block (human mode).
fn show_evidence(format: Format, outcome: &claim_core::CheckOutcome) {
    note(
        format,
        &format!(
            "  [amended check] {} ({})",
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

/// A capitalized label for a verdict, for narration (matches `add`'s narration).
fn verdict_label(v: Verdict) -> &'static str {
    match v {
        Verdict::Held => "Held",
        Verdict::Drifted => "Drifted",
        Verdict::Broken => "Broken",
        Verdict::Unverifiable => "Unverifiable",
    }
}

/// Confirm the amendment, name what changed, and tell the user what to commit.
fn human(report: &AmendReport) {
    println!(
        "Amended claim '{}' ({} changed).",
        report.id,
        report.changed.join(", ")
    );
    println!("The amended check was confirmed Held; the verdict history is preserved.");
    println!(
        "Recorded the confirming verdict at commit {}.",
        git::short_commit(&report.commit)
    );
    println!("\nNothing is committed yet. Review, then commit:");
    println!(
        "  git -C {} add {}",
        report.root,
        report.to_commit.join(" ")
    );
    println!(
        "  git -C {} commit -m \"Amend claim {}\"",
        report.root, report.id
    );
}

/// Render `path` relative to `root` for display, falling back to the full path.
fn relative_to(root: &std::path::Path, path: &std::path::Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}
