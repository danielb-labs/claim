//! Resolving the opt-in agent-check runner from the environment.
//!
//! An `agent` check runs only when an operator supplies a runner — a shell command
//! fed the verdict prompt on stdin, emitting the verdict JSON on stdout. Both front
//! doors read it from the same `CLAIM_AGENT_CMD` variable, and the blank-value error
//! is a user-facing contract, so the reader lives here once rather than being copied
//! (and allowed to drift) between the CLI's `check` and the MCP server. With the
//! variable unset — the default — no runner is attached, so agent checks are
//! `Unverifiable` and nothing is spawned: a default run never reaches a model.

use claim_core::AgentRunner;

/// The environment variable that opts a run into executing `agent` checks: a shell
/// command receiving the verdict prompt on stdin and printing the verdict JSON on
/// stdout. Unset leaves agent checks unverifiable and spawns nothing.
pub const CLAIM_AGENT_CMD_ENV: &str = "CLAIM_AGENT_CMD";

/// Why the agent runner could not be resolved from the environment. A misconfiguration
/// each front door maps to its own surface (the CLI's `anyhow`, the server's protocol
/// error); the wording is shared so the contract reads the same either way.
#[derive(Debug, thiserror::Error)]
pub enum AgentCmdError {
    /// `CLAIM_AGENT_CMD` is set but blank. Rejected loudly rather than silently
    /// ignored: a run that meant to configure a runner but set it to whitespace must
    /// not quietly fall back to leaving every agent check unverifiable.
    #[error(
        "CLAIM_AGENT_CMD is set but empty; unset it to leave agent checks unverifiable, or set it \
         to a runner command that reads the prompt on stdin and prints the verdict JSON on stdout"
    )]
    Blank,

    /// `CLAIM_AGENT_CMD` is set to bytes that are not valid UTF-8, so it cannot be a
    /// shell command.
    #[error("CLAIM_AGENT_CMD is set to a non-UTF-8 value")]
    NotUnicode,
}

/// Resolve the agent runner from [`CLAIM_AGENT_CMD_ENV`], if set.
///
/// A set value is a [`AgentRunner::Shell`] so an operator can express the runner as a
/// one-liner. A whitespace-only value is [`AgentCmdError::Blank`] and a non-UTF-8 value
/// is [`AgentCmdError::NotUnicode`] — both loud, never a silent fallback. Unset returns
/// `Ok(None)`, the ordinary default where agent checks are unverifiable and nothing is
/// spawned.
///
/// # Errors
///
/// Returns [`AgentCmdError::Blank`] for a whitespace-only value and
/// [`AgentCmdError::NotUnicode`] for a non-UTF-8 value.
pub fn agent_runner_from_env() -> Result<Option<AgentRunner>, AgentCmdError> {
    match std::env::var(CLAIM_AGENT_CMD_ENV) {
        Ok(cmd) if cmd.trim().is_empty() => Err(AgentCmdError::Blank),
        Ok(cmd) => Ok(Some(AgentRunner::Shell(cmd))),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(AgentCmdError::NotUnicode),
    }
}
