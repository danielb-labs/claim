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
//! - **#6, the failure mode is a nag.** An `agent` or `human` check is not
//!   silently treated as passing: [`run_check`] returns [`Verdict::Unverifiable`]
//!   with a note that it needs a lane not built in v1, so the claim ages toward a
//!   human instead of faking freshness.
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

use crate::claim::{Check, CheckKind, SupportTarget};
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
    /// [`Verdict::Broken`] (a spawn failure), never a pass.
    pub cwd: PathBuf,
    /// The wall-clock budget. On expiry the check's process group is killed and
    /// the verdict is [`Verdict::Broken`]. See [`DEFAULT_TIMEOUT`].
    pub timeout: Duration,
    /// The maximum number of bytes of combined stdout+stderr to retain as
    /// evidence. Output past this is dropped and the evidence notes truncation.
    /// See [`DEFAULT_OUTPUT_CAP`].
    pub output_cap: usize,
}

impl CheckContext {
    /// A context rooted at `cwd` with the default timeout and output cap.
    ///
    /// The one field with no sensible default is where the check runs, so it is
    /// the only argument; [`timeout`](CheckContext::timeout) and
    /// [`output_cap`](CheckContext::output_cap) can be overwritten after
    /// construction.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        CheckContext {
            cwd: cwd.into(),
            timeout: DEFAULT_TIMEOUT,
            output_cap: DEFAULT_OUTPUT_CAP,
        }
    }
}

/// How a check's process ended, before negation is applied.
///
/// A closed set of terminal outcomes so [`classify_exit`] is a total `match` with
/// no catch-all that could accidentally map an unexpected case to a pass. Only
/// [`Exited`](RunResult::Exited) carries a code that can become `Held`; every
/// other variant is unconditionally [`Verdict::Broken`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum RunResult {
    /// The process ran to completion and returned an exit code. On unix this is
    /// present only when the process was *not* killed by a signal.
    Exited(i32),
    /// The process was terminated by a signal (e.g. `SIGKILL`, `SIGSEGV`) and so
    /// has no exit code of its own. Always `Broken`: a check that was killed did
    /// not deliberately report anything.
    Signalled,
    /// We killed the process because it exceeded the timeout. Always `Broken`.
    TimedOut,
    /// The process could never start — a missing shell, a non-existent working
    /// directory, an exhausted process table. Always `Broken`.
    SpawnFailed(String),
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
    /// A short, human-readable description of how the process ended — `exit 0`,
    /// `exit 127`, `killed by signal`, `timed out after 60s`, or the spawn error.
    /// Recorded so a `Broken` verdict in the log says *why* it broke without a
    /// caller re-deriving it.
    pub status: String,
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

/// Run one check and classify its outcome into a [`CheckOutcome`].
///
/// Total by construction: it never returns an error. Every way a check can fail
/// to produce a clean answer — a missing binary, a bad working directory, a
/// signal, a timeout — resolves to a [`Verdict::Broken`] outcome, because a
/// caller that had to handle a `Result` here could forget the error arm and let a
/// broken check read as anything other than broken (invariant #1).
///
/// Only [`CheckKind::Cmd`] is executed. An [`CheckKind::Agent`] or
/// [`CheckKind::Human`] check returns [`Verdict::Unverifiable`] with a note that
/// it needs a lane not built in v1: it is *not* silently passed, so the claim
/// ages toward a human (invariant #6). The exhaustive `match` on [`CheckKind`]
/// means a future kind cannot be added without a decision here being forced.
///
/// This is the primitive `claim add`'s witnessed-red workflow (item 4) will call
/// twice: once against the true state, expecting [`Verdict::Held`], and once
/// against a perturbed state, expecting [`Verdict::Drifted`], to prove the check
/// can actually go red before it is trusted. The signature is kept convenient for
/// that — a borrowed check and context in, a plain outcome out, no I/O setup the
/// caller must thread through.
#[must_use]
pub fn run_check(check: &Check, ctx: &CheckContext) -> CheckOutcome {
    match &check.kind {
        CheckKind::Cmd { run, negate } => run_cmd(run, *negate, ctx),
        CheckKind::Agent { .. } => CheckOutcome {
            verdict: Verdict::Unverifiable,
            status: "not executed".to_owned(),
            evidence: Some(
                "agent checks are not executed in v1; this claim needs the agent \
                 investigation lane, which is not built yet"
                    .to_owned(),
            ),
            duration: Duration::ZERO,
        },
        CheckKind::Human { .. } => CheckOutcome {
            verdict: Verdict::Unverifiable,
            status: "not executed".to_owned(),
            evidence: Some(
                "human checks are not executed by the tool; this claim needs a \
                 scheduled human look, which is not built yet"
                    .to_owned(),
            ),
            duration: Duration::ZERO,
        },
    }
}

/// Execute a `cmd` check's `run` string and classify the result.
///
/// The command is run through the platform shell so pipes, globs, and quoting in
/// the `run` string behave as an author expects (PRODUCT.md's examples rely on
/// shell features). The shell interprets syntax only; the exit-code-to-verdict
/// mapping and negation are the tool's, per invariants #1 and #2.
fn run_cmd(run: &str, negate: bool, ctx: &CheckContext) -> CheckOutcome {
    let started = Instant::now();
    let (result, output) = execute(run, ctx);
    let duration = started.elapsed();

    let base = classify_exit(&result);
    let verdict = apply_negation(base, negate);
    CheckOutcome {
        verdict,
        status: describe(&result, ctx.timeout),
        evidence: evidence_from(output, ctx.output_cap),
        duration,
    }
}

/// Map a process outcome to a verdict, before negation — the exit-code contract.
///
/// The whole product turns on this being exactly right, so it is a total `match`
/// with no wildcard: a deliberate exit 0 is the *only* path to
/// [`Verdict::Held`], exit 1 is [`Verdict::Drifted`], and everything else — any
/// other exit code (2 for a grep error, 126 for not-executable, 127 for
/// not-found, 130 for Ctrl-C, …), death by signal, a timeout, or a spawn failure
/// — is [`Verdict::Broken`]. A check that could not run cannot report that the
/// fact is fine.
fn classify_exit(result: &RunResult) -> Verdict {
    match result {
        RunResult::Exited(0) => Verdict::Held,
        RunResult::Exited(1) => Verdict::Drifted,
        RunResult::Exited(_)
        | RunResult::Signalled
        | RunResult::TimedOut
        | RunResult::SpawnFailed(_) => Verdict::Broken,
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

/// A one-line description of how the process ended, for the log entry.
fn describe(result: &RunResult, timeout: Duration) -> String {
    match result {
        RunResult::Exited(code) => format!("exit {code}"),
        RunResult::Signalled => "killed by signal".to_owned(),
        RunResult::TimedOut => format!("timed out after {}s", timeout.as_secs()),
        RunResult::SpawnFailed(reason) => format!("failed to spawn: {reason}"),
    }
}

/// Turn captured output into evidence, applying the cap.
///
/// `output` already carries whether it was truncated (the reader stopped at the
/// cap). Empty output yields `None` so the log entry's `evidence` is absent
/// rather than an empty string. Non-UTF-8 bytes are replaced rather than dropped,
/// so binary noise in a command's output cannot lose the readable parts around
/// it.
fn evidence_from(output: Captured, cap: usize) -> Option<String> {
    if output.bytes.is_empty() {
        return None;
    }
    let mut text = String::from_utf8_lossy(&output.bytes).into_owned();
    if output.truncated {
        text.push_str(&format!(
            "\n[output truncated at {cap} bytes; the check produced more]"
        ));
    }
    Some(text)
}

/// stdout and stderr captured up to the cap, and whether more was produced.
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Spawn the shell command, capture bounded output, and enforce the timeout.
///
/// Returns the terminal [`RunResult`] and the captured output. This is the only
/// function that touches the process; all judgement is downstream of it, so the
/// honesty rules stay readable and testable apart from the I/O.
#[cfg(unix)]
fn execute(run: &str, ctx: &CheckContext) -> (RunResult, Captured) {
    use std::os::unix::process::CommandExt;
    use std::os::unix::process::ExitStatusExt;
    use wait_timeout::ChildExt;

    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(run)
        .current_dir(&ctx.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Put the shell in its own process group (pgid == the shell's pid) so a
    // command that spawns children — `sleep 100 | foo` — puts them in the same
    // group. On timeout we kill the *group*, so no grandchild is orphaned. Set on
    // the child only; passing 0 makes the child its own leader without disturbing
    // the tool's own group.
    command.process_group(0);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return (RunResult::SpawnFailed(e.to_string()), Captured::empty()),
    };
    let pgid = child.id() as libc::pid_t;

    // Drain stdout and stderr on their own threads. A command that writes more
    // than a pipe buffer holds would otherwise block on the write while we block
    // on the wait — a deadlock that no timeout could break, because the process
    // never reaches the point where it could be reaped. The readers also enforce
    // the cap, so a flood of output is bounded in memory.
    let cap = ctx.output_cap;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_reader = stdout.map(|s| spawn_reader(s, cap));
    let err_reader = stderr.map(|s| spawn_reader(s, cap));

    let result = match child.wait_timeout(ctx.timeout) {
        Ok(Some(status)) => {
            // `code()` is `None` exactly when a signal killed the process; that is
            // never a deliberate answer, so it is `Broken` via `Signalled`.
            match status.code() {
                Some(code) => RunResult::Exited(code),
                None => {
                    debug_assert!(
                        status.signal().is_some(),
                        "a unix ExitStatus with no code must carry a signal"
                    );
                    RunResult::Signalled
                }
            }
        }
        Ok(None) => {
            // The deadline passed and the child is still running. Kill the whole
            // group so children the shell spawned die too, then block until the
            // shell is reaped so it is not left a zombie and its threads finish.
            kill_group(pgid);
            let _ = child.wait();
            RunResult::TimedOut
        }
        Err(e) => {
            // Waiting itself failed (an OS-level fault). Treat it as unrunnable —
            // Broken, never a pass — and still make an effort to reap the child.
            kill_group(pgid);
            let _ = child.wait();
            RunResult::SpawnFailed(format!("waiting for the check failed: {e}"))
        }
    };

    // Join the readers now that the process has ended and both pipe ends are
    // closed, so the reads return EOF. Combine stdout then stderr into one buffer,
    // re-capped in case both streams were near the cap.
    let mut captured = out_reader.map_or_else(Captured::empty, join_reader);
    if let Some(err) = err_reader {
        captured.extend(join_reader(err), cap);
    }
    (result, captured)
}

/// Kill an entire process group by its leader's pid.
///
/// A timed-out check's shell and every child it spawned share this group (see
/// [`execute`]'s `process_group(0)`), so signalling the group — not just the
/// shell — is what prevents an orphaned grandchild outliving the tool. `SIGKILL`
/// rather than `SIGTERM`: a hung check has already ignored its budget, and a
/// clean shutdown it might trap is not owed to it. A failure (the group already
/// gone) is ignored; the goal is that nothing survives, and an already-dead group
/// meets it.
#[cfg(unix)]
fn kill_group(pgid: libc::pid_t) {
    // SAFETY: `killpg` is an FFI call with no memory effects; it only delivers a
    // signal. Any error (ESRCH: the group is already gone) is intentionally
    // ignored, since a vanished group is the outcome we want.
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
        }
    }

    /// Append another stream's capture, keeping the combined total within `cap`.
    fn extend(&mut self, other: Captured, cap: usize) {
        self.truncated |= other.truncated;
        let room = cap.saturating_sub(self.bytes.len());
        if other.bytes.len() > room {
            self.truncated = true;
            self.bytes.extend_from_slice(&other.bytes[..room]);
        } else {
            self.bytes.extend_from_slice(&other.bytes);
        }
    }
}

/// Spawn a thread that reads a child stream into a capped buffer.
///
/// Returned as a join handle so the caller collects the bytes after the process
/// ends. Reading on a thread (rather than after `wait`) is what avoids the
/// pipe-buffer deadlock described in [`execute`].
#[cfg(unix)]
fn spawn_reader<R: Read + Send + 'static>(
    reader: R,
    cap: usize,
) -> std::thread::JoinHandle<Captured> {
    std::thread::spawn(move || read_capped(reader, cap))
}

/// Join a reader thread, treating a panicked reader as "no output".
///
/// A reader thread only panics on an allocation failure, which is already an
/// environment fault; losing its (partial) evidence is acceptable and must not
/// bring down the tool, so the panic is swallowed into empty output rather than
/// re-raised.
#[cfg(unix)]
fn join_reader(handle: std::thread::JoinHandle<Captured>) -> Captured {
    handle.join().unwrap_or_else(|_| Captured::empty())
}

/// Read from `reader` until EOF or the cap, whichever comes first.
///
/// Stops reading once `cap` bytes are retained, marking the result truncated. It
/// keeps consuming past the cap in fixed chunks so the writing child sees its
/// pipe drained and can exit rather than block forever on a full pipe — draining
/// without retaining. A read error ends the loop with whatever was gathered; a
/// broken pipe is not itself a check failure.
#[cfg(unix)]
fn read_capped<R: Read>(mut reader: R, cap: usize) -> Captured {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 8 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if bytes.len() < cap {
                    let room = cap - bytes.len();
                    let take = n.min(room);
                    bytes.extend_from_slice(&buf[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    Captured { bytes, truncated }
}

/// Whether a claim's `supports` target still resolves, and why not if it does not.
///
/// Reported per target and kept *separate* from the check [`Verdict`] on purpose:
/// a deleted decision is its own loud condition — "the thing this claim justifies
/// is gone" — not a check failure. Folding it into `Broken` would conflate "the
/// check could not run" with "the decision vanished", and the CLI needs to say
/// which. A claim with an unresolved support goes loud instead of staying quietly
/// green (PRODUCT.md section 4).
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
/// this needs no second store read and stays deterministic and testable.
///
/// Resolution rules, each yielding a reason when it fails:
///
/// - **`path#anchor`**: unambiguously a decision ref, because a claim id can
///   never contain `#` (see [`crate::claim::ClaimId`]). Resolves iff the file
///   exists under `repo_root` *and* the anchor text occurs somewhere in it. The
///   anchor check is a plain substring scan — cheap, and enough to catch a
///   decision heading that was deleted or renamed. It is intentionally not a
///   Markdown-aware anchor match: over-precise matching would produce false
///   "unresolved" alarms on valid files, and the goal is to catch a *deleted*
///   decision, not to police heading syntax.
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
    known_claim_ids: &[&str],
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
fn resolve_one(target: &str, repo_root: &Path, known_claim_ids: &[&str]) -> SupportResolution {
    if let Some((path_part, anchor)) = target.split_once('#') {
        let candidate = resolve_path(repo_root, path_part);
        return resolve_decision_ref(target, &candidate, path_part, Some(anchor));
    }

    // No anchor: resolve as a claim id or as a bare file path, whichever is real.
    if known_claim_ids.contains(&target) {
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
            Ok(contents) if contents.contains(anchor) => resolved(target),
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
        assert_eq!(outcome.status, "exit 127");
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
        assert_eq!(outcome.status, "exit 126");
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
        assert_eq!(outcome.status, "killed by signal");
    }

    #[test]
    fn segfault_signal_is_broken() {
        // A different signal path: SIGSEGV rather than SIGKILL, to prove the
        // classification is on "no exit code", not on a specific signal.
        let outcome = run("kill -SEGV $$", false);
        assert_eq!(outcome.verdict, Verdict::Broken);
        assert_eq!(outcome.status, "killed by signal");
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
            outcome.status.starts_with("failed to spawn"),
            "status: {}",
            outcome.status
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
            outcome.status.starts_with("timed out"),
            "status: {}",
            outcome.status
        );
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

    // --- Non-cmd kinds are never silently passed (invariant #6). ---

    #[test]
    fn agent_check_is_unverifiable_never_held() {
        let check = Check {
            kind: CheckKind::Agent {
                instruction: "look into it".to_owned(),
            },
            when: Trigger::OnChange,
        };
        let outcome = run_check(&check, &CheckContext::new("."));
        assert_eq!(outcome.verdict, Verdict::Unverifiable);
        assert_ne!(outcome.verdict, Verdict::Held);
        assert!(outcome.evidence.is_some());
    }

    #[test]
    fn human_check_is_unverifiable_never_held() {
        let check = Check {
            kind: CheckKind::Human {
                prompt: Some("eyeball it".to_owned()),
            },
            when: Trigger::OnChange,
        };
        let outcome = run_check(&check, &CheckContext::new("."));
        assert_eq!(outcome.verdict, Verdict::Unverifiable);
        assert!(outcome.evidence.is_some());
    }

    // --- Duration and status descriptions. ---

    #[test]
    fn status_describes_the_exit() {
        assert_eq!(run("exit 0", false).status, "exit 0");
        assert_eq!(run("exit 1", false).status, "exit 1");
        assert_eq!(run("exit 42", false).status, "exit 42");
    }

    // --- Supports resolution (spec item 7). ---

    /// Wrap raw strings as `SupportTarget`s via a claim parse round-trip is
    /// overkill; construct them through the public parser path instead by using
    /// the claim module. Since `SupportTarget` has no public constructor, build
    /// targets by parsing a minimal claim carrying them.
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
            &["payments/libfoo-pin"],
        );
        assert!(res[0].resolved, "{:?}", res[0]);
    }

    #[test]
    fn namespaced_id_that_is_neither_file_nor_id_is_unresolved() {
        let tmp = tempdir();
        let res = resolve_supports(&targets(&["payments/gone"]), tmp.path(), &["other/id"]);
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
    fn resolving_bare_claim_id_resolves() {
        let tmp = tempdir();
        let res = resolve_supports(&targets(&["other-claim"]), tmp.path(), &["other-claim"]);
        assert!(res[0].resolved, "{:?}", res[0]);
    }

    #[test]
    fn missing_bare_claim_id_is_unresolved_with_reason() {
        let tmp = tempdir();
        let res = resolve_supports(&targets(&["other-claim"]), tmp.path(), &["some-other-id"]);
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
            &["known-id"],
        );
        assert!(res[0].resolved);
        assert!(!res[1].resolved);
        assert!(res[2].resolved);
    }

    // --- classify_exit unit coverage, independent of the shell. ---

    #[test]
    fn classify_exit_is_total_and_correct() {
        assert_eq!(classify_exit(&RunResult::Exited(0)), Verdict::Held);
        assert_eq!(classify_exit(&RunResult::Exited(1)), Verdict::Drifted);
        assert_eq!(classify_exit(&RunResult::Exited(2)), Verdict::Broken);
        assert_eq!(classify_exit(&RunResult::Exited(127)), Verdict::Broken);
        assert_eq!(classify_exit(&RunResult::Exited(-1)), Verdict::Broken);
        assert_eq!(classify_exit(&RunResult::Signalled), Verdict::Broken);
        assert_eq!(classify_exit(&RunResult::TimedOut), Verdict::Broken);
        assert_eq!(
            classify_exit(&RunResult::SpawnFailed("x".to_owned())),
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
