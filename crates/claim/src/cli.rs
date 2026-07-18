//! The command-line surface: the `clap` derive types for `claim` and its verbs.
//!
//! This module is *only* the shape of the CLI — the parsed arguments — with no
//! behavior, so the grammar of the tool can be read in one place and each verb's
//! logic lives in [`crate::commands`]. Two conventions are wired here for every
//! verb, present and future:
//!
//! - **A global `--json` flag** ([`Cli::json`]). Agents are the heaviest readers
//!   (PRODUCT.md section 5), so every command owes a stable machine form. The flag
//!   is parsed once at the top level and threaded to each command; `init` and `add`
//!   honor it now, and a later verb inherits the plumbing for free.
//! - **A stub for every unbuilt verb** ([`Command`]). The full v1 verb list is
//!   declared so `claim --help` shows the real surface from the first item and a
//!   later item slots its logic into an existing arm rather than adding one. An
//!   unbuilt verb exits 2 (usage/other error) with a "not implemented" message,
//!   never 0 — an unfinished command must not look like a success.

use clap::{Parser, Subcommand};

/// `claim` binds plain-language facts to executable checks so recorded knowledge
/// cannot silently rot.
#[derive(Debug, Parser)]
#[command(
    name = "claim",
    version,
    about = "Bind plain-language facts to executable checks so knowledge cannot silently rot.",
    // Subcommand is required: bare `claim` prints help and exits, rather than
    // doing something surprising.
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    /// Emit machine-readable JSON instead of human-readable text.
    ///
    /// Global so it can precede or follow the verb (`claim --json add …` or
    /// `claim add … --json`), and so every command shares one output contract.
    #[arg(long, global = true)]
    pub json: bool,

    /// The verb to run.
    #[command(subcommand)]
    pub command: Command,
}

/// The `claim` verbs. The full v1 set (PRODUCT.md section 5) is declared so the
/// help text and dispatch are complete from the start; only the verbs a build item
/// has reached are implemented, the rest are honest stubs.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Scaffold a `.claims/` store in the current directory.
    Init(InitArgs),

    /// Create a claim: run its check now, witness it failing, write the first log
    /// entry.
    Add(AddArgs),

    /// Run checks and report holds/drifted/inconclusive/broken. (Not yet built.)
    Check,
    /// Filter claims by text, path, status, staleness, or supports. (Not yet built.)
    List,
    /// Show a claim's full history and evidence. (Not yet built.)
    Log,
    /// List drifted and due claims with what each supports. (Not yet built.)
    Drift,
    /// Resolve drift by fixing a claim's statement and check. (Not yet built.)
    Amend,
    /// Resolve drift by closing a claim with a note. (Not yet built.)
    Retire,
    /// Pilot metrics: drifts caught, false alarms, minutes per claim. (Not yet built.)
    Stats,
}

impl Command {
    /// The verb's name, for the "not implemented" message of an unbuilt stub.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Command::Init(_) => "init",
            Command::Add(_) => "add",
            Command::Check => "check",
            Command::List => "list",
            Command::Log => "log",
            Command::Drift => "drift",
            Command::Amend => "amend",
            Command::Retire => "retire",
            Command::Stats => "stats",
        }
    }
}

/// Arguments to `claim init`.
#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// The directory to scaffold the store in. Defaults to the current directory.
    ///
    /// A claim store lives at a repository's root by convention; passing a path
    /// lets a script create one elsewhere without `cd`-ing first.
    #[arg(long, value_name = "DIR")]
    pub dir: Option<std::path::PathBuf>,
}

/// Arguments to `claim add`.
///
/// The flags cover the whole claim schema so an agent or script can author a claim
/// non-interactively; when a flag is absent and stdin is a TTY, [`crate::commands::add`]
/// falls back to prompting. The witnessed-red flags ([`AddArgs::witness_cmd`],
/// [`AddArgs::unwitnessed`]) are the mechanized form of invariant #5 — see the
/// command's module docs for the workflow.
#[derive(Debug, clap::Args)]
pub struct AddArgs {
    /// The claim's id: a kebab-case slug, optionally namespaced with `/`
    /// (e.g. `payments/libfoo-pin`). Validated by `claim-core`.
    #[arg(long)]
    pub id: Option<String>,

    /// The plain-language statement — the fact the claim records.
    #[arg(long)]
    pub statement: Option<String>,

    /// The `cmd` check's command line. Runs through the shell; exit 0 means the
    /// fact holds, exit 1 means it drifted (unless `--negate` inverts).
    #[arg(long, value_name = "CMD")]
    pub run: Option<String>,

    /// When the check runs: `on-change` or `every <N>d` (e.g. `every 30d`).
    /// Defaults to `on-change`.
    #[arg(long, value_name = "TRIGGER")]
    pub when: Option<String>,

    /// Invert the check's `Held`/`Drifted` sense (the tool owns the inversion; it
    /// never wraps the command in a shell `!`).
    #[arg(long)]
    pub negate: bool,

    /// The dead-man's switch: how long a passing check keeps the claim fresh, as
    /// `<N>d` (e.g. `120d`). Validated by `claim-core`.
    #[arg(long, value_name = "DAYS")]
    pub max_age: Option<String>,

    /// A `supports` target this claim justifies — a decision ref
    /// (`requirements.txt#libfoo`) or a bare claim id. Repeatable.
    #[arg(long = "supports", value_name = "TARGET")]
    pub supports: Vec<String>,

    /// A command that perturbs the tree so the fact becomes false, for the
    /// scripted witnessed-red flow.
    ///
    /// The default (invariant #5) is to observe the check actually go `Drifted`.
    /// Non-interactively, this supplies that observation: the tool runs the green
    /// check (expecting `Held`), then this command, then re-runs the check
    /// (expecting `Drifted`), then restores the tree and confirms `Held` again. The
    /// red is *observed*, never asserted. Restoration reverts tracked changes with
    /// git unless `--restore-cmd` is given; it never runs `git clean`, so the
    /// untracked store is never at risk.
    #[arg(long, value_name = "CMD", conflicts_with = "unwitnessed")]
    pub witness_cmd: Option<String>,

    /// A command that undoes `--witness-cmd`, restoring the tree to where the fact
    /// holds again.
    ///
    /// Optional. When omitted, the tool reverts *tracked* changes with
    /// `git checkout -- .`. Supply this when the perturbation created untracked
    /// files, or when the repository has no commit yet (an unborn HEAD), where there
    /// is no committed state for git to revert to. Only meaningful with
    /// `--witness-cmd`.
    #[arg(long, value_name = "CMD", requires = "witness_cmd")]
    pub restore_cmd: Option<String>,

    /// Record the claim without a witnessed failure, marking it unverified.
    ///
    /// The visible escape hatch (invariant #5) for a fact whose red genuinely
    /// cannot be staged. The claim is recorded with an evidence note that its check
    /// was never witnessed failing, so `claim list --unverified` (a later verb) can
    /// surface it — it is never silently trusted.
    #[arg(long)]
    pub unwitnessed: bool,
}
