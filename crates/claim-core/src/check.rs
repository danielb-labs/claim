//! Running a check and turning the result into a [`Verdict`] — the honesty core.
//!
//! This module is the one place where a running command becomes a judgement, so
//! it is where golden invariants #1, #2, and #6 are enforced in code rather than
//! merely stated:
//!
//! - **#1, a broken check never reports a pass.** [`run_check`] maps a process
//!   outcome to a verdict as a *total* classification: only a deliberate exit 0
//!   is [`Verdict::Held`], exit 1 is [`Verdict::Drifted`], and every other
//!   outcome — any other exit code, death by signal, a failure to spawn, a
//!   timeout we enforced — is [`Verdict::Broken`]. There is no arm from "could
//!   not run" to `Held`.
//! - **#2, the tool owns negation.** A `negate: true` command has its verdict
//!   inverted *here, in Rust, after* the exit code is classified, swapping
//!   `Held`↔`Drifted` only. `Broken` and `Unverifiable` never invert into a
//!   pass, so a `negate` check whose binary is missing stays `Broken` — never a
//!   false green. The command is never wrapped in a shell `!`, which would let a
//!   missing binary invert to success.
//! - **#6, the failure mode is a nag.** A `human` check, and an `agent` check with
//!   no runner configured, is not silently treated as passing: [`run_check`]
//!   returns [`Verdict::Unverifiable`] with a note that it needs a lane the run was
//!   not given, so the claim ages toward a human instead of faking freshness. When
//!   an [`AgentRunner`] *is* configured, an `agent` check runs it under the same
//!   honesty mapping as `cmd`: a crash, a timeout, or output that is not a
//!   well-formed verdict is [`Verdict::Broken`], never a fabricated pass.
//!
//! **Authorization.** A check's `run` string is executed via the platform shell
//! with no sandboxing here. This is authorized by construction: the command lives
//! in a claim file that was reviewed and merged like any other code (invariant
//! #4), and a verifier being a process is the design. Sandboxing, privilege
//! dropping, and network policy are a *runner's* concern layered on later items;
//! core's single job is to execute the authorized command honestly and classify
//! its result without inventing a pass.
//!
//! **Supports resolution** ([`resolve_supports`]) is deliberately kept *out* of
//! the verdict. A claim whose `supports` target was deleted must go loud on its
//! own axis — "the decision this rested on is gone" — rather than being folded
//! into `Broken`, which would conflate "the check could not run" with "the thing
//! it justifies vanished". The CLI surfaces the two independently.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use jiff::Timestamp;

use crate::claim::{Check, CheckKind, ClaimId, Skip, SupportTarget};
use crate::verdict::Verdict;

// Check execution relies on unix process semantics — a shell, process groups, and
// `killpg` — to keep a hung check's grandchildren from being orphaned (invariant
// #6). v1's target is a unix dev and CI environment. Rather than ship a Windows
// path that silently could not deliver the no-orphan guarantee, fail the build
// loudly on a platform this honesty core is not written for.
#[cfg(not(unix))]
compile_error!(
    "claim-core check execution is unix-only in v1: it depends on a POSIX shell and \
     process-group signalling to avoid orphaning a timed-out check's children"
);

/// The default wall-clock budget for a single check, 60 seconds.
///
/// A check that has not finished in a minute is treated as hung and killed to
/// [`Verdict::Broken`]. Sixty seconds is generous for the cheap `cmd` checks v1
/// runs (a grep, a version pin) yet short enough that a wedged command cannot
/// stall a CI job or the clock lane. Callers that need a different budget set
/// [`CheckContext::timeout`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// The default cap on retained check output, 8 KiB.
///
/// Output is evidence for the verdict log, not a transcript: a few kilobytes is
/// enough to show the grep line that matched or the error that broke the check,
/// while a hard cap keeps a runaway command (a megabyte of build spam) from
/// bloating the log or memory. Retention stops at the cap and the evidence is
/// marked truncated; see [`CheckOutcome::evidence`].
pub const DEFAULT_OUTPUT_CAP: usize = 8 * 1024;

/// How a check is executed: where it runs, how long it may take, and how much of
/// its output to keep.
///
/// The working directory is load-bearing, not a convenience: a claim's command
/// (`grep -q 'libfoo==4.2' requirements.txt`) is written relative to the store or
/// repo root, so running it anywhere else would silently check the wrong tree.
/// The timeout and output cap have sane defaults ([`CheckContext::new`]); a caller
/// tunes them only when it has a reason.
#[derive(Debug, Clone)]
pub struct CheckContext {
    /// The directory the command runs in. A `cmd` check's `run` string is
    /// interpreted relative to this, so it must be the root the claim's paths are
    /// written against. A missing or non-directory path makes the check
    /// [`Verdict::Broken`] (a spawn failure), never a pass. An `agent` runner is
    /// spawned in this directory too, so an instruction that inspects the tree
    /// reads the same root the claim's paths are written against.
    pub cwd: PathBuf,
    /// The wall-clock budget. On expiry the check's process group is killed and
    /// the verdict is [`Verdict::Broken`]. See [`DEFAULT_TIMEOUT`].
    pub timeout: Duration,
    /// The maximum number of bytes of combined stdout+stderr to retain as
    /// evidence. Output past this is dropped and the evidence notes truncation.
    /// See [`DEFAULT_OUTPUT_CAP`].
    pub output_cap: usize,
    /// The runner an [`CheckKind::Agent`] check is executed by, if any. `None` —
    /// the default — means agent checks are *not executed*: they return
    /// [`Verdict::Unverifiable`] with a "no agent runner configured" note, never a
    /// fabricated pass, and no subprocess is spawned. This is what makes agent
    /// execution strictly opt-in: a run that was handed no runner cannot spawn one
    /// or reach a model. See [`AgentRunner`].
    pub agent_runner: Option<AgentRunner>,
}

/// How an [`CheckKind::Agent`] check is executed: the operator-supplied command
/// that receives the verdict prompt on stdin and must emit the verdict JSON on
/// stdout.
///
/// This is deliberately an operator's concern, not the tool's: `claim-core` never
/// embeds a model client or a network call. The runner is whatever wrapper the
/// operator provides around a model CLI (its cost and credentials theirs to own),
/// and the tool's single job is to feed it the prompt, bound its run exactly like a
/// `cmd` check, and map its structured answer to a [`Verdict`] under the same
/// honesty contract — a crash, a timeout, or malformed output is [`Verdict::Broken`],
/// never a pass. The contract the runner must satisfy is documented on
/// [`build_agent_prompt`] and `docs/agent-checks.md`.
#[derive(Debug, Clone)]
pub enum AgentRunner {
    /// An argv: `program[0]` executed directly with `program[1..]` as its
    /// arguments, no shell involved. The prompt is written to the program's stdin.
    /// An empty argv is a spawn failure (`Broken`), never a pass.
    Argv(Vec<String>),
    /// A shell command run as `sh -c <command>`, for a runner an operator finds
    /// easier to express as a one-liner (a pipeline, an env-substituted flag). The
    /// prompt is written to the shell's stdin. This is the *runner* command an
    /// operator configured and reviewed, not a claim's `run`; it is never wrapped
    /// in a `!` and its exit code is classified by the tool, so the negation and
    /// broken-never-passes invariants still hold.
    Shell(String),
}

impl CheckContext {
    /// A context rooted at `cwd` with the default timeout and output cap and no
    /// agent runner.
    ///
    /// The one field with no sensible default is where the check runs, so it is
    /// the only argument; [`timeout`](CheckContext::timeout),
    /// [`output_cap`](CheckContext::output_cap), and
    /// [`agent_runner`](CheckContext::agent_runner) can be overwritten after
    /// construction. Agent checks are unverifiable until a runner is set, so the
    /// default context never spawns an agent.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        CheckContext {
            cwd: cwd.into(),
            timeout: DEFAULT_TIMEOUT,
            output_cap: DEFAULT_OUTPUT_CAP,
            agent_runner: None,
        }
    }

    /// The same context with `runner` set as the agent runner.
    ///
    /// A convenience for the CLI, which builds one context and, if `CLAIM_AGENT_CMD`
    /// is set, attaches the runner it parsed. Passing `None` leaves agent checks
    /// unverifiable, the same as never calling this.
    #[must_use]
    pub fn with_agent_runner(mut self, runner: Option<AgentRunner>) -> Self {
        self.agent_runner = runner;
        self
    }
}

/// How a check's process ended — the structured truth behind the verdict.
///
/// A closed set of terminal outcomes so the exit-code classification is a total
/// `match` with no catch-all that could accidentally map an unexpected case to a
/// pass. Only [`Exited`](ProcessEnd::Exited) carries a code that can become
/// `Held`; every other variant is unconditionally [`Verdict::Broken`].
///
/// Exposed on [`CheckOutcome::end`] rather than only as a prose string because
/// item 4 serializes outcomes into the committed verdict log: a structured value
/// lets that history be filtered by machine ("show every timeout") and does not
/// bake a run's configured timeout into the record the way a `"timed out after
/// 60s"` string would. The [`Display`](std::fmt::Display) impl renders the human
/// one-liner. `#[non_exhaustive]` because a future execution lane may add ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
#[non_exhaustive]
pub enum ProcessEnd {
    /// The process ran to completion and returned an exit code. On unix this is
    /// present only when the process was *not* killed by a signal. Exit 0 is the
    /// only path to [`Verdict::Held`]; exit 1 is [`Verdict::Drifted`]; every other
    /// code is [`Verdict::Broken`].
    Exited {
        /// The process's exit code.
        code: i32,
    },
    /// The process was terminated by a signal (e.g. `SIGKILL`, `SIGSEGV`) and so
    /// has no exit code of its own. Always `Broken`: a check that was killed did
    /// not deliberately report anything.
    Signalled,
    /// We killed the process because it exceeded the timeout. Always `Broken`.
    /// Carries the budget it exceeded so the human line is self-contained and the
    /// configured value is not lost.
    TimedOut {
        /// The timeout budget that was exceeded.
        after: Duration,
    },
    /// The process could never start — a missing shell, a non-existent working
    /// directory, an exhausted process table — or, defensively, a `run` string
    /// that was empty so there was nothing to execute. Always `Broken`.
    SpawnFailed {
        /// Why the spawn (or pre-spawn validation) failed.
        reason: String,
    },
    /// No process was run at all: an `agent` or `human` check, which has no
    /// command lane in v1. Distinct from `SpawnFailed` (which is a *cmd* check
    /// that could not start, and is `Broken`): a not-executed check is
    /// [`Verdict::Unverifiable`], so the claim ages toward a human rather than
    /// being recorded as broken. Never [`Verdict::Held`].
    NotExecuted {
        /// Which lane the check needs, for the human line.
        note: String,
    },
}

impl std::fmt::Display for ProcessEnd {
    /// The human one-liner recorded alongside a `Broken` verdict so the log says
    /// *why* it broke. A sub-second timeout renders in milliseconds rather than
    /// collapsing to `0s`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcessEnd::Exited { code } => write!(f, "exit {code}"),
            ProcessEnd::Signalled => f.write_str("killed by signal"),
            ProcessEnd::TimedOut { after } => {
                if *after < Duration::from_secs(1) {
                    write!(f, "timed out after {}ms", after.as_millis())
                } else {
                    write!(f, "timed out after {}s", after.as_secs())
                }
            }
            ProcessEnd::SpawnFailed { reason } => write!(f, "failed to spawn: {reason}"),
            ProcessEnd::NotExecuted { note } => write!(f, "not executed: {note}"),
        }
    }
}

/// The result of running one check: its verdict and the evidence a caller records.
///
/// This is the hand-off to the verdict log (`crate::log::append_entry`), and
/// deliberately nothing more: producing it is core's job, recording it (with a
/// commit sha, an actor, and a timestamp) is a later item's. The fields are the
/// facts that cannot be recovered afterward — what the tool concluded, how long
/// it took, what the process actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckOutcome {
    /// The verdict, after exit-code classification and any negation. The single
    /// answer to "did this check pass"; only [`Verdict::Held`] does.
    pub verdict: Verdict,
    /// How the process ended, structured — the machine-readable truth the verdict
    /// was derived from. Recorded so a `Broken` verdict in the log says *why* it
    /// broke and can be filtered by kind. [`status`](CheckOutcome::status) renders
    /// its human one-liner.
    pub end: ProcessEnd,
    /// Combined stdout+stderr, truncated to the context's `output_cap`. `None`
    /// when the process produced no output or never started. When truncated, the
    /// retained bytes end with a marker line naming the cap, so a reader is never
    /// misled into thinking they have the whole output. This is the evidence for
    /// the log entry.
    pub evidence: Option<String>,
    /// Wall-clock time from spawn to conclusion. Useful for spotting a check
    /// creeping toward its timeout before it actually breaks.
    pub duration: Duration,
}

impl CheckOutcome {
    /// The human one-liner describing how the process ended — `exit 0`,
    /// `exit 127`, `killed by signal`, `timed out after 300ms`, or the spawn
    /// error. A convenience over `outcome.end.to_string()` for display sites.
    #[must_use]
    pub fn status(&self) -> String {
        self.end.to_string()
    }
}

/// Run one check and classify its outcome into a [`CheckOutcome`].
///
/// Total by construction, barring an OS-level failure: it never returns an error.
/// Every way a check can fail to produce a clean answer — a missing binary, a bad
/// working directory, a signal, a timeout — resolves to a [`Verdict::Broken`]
/// outcome, because a caller that had to handle a `Result` here could forget the
/// error arm and let a broken check read as anything other than broken (invariant
/// #1). The one thing outside this guarantee is the host OS itself giving way (an
/// allocation failure aborts; the timed wait can, on an internal fault, panic) —
/// no verdict can be honestly synthesized from a broken runtime.
///
/// **Process-global side effect.** Executing a `cmd` check uses `wait-timeout`,
/// which installs a process-wide `SIGCHLD` handler on first use and reaps child
/// processes through it. An embedder that also manages child processes in the same
/// process — notably `claim-mcp` — must be aware that this handler exists
/// process-wide, not scoped to this call.
///
/// A [`CheckKind::Cmd`] check is always executed. A [`CheckKind::Agent`] check is
/// executed only when the context carries an [`AgentRunner`]
/// ([`CheckContext::agent_runner`]); with no runner it returns
/// [`Verdict::Unverifiable`] with a "no agent runner configured" note, and *no*
/// subprocess is spawned — agent execution is strictly opt-in, so a default run
/// never reaches a model. A [`CheckKind::Human`] check is never executed. A
/// not-executed check is *not* silently passed, so the claim ages toward a human
/// (invariant #6). The exhaustive `match` on [`CheckKind`] means a future kind
/// cannot be added without a decision here being forced.
///
/// This is the primitive every verb that runs a check builds on: `claim add` and
/// `claim amend` call it against the current tree expecting [`Verdict::Held`], and
/// `add`'s optional `--witness-cmd` calls it again inside an isolated worktree
/// expecting [`Verdict::Drifted`] to confirm the check discriminates. The signature
/// is kept convenient — a borrowed check and context in, a plain outcome out, no I/O
/// setup the caller must thread through — and the context's `cwd` is what lets the
/// same primitive run against either the real tree or a throwaway worktree.
#[must_use]
pub fn run_check(check: &Check, ctx: &CheckContext) -> CheckOutcome {
    match &check.kind {
        CheckKind::Cmd { run, negate } => run_cmd(run, *negate, ctx),
        CheckKind::Agent { instruction } => run_agent(instruction, ctx),
        CheckKind::Human { .. } => CheckOutcome {
            verdict: Verdict::Unverifiable,
            end: ProcessEnd::NotExecuted {
                note: "scheduled human look, not built in v1".to_owned(),
            },
            evidence: Some(
                "human checks are not executed by the tool; this claim needs a \
                 scheduled human look, which is not built yet"
                    .to_owned(),
            ),
            duration: Duration::ZERO,
        },
    }
}

/// The decision a check's [`Skip`] yields for one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipDecision {
    /// The skip does not suppress this run; run the check. The optional note records
    /// *why* the skip did not apply — an expiry, or a condition that could not be
    /// evaluated — for the report.
    Run(Option<String>),
    /// The skip suppresses this run; do not run the check and record no verdict.
    Skip,
}

/// Decide whether a check's declared [`Skip`] suppresses this run, honestly.
///
/// The rules exist so a skip can never become a silent, permanent mute — the exact
/// stale-green-light this tool refuses:
/// - An expired `until` (now on or after it) *always* runs the check — the debt is
///   called regardless of `unless` — and the note records the lapse.
/// - An `unless` command that succeeds (exit 0) cancels the skip and runs the check;
///   one that fails (exit 1) leaves the skip in force; one that cannot be evaluated
///   (any other exit, a spawn failure, a timeout) runs the check rather than muting
///   it, because a broken condition must never hide drift.
/// - With neither an expiry nor an `unless`, the skip holds — an indefinite skip,
///   which callers surface loudly rather than treat as healthy.
///
/// `now` is a parameter (never the wall clock) so the decision is deterministic under
/// test. Evaluating `unless` runs a command through the same bounded runner a `cmd`
/// check uses, so a hung condition times out to a run, not a hang.
pub fn evaluate_skip(skip: &Skip, ctx: &CheckContext, now: Timestamp) -> SkipDecision {
    if let Some(until) = skip.until {
        if now >= until {
            return SkipDecision::Run(Some(format!("skip expired {until}")));
        }
    }
    if let Some(condition) = &skip.unless {
        return match run_cmd(condition, false, ctx).verdict {
            // exit 0: the condition holds — this environment can verify, so run.
            Verdict::Held => SkipDecision::Run(None),
            // exit 1: the condition does not hold — the skip stands.
            Verdict::Drifted => SkipDecision::Skip,
            // Anything else could not evaluate the condition; run rather than
            // silently skip, so a broken `unless` can never mute a check.
            _ => SkipDecision::Run(Some(format!(
                "skip condition `{condition}` could not be evaluated; running the check"
            ))),
        };
    }
    SkipDecision::Skip
}

/// Execute a `cmd` check's `run` string and classify the result.
///
/// The command is run through the platform shell so pipes, globs, and quoting in
/// the `run` string behave as an author expects (docs/design/PRODUCT.md's examples rely on
/// shell features). The shell interprets syntax only; the exit-code-to-verdict
/// mapping and negation are the tool's, per invariants #1 and #2.
fn run_cmd(run: &str, negate: bool, ctx: &CheckContext) -> CheckOutcome {
    // Defense-in-depth against a vacuous pass: an empty or whitespace-only `run`
    // would execute as `sh -c ""`, exit 0, and report Held forever — a check that
    // can never go red. The parser already rejects this at authoring time; this
    // second guard means even a `Check` built by some other path cannot slip a
    // blank command past the honesty core. Broken, never Held (and never Drifted
    // under negate, since Broken does not invert).
    if run.trim().is_empty() {
        return CheckOutcome {
            verdict: Verdict::Broken,
            end: ProcessEnd::SpawnFailed {
                reason: "run is empty; nothing to verify".to_owned(),
            },
            evidence: None,
            duration: Duration::ZERO,
        };
    }

    let started = Instant::now();
    let (end, output) = execute(run, ctx);
    let duration = started.elapsed();

    let base = classify_exit(&end);
    let verdict = apply_negation(base, negate);
    CheckOutcome {
        verdict,
        end,
        evidence: evidence_from(output, ctx.output_cap),
        duration,
    }
}

/// Execute an `agent` check by running the configured [`AgentRunner`] and mapping
/// its structured answer to a verdict under the same broken-never-passes contract
/// as `cmd`.
///
/// This is the second place a check becomes a judgement, so invariant #1 is
/// enforced here as strictly as in [`classify_exit`]. The mapping is total and
/// leaves no path from a misbehaving or malicious runner to a false [`Verdict::Held`]:
///
/// - No runner configured → [`Verdict::Unverifiable`], and nothing is spawned. A
///   default run cannot reach a model.
/// - Runner fails to spawn, is killed by a signal, times out, or exits non-zero →
///   [`Verdict::Broken`]. The runner could not cleanly answer, so its output is not
///   trusted — even a `{"verdict":"held"}` on a non-zero exit is discarded.
/// - Runner exits 0 but its stdout is not a well-formed verdict object with a valid
///   `verdict` field → [`Verdict::Broken`]. Malformed output is a runner that
///   failed to produce an answer, never a guessed pass.
/// - Runner exits 0 with a valid object → `held`/`drifted`/`unverifiable` maps to
///   the matching verdict, and its `evidence` and `citations` become the outcome's
///   evidence.
///
/// Negation is intentionally not applied to an agent verdict: an agent check has no
/// `negate` field (see [`CheckKind::Agent`]), because the runner reports the
/// verdict directly rather than through an exit code that a claim might need to
/// invert.
#[cfg(unix)]
fn run_agent(instruction: &str, ctx: &CheckContext) -> CheckOutcome {
    let Some(runner) = &ctx.agent_runner else {
        return CheckOutcome {
            verdict: Verdict::Unverifiable,
            end: ProcessEnd::NotExecuted {
                note: "no agent runner configured".to_owned(),
            },
            evidence: Some(
                "no agent runner is configured, so this agent check was not executed; set \
                 CLAIM_AGENT_CMD to a runner that reads the prompt on stdin and emits the \
                 verdict JSON on stdout"
                    .to_owned(),
            ),
            duration: Duration::ZERO,
        };
    };

    let command = match build_runner_command(runner) {
        Ok(command) => command,
        Err(reason) => {
            return CheckOutcome {
                verdict: Verdict::Broken,
                end: ProcessEnd::SpawnFailed { reason },
                evidence: None,
                duration: Duration::ZERO,
            };
        }
    };

    let prompt = build_agent_prompt(instruction);
    let started = Instant::now();
    let (end, stdout, stderr) = run_process(command, Some(prompt.into_bytes()), ctx);
    let duration = started.elapsed();

    classify_agent(end, stdout, stderr, ctx.output_cap, duration)
}

/// Build the child process for an [`AgentRunner`], with no shell for the argv form.
///
/// An `Argv` runs its program directly, so a runner path with spaces or special
/// characters is never re-parsed by a shell. An empty argv has no program to run
/// and is a spawn failure (`Broken`), never a pass. A `Shell` runner is the
/// operator's own reviewed one-liner, run as `sh -c`. Stdin disposition is left to
/// [`run_process`], which sets a pipe because the prompt is fed on stdin.
#[cfg(unix)]
fn build_runner_command(runner: &AgentRunner) -> std::result::Result<Command, String> {
    match runner {
        AgentRunner::Argv(argv) => {
            let (program, args) = argv
                .split_first()
                .ok_or_else(|| "the agent runner argv is empty; nothing to execute".to_owned())?;
            let mut command = Command::new(program);
            command.args(args);
            Ok(command)
        }
        AgentRunner::Shell(script) => {
            if script.trim().is_empty() {
                return Err("the agent runner command is empty; nothing to execute".to_owned());
            }
            let mut command = Command::new("sh");
            command.arg("-c").arg(script);
            Ok(command)
        }
    }
}

/// Map an agent runner's process outcome and captured streams to a verdict.
///
/// The verdict is parsed from **`stdout` only**; `stderr` is the runner's
/// diagnostics and is never a verdict source — a decoy `{"verdict":"held"}` written
/// to stderr cannot become a pass. `stderr` is used solely to enrich the evidence on
/// a broken outcome, so a human debugging a failed runner still sees what it
/// complained about.
///
/// Split out from [`run_agent`] so the honesty mapping is unit-testable without
/// spawning: given a [`ProcessEnd`] and the bytes the runner wrote, does the right
/// verdict fall out? The rule, in order:
///
/// - Any process end other than a clean `exit 0` is [`Verdict::Broken`]; the
///   runner did not cleanly answer and its output is not consulted.
/// - On `exit 0`, `stdout` is parsed for the verdict object. A parse failure — no
///   object, a missing/malformed/unrecognized `verdict`, a duplicate `verdict` key,
///   or conflicting verdicts — is [`Verdict::Broken`], with the raw output kept as
///   evidence so a human can see what the runner actually said.
#[cfg(unix)]
fn classify_agent(
    end: ProcessEnd,
    stdout: Captured,
    stderr: Captured,
    cap: usize,
    duration: Duration,
) -> CheckOutcome {
    if !matches!(end, ProcessEnd::Exited { code: 0 }) {
        // The runner crashed, was signalled, timed out, or exited non-zero. Its
        // output — even a well-formed `held` on stdout — is discarded: a check that
        // could not run cannot report a pass (invariant #1). Both streams are kept as
        // evidence so the failure is diagnosable.
        let mut combined = stdout;
        combined.extend(stderr, cap);
        return CheckOutcome {
            verdict: Verdict::Broken,
            end,
            evidence: evidence_from(combined, cap),
            duration,
        };
    }

    let raw = String::from_utf8_lossy(&stdout.bytes);
    match parse_agent_response(&raw) {
        Ok(response) => {
            // A well-formed verdict with no evidence text records `None` rather than
            // an empty string, matching the cmd path's "no output → no evidence"
            // convention; a verdict with evidence records it, capped.
            let note = response.evidence_note();
            let evidence = (!note.is_empty()).then(|| cap_evidence(note, cap));
            CheckOutcome {
                verdict: response.verdict,
                end,
                evidence,
                duration,
            }
        }
        Err(reason) => {
            // Exit 0 but stdout was not a well-formed, unambiguous verdict. This is
            // the malicious/broken-runner guard: prose, an empty body,
            // `{"verdict":"maybe"}`, conflicting verdicts, or a stderr-only decoy is
            // Broken, never a fabricated Held. Both streams are retained (capped) so
            // the failure is diagnosable.
            let mut note = format!("agent runner produced no usable verdict: {reason}");
            let stdout_trimmed = raw.trim();
            if !stdout_trimmed.is_empty() {
                note.push_str("\nstdout: ");
                note.push_str(stdout_trimmed);
            }
            let stderr_text = String::from_utf8_lossy(&stderr.bytes);
            let stderr_trimmed = stderr_text.trim();
            if !stderr_trimmed.is_empty() {
                note.push_str("\nstderr: ");
                note.push_str(stderr_trimmed);
            }
            CheckOutcome {
                verdict: Verdict::Broken,
                end,
                evidence: Some(cap_evidence(note, cap)),
                duration,
            }
        }
    }
}

/// A parsed, validated agent verdict response.
#[derive(Debug)]
struct AgentResponse {
    verdict: Verdict,
    evidence: String,
    citations: Vec<String>,
}

impl AgentResponse {
    /// The evidence note recorded in the verdict log: the runner's stated evidence
    /// followed by its citations, one per line. Kept human-readable because the
    /// evidence is the point of an agent check — a person reading the log sees the
    /// reasoning and the sources, not a bare verdict.
    fn evidence_note(&self) -> String {
        let mut note = self.evidence.clone();
        if !self.citations.is_empty() {
            if !note.is_empty() {
                note.push('\n');
            }
            note.push_str("citations:");
            for citation in &self.citations {
                note.push_str("\n- ");
                note.push_str(citation);
            }
        }
        note
    }
}

/// Parse a runner's stdout into a validated [`AgentResponse`], or a reason it is
/// not a usable verdict.
///
/// The runner is asked to emit exactly one JSON object, but a model may wrap it in
/// prose or reason out loud before its final answer, so **every** balanced `{…}`
/// span that carries a `verdict` is examined — not just the first, and not just the
/// last, because both are gameable by a model that emits a tentative verdict and
/// then a corrected one. The rule the honesty core turns on:
///
/// - Zero verdict-bearing objects → error (garbled or prose-only output).
/// - A verdict-bearing object whose `verdict` is malformed — not a string, not one
///   of the three values, or a span with a *duplicate* `verdict` key (which
///   `serde_json` would silently resolve to the last value) — → error. Ambiguity is
///   never resolved toward a value.
/// - More than one *distinct* verdict across the objects → error ("conflicting
///   verdicts"). A run that did not cleanly conclude one thing is not a pass.
/// - Exactly one distinct verdict across every bearing object → that verdict, with
///   the evidence and citations of the first object that carried it.
///
/// Every error maps to [`Verdict::Broken`], so a narrating, conflicted, or garbled
/// runner never reads as a pass. `evidence` and `citations` are optional and
/// default to empty: the verdict is the honesty-critical field, and missing
/// evidence is a weaker answer, not a broken one.
fn parse_agent_response(raw: &str) -> std::result::Result<AgentResponse, String> {
    let mut found: Option<AgentResponse> = None;
    for candidate in json_object_candidates(raw) {
        let Ok(serde_json::Value::Object(map)) =
            serde_json::from_str::<serde_json::Value>(candidate)
        else {
            continue;
        };
        // A candidate object that parses but carries no `verdict` is not an answer;
        // skip it (a model may emit a stray `{}` or an aside object before or after
        // the real one) rather than failing on it.
        if !map.contains_key("verdict") {
            continue;
        }
        // A single object with a duplicate `verdict` key is ambiguous: `serde_json`
        // silently kept the last, so the parsed map cannot be trusted to represent
        // the model's answer. Broken, never resolved toward a value.
        if duplicate_verdict_key(candidate) {
            return Err(
                "an output object has more than one 'verdict' key, which is ambiguous".to_owned(),
            );
        }
        let verdict = parse_verdict_value(&map["verdict"])?;
        let response = agent_response_from(&map, verdict);
        match &found {
            // A second, disagreeing verdict means the runner did not cleanly conclude
            // one thing. Broken — never a chosen pass, in either direction.
            Some(prior) if prior.verdict != verdict => {
                return Err(format!(
                    "runner emitted conflicting verdicts ('{}' and '{}')",
                    verdict_str(prior.verdict),
                    verdict_str(verdict)
                ));
            }
            // A repeat of the same verdict is consistent; keep the first (its
            // evidence).
            Some(_) => {}
            None => found = Some(response),
        }
    }
    found.ok_or_else(|| "no JSON object with a 'verdict' field was found in the output".to_owned())
}

/// Map a `verdict` JSON value to a [`Verdict`], or a reason it is not one of the
/// three allowed strings. Never returns [`Verdict::Broken`] — that is the caller's
/// mapping for the error case; this only recognizes a valid answer.
fn parse_verdict_value(value: &serde_json::Value) -> std::result::Result<Verdict, String> {
    let Some(s) = value.as_str() else {
        return Err("the 'verdict' field is not a string".to_owned());
    };
    match s {
        "held" => Ok(Verdict::Held),
        "drifted" => Ok(Verdict::Drifted),
        "unverifiable" => Ok(Verdict::Unverifiable),
        other => Err(format!(
            "'verdict' was '{other}', not one of held, drifted, unverifiable"
        )),
    }
}

/// The lowercase wire name of a verdict, for error messages naming what conflicted.
fn verdict_str(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Held => "held",
        Verdict::Drifted => "drifted",
        Verdict::Unverifiable => "unverifiable",
        Verdict::Broken => "broken",
    }
}

/// Build an [`AgentResponse`] from a parsed object and its already-validated verdict.
fn agent_response_from(
    map: &serde_json::Map<String, serde_json::Value>,
    verdict: Verdict,
) -> AgentResponse {
    let evidence = map
        .get("evidence")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let citations = map
        .get("citations")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    AgentResponse {
        verdict,
        evidence,
        citations,
    }
}

/// Whether a single JSON object span contains more than one `"verdict"` key at its
/// top level.
///
/// `serde_json` silently keeps the last of duplicate keys, so a
/// `{"verdict":"drifted","verdict":"held"}` would deserialize to `held` and hide
/// the conflict — a false-pass vector. This scans the object's raw span, tracking
/// string literals (so a `"verdict"` inside a value string does not count) and
/// brace depth (so a `"verdict"` key in a nested object does not count), and returns
/// `true` when two top-level `verdict` keys are seen. A key is a `"verdict"` string
/// immediately followed (after optional whitespace) by a `:`.
fn duplicate_verdict_key(span: &str) -> bool {
    let bytes = span.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut string_start = 0usize;
    let mut count = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
                // A key sits at object depth 1 (inside the outer braces, not nested)
                // and is followed by a colon. Check the just-closed string literal.
                if depth == 1
                    && &span[string_start..i] == "verdict"
                    && next_nonspace_is_colon(bytes, i + 1)
                {
                    count += 1;
                    if count > 1 {
                        return true;
                    }
                }
            }
        } else if b == b'"' {
            in_string = true;
            string_start = i + 1;
        } else if b == b'{' {
            depth += 1;
        } else if b == b'}' {
            depth = depth.saturating_sub(1);
        }
        i += 1;
    }
    false
}

/// Whether the next non-whitespace byte at or after `from` is a `:`.
fn next_nonspace_is_colon(bytes: &[u8], from: usize) -> bool {
    bytes[from..]
        .iter()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|b| *b == b':')
}

/// Every balanced-brace `{…}` span in `raw`, outermost-first, as candidate JSON
/// objects.
///
/// A model may print the required object amid prose (`Here is my answer: {…}`), so
/// the parser cannot assume the whole output is JSON. This yields each top-level
/// `{…}` span — brace depth tracked so a nested object does not end the span early
/// — for [`parse_agent_response`] to try in turn. Braces inside JSON string
/// literals are skipped (with escape handling) so a `{` or `}` inside an evidence
/// string does not desynchronize the depth count. This is a locator, not a full
/// tokenizer: `serde_json` is the actual validator, and a span that is not valid
/// JSON is skipped.
fn json_object_candidates(raw: &str) -> Vec<&str> {
    let bytes = raw.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        let start = i;
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut end = None;
        let mut j = i;
        while j < bytes.len() {
            let b = bytes[j];
            if in_string {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    in_string = false;
                }
            } else if b == b'"' {
                in_string = true;
            } else if b == b'{' {
                depth += 1;
            } else if b == b'}' {
                // The scan enters the inner loop at a `{`, which sets `depth` to 1
                // before any `}` is seen, and breaks the instant `depth` returns to
                // 0 — so `depth` is always at least 1 here and this cannot underflow.
                depth -= 1;
                if depth == 0 {
                    end = Some(j + 1);
                    break;
                }
            }
            j += 1;
        }
        match end {
            Some(e) => {
                spans.push(&raw[start..e]);
                // Continue scanning after this object so a second object later in the
                // output is also considered.
                i = e;
            }
            // An unbalanced `{` cannot start a valid object; skip past it.
            None => i = start + 1,
        }
    }
    spans
}

/// The fixed directive appended to an agent check's instruction, stating the exact
/// response the runner must produce.
///
/// Kept as a constant, and asserted in tests, because the response schema is a
/// contract: the parser in [`parse_agent_response`] and this prompt must name the
/// same fields and the same three verdict values, or a runner told one thing would
/// be judged by another.
const AGENT_RESPONSE_DIRECTIVE: &str = "\
Respond on stdout with exactly one JSON object and no other text, in this shape:
  {\"verdict\": \"held\" | \"drifted\" | \"unverifiable\", \"evidence\": \"<why>\", \"citations\": [\"<source>\", ...]}
Emit only your final verdict object. Do not print any earlier, draft, or intermediate JSON object: more than one object, or two objects that disagree, is treated as a failure to decide (broken), not a pass.
Rules:
- \"held\": the fact stated above is still true.
- \"drifted\": the fact is now false.
- \"unverifiable\": the evidence is insufficient or conflicting to decide.
Report \"held\" only if you are confident the fact still holds. When in doubt, use \"unverifiable\" rather than guessing. Put your reasoning in \"evidence\" and cite your sources in \"citations\".";

/// Build the full prompt sent to an [`AgentRunner`] on stdin: the claim's
/// instruction, then the fixed response directive.
///
/// The instruction is passed on stdin, not as a shell argument, so a long
/// natural-language instruction is never subject to shell quoting or injection —
/// the runner reads exactly these bytes. The directive is appended (not prepended)
/// so the instruction leads and the machine-readable contract follows, and it names
/// the same field and verdict values the response parser enforces, so a runner is
/// never told one schema and judged by another.
#[must_use]
pub fn build_agent_prompt(instruction: &str) -> String {
    format!("{instruction}\n\n{AGENT_RESPONSE_DIRECTIVE}\n")
}

/// Truncate an evidence note to `cap` bytes on a char boundary, marking truncation.
///
/// Mirrors the cmd path's cap ([`evidence_from`]) so an agent's evidence cannot
/// bloat the verdict log any more than a command's output can. Truncation is at a
/// char boundary so the retained text is always valid UTF-8.
fn cap_evidence(note: String, cap: usize) -> String {
    if note.len() <= cap {
        return note;
    }
    let mut end = cap;
    while end > 0 && !note.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = note[..end].to_owned();
    truncated.push_str(&format!(
        "\n[evidence truncated at {cap} bytes; the runner produced more]"
    ));
    truncated
}

/// Map a process outcome to a verdict, before negation — the exit-code contract.
///
/// The whole product turns on this being exactly right, so it is a total `match`
/// with no wildcard: a deliberate exit 0 is the *only* path to
/// [`Verdict::Held`], exit 1 is [`Verdict::Drifted`], and everything else — any
/// other exit code (2 for a grep error, 126 for not-executable, 127 for
/// not-found, 130 for Ctrl-C, …), death by signal, a timeout, or a spawn failure
/// — is [`Verdict::Broken`]. A check that could not run cannot report that the
/// fact is fine. `NotExecuted` never reaches here (agent/human checks bypass
/// classification), but the arm is present so the `match` is total and its floor
/// is `Broken`, never a pass.
fn classify_exit(end: &ProcessEnd) -> Verdict {
    match end {
        ProcessEnd::Exited { code: 0 } => Verdict::Held,
        ProcessEnd::Exited { code: 1 } => Verdict::Drifted,
        ProcessEnd::Exited { .. }
        | ProcessEnd::Signalled
        | ProcessEnd::TimedOut { .. }
        | ProcessEnd::SpawnFailed { .. }
        | ProcessEnd::NotExecuted { .. } => Verdict::Broken,
    }
}

/// Invert a `negate` check's verdict, swapping `Held`↔`Drifted` only.
///
/// Negation is a property of the claim, applied here to an *already classified*
/// verdict — never by asking a shell to interpret `!`. That is what keeps a
/// `negate` check honest: [`Verdict::Broken`] and [`Verdict::Unverifiable`] pass
/// through unchanged, so a `negate` check whose binary is missing stays `Broken`
/// rather than inverting a spawn failure into a false pass (invariant #2).
fn apply_negation(verdict: Verdict, negate: bool) -> Verdict {
    if !negate {
        return verdict;
    }
    match verdict {
        Verdict::Held => Verdict::Drifted,
        Verdict::Drifted => Verdict::Held,
        Verdict::Broken | Verdict::Unverifiable => verdict,
    }
}

/// Turn captured output into evidence, applying the cap.
///
/// `output` already carries whether it was truncated at the cap and whether a
/// reader was detached because a process outlived the check and held its output
/// pipe open (the escapee case; see [`execute`]). Empty output yields `None` so
/// the log entry's `evidence` is absent rather than an empty string — unless a
/// reader was detached, in which case the note is worth recording even with no
/// bytes. Non-UTF-8 bytes are replaced rather than dropped, so binary noise in a
/// command's output cannot lose the readable parts around it.
fn evidence_from(output: Captured, cap: usize) -> Option<String> {
    if output.bytes.is_empty() && !output.escapee {
        return None;
    }
    let mut text = String::from_utf8_lossy(&output.bytes).into_owned();
    if output.truncated {
        text.push_str(&format!(
            "\n[output truncated at {cap} bytes; the check produced more]"
        ));
    }
    if output.escapee {
        text.push_str(
            "\n[output truncated: a process outlived the check and held its output open]",
        );
    }
    Some(text)
}

/// stdout and stderr captured up to the cap.
///
/// `truncated` is set when the cap was hit; `escapee` when a reader had to be
/// detached because a process outlived the check and kept the pipe open, so the
/// capture is whatever had arrived by the grace deadline. Both are surfaced in
/// the evidence so a reader is never misled that partial output is complete.
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
    escapee: bool,
}

/// How long to wait for the output readers to drain after the check's own
/// process has been reaped and its group killed, before detaching a stuck reader.
///
/// After [`execute`] reaps the shell and kills its process group, any reader
/// still blocked can only be one whose pipe is held open by a process that
/// *escaped* the group — a `setsid()` daemon the check spawned, which no
/// group-kill can reach. Waiting on it forever would let a check's timeout fail
/// to bound [`run_check`] (the liveness the timeout exists to protect). A short
/// fixed grace lets a normal reader finish draining the last buffered bytes, then
/// the escapee's reader is detached and whatever it captured so far is kept. Two
/// seconds is comfortably longer than draining a closed pipe takes yet negligible
/// against the check timeout.
#[cfg(unix)]
const READER_JOIN_GRACE: Duration = Duration::from_secs(2);

/// Spawn a `cmd` check's shell command with no stdin, capture bounded output, and
/// enforce the timeout.
///
/// A thin builder over [`run_process`]: it constructs the `sh -c <run>` command
/// with stdin detached (a `cmd` check reads nothing) and hands the shared
/// execution machinery the rest. Factored this way so the agent path
/// ([`run_agent`]) reuses the identical process-group, timeout, group-kill, and
/// bounded-capture behavior instead of duplicating it — the one place a check
/// becomes a process must behave the same for every kind.
///
/// A `cmd` check's evidence is its combined stdout+stderr, so the two separate
/// captures [`run_process`] returns are merged here (within the cap). The agent path
/// keeps them apart, because it parses a verdict from stdout only.
#[cfg(unix)]
fn execute(run: &str, ctx: &CheckContext) -> (ProcessEnd, Captured) {
    let mut command = Command::new("sh");
    command.arg("-c").arg(run).stdin(Stdio::null());
    let (end, mut stdout, stderr) = run_process(command, None, ctx);
    stdout.extend(stderr, ctx.output_cap);
    (end, stdout)
}

/// Drive a fully-configured child process to a terminal outcome: spawn it in its
/// own group, optionally feed `stdin` bytes, capture bounded stdout+stderr, and
/// enforce the timeout.
///
/// Returns the terminal [`ProcessEnd`] and the captured `(stdout, stderr)`, kept
/// separate so a caller that parses one stream (the agent path parses a verdict from
/// stdout only) is never fooled by the other. This is the only function that touches
/// the process; all judgement is downstream of it, so the honesty rules stay
/// readable and testable apart from the I/O. The caller sets `command`'s program,
/// args, and stdin disposition; this function forces the working directory, piped
/// stdout/stderr, and the process group, so no caller can forget the no-orphan setup.
///
/// When `stdin` is `Some(bytes)`, the command is spawned with a piped stdin and the
/// bytes are written on a dedicated thread. Writing on a thread (rather than before
/// the wait) is the same anti-deadlock discipline the readers use: a runner that
/// emits more than a pipe buffer of output before draining its stdin would
/// otherwise wedge a foreground write. The writer thread is never joined — like a
/// stuck reader it is bounded by the group-kill, so a runner that ignores its stdin
/// cannot stall the timeout.
///
/// Liveness is the load-bearing property here, and it is why the readers are
/// drained through shared buffers with a *bounded* join rather than a plain
/// `JoinHandle::join`. A plain join blocks until the pipe's write end closes,
/// which a process that outlived the check — a backgrounded daemon that inherited
/// stdout, or one that called `setsid()` and escaped the group — never does. That
/// would make the check's timeout fail to bound this function at all. Instead,
/// once the child is reaped and its group killed, the readers are given
/// [`READER_JOIN_GRACE`] to finish; a reader still stuck past that is detached
/// (its thread deliberately leaked, bounded and rare) with whatever it captured.
#[cfg(unix)]
fn run_process(
    mut command: Command,
    stdin: Option<Vec<u8>>,
    ctx: &CheckContext,
) -> (ProcessEnd, Captured, Captured) {
    use std::os::unix::process::CommandExt;
    use std::os::unix::process::ExitStatusExt;
    use wait_timeout::ChildExt;

    command
        .current_dir(&ctx.cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }

    // Put the child in its own process group (pgid == the child's pid) so a
    // command that spawns children — `sleep 100 | foo` — puts them in the same
    // group. We kill the *group*, so no in-group grandchild is orphaned. Set on
    // the child only; passing 0 makes the child its own leader without disturbing
    // the tool's own group.
    command.process_group(0);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            return (
                ProcessEnd::SpawnFailed {
                    reason: e.to_string(),
                },
                Captured::empty(),
                Captured::empty(),
            )
        }
    };
    // A pid is always well within i32 (kernel PID_MAX is far below i32::MAX), so
    // this cast cannot truncate; it only bridges std's u32 to libc's signed pid_t.
    let pgid = child.id() as libc::pid_t;

    // Feed the prompt on a detached thread if there is one. The pipe is moved into
    // the thread so it is closed (signalling EOF to the child) when the write
    // finishes; a write error — the runner closed its stdin early — is not itself a
    // check failure, so it is ignored and the runner's own exit decides the verdict.
    if let (Some(bytes), Some(mut sink)) = (stdin, child.stdin.take()) {
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = sink.write_all(&bytes);
            let _ = sink.flush();
        });
    }

    // Drain stdout and stderr on their own threads into shared buffers. Draining
    // concurrently avoids the deadlock where a command that writes more than a
    // pipe buffer holds blocks on the write while we block on the wait. Sharing
    // the buffer (rather than returning it from the thread) is what lets us take
    // the captured-so-far bytes even when a reader must be detached.
    let cap = ctx.output_cap;
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let out_reader = child
        .stdout
        .take()
        .map(|s| spawn_reader(s, cap, done_tx.clone()));
    let err_reader = child.stderr.take().map(|s| spawn_reader(s, cap, done_tx));

    let end = match child.wait_timeout(ctx.timeout) {
        Ok(Some(status)) => match status.code() {
            // `code()` is `None` exactly when a signal killed the process; that is
            // never a deliberate answer, so it is `Broken` via `Signalled`.
            Some(code) => ProcessEnd::Exited { code },
            None => {
                debug_assert!(
                    status.signal().is_some(),
                    "a unix ExitStatus with no code must carry a signal"
                );
                ProcessEnd::Signalled
            }
        },
        Ok(None) => {
            // The deadline passed and the child is still running. Kill the group
            // *before* the reaping `wait()`, or the wait would block for the whole
            // remaining lifetime of the still-running command.
            kill_group(pgid);
            let _ = child.wait();
            ProcessEnd::TimedOut { after: ctx.timeout }
        }
        Err(e) => {
            // Waiting itself faulted; kill the group before reaping for the same
            // reason, then reap best-effort.
            kill_group(pgid);
            let _ = child.wait();
            ProcessEnd::SpawnFailed {
                reason: format!("waiting for the check failed: {e}"),
            }
        }
    };

    // Kill the process group on EVERY terminal path — not only the timeout arms
    // above (which killed before reaping). A check that *completed* may still have
    // leaked a background child; killing the group after the child is reaped reaps
    // that in-group child too — no orphan survives a completed check — and closes
    // any pipe an in-group child still holds, which is what lets the readers reach
    // EOF promptly in the common case. A second kill on the timeout paths is a
    // harmless ESRCH (the group is already gone). ESRCH on a clean exit is equally
    // harmless: the group emptied itself.
    kill_group(pgid);

    let (stdout, stderr) = collect_output(out_reader, err_reader, &done_rx);
    (end, stdout, stderr)
}

/// Collect the two readers' captured output with a bounded wait, keeping stdout and
/// stderr *separate*.
///
/// Returns `(stdout, stderr)`. Keeping the streams apart is load-bearing for the
/// agent path: a verdict is parsed from stdout only, so a diagnostic a runner writes
/// to stderr — even one containing a `{"verdict":...}` fragment — can never be
/// mistaken for the runner's answer. The cmd path combines them itself for its
/// evidence; separation costs it nothing.
///
/// The child is already reaped and its group killed, so any reader that is not
/// finished is blocked on a pipe held open by a process that escaped the group.
/// Wait up to [`READER_JOIN_GRACE`] *total* for both readers to signal
/// completion; then snapshot each shared buffer regardless. A reader that
/// finished is joined (its thread is done); one that did not is detached — its
/// thread is deliberately leaked rather than joined, so a single escapee cannot
/// wedge the tool — and its buffer is marked as an escapee capture.
#[cfg(unix)]
fn collect_output(
    out_reader: Option<Reader>,
    err_reader: Option<Reader>,
    done_rx: &std::sync::mpsc::Receiver<()>,
) -> (Captured, Captured) {
    let expected = out_reader.is_some() as usize + err_reader.is_some() as usize;
    let deadline = Instant::now() + READER_JOIN_GRACE;
    for _ in 0..expected {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if done_rx.recv_timeout(remaining).is_err() {
            break;
        }
    }

    let stdout = out_reader.map_or_else(Captured::empty, Reader::take);
    let stderr = err_reader.map_or_else(Captured::empty, Reader::take);
    (stdout, stderr)
}

/// Kill an entire process group by its leader's pid.
///
/// A check's shell and every child that stayed in its group share this pgid (see
/// [`execute`]'s `process_group(0)`), so signalling the group — not just the
/// shell — is what prevents an in-group grandchild outliving the tool. `SIGKILL`
/// rather than `SIGTERM`: a check that has to be group-killed has already either
/// timed out or exited, and a clean shutdown it might trap is not owed to it. A
/// failure is ignored because the goal is only that nothing in the group survives.
#[cfg(unix)]
fn kill_group(pgid: libc::pid_t) {
    // SAFETY: `killpg` is a plain FFI syscall with no memory effects; it delivers
    // a signal and returns. The precondition it needs is that the check's own
    // process is not reaped by anyone but us: `execute` always `wait()`s the child
    // before this call, and the child leads this group, so on the normal paths the
    // pgid still refers to this check's group (or is already empty → ESRCH), never
    // a recycled unrelated group. Errors are intentionally ignored: ESRCH means
    // the group is already gone (the outcome we want); EPERM would mean a member
    // changed credentials (a setuid grandchild) and we may not signal it — that
    // grandchild then escapes the no-orphan guarantee, an accepted limitation of a
    // core that does not run privileged.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

impl Captured {
    /// No output — the process produced nothing or never started.
    fn empty() -> Self {
        Captured {
            bytes: Vec::new(),
            truncated: false,
            escapee: false,
        }
    }

    /// Append another stream's capture, keeping the combined total within `cap`.
    fn extend(&mut self, other: Captured, cap: usize) {
        self.truncated |= other.truncated;
        self.escapee |= other.escapee;
        let room = cap.saturating_sub(self.bytes.len());
        if other.bytes.len() > room {
            self.truncated = true;
            self.bytes.extend_from_slice(&other.bytes[..room]);
        } else {
            self.bytes.extend_from_slice(&other.bytes);
        }
    }
}

/// A running output reader: the thread draining one stream, and the shared buffer
/// it writes into.
///
/// Holding the buffer in an [`Arc<Mutex<_>>`](std::sync::Mutex) shared with the
/// thread — rather than returning it by joining — is what lets [`execute`] take
/// the captured-so-far bytes even when the thread must be *detached* because a
/// process outlived the check and is holding the pipe open.
#[cfg(unix)]
struct Reader {
    handle: std::thread::JoinHandle<()>,
    buf: std::sync::Arc<std::sync::Mutex<Captured>>,
    /// Set by the reader thread as its final action, so this is a race-free
    /// "the drain is complete" signal. `JoinHandle::is_finished` can briefly lag a
    /// thread that has finished its work but not yet been torn down, which would
    /// spuriously flag a completed reader as an escapee; this flag does not.
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(unix)]
impl Reader {
    /// Take this reader's captured output, joining the thread if its drain is
    /// complete and detaching it (leaking the thread, deliberately) if it is not.
    ///
    /// A completed drain means the thread is exiting, so the join is prompt. An
    /// incomplete one can only be blocked on a pipe held by an escaped process;
    /// joining would block [`execute`] indefinitely, so instead the thread is
    /// dropped — a bounded, rare leak — and the buffer is marked as an escapee
    /// capture so the evidence says the output is partial.
    fn take(self) -> Captured {
        if self.done.load(std::sync::atomic::Ordering::Acquire) {
            // The drain finished; join reaps the exiting thread. A panic in the
            // reader (only an allocation abort could get here, and that aborts
            // rather than unwinds, so this is defense-in-depth) degrades to no
            // output rather than propagating.
            let _ = self.handle.join();
            std::mem::take(&mut *lock(&self.buf))
        } else {
            let mut captured = std::mem::take(&mut *lock(&self.buf));
            captured.escapee = true;
            captured
            // `self.handle` is dropped here without a join: the thread is detached
            // and will exit on its own if the escaped process ever closes the pipe.
        }
    }
}

impl Default for Captured {
    fn default() -> Self {
        Captured::empty()
    }
}

/// Lock a mutex, recovering the guard even if a previous holder panicked.
///
/// A poisoned buffer is not a reason to lose the evidence in it, so the poison is
/// stepped over rather than propagated.
#[cfg(unix)]
fn lock(buf: &std::sync::Mutex<Captured>) -> std::sync::MutexGuard<'_, Captured> {
    buf.lock().unwrap_or_else(|e| e.into_inner())
}

/// Spawn a thread that drains a child stream into a shared capped buffer, then
/// signals completion on `done`.
///
/// Reading on a thread (rather than after `wait`) is what avoids the pipe-buffer
/// deadlock described in [`execute`]; the shared buffer is what lets its bytes be
/// recovered even if the thread must later be detached. The completion signal on
/// `done` lets the collector wait for a bounded grace rather than an unbounded
/// join.
#[cfg(unix)]
fn spawn_reader<R: Read + Send + 'static>(
    reader: R,
    cap: usize,
    signal: std::sync::mpsc::Sender<()>,
) -> Reader {
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Captured::empty()));
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread_buf = std::sync::Arc::clone(&buf);
    let thread_done = std::sync::Arc::clone(&done);
    let handle = std::thread::spawn(move || {
        drain_into(reader, cap, &thread_buf);
        // Mark done before waking the collector, so a collector that observes the
        // wakeup and then reads the flag always sees `true` (release/acquire pair).
        thread_done.store(true, std::sync::atomic::Ordering::Release);
        // A send failure means the collector already stopped waiting (its grace
        // elapsed); nothing to signal, so the error is ignored.
        let _ = signal.send(());
    });
    Reader { handle, buf, done }
}

/// Read from `reader` until EOF or error, appending into the shared buffer up to
/// `cap` bytes.
///
/// Keeps consuming past the cap in fixed chunks so the writing child sees its pipe
/// drained and can exit rather than block forever on a full pipe — draining
/// without retaining, and marking the capture truncated. The read is done into a
/// local buffer and the shared lock is taken only to append, so the lock is never
/// held across a blocking `read` and the collector can always snapshot promptly. A
/// read error ends the loop; a broken pipe is not itself a check failure.
#[cfg(unix)]
fn drain_into<R: Read>(mut reader: R, cap: usize, shared: &std::sync::Mutex<Captured>) {
    let mut buf = [0u8; 8 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let mut captured = lock(shared);
                if captured.bytes.len() < cap {
                    let room = cap - captured.bytes.len();
                    let take = n.min(room);
                    captured.bytes.extend_from_slice(&buf[..take]);
                    if take < n {
                        captured.truncated = true;
                    }
                } else {
                    captured.truncated = true;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

/// Whether a claim's `supports` target still resolves, and why not if it does not.
///
/// Reported per target and kept *separate* from the check [`Verdict`] on purpose:
/// a deleted decision is its own loud condition — "the thing this claim justifies
/// is gone" — not a check failure. Folding it into `Broken` would conflate "the
/// check could not run" with "the decision vanished", and the CLI needs to say
/// which. A claim with an unresolved support goes loud instead of staying quietly
/// green (docs/design/PRODUCT.md section 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupportResolution {
    /// The target as written in the claim's `supports` list.
    pub target: String,
    /// Whether it still resolves against the current tree and store.
    pub resolved: bool,
    /// When unresolved, why — the missing file, the missing anchor, the unknown
    /// claim id. `None` when resolved. Phrased for a human deciding what to do.
    pub reason: Option<String>,
}

/// Resolve every `supports` target of a claim against the repo and store.
///
/// A `supports` target is either a decision ref `path#anchor` (or a bare `path`)
/// or a bare claim id. Resolution is a pure function of its inputs: the caller
/// supplies `known_claim_ids`, the set of ids it collected from the store, so
/// this needs no second store read and stays deterministic and testable. Taking
/// [`ClaimId`]s (not raw strings) matches what the CLI already holds and avoids a
/// stringly-typed boundary.
///
/// Resolution rules, each yielding a reason when it fails:
///
/// - **`path#anchor`**: unambiguously a decision ref, because a claim id can
///   never contain `#` (see [`crate::claim::ClaimId`]). Resolves iff the file
///   exists under `repo_root` *and* the anchor still occurs in it *at a word
///   boundary* — the anchor bounded by non-word characters (or the file edges),
///   so a deleted `## libfoo` heading is not reported resolved merely because the
///   substring `libfoo` survives inside `libfoobar` elsewhere. It stays a text
///   scan, not a Markdown-aware anchor match: over-precise structural matching
///   would raise false "unresolved" alarms on valid files, and the goal is to
///   catch a *deleted* decision, not to police heading syntax. This is a
///   best-effort collision-reducer, not a guarantee; authors should pick
///   distinctive anchors (a decision id, not a common word) so resolution has an
///   unambiguous mark to look for.
/// - **anything else** (no `#`): a namespaced claim id (`payments/libfoo-pin`)
///   is shaped exactly like a path, so the target is not forced into one
///   interpretation. It resolves if it is *either* a known claim id *or* an
///   existing file under `repo_root`; it is unresolved only when it is neither.
///   Resolving under either reading is deliberately safe here — the point is to
///   catch a target that has become *nothing*, and a target that is still a real
///   file or a real claim has not.
///
/// `repo_root` anchors relative paths the same way [`CheckContext::cwd`] anchors
/// a command, so a `supports: [requirements.txt#libfoo]` is checked against the
/// same tree the claim's command runs against. An absolute path in a target is
/// used as-is.
///
/// This does not read the wall clock or mutate anything; it only stats files and
/// consults the provided id set.
#[must_use]
pub fn resolve_supports(
    supports: &[SupportTarget],
    repo_root: &Path,
    known_claim_ids: &[ClaimId],
) -> Vec<SupportResolution> {
    supports
        .iter()
        .map(|target| resolve_one(target.as_str(), repo_root, known_claim_ids))
        .collect()
}

/// Resolve a single `supports` target. See [`resolve_supports`].
///
/// A target carrying a `#` is unambiguously a decision ref — claim ids never
/// contain `#` (see [`crate::claim::ClaimId`]), so an anchor can only mean a file
/// heading, and it is resolved purely as a file. A target with no `#` is
/// genuinely ambiguous: a namespaced claim id (`payments/libfoo-pin`) has the
/// same shape as a path. Rather than guess from punctuation — which would
/// misclassify a namespaced id as a missing file — such a target resolves if it
/// is *either* a known claim id *or* an existing file, and only when it is
/// neither does it go unresolved, with a reason naming both possibilities. A
/// claim id and a file path colliding cannot mask a real failure: the target
/// resolves precisely when at least one interpretation is real.
fn resolve_one(target: &str, repo_root: &Path, known_claim_ids: &[ClaimId]) -> SupportResolution {
    if let Some((path_part, anchor)) = target.split_once('#') {
        let candidate = resolve_path(repo_root, path_part);
        return resolve_decision_ref(target, &candidate, path_part, Some(anchor));
    }

    // No anchor: resolve as a claim id or as a bare file path, whichever is real.
    if known_claim_ids.iter().any(|id| id.as_str() == target) {
        return resolved(target);
    }
    let candidate = resolve_path(repo_root, target);
    if candidate.exists() {
        return resolved(target);
    }
    SupportResolution {
        target: target.to_owned(),
        resolved: false,
        reason: Some(format!(
            "'{target}' resolves to neither an existing file nor a claim id in the \
             store; the decision or claim it supports may have been deleted"
        )),
    }
}

/// Resolve a decision ref (`path` or `path#anchor`) against the filesystem.
fn resolve_decision_ref(
    target: &str,
    candidate: &Path,
    path_part: &str,
    anchor: Option<&str>,
) -> SupportResolution {
    if !candidate.exists() {
        return SupportResolution {
            target: target.to_owned(),
            resolved: false,
            reason: Some(format!(
                "the file '{path_part}' this claim supports no longer exists"
            )),
        };
    }
    if let Some(anchor) = anchor {
        match std::fs::read_to_string(candidate) {
            Ok(contents) if anchor_occurs(&contents, anchor) => resolved(target),
            Ok(_) => SupportResolution {
                target: target.to_owned(),
                resolved: false,
                reason: Some(format!(
                    "'{path_part}' exists but no longer contains the anchor '{anchor}'; \
                     the decision it pointed at may have been deleted or renamed"
                )),
            },
            Err(e) => SupportResolution {
                target: target.to_owned(),
                resolved: false,
                // Unreadable is unresolved, not resolved: we cannot confirm the
                // anchor, and a support that cannot be confirmed must go loud, not
                // stay green.
                reason: Some(format!(
                    "'{path_part}' could not be read to check its anchor: {e}"
                )),
            },
        }
    } else {
        resolved(target)
    }
}

/// A resolved target with no reason.
fn resolved(target: &str) -> SupportResolution {
    SupportResolution {
        target: target.to_owned(),
        resolved: true,
        reason: None,
    }
}

/// Join a target's path part onto `repo_root`, honoring an absolute path.
fn resolve_path(repo_root: &Path, path_part: &str) -> PathBuf {
    let p = Path::new(path_part);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        repo_root.join(p)
    }
}

/// Whether `anchor` occurs in `haystack` at a word boundary.
///
/// A boundary is a non-word character (anything but `[A-Za-z0-9_]`) or a file
/// edge on each side, so the anchor `libfoo` matches `## libfoo` and `libfoo:`
/// but not `libfoobar`. This reduces the false-resolve where a deleted heading's
/// keyword survives as a fragment of some unrelated identifier — a soft
/// false-green off the verdict path. It is a text scan, not a Markdown parse: the
/// aim is to catch a *removed* decision, not to validate heading syntax. An empty
/// anchor never matches (an empty `#` fragment is a malformed target, not a
/// license to resolve).
fn anchor_occurs(haystack: &str, anchor: &str) -> bool {
    if anchor.is_empty() {
        return false;
    }
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let hay = haystack.as_bytes();
    let needle = anchor.as_bytes();
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(anchor) {
        let start = from + rel;
        let end = start + needle.len();
        let left_ok = start == 0 || !is_word(hay[start - 1]);
        let right_ok = end == hay.len() || !is_word(hay[end]);
        if left_ok && right_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::claim::Trigger;

    /// A `cmd` check with the given `run` and `negate`, on the on-change trigger
    /// (the trigger is irrelevant to execution; `run_check` never reads it).
    fn cmd(run: &str, negate: bool) -> Check {
        Check {
            kind: CheckKind::Cmd {
                run: run.to_owned(),
                negate,
            },
            when: Trigger::OnChange,
            skip: None,
        }
    }

    /// Run a `cmd` check in a fresh temp directory with default timeout/cap.
    fn run(run: &str, negate: bool) -> CheckOutcome {
        let tmp = tempdir();
        run_check(&cmd(run, negate), &CheckContext::new(tmp.path()))
    }

    // --- The exit-code contract (invariant #1). One test per arm. ---

    #[test]
    fn exit_zero_is_held() {
        assert_eq!(run("true", false).verdict, Verdict::Held);
        assert_eq!(run("exit 0", false).verdict, Verdict::Held);
    }

    #[test]
    fn exit_one_is_drifted() {
        assert_eq!(run("false", false).verdict, Verdict::Drifted);
        assert_eq!(run("exit 1", false).verdict, Verdict::Drifted);
    }

    #[test]
    fn exit_two_is_broken() {
        // A grep syntax error, a misused builtin: exit 2 is not a clean drift and
        // must never read as Drifted, let alone Held.
        assert_eq!(run("exit 2", false).verdict, Verdict::Broken);
    }

    #[test]
    fn missing_binary_exit_127_is_broken() {
        // The shell exits 127 for a command it cannot find. This is the canonical
        // "the check could not run" case and must be Broken, never a pass.
        let outcome = run("this-binary-does-not-exist-anywhere", false);
        assert_eq!(outcome.verdict, Verdict::Broken);
        assert_eq!(outcome.end, ProcessEnd::Exited { code: 127 });
        assert_eq!(outcome.status(), "exit 127");
    }

    #[test]
    fn not_executable_exit_126_is_broken() {
        // A file that exists but cannot be executed exits 126 through the shell.
        let tmp = tempdir();
        let script = tmp.path().join("not-exec.sh");
        std::fs::write(&script, b"#!/bin/sh\ntrue\n").unwrap();
        // Deliberately not chmod +x, so exec fails with 126.
        let outcome = run_check(
            &cmd(&format!("{}", script.display()), false),
            &CheckContext::new(tmp.path()),
        );
        assert_eq!(outcome.verdict, Verdict::Broken);
        assert_eq!(outcome.end, ProcessEnd::Exited { code: 126 });
        assert_eq!(outcome.status(), "exit 126");
    }

    #[test]
    fn high_exit_codes_are_broken() {
        // Any code that is neither 0 nor 1 is Broken. Spot-check a spread.
        for code in [3, 42, 125, 200, 255] {
            let outcome = run(&format!("exit {code}"), false);
            assert_eq!(
                outcome.verdict,
                Verdict::Broken,
                "exit {code} must be Broken"
            );
        }
    }

    #[test]
    fn signal_death_is_broken() {
        // A process that kills itself with a signal has no exit code; it must be
        // Broken (via the Signalled arm), never Held. `kill -9 $$` makes the shell
        // signal itself, so wait returns a signal status with no code.
        let outcome = run("kill -9 $$", false);
        assert_eq!(outcome.verdict, Verdict::Broken);
        assert_eq!(outcome.end, ProcessEnd::Signalled);
        assert_eq!(outcome.status(), "killed by signal");
    }

    #[test]
    fn segfault_signal_is_broken() {
        // A different signal path: SIGSEGV rather than SIGKILL, to prove the
        // classification is on "no exit code", not on a specific signal.
        let outcome = run("kill -SEGV $$", false);
        assert_eq!(outcome.verdict, Verdict::Broken);
        assert_eq!(outcome.status(), "killed by signal");
    }

    #[test]
    fn bogus_cwd_is_a_spawn_failure_and_broken() {
        // A working directory that does not exist makes the spawn itself fail. It
        // must be Broken (a spawn failure), never a pass — the critical "could not
        // even start" case.
        let ctx = CheckContext::new("/no/such/directory/anywhere/xyz");
        let outcome = run_check(&cmd("true", false), &ctx);
        assert_eq!(outcome.verdict, Verdict::Broken);
        assert!(
            matches!(outcome.end, ProcessEnd::SpawnFailed { .. }),
            "end: {:?}",
            outcome.end
        );
        assert!(
            outcome.status().starts_with("failed to spawn"),
            "status: {}",
            outcome.status()
        );
    }

    // --- Negation truth table (invariant #2). ---

    #[test]
    fn negation_inverts_held_and_drifted_only() {
        // exit 0: Held, negated to Drifted.
        assert_eq!(run("true", true).verdict, Verdict::Drifted);
        // exit 1: Drifted, negated to Held.
        assert_eq!(run("false", true).verdict, Verdict::Held);
    }

    #[test]
    fn negation_never_turns_broken_into_a_pass() {
        // The critical anti-vacuous-pass case: a negate:true check whose binary is
        // missing exits 127 → Broken, and negation must NOT invert it to Held. If
        // this ever returns Held, the whole product is compromised.
        let outcome = run("this-binary-does-not-exist-anywhere", true);
        assert_eq!(
            outcome.verdict,
            Verdict::Broken,
            "a negate check with a missing binary must stay Broken, never a pass"
        );
    }

    #[test]
    fn negation_never_turns_a_bad_exit_into_a_pass() {
        // exit 2 under negation stays Broken, not Held.
        assert_eq!(run("exit 2", true).verdict, Verdict::Broken);
    }

    #[test]
    fn negation_never_turns_signal_death_into_a_pass() {
        assert_eq!(run("kill -9 $$", true).verdict, Verdict::Broken);
    }

    #[test]
    fn negation_never_turns_a_spawn_failure_into_a_pass() {
        let ctx = CheckContext::new("/no/such/directory/anywhere/xyz");
        let outcome = run_check(&cmd("true", true), &ctx);
        assert_eq!(outcome.verdict, Verdict::Broken);
    }

    /// The full negation truth table over the classifier, exhaustively.
    #[test]
    fn apply_negation_truth_table() {
        // Without negation, every verdict is itself.
        assert_eq!(apply_negation(Verdict::Held, false), Verdict::Held);
        assert_eq!(apply_negation(Verdict::Drifted, false), Verdict::Drifted);
        assert_eq!(apply_negation(Verdict::Broken, false), Verdict::Broken);
        assert_eq!(
            apply_negation(Verdict::Unverifiable, false),
            Verdict::Unverifiable
        );
        // With negation, only Held/Drifted swap.
        assert_eq!(apply_negation(Verdict::Held, true), Verdict::Drifted);
        assert_eq!(apply_negation(Verdict::Drifted, true), Verdict::Held);
        assert_eq!(apply_negation(Verdict::Broken, true), Verdict::Broken);
        assert_eq!(
            apply_negation(Verdict::Unverifiable, true),
            Verdict::Unverifiable
        );
    }

    // --- Shell features work, and the cwd is honored. ---

    #[test]
    fn shell_pipes_and_quoting_work() {
        // The run string is interpreted by a shell, so a pipe and a quoted grep
        // behave as an author wrote them.
        assert_eq!(
            run("echo libfoo==4.2 | grep -q 'libfoo==4.2'", false).verdict,
            Verdict::Held
        );
        assert_eq!(
            run("echo libfoo==5.0 | grep -q 'libfoo==4.2'", false).verdict,
            Verdict::Drifted
        );
    }

    #[test]
    fn command_runs_in_the_context_cwd() {
        // A check's paths are relative to the context cwd; a file present there is
        // found, proving the working directory is honored.
        let tmp = tempdir();
        std::fs::write(tmp.path().join("marker.txt"), b"present").unwrap();
        let outcome = run_check(
            &cmd("test -f marker.txt", false),
            &CheckContext::new(tmp.path()),
        );
        assert_eq!(outcome.verdict, Verdict::Held);
    }

    // --- Timeout and no orphans (spec item 4). ---

    #[test]
    fn a_command_past_the_timeout_is_broken() {
        let tmp = tempdir();
        let mut ctx = CheckContext::new(tmp.path());
        ctx.timeout = Duration::from_millis(200);
        let outcome = run_check(&cmd("sleep 30", false), &ctx);
        assert_eq!(outcome.verdict, Verdict::Broken);
        assert!(
            matches!(outcome.end, ProcessEnd::TimedOut { .. }),
            "end: {:?}",
            outcome.end
        );
        // Sub-second budgets render in milliseconds, not a misleading "0s".
        assert_eq!(outcome.status(), "timed out after 200ms");
        // It timed out fast, not after the full sleep.
        assert!(
            outcome.duration < Duration::from_secs(5),
            "should have been killed promptly, took {:?}",
            outcome.duration
        );
    }

    #[test]
    fn timeout_kills_the_whole_group_leaving_no_grandchild() {
        // The load-bearing no-orphan test. The shell backgrounds a grandchild that
        // sleeps, then writes a sentinel file. On timeout we kill the process
        // *group*, so the grandchild dies before it can write. If we killed only
        // the shell, the grandchild would survive and the file would appear.
        let tmp = tempdir();
        let sentinel = tmp.path().join("grandchild-survived.txt");
        // `sh -c` (the shell) backgrounds a subshell that sleeps then writes. The
        // outer shell then waits, so it stays alive past the timeout and is the
        // thing we time out and kill.
        let run = format!(
            "( sleep 3; echo alive > '{}' ) & sleep 30",
            sentinel.display()
        );
        let mut ctx = CheckContext::new(tmp.path());
        ctx.timeout = Duration::from_millis(300);
        let outcome = run_check(&cmd(&run, false), &ctx);
        assert_eq!(outcome.verdict, Verdict::Broken);

        // Wait well past the grandchild's own sleep. If the group kill worked, the
        // grandchild is dead and the sentinel never appears.
        std::thread::sleep(Duration::from_secs(5));
        assert!(
            !sentinel.exists(),
            "the grandchild survived the timeout and wrote {}; the process group \
             was not killed",
            sentinel.display()
        );
    }

    #[test]
    fn collect_output_is_bounded_when_a_reader_never_reaches_eof() {
        // M1 (liveness) at the unit level, OS-quirk-free: a reader whose pipe is
        // held open by a process that outlived the check (an escapee the group-kill
        // cannot reach) never reaches EOF. `collect_output` must still return in
        // roughly the reader grace by detaching that reader — never blocking on an
        // unbounded join, which is what made the check's timeout fail to bound
        // `run_check`. A held-open real pipe (a `sleep` we deliberately do not kill
        // here) is the escapee stand-in.
        //
        // Testing `collect_output` directly, rather than through a shell trick that
        // manufactures a true session escape, keeps this deterministic across
        // platforms whose non-interactive shells differ on whether a backgrounded
        // job can `setsid()`.
        let mut sleeper = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = sleeper.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let reader = spawn_reader(stdout, DEFAULT_OUTPUT_CAP, tx);

        let started = Instant::now();
        let (stdout_captured, _stderr) = collect_output(Some(reader), None, &rx);
        let elapsed = started.elapsed();

        // Bounded by the grace, not the sleeper's 30s lifetime.
        assert!(
            elapsed < READER_JOIN_GRACE + Duration::from_secs(2),
            "collect_output must be bounded by the reader grace ({READER_JOIN_GRACE:?}); \
             took {elapsed:?}"
        );
        // The unfinished reader was detached and its capture flagged as an escapee.
        assert!(
            stdout_captured.escapee,
            "a detached reader's capture must be marked as an escapee"
        );

        // Clean up the sleeper so it does not linger past the test.
        let _ = sleeper.kill();
        let _ = sleeper.wait();
    }

    #[test]
    fn escapee_capture_is_noted_in_the_evidence() {
        // The escapee marker must surface to a human in the evidence, so a partial
        // capture is never mistaken for the whole output.
        let escapee = Captured {
            bytes: b"partial".to_vec(),
            truncated: false,
            escapee: true,
        };
        let evidence = evidence_from(escapee, DEFAULT_OUTPUT_CAP).unwrap();
        assert!(evidence.contains("partial"));
        assert!(
            evidence.contains("a process outlived the check"),
            "evidence must note the escapee: {evidence}"
        );
    }

    #[test]
    fn a_completed_check_leaves_no_in_group_orphan() {
        // M2: the no-orphan guarantee must hold on the COMPLETION path too, not
        // only the timeout path. The shell exits 0 immediately after backgrounding
        // an in-group grandchild that would write a sentinel after a sleep. Killing
        // the group unconditionally after the shell is reaped kills that grandchild
        // before it can write. Without the unconditional group-kill, the sentinel
        // would appear.
        let tmp = tempdir();
        let sentinel = tmp.path().join("orphan-wrote-this.txt");
        let run = format!(
            "( sleep 3; echo alive > '{}' ) & exit 0",
            sentinel.display()
        );
        let outcome = run_check(&cmd(&run, false), &CheckContext::new(tmp.path()));
        assert_eq!(outcome.verdict, Verdict::Held);

        std::thread::sleep(Duration::from_secs(5));
        assert!(
            !sentinel.exists(),
            "a grandchild of a COMPLETED check survived and wrote {}; the group was \
             not killed on the completion path",
            sentinel.display()
        );
    }

    // --- Empty run is never a vacuous pass (C1, defense-in-depth). ---

    #[test]
    fn empty_run_is_broken_never_held() {
        // The execution-side guard mirroring the parser's rejection: even a `Check`
        // built by some non-parser path with a blank command must be Broken, never
        // a pass, and never Drifted under negation.
        for blank in ["", "   ", "\t\n"] {
            let outcome = run(blank, false);
            assert_eq!(
                outcome.verdict,
                Verdict::Broken,
                "blank run {blank:?} must be Broken"
            );
            assert!(matches!(outcome.end, ProcessEnd::SpawnFailed { .. }));
            // And under negate it stays Broken — a blank run cannot become a pass.
            assert_eq!(run(blank, true).verdict, Verdict::Broken);
        }
    }

    // --- Output capture and truncation (spec item 5). ---

    #[test]
    fn stdout_and_stderr_are_captured() {
        let outcome = run("echo to-out; echo to-err >&2", false);
        let evidence = outcome.evidence.expect("output should be captured");
        assert!(evidence.contains("to-out"), "evidence: {evidence}");
        assert!(evidence.contains("to-err"), "evidence: {evidence}");
    }

    #[test]
    fn no_output_yields_no_evidence() {
        let outcome = run("true", false);
        assert_eq!(outcome.evidence, None);
    }

    #[test]
    fn output_is_truncated_at_the_cap_and_marked() {
        let tmp = tempdir();
        let mut ctx = CheckContext::new(tmp.path());
        ctx.output_cap = 64;
        // Print far more than the cap.
        let outcome = run_check(
            &cmd("for i in $(seq 1 1000); do echo LINE$i; done", false),
            &ctx,
        );
        let evidence = outcome.evidence.expect("should have output");
        assert!(
            evidence.contains("output truncated at 64 bytes"),
            "evidence should note truncation: {evidence}"
        );
        // The retained payload (before the marker line) is bounded by the cap.
        let payload = evidence.split("\n[output truncated").next().unwrap();
        assert!(
            payload.len() <= 64,
            "retained payload {} exceeds cap 64",
            payload.len()
        );
    }

    #[test]
    fn large_output_does_not_deadlock() {
        // A command that writes far more than a pipe buffer (64 KiB on Linux) must
        // not deadlock the wait: the reader threads drain the pipe. Cap is small,
        // so most is discarded, but the process must still complete cleanly.
        let outcome = run(
            "for i in $(seq 1 200000); do echo spammy-line-of-output; done",
            false,
        );
        assert_eq!(outcome.verdict, Verdict::Held);
    }

    #[test]
    fn timeout_retains_output_produced_before_the_kill() {
        // m6: partial output a check emitted before it was timed out must survive
        // into the evidence, so a Broken-by-timeout verdict still shows what the
        // check had said. The command prints a marker, flushes by newline, then
        // hangs past the timeout.
        let tmp = tempdir();
        let mut ctx = CheckContext::new(tmp.path());
        ctx.timeout = Duration::from_millis(400);
        let outcome = run_check(&cmd("echo early-marker; sleep 30", false), &ctx);
        assert!(matches!(outcome.end, ProcessEnd::TimedOut { .. }));
        let evidence = outcome
            .evidence
            .expect("output produced before the kill must be retained");
        assert!(
            evidence.contains("early-marker"),
            "pre-kill output should be in the evidence: {evidence}"
        );
    }

    // --- Non-cmd kinds are never silently passed (invariant #6). ---

    #[test]
    fn agent_check_with_no_runner_is_unverifiable_never_held() {
        // The opt-in guarantee: a context with no runner (the default) does not
        // execute the agent check and does not spawn anything. It is Unverifiable
        // with a note that names the missing config, never Held.
        let check = Check {
            kind: CheckKind::Agent {
                instruction: "look into it".to_owned(),
            },
            when: Trigger::OnChange,
            skip: None,
        };
        let outcome = run_check(&check, &CheckContext::new("."));
        assert_eq!(outcome.verdict, Verdict::Unverifiable);
        match &outcome.end {
            ProcessEnd::NotExecuted { note } => {
                assert!(note.contains("no agent runner"), "note: {note}");
            }
            other => panic!("expected NotExecuted, got {other:?}"),
        }
        let evidence = outcome.evidence.expect("a not-executed note is evidence");
        assert!(evidence.contains("CLAIM_AGENT_CMD"), "evidence: {evidence}");
    }

    #[test]
    fn human_check_is_unverifiable_never_held() {
        let check = Check {
            kind: CheckKind::Human {
                prompt: Some("eyeball it".to_owned()),
            },
            when: Trigger::OnChange,
            skip: None,
        };
        let outcome = run_check(&check, &CheckContext::new("."));
        assert_eq!(outcome.verdict, Verdict::Unverifiable);
        assert!(outcome.evidence.is_some());
    }

    // --- Agent checks (item 12): the honesty mapping under a MOCK runner. ---
    //
    // Every test here runs against a tiny shell script the test writes to a tempdir
    // that reads stdin and emits canned stdout. No real model or paid API is ever
    // invoked, and none can be: the runner is always a local script under our
    // control.

    /// An `agent` check with the given instruction, on the on-change trigger.
    fn agent(instruction: &str) -> Check {
        Check {
            kind: CheckKind::Agent {
                instruction: instruction.to_owned(),
            },
            when: Trigger::OnChange,
            skip: None,
        }
    }

    /// Write an executable mock runner script into `dir` and return an
    /// [`AgentRunner::Argv`] that invokes it. The script's body is the shell after
    /// `#!/bin/sh`, so a test controls exactly what canned output (or misbehavior)
    /// the runner produces.
    fn mock_runner(dir: &Path, body: &str) -> AgentRunner {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("mock-runner.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        AgentRunner::Argv(vec![path.to_string_lossy().into_owned()])
    }

    /// Run an agent check whose runner emits `body` (a shell snippet), in a fresh
    /// temp dir with the given timeout, and return the outcome.
    fn run_agent_with(instruction: &str, runner_body: &str, timeout: Duration) -> CheckOutcome {
        let tmp = tempdir();
        let runner = mock_runner(tmp.path(), runner_body);
        let mut ctx = CheckContext::new(tmp.path());
        ctx.timeout = timeout;
        ctx.agent_runner = Some(runner);
        run_check(&agent(instruction), &ctx)
    }

    #[test]
    fn agent_held_json_maps_to_held_with_evidence_and_citations() {
        let outcome = run_agent_with(
            "is the fix still absent?",
            r#"cat >/dev/null; echo '{"verdict":"held","evidence":"still pinned at 4.2","citations":["CHANGELOG.md","https://example.test/issue/1"]}'"#,
            DEFAULT_TIMEOUT,
        );
        assert_eq!(outcome.verdict, Verdict::Held);
        let evidence = outcome.evidence.expect("held carries evidence");
        assert!(evidence.contains("still pinned at 4.2"), "{evidence}");
        assert!(evidence.contains("CHANGELOG.md"), "{evidence}");
        assert!(
            evidence.contains("https://example.test/issue/1"),
            "{evidence}"
        );
    }

    #[test]
    fn agent_drifted_json_maps_to_drifted() {
        let outcome = run_agent_with(
            "did a fix ship?",
            r#"cat >/dev/null; echo '{"verdict":"drifted","evidence":"5.2 shipped the CJK fix"}'"#,
            DEFAULT_TIMEOUT,
        );
        assert_eq!(outcome.verdict, Verdict::Drifted);
        assert!(outcome
            .evidence
            .unwrap()
            .contains("5.2 shipped the CJK fix"));
    }

    #[test]
    fn agent_unverifiable_json_maps_to_unverifiable() {
        let outcome = run_agent_with(
            "is it fixed?",
            r#"cat >/dev/null; echo '{"verdict":"unverifiable","evidence":"changelog was ambiguous"}'"#,
            DEFAULT_TIMEOUT,
        );
        assert_eq!(outcome.verdict, Verdict::Unverifiable);
        assert!(outcome.evidence.unwrap().contains("ambiguous"));
    }

    #[test]
    fn agent_json_wrapped_in_prose_is_parsed_to_its_verdict() {
        // A model that narrates before answering must still be read: the object is
        // located within the surrounding prose and parsed to its verdict.
        let outcome = run_agent_with(
            "check it",
            r#"cat >/dev/null; printf 'Let me look... I checked the changelog.\nHere is my answer:\n{"verdict":"drifted","evidence":"fixed in 5.2"}\nDone.\n'"#,
            DEFAULT_TIMEOUT,
        );
        assert_eq!(outcome.verdict, Verdict::Drifted);
        assert!(outcome.evidence.unwrap().contains("fixed in 5.2"));
    }

    #[test]
    fn agent_runner_nonzero_exit_is_broken_never_held() {
        // The runner even printed a well-formed held, but it exited non-zero: the
        // output is discarded and the verdict is Broken. A misbehaving runner cannot
        // fake a pass by printing held while failing.
        let outcome = run_agent_with(
            "check it",
            r#"cat >/dev/null; echo '{"verdict":"held","evidence":"trust me"}'; exit 3"#,
            DEFAULT_TIMEOUT,
        );
        assert_eq!(outcome.verdict, Verdict::Broken);
        assert_eq!(outcome.end, ProcessEnd::Exited { code: 3 });
    }

    #[test]
    fn agent_runner_timeout_is_broken_and_leaves_no_orphan() {
        // A runner that hangs past the timeout is Broken, and the group-kill leaves
        // no surviving grandchild — the same no-orphan guarantee the cmd path has.
        let tmp = tempdir();
        let sentinel = tmp.path().join("agent-grandchild-survived.txt");
        // The runner backgrounds a grandchild that would write a sentinel after a
        // sleep, then hangs itself past the timeout.
        let body = format!(
            "( sleep 3; echo alive > '{}' ) & cat >/dev/null; sleep 30",
            sentinel.display()
        );
        let runner = mock_runner(tmp.path(), &body);
        let mut ctx = CheckContext::new(tmp.path());
        ctx.timeout = Duration::from_millis(300);
        ctx.agent_runner = Some(runner);
        let outcome = run_check(&agent("check it"), &ctx);
        assert_eq!(outcome.verdict, Verdict::Broken);
        assert!(
            matches!(outcome.end, ProcessEnd::TimedOut { .. }),
            "end: {:?}",
            outcome.end
        );
        std::thread::sleep(Duration::from_secs(5));
        assert!(
            !sentinel.exists(),
            "the agent runner's grandchild survived the timeout; the group was not killed"
        );
    }

    #[test]
    fn agent_runner_spawn_failure_is_broken() {
        // A runner pointing at a non-existent program cannot spawn: Broken, never a
        // pass.
        let tmp = tempdir();
        let mut ctx = CheckContext::new(tmp.path());
        ctx.agent_runner = Some(AgentRunner::Argv(vec![
            "/no/such/agent-runner/anywhere".to_owned()
        ]));
        let outcome = run_check(&agent("check it"), &ctx);
        assert_eq!(outcome.verdict, Verdict::Broken);
        assert!(matches!(outcome.end, ProcessEnd::SpawnFailed { .. }));
    }

    #[test]
    fn agent_empty_argv_is_broken() {
        let tmp = tempdir();
        let mut ctx = CheckContext::new(tmp.path());
        ctx.agent_runner = Some(AgentRunner::Argv(vec![]));
        let outcome = run_check(&agent("check it"), &ctx);
        assert_eq!(outcome.verdict, Verdict::Broken);
        assert!(matches!(outcome.end, ProcessEnd::SpawnFailed { .. }));
    }

    #[test]
    fn agent_garbage_output_is_broken_never_held() {
        // Exit 0 but non-JSON prose: the runner produced no verdict, so Broken.
        let outcome = run_agent_with(
            "check it",
            "cat >/dev/null; echo 'I could not figure this out, sorry.'",
            DEFAULT_TIMEOUT,
        );
        assert_eq!(outcome.verdict, Verdict::Broken);
        assert_ne!(outcome.verdict, Verdict::Held);
        assert!(outcome.evidence.unwrap().contains("no usable verdict"));
    }

    #[test]
    fn agent_json_missing_verdict_field_is_broken() {
        let outcome = run_agent_with(
            "check it",
            r#"cat >/dev/null; echo '{"evidence":"I looked but omitted the verdict"}'"#,
            DEFAULT_TIMEOUT,
        );
        assert_eq!(outcome.verdict, Verdict::Broken);
        assert_ne!(outcome.verdict, Verdict::Held);
    }

    #[test]
    fn agent_invalid_verdict_value_is_broken_never_held() {
        // `"maybe"` is not one of the three allowed verdicts. It must not be coerced
        // to anything, least of all Held.
        let outcome = run_agent_with(
            "check it",
            r#"cat >/dev/null; echo '{"verdict":"maybe","evidence":"unsure"}'"#,
            DEFAULT_TIMEOUT,
        );
        assert_eq!(outcome.verdict, Verdict::Broken);
        assert_ne!(outcome.verdict, Verdict::Held);
    }

    #[test]
    fn agent_empty_output_is_broken() {
        // Exit 0 with no output at all is not an answer.
        let outcome = run_agent_with("check it", "cat >/dev/null; true", DEFAULT_TIMEOUT);
        assert_eq!(outcome.verdict, Verdict::Broken);
    }

    #[test]
    fn agent_held_with_no_evidence_records_none() {
        // A valid held with no evidence field records `None`, not an empty string,
        // matching the cmd path's no-output convention.
        let outcome = run_agent_with(
            "check it",
            r#"cat >/dev/null; echo '{"verdict":"held"}'"#,
            DEFAULT_TIMEOUT,
        );
        assert_eq!(outcome.verdict, Verdict::Held);
        assert_eq!(outcome.evidence, None);
    }

    #[test]
    fn agent_verdict_as_nonstring_is_broken() {
        // `"verdict": 0` — a number, not one of the strings — must not read as Held
        // (which is exit-code 0's meaning on the cmd path; there is no such crossover
        // here).
        let outcome = run_agent_with(
            "check it",
            r#"cat >/dev/null; echo '{"verdict":0}'"#,
            DEFAULT_TIMEOUT,
        );
        assert_eq!(outcome.verdict, Verdict::Broken);
    }

    #[test]
    fn agent_held_with_evidence_within_the_cap_keeps_a_bounded_note() {
        // A held verdict whose evidence fits within the capture cap parses cleanly
        // and its evidence is retained. `output_cap` is generous enough to hold the
        // whole JSON object; the resulting note is bounded by construction (it is
        // derived from bytes already bounded by the cap). This is the ordinary
        // held-with-a-lot-of-evidence path.
        let tmp = tempdir();
        let runner = mock_runner(
            tmp.path(),
            r#"cat >/dev/null; printf '{"verdict":"held","evidence":"'; for i in $(seq 1 100); do printf 'EVIDENCE-CHUNK-%s ' "$i"; done; printf '"}'"#,
        );
        let mut ctx = CheckContext::new(tmp.path());
        ctx.output_cap = 8 * 1024;
        ctx.agent_runner = Some(runner);
        let outcome = run_check(&agent("check it"), &ctx);
        assert_eq!(outcome.verdict, Verdict::Held);
        let evidence = outcome.evidence.expect("evidence present");
        assert!(evidence.contains("EVIDENCE-CHUNK-1 "), "{evidence:.80}");
        assert!(
            evidence.len() <= ctx.output_cap + 128,
            "note must stay bounded, was {} bytes",
            evidence.len()
        );
    }

    #[test]
    fn agent_flooding_output_past_the_cap_is_broken_never_held() {
        // The honesty guard on a runner that floods stdout: once the captured output
        // is truncated at the cap, the JSON object is cut off and no longer parses,
        // so the verdict is Broken — a runner cannot bury a fake `held` under a
        // megabyte of preamble and have it read as a pass.
        let tmp = tempdir();
        let runner = mock_runner(
            tmp.path(),
            r#"cat >/dev/null; for i in $(seq 1 5000); do printf 'PADDING-LINE-%s\n' "$i"; done; echo '{"verdict":"held","evidence":"buried"}'"#,
        );
        let mut ctx = CheckContext::new(tmp.path());
        ctx.output_cap = 256;
        ctx.agent_runner = Some(runner);
        let outcome = run_check(&agent("check it"), &ctx);
        assert_eq!(outcome.verdict, Verdict::Broken);
        assert_ne!(outcome.verdict, Verdict::Held);
    }

    #[test]
    fn agent_runner_receives_the_prompt_on_stdin() {
        // The instruction and the response directive both reach the runner on stdin.
        // The runner writes what it read to a file the test inspects, proving the
        // prompt was fed and carried both the instruction and the fixed contract.
        let tmp = tempdir();
        let seen = tmp.path().join("prompt-seen.txt");
        let runner = mock_runner(
            tmp.path(),
            &format!(
                "cat > '{}'; echo '{{\"verdict\":\"held\",\"evidence\":\"ok\"}}'",
                seen.display()
            ),
        );
        let mut ctx = CheckContext::new(tmp.path());
        ctx.agent_runner = Some(runner);
        let outcome = run_check(&agent("INSTRUCTION-SENTINEL: is it still true?"), &ctx);
        assert_eq!(outcome.verdict, Verdict::Held);

        let prompt = std::fs::read_to_string(&seen).expect("runner should have received stdin");
        assert!(
            prompt.contains("INSTRUCTION-SENTINEL: is it still true?"),
            "the instruction must reach the runner on stdin: {prompt}"
        );
        // The fixed contract (the response directive) must also be present, so the
        // runner is told the exact schema.
        assert!(
            prompt.contains("\"verdict\""),
            "the response directive must reach the runner: {prompt}"
        );
        assert!(prompt.contains("unverifiable"), "prompt: {prompt}");
    }

    #[test]
    fn agent_shell_runner_form_works() {
        // The AgentRunner::Shell form runs via `sh -c` and is fed the prompt on
        // stdin, just like the argv form.
        let tmp = tempdir();
        let mut ctx = CheckContext::new(tmp.path());
        ctx.agent_runner = Some(AgentRunner::Shell(
            r#"cat >/dev/null; echo '{"verdict":"held","evidence":"shell runner ok"}'"#.to_owned(),
        ));
        let outcome = run_check(&agent("check it"), &ctx);
        assert_eq!(outcome.verdict, Verdict::Held);
        assert!(outcome.evidence.unwrap().contains("shell runner ok"));
    }

    #[test]
    fn agent_empty_shell_runner_is_broken() {
        let tmp = tempdir();
        let mut ctx = CheckContext::new(tmp.path());
        ctx.agent_runner = Some(AgentRunner::Shell("   ".to_owned()));
        let outcome = run_check(&agent("check it"), &ctx);
        assert_eq!(outcome.verdict, Verdict::Broken);
        assert!(matches!(outcome.end, ProcessEnd::SpawnFailed { .. }));
    }

    // --- Agent classification and parsing, unit-level (no spawn). ---

    /// A `Captured` from raw bytes (not truncated, not an escapee), for driving
    /// `classify_agent` in unit tests without spawning.
    fn captured(bytes: &[u8]) -> Captured {
        Captured {
            bytes: bytes.to_vec(),
            truncated: false,
            escapee: false,
        }
    }

    #[test]
    fn classify_agent_maps_clean_exit_and_parsed_verdict() {
        let outcome = classify_agent(
            ProcessEnd::Exited { code: 0 },
            captured(br#"{"verdict":"held","evidence":"ok"}"#),
            Captured::empty(),
            DEFAULT_OUTPUT_CAP,
            Duration::ZERO,
        );
        assert_eq!(outcome.verdict, Verdict::Held);
    }

    #[test]
    fn classify_agent_never_maps_a_nonzero_exit_to_held() {
        // Even with a perfectly-formed held on stdout, a non-zero exit is Broken.
        for code in [1, 2, 42, 127, 137] {
            let outcome = classify_agent(
                ProcessEnd::Exited { code },
                captured(br#"{"verdict":"held","evidence":"ok"}"#),
                Captured::empty(),
                DEFAULT_OUTPUT_CAP,
                Duration::ZERO,
            );
            assert_eq!(
                outcome.verdict,
                Verdict::Broken,
                "exit {code} with a held body must be Broken"
            );
        }
    }

    #[test]
    fn classify_agent_maps_signal_and_timeout_to_broken() {
        for end in [
            ProcessEnd::Signalled,
            ProcessEnd::TimedOut {
                after: Duration::from_secs(1),
            },
            ProcessEnd::SpawnFailed {
                reason: "x".to_owned(),
            },
        ] {
            let outcome = classify_agent(
                end.clone(),
                captured(br#"{"verdict":"held"}"#),
                Captured::empty(),
                DEFAULT_OUTPUT_CAP,
                Duration::ZERO,
            );
            assert_eq!(
                outcome.verdict,
                Verdict::Broken,
                "end {end:?} must be Broken"
            );
        }
    }

    #[test]
    fn classify_agent_parses_stdout_only_never_a_stderr_decoy() {
        // Finding #2: a decoy verdict on stderr must not be read as the answer.
        // stdout carries no parseable verdict; stderr carries a well-formed held.
        // The result is Broken (stdout had no verdict), never Held.
        let outcome = classify_agent(
            ProcessEnd::Exited { code: 0 },
            captured(b"working on it...\n"),
            captured(br#"{"verdict":"held","evidence":"decoy on stderr"}"#),
            DEFAULT_OUTPUT_CAP,
            Duration::ZERO,
        );
        assert_eq!(outcome.verdict, Verdict::Broken);
        assert_ne!(outcome.verdict, Verdict::Held);
    }

    #[test]
    fn parse_agent_response_reads_the_three_verdicts() {
        assert_eq!(
            parse_agent_response(r#"{"verdict":"held"}"#)
                .unwrap()
                .verdict,
            Verdict::Held
        );
        assert_eq!(
            parse_agent_response(r#"{"verdict":"drifted"}"#)
                .unwrap()
                .verdict,
            Verdict::Drifted
        );
        assert_eq!(
            parse_agent_response(r#"{"verdict":"unverifiable"}"#)
                .unwrap()
                .verdict,
            Verdict::Unverifiable
        );
    }

    #[test]
    fn parse_agent_response_rejects_malformed() {
        assert!(parse_agent_response("not json at all").is_err());
        assert!(parse_agent_response("").is_err());
        assert!(parse_agent_response("{}").is_err());
        assert!(parse_agent_response(r#"{"evidence":"no verdict"}"#).is_err());
        assert!(parse_agent_response(r#"{"verdict":"maybe"}"#).is_err());
        assert!(parse_agent_response(r#"{"verdict":42}"#).is_err());
        assert!(parse_agent_response(r#"["verdict","held"]"#).is_err());
    }

    #[test]
    fn parse_agent_response_skips_non_verdict_objects_and_reads_the_sole_verdict() {
        // Non-verdict objects (a stray `{}`, an aside) are skipped; with exactly one
        // verdict-bearing object anywhere in the output, that verdict is read. Its
        // position (here, last) is irrelevant — the invariant is "exactly one distinct
        // verdict", not "first" or "last". The conflict tests below prove position
        // does not decide a winner when two verdicts disagree.
        let sole_first =
            r#"{"verdict":"drifted","evidence":"found"} then aside {} and {"note":"x"}"#;
        let response = parse_agent_response(sole_first).unwrap();
        assert_eq!(response.verdict, Verdict::Drifted);
        assert_eq!(response.evidence, "found");

        let sole_last =
            r#"warmup {} then {"note":"aside"} and finally {"verdict":"held","evidence":"f"}"#;
        assert_eq!(
            parse_agent_response(sole_last).unwrap().verdict,
            Verdict::Held
        );
    }

    #[test]
    fn parse_agent_response_repeated_same_verdict_is_consistent() {
        // The same verdict emitted twice (a model that restated its answer) is not a
        // conflict; it resolves to that verdict, keeping the first object's evidence.
        let raw =
            r#"{"verdict":"held","evidence":"first"} ... {"verdict":"held","evidence":"again"}"#;
        let response = parse_agent_response(raw).unwrap();
        assert_eq!(response.verdict, Verdict::Held);
        assert_eq!(response.evidence, "first");
    }

    // --- C1: a narrating/conflicted runner never resolves to a chosen pass. ---

    #[test]
    fn parse_agent_response_conflicting_held_then_drifted_is_broken() {
        // The Critical case: a model reasons out loud, emitting a tentative held then
        // its corrected drifted. Neither "first" nor "last" may win — conflicting
        // verdicts are an error (→ Broken), never a chosen pass.
        let raw = r#"Thinking: {"verdict":"held"} ... on reflection {"verdict":"drifted"}"#;
        let err = parse_agent_response(raw).unwrap_err();
        assert!(err.contains("conflicting"), "reason: {err}");
    }

    #[test]
    fn parse_agent_response_conflicting_held_then_unverifiable_is_broken() {
        let raw =
            r#"{"verdict":"held","evidence":"a"} then {"verdict":"unverifiable","evidence":"b"}"#;
        assert!(parse_agent_response(raw)
            .unwrap_err()
            .contains("conflicting"));
    }

    #[test]
    fn parse_agent_response_conflicting_order_independent() {
        // The reverse order is equally Broken: the tool does not prefer whichever
        // verdict appears first (or last).
        let drift_first = r#"{"verdict":"drifted"} ... {"verdict":"held"}"#;
        assert!(parse_agent_response(drift_first).is_err());
        let held_first = r#"{"verdict":"held"} ... {"verdict":"drifted"}"#;
        assert!(parse_agent_response(held_first).is_err());
    }

    #[test]
    fn parse_agent_response_duplicate_verdict_key_is_broken() {
        // M1: serde silently keeps the last of duplicate keys, so
        // `{"verdict":"drifted","verdict":"held"}` would deserialize to held. Detect
        // the duplicate key in the raw span and treat it as ambiguous → Broken, never
        // resolved toward a value.
        let raw = r#"{"verdict":"drifted","verdict":"held"}"#;
        let err = parse_agent_response(raw).unwrap_err();
        assert!(err.contains("more than one 'verdict' key"), "reason: {err}");

        // The reverse spelling is equally Broken; the value order does not matter.
        assert!(parse_agent_response(r#"{"verdict":"held","verdict":"drifted"}"#).is_err());
    }

    #[test]
    fn duplicate_verdict_key_ignores_nested_and_string_occurrences() {
        // Only a *top-level* duplicate `verdict` key is a conflict. A single top-level
        // key is fine even when the word "verdict" appears inside a value string or a
        // nested object.
        assert!(!duplicate_verdict_key(
            r#"{"verdict":"held","evidence":"the verdict is in"}"#
        ));
        assert!(!duplicate_verdict_key(
            r#"{"verdict":"held","meta":{"verdict":"nested"}}"#
        ));
        assert!(duplicate_verdict_key(
            r#"{"verdict":"drifted","verdict":"held"}"#
        ));
    }

    #[test]
    fn parse_agent_response_single_clean_object_reads_its_verdict() {
        // A single clean object anywhere resolves to its verdict — the ordinary case
        // the conflict rule must not regress.
        let response = parse_agent_response(r#"Here: {"verdict":"held","evidence":"ok"}"#).unwrap();
        assert_eq!(response.verdict, Verdict::Held);
    }

    #[test]
    fn parse_agent_response_nested_verdict_in_metadata_is_not_a_conflict() {
        // A `verdict` nested inside a value object is metadata, not a second answer:
        // the object locator advances past the whole outer object (so the nested one
        // is never a separate top-level candidate) and the duplicate-key check counts
        // only top-level keys. The top-level verdict decides, unambiguously.
        let raw = r#"{"verdict":"held","meta":{"verdict":"drifted","note":"prior draft"}}"#;
        let response = parse_agent_response(raw).unwrap();
        assert_eq!(response.verdict, Verdict::Held);
    }

    #[test]
    fn parse_agent_response_handles_braces_inside_strings() {
        // A brace inside an evidence string must not desynchronize the object
        // locator.
        let raw = r#"{"verdict":"drifted","evidence":"the config had {nested} braces"}"#;
        let response = parse_agent_response(raw).unwrap();
        assert_eq!(response.verdict, Verdict::Drifted);
        assert!(response.evidence.contains("{nested}"));
    }

    #[test]
    fn agent_prompt_carries_the_instruction_and_the_contract() {
        let prompt = build_agent_prompt("Is libfoo still pinned at 4.2?");
        assert!(prompt.contains("Is libfoo still pinned at 4.2?"));
        // Every field name the parser reads and every verdict value it accepts must
        // appear, so a rename of any of them in the directive is caught here rather
        // than silently telling the runner one schema while judging it by another.
        for token in [
            "verdict",
            "evidence",
            "citations",
            "held",
            "drifted",
            "unverifiable",
        ] {
            assert!(prompt.contains(token), "prompt must mention '{token}'");
        }
        // The directive must instruct exactly-one-object and forbid drafts, the
        // source-level defense against the conflicting-verdict case.
        assert!(prompt.contains("exactly one JSON object"));
    }

    #[test]
    fn cap_evidence_truncates_and_marks() {
        let big = "x".repeat(1000);
        let capped = cap_evidence(big, 100);
        assert!(capped.contains("evidence truncated at 100 bytes"));
        let payload = capped.split("\n[evidence truncated").next().unwrap();
        assert!(payload.len() <= 100);
    }

    #[test]
    fn cap_evidence_leaves_short_notes_untouched() {
        assert_eq!(cap_evidence("short".to_owned(), 100), "short");
    }

    // --- Duration and status descriptions. ---

    #[test]
    fn status_describes_the_exit() {
        assert_eq!(run("exit 0", false).status(), "exit 0");
        assert_eq!(run("exit 1", false).status(), "exit 1");
        assert_eq!(run("exit 42", false).status(), "exit 42");
    }

    // --- Supports resolution (spec item 7). ---

    /// Build `SupportTarget`s from raw strings. `SupportTarget` has no public
    /// constructor, so they are produced the only way a caller can: by parsing a
    /// minimal claim that carries them in its `supports` list.
    fn targets(raw: &[&str]) -> Vec<SupportTarget> {
        let list = raw
            .iter()
            .map(|t| format!("  - \"{t}\""))
            .collect::<Vec<_>>()
            .join("\n");
        let src = format!(
            "---\nid: c\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: 1d\nsupports:\n{list}\n---\nS.\n"
        );
        crate::claim::parse_claim_file("f.md", &src)
            .unwrap()
            .supports
    }

    /// Build a `Vec<ClaimId>` for the `known_claim_ids` argument from raw slugs.
    fn ids(raw: &[&str]) -> Vec<ClaimId> {
        raw.iter().map(|s| s.parse::<ClaimId>().unwrap()).collect()
    }

    #[test]
    fn resolving_file_target_resolves() {
        let tmp = tempdir();
        std::fs::write(tmp.path().join("requirements.txt"), b"libfoo==4.2\n").unwrap();
        let res = resolve_supports(&targets(&["requirements.txt"]), tmp.path(), &[]);
        assert!(res[0].resolved, "{:?}", res[0]);
        assert_eq!(res[0].reason, None);
    }

    #[test]
    fn missing_file_target_is_unresolved_with_reason() {
        // A no-anchor target that is neither a known claim id nor an existing file
        // is unresolved; the reason names both interpretations it failed.
        let tmp = tempdir();
        let res = resolve_supports(&targets(&["requirements.txt"]), tmp.path(), &[]);
        assert!(!res[0].resolved);
        let reason = res[0].reason.as_ref().unwrap();
        assert!(
            reason.contains("neither an existing file nor a claim id"),
            "reason: {reason}"
        );
    }

    #[test]
    fn missing_file_anchor_ref_reports_the_missing_file() {
        // With an anchor the target is unambiguously a decision ref, so a missing
        // file is reported as a missing file, not the generic neither-message.
        let tmp = tempdir();
        let res = resolve_supports(&targets(&["requirements.txt#libfoo"]), tmp.path(), &[]);
        assert!(!res[0].resolved);
        assert!(res[0].reason.as_ref().unwrap().contains("no longer exists"));
    }

    #[test]
    fn namespaced_claim_id_resolves_as_an_id_despite_looking_like_a_path() {
        // A namespaced id (`payments/libfoo-pin`) has a `/` and so is path-shaped,
        // but with no matching file it must still resolve via the id set — not be
        // misclassified as a missing file. This is the disambiguation that keeps a
        // real supports edge from reading as broken.
        let tmp = tempdir();
        let res = resolve_supports(
            &targets(&["payments/libfoo-pin"]),
            tmp.path(),
            &ids(&["payments/libfoo-pin"]),
        );
        assert!(res[0].resolved, "{:?}", res[0]);
    }

    #[test]
    fn namespaced_id_that_is_neither_file_nor_id_is_unresolved() {
        let tmp = tempdir();
        let res = resolve_supports(
            &targets(&["payments/gone"]),
            tmp.path(),
            &ids(&["other/id"]),
        );
        assert!(!res[0].resolved);
        assert!(res[0]
            .reason
            .as_ref()
            .unwrap()
            .contains("neither an existing file nor a claim id"));
    }

    #[test]
    fn anchor_present_resolves() {
        let tmp = tempdir();
        std::fs::write(
            tmp.path().join("requirements.txt"),
            b"# libfoo\nlibfoo==4.2\n",
        )
        .unwrap();
        let res = resolve_supports(&targets(&["requirements.txt#libfoo"]), tmp.path(), &[]);
        assert!(res[0].resolved, "{:?}", res[0]);
    }

    #[test]
    fn anchor_missing_is_unresolved_with_reason() {
        let tmp = tempdir();
        std::fs::write(tmp.path().join("requirements.txt"), b"nothing relevant\n").unwrap();
        let res = resolve_supports(&targets(&["requirements.txt#libfoo"]), tmp.path(), &[]);
        assert!(!res[0].resolved);
        let reason = res[0].reason.as_ref().unwrap();
        assert!(reason.contains("anchor"), "reason: {reason}");
    }

    #[test]
    fn anchor_matches_at_word_boundary_not_as_a_substring() {
        // Anchor-collision fix: a deleted `libfoo` heading must read as unresolved
        // even when the substring `libfoo` survives inside an unrelated identifier
        // (`libfoobar`). A bare `contains` would soft-false-green here.
        let tmp = tempdir();
        std::fs::write(
            tmp.path().join("decisions.md"),
            b"see libfoobar for the other thing\n",
        )
        .unwrap();
        let res = resolve_supports(&targets(&["decisions.md#libfoo"]), tmp.path(), &[]);
        assert!(
            !res[0].resolved,
            "libfoo must not resolve via the substring in libfoobar: {:?}",
            res[0]
        );
    }

    #[test]
    fn anchor_occurs_respects_boundaries() {
        // Direct coverage of the boundary predicate: bounded by punctuation, file
        // edges, and whitespace resolve; a run-on identifier does not.
        assert!(anchor_occurs("## libfoo pin", "libfoo"));
        assert!(anchor_occurs("libfoo", "libfoo"));
        assert!(anchor_occurs("(libfoo)", "libfoo"));
        assert!(anchor_occurs("id: libfoo\n", "libfoo"));
        assert!(!anchor_occurs("libfoobar", "libfoo"));
        assert!(!anchor_occurs("prefixlibfoo", "libfoo"));
        assert!(!anchor_occurs("", "libfoo"));
        assert!(!anchor_occurs("anything", ""));
    }

    #[test]
    fn resolving_bare_claim_id_resolves() {
        let tmp = tempdir();
        let res = resolve_supports(
            &targets(&["other-claim"]),
            tmp.path(),
            &ids(&["other-claim"]),
        );
        assert!(res[0].resolved, "{:?}", res[0]);
    }

    #[test]
    fn missing_bare_claim_id_is_unresolved_with_reason() {
        let tmp = tempdir();
        let res = resolve_supports(
            &targets(&["other-claim"]),
            tmp.path(),
            &ids(&["some-other-id"]),
        );
        assert!(!res[0].resolved);
        let reason = res[0].reason.as_ref().unwrap();
        assert!(reason.contains("other-claim"), "reason: {reason}");
    }

    #[test]
    fn extensionless_existing_file_resolves_as_a_path_not_an_id() {
        // A target like `LICENSE` with no `/` or `.` must still resolve as a path
        // when the file exists, not be mistaken for a claim id.
        let tmp = tempdir();
        std::fs::write(tmp.path().join("LICENSE"), b"Apache-2.0\n").unwrap();
        let res = resolve_supports(&targets(&["LICENSE"]), tmp.path(), &[]);
        assert!(res[0].resolved, "{:?}", res[0]);
    }

    #[test]
    fn multiple_targets_resolve_independently() {
        let tmp = tempdir();
        std::fs::write(tmp.path().join("here.txt"), b"x\n").unwrap();
        let res = resolve_supports(
            &targets(&["here.txt", "gone.txt", "known-id"]),
            tmp.path(),
            &ids(&["known-id"]),
        );
        assert!(res[0].resolved);
        assert!(!res[1].resolved);
        assert!(res[2].resolved);
    }

    // --- classify_exit unit coverage, independent of the shell. ---

    #[test]
    fn classify_exit_is_total_and_correct() {
        assert_eq!(
            classify_exit(&ProcessEnd::Exited { code: 0 }),
            Verdict::Held
        );
        assert_eq!(
            classify_exit(&ProcessEnd::Exited { code: 1 }),
            Verdict::Drifted
        );
        assert_eq!(
            classify_exit(&ProcessEnd::Exited { code: 2 }),
            Verdict::Broken
        );
        assert_eq!(
            classify_exit(&ProcessEnd::Exited { code: 127 }),
            Verdict::Broken
        );
        assert_eq!(
            classify_exit(&ProcessEnd::Exited { code: -1 }),
            Verdict::Broken
        );
        assert_eq!(classify_exit(&ProcessEnd::Signalled), Verdict::Broken);
        assert_eq!(
            classify_exit(&ProcessEnd::TimedOut {
                after: Duration::from_secs(1)
            }),
            Verdict::Broken
        );
        assert_eq!(
            classify_exit(&ProcessEnd::SpawnFailed {
                reason: "x".to_owned()
            }),
            Verdict::Broken
        );
        assert_eq!(
            classify_exit(&ProcessEnd::NotExecuted {
                note: "x".to_owned()
            }),
            Verdict::Broken
        );
    }

    // --- Minimal temp-directory helper, to avoid a dev-dep for the unit tests. ---

    /// A throwaway directory removed on drop. `tempfile` is a workspace test dep
    /// for CLI tests, but the core unit tests keep their own tiny helper so this
    /// module has no new dependency and each test gets an isolated cwd.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn tempdir() -> TempDir {
        let mut base = std::env::temp_dir();
        // A unique-enough name without pulling in a rng: pid plus a per-process
        // atomic counter is unique across concurrent tests in one process and
        // across processes.
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        base.push(format!("claim-check-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        TempDir { path: base }
    }
}

/// The skip-decision honesty rules: a skip must never become a silent, permanent
/// mute, so every negative path here (broken condition, expired bound) resolves to
/// *running* the check, never to a quiet skip.
#[cfg(test)]
mod skip_eval_tests {
    use super::*;
    use crate::claim::Skip;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn skip(reason: &str, unless: Option<&str>, until: Option<&str>) -> Skip {
        Skip {
            reason: reason.to_owned(),
            unless: unless.map(ToOwned::to_owned),
            until: until.map(ts),
        }
    }

    const NOW: &str = "2026-01-01T00:00:00Z";

    #[test]
    fn an_unconditional_skip_holds() {
        let s = skip("parked", None, None);
        assert_eq!(
            evaluate_skip(&s, &CheckContext::new("."), ts(NOW)),
            SkipDecision::Skip
        );
    }

    #[test]
    fn unless_exit_zero_cancels_the_skip_and_runs() {
        // The condition holds (`true` exits 0): this environment can verify, so run.
        let s = skip("parked", Some("true"), None);
        assert_eq!(
            evaluate_skip(&s, &CheckContext::new("."), ts(NOW)),
            SkipDecision::Run(None)
        );
    }

    #[test]
    fn unless_exit_one_leaves_the_skip_in_force() {
        // The condition does not hold (`false` exits 1): the skip stands.
        let s = skip("parked", Some("false"), None);
        assert_eq!(
            evaluate_skip(&s, &CheckContext::new("."), ts(NOW)),
            SkipDecision::Skip
        );
    }

    #[test]
    fn a_broken_unless_runs_the_check_never_silently_mutes() {
        // A condition that is neither exit 0 nor 1 cannot be evaluated; the check must
        // run rather than be silently muted — a broken mute is how drift hides.
        let s = skip("parked", Some("exit 3"), None);
        assert!(matches!(
            evaluate_skip(&s, &CheckContext::new("."), ts(NOW)),
            SkipDecision::Run(Some(_))
        ));
    }

    #[test]
    fn an_expired_until_runs_regardless_of_a_still_true_unless() {
        // `until` is in the past: the debt is called even though `false` would keep
        // the skip in force. The expiry wins, and the run carries a lapse note.
        let s = skip("parked", Some("false"), Some("2025-01-01T00:00:00Z"));
        assert!(matches!(
            evaluate_skip(&s, &CheckContext::new("."), ts(NOW)),
            SkipDecision::Run(Some(_))
        ));
    }

    #[test]
    fn an_unexpired_until_still_skips() {
        let s = skip("parked", None, Some("2027-01-01T00:00:00Z"));
        assert_eq!(
            evaluate_skip(&s, &CheckContext::new("."), ts(NOW)),
            SkipDecision::Skip
        );
    }
}
