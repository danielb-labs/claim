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
//! nothing). Given `--witness-cmd`, the tool creates a throwaway `git worktree`
//! detached at `HEAD` *outside* the repository, runs the witness command there to
//! perturb that isolated checkout, re-runs the check there and requires an *observed*
//! `Drifted` (a still-`Held` check does not detect the fact going false and is
//! refused; a `Broken` means the perturbation broke execution and is refused), then
//! tears the worktree down even on failure.
//!
//! The observed red is recorded as an evidence note on the establishing entry. The
//! user's working tree is never mutated, so `--witness-cmd` needs no clean-tree
//! requirement and can never lose uncommitted work. Witnessing needs a born `HEAD`
//! (the worktree checks out a commit); on an unborn repository `--witness-cmd` is
//! refused with the fix, while the default no-witness path still works.

use std::io::Write;

use anyhow::{bail, Context, Result};
use claim_core::{
    resolve_supports, run_check, Check, CheckContext, CheckOutcome, Claim, ClaimId, Timestamp,
    Verdict,
};
use serde::Serialize;

use crate::apperror::{app, ErrorKind};
use crate::claimfile::{
    primary_cmd_check, render_and_validate, CheckDraft, CheckDraftKind, ClaimDraft,
};
use crate::cli::AddArgs;
use crate::output::{emit, note, warn, Format};
use claim_store::{author_claim, discover, git, AuthorError, Authored, Store, StoreLoad};

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
/// field without `--interactive` to prompt for it; an invalid id, trigger, or
/// `max-age`; a
/// duplicate id; a check that is `Drifted` or `Broken` against the current tree; a
/// `--witness-cmd` whose red is not observed (or that is requested on an unborn
/// HEAD); a git provenance failure; or an I/O failure writing the file or log.
pub fn run(args: &AddArgs, format: Format) -> Result<()> {
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    // `discover` returns `StoreError::NoStore` for a missing store, which
    // `apperror::kind_of` maps to the `no-store` kind, so no per-verb remapping is
    // needed — every verb agrees on the kind.
    let store = discover(&cwd)?;

    let draft = gather_draft(args)?;

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

    // Load the corpus once and reuse it: the shared authoring core's duplicate-id
    // check needs every existing id, and the supports warning needs them as the
    // known-id set.
    let existing = store.load_all()?;
    warn_unresolved_supports(&store, &existing, &claim);

    let check = primary_cmd_check(&claim)
        .context("claim add authors a single cmd check; this claim has none to run")?;

    note(format, "Running the check against the current tree...");
    let ctx = CheckContext::new(store.root());
    let now = Timestamp::now();

    // The optional `--witness-cmd` dance runs as the `on_established` hook — after
    // `author_claim` has confirmed the id is new and the establishing check holds — so
    // its isolated worktree and side-effecting witness command never fire for an add
    // that a duplicate id or a non-holding check has already doomed (fail fast before
    // any side effect). On success its observed-red note is folded onto the birth
    // entry. A witness failure is a CLI contract error whose `ErrorKind` the JSON
    // output must preserve, so it is stashed here and re-raised after `author_claim`
    // returns, rather than flattened through `AuthorError`.
    let mut witness_error: Option<anyhow::Error> = None;
    let on_established = |_outcome: &CheckOutcome| -> Result<Option<String>, AuthorError> {
        let Some(witness_cmd) = &args.witness_cmd else {
            return Ok(None);
        };
        let head = git::resolve_commit(store.root()).map_err(AuthorError::Provenance)?;
        match witness_in_isolation(&store, &head, check, witness_cmd, format) {
            Ok(()) => Ok(Some(witness_note())),
            Err(e) => {
                // Stash the real (kind-bearing) error and abort the write with a
                // sentinel; the stash is re-raised below so `main` sees the original.
                witness_error = Some(e);
                Err(AuthorError::WitnessAborted)
            }
        }
    };

    let authored = match author_claim(
        &store,
        &claim,
        &file_text,
        &existing,
        &ctx,
        now,
        on_established,
    ) {
        Ok(authored) => authored,
        Err(AuthorError::WitnessAborted) => {
            return Err(witness_error.expect("WitnessAborted is set only after stashing the error"))
        }
        Err(e) => {
            // Restore main's contract: a refused establish shows the check's
            // evidence in human mode so the author sees *why* (e.g. `sh: cmd: not
            // found`), before the error itself. `map_author_error` only renders the
            // message and kind, so the evidence must be surfaced here where the
            // format is known.
            if let AuthorError::NotHeld {
                verdict, evidence, ..
            } = &e
            {
                show_refused_evidence(format, *verdict, evidence.as_deref());
            }
            return Err(map_author_error(e));
        }
    };

    show_evidence(format, "check", &authored.establishing);
    report_created(
        &store,
        &claim,
        &authored,
        args.witness_cmd.is_some(),
        format,
    )
}

/// Narrate a refused establishing check's verdict and evidence in human mode, before
/// the error is raised — the "so the author sees why" contract the establishing run
/// has always kept. Silent in `--json` mode, where the message and evidence would
/// contaminate the single error object on stderr; the JSON error carries the reason in
/// its message instead.
fn show_refused_evidence(format: Format, verdict: Verdict, evidence: Option<&str>) {
    note(format, &format!("  [check] {}", verdict_label(verdict)));
    if let Some(ev) = evidence {
        for line in ev.lines() {
            note(format, &format!("    | {line}"));
        }
    }
}

/// Map a shared [`AuthorError`] onto the CLI's [`ErrorKind`]s, so the `--json` error
/// object carries the stable `kind` an agent branches on. `WitnessAborted` never
/// reaches here (it is handled in [`run`], which re-raises the stashed witness error);
/// a `NotHeld` from the establishing run is classified by *which* verdict was observed,
/// matching the distinct `drifted-green`/`broken-green` kinds the CLI has always used.
fn map_author_error(err: AuthorError) -> anyhow::Error {
    match err {
        // The two duplicate cases keep their distinct "already exists at" / "already
        // declared in" wording (the shared error's own Display names the file and the
        // fix), tagged with the stable `duplicate-id` kind an agent branches on.
        dup @ (AuthorError::DuplicateId { .. } | AuthorError::IdAlreadyDeclared { .. }) => {
            app(ErrorKind::DuplicateId, dup.to_string())
        }
        // A `Drifted` fact is already false against the current tree.
        AuthorError::NotHeld {
            verdict: Verdict::Drifted,
            status,
            ..
        } => app(
            ErrorKind::DriftedGreen,
            format!(
                "the check reports Drifted against the current tree ({status}): the fact is \
                 already false, so there is nothing true to record. Fix the fact or the check \
                 first."
            ),
        ),
        // A `Broken` check could not run: the command errored.
        AuthorError::NotHeld {
            verdict: Verdict::Broken,
            status,
            ..
        } => app(
            ErrorKind::BrokenGreen,
            format!(
                "the check is Broken against the current tree ({status}): it cannot run, so it \
                 cannot be trusted. Fix the command first."
            ),
        ),
        // `Unverifiable` here means an agent/human check reached `add`, which authors
        // only cmd checks in v1 — a distinct message from Broken, restoring main's.
        AuthorError::NotHeld {
            verdict: Verdict::Unverifiable,
            status,
            ..
        } => app(
            ErrorKind::BrokenGreen,
            format!(
                "the check is Unverifiable ({status}): claim add authors cmd checks, which never \
                 return this; this indicates an agent/human check, not supported by add in v1."
            ),
        ),
        AuthorError::NotHeld {
            verdict: Verdict::Held,
            ..
        } => unreachable!("author_claim returns NotHeld only for a non-Held verdict"),
        // The caller-hook sentinel is handled in `run` before this maps anything; if it
        // ever reached here it would be a logic error, so name it rather than defaulting.
        AuthorError::WitnessAborted => {
            unreachable!(
                "WitnessAborted is intercepted in run() and re-raised as the witness error"
            )
        }
        // Provenance (no git identity, corrupt HEAD) and Write (I/O) are environment
        // faults with no specific contract kind; they surface as `other` with the
        // shared crate's message, whose cause chain names the fix.
        err @ (AuthorError::Provenance(_) | AuthorError::Write(_)) => anyhow::Error::new(err),
    }
}

/// Gather every claim field from flags, prompting for absent required fields only
/// under `--interactive`. By default a missing required field is a loud, machine-
/// actionable error naming the flag — never a silent default (except `when`, which
/// sensibly defaults to `on-change`) and never a prompt that could hang an agent.
fn gather_draft(args: &AddArgs) -> Result<ClaimDraft> {
    let interactive = args.interactive;

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
        format!("missing {field}; pass {flag} (or run with --interactive to be prompted)"),
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

/// Warn — never fail — for each `supports` target that does not resolve now.
///
/// A support edge is a soft signal, and a forward reference (a decision ref to a
/// file or anchor not yet written) is legitimate, so an unresolvable target at
/// author time is a warning, not a hard error: it nags the author to fix a typo
/// (the common case — a GitHub-slug `#approved-dependencies` where the file says
/// "Approved dependencies") without blocking a deliberate forward reference.
/// Without this, an author sees the target accepted silently and is surprised by
/// `UNRESOLVED` only at `check` time.
///
/// Resolution reuses claim-core's [`resolve_supports`] against the same store root
/// and known-id set `check` uses, so `add` and `check` agree on what resolves. The
/// warning goes to stderr in every mode ([`warn`]), so a `--json` caller still sees
/// it without its stdout being contaminated.
fn warn_unresolved_supports(store: &Store, existing: &StoreLoad, claim: &Claim) {
    if claim.supports.is_empty() {
        return;
    }
    let known_ids: Vec<ClaimId> = existing.claims.iter().map(|c| c.claim.id.clone()).collect();
    for res in resolve_supports(&claim.supports, store.root(), &known_ids) {
        if !res.resolved {
            let reason = res.reason.as_deref().unwrap_or("does not resolve");
            warn(&format!(
                "supports target '{}' does not resolve: {reason}. The claim will be created, but \
                 `claim check` will flag this as an unresolved support until it resolves. If you \
                 meant a Markdown heading, `#anchor` is a case-sensitive text scan, not a slug — \
                 use the words as written and matching case (`#Approved dependencies`, not \
                 `#approved-dependencies`).",
                res.target
            ));
        }
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

/// Report a created claim: build the machine record from the shared [`Authored`]
/// outcome and print the human handoff. The claim and its birth verdict are already
/// on disk (via [`author_claim`]); this only renders what to commit — the tool does
/// not commit (invariant #4).
fn report_created(
    store: &Store,
    claim: &Claim,
    authored: &Authored,
    witnessed_red: bool,
    format: Format,
) -> Result<()> {
    let file_rel = relative_to(store.root(), &authored.claim_file);
    let to_commit = vec![
        file_rel.clone(),
        relative_to(store.root(), &authored.log_file),
    ];

    let root = store.root().display().to_string();
    let report = AddReport {
        status: "ok",
        id: claim.id.to_string(),
        root: root.clone(),
        file: file_rel,
        commit: authored.provenance.commit.clone(),
        actor: authored.provenance.actor.clone(),
        verdict: authored.establishing.verdict,
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

/// A display label for a verdict, for narration.
fn verdict_label(v: Verdict) -> &'static str {
    match v {
        Verdict::Held => "Held",
        Verdict::Drifted => "Drifted",
        Verdict::Broken => "Broken",
        Verdict::Unverifiable => "Unverifiable",
    }
}

/// Render `path` relative to `root` for display, falling back to the full path.
fn relative_to(root: &std::path::Path, path: &std::path::Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}
