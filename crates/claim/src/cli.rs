//! The command-line surface: the `clap` derive types for `claim` and its verbs.
//!
//! This module is *only* the shape of the CLI — the parsed arguments — with no
//! behavior, so the grammar of the tool can be read in one place and each verb's
//! logic lives in [`crate::commands`]. Two conventions are wired here for every
//! verb, present and future:
//!
//! - **A global `--json` flag** ([`Cli::json`]). Agents are the heaviest readers
//!   (PRODUCT.md section 5), so every command owes a stable machine form. The flag
//!   is parsed once at the top level and threaded to each command.
//! - **A stub for every unbuilt verb** ([`Command`]). The full v1 verb list is
//!   declared so `claim --help` shows the real surface from the first item and a
//!   later item slots its logic into an existing arm rather than adding one. An
//!   unbuilt verb (`amend`, `retire`, `stats`) exits 2 (usage/other error) with a
//!   "not implemented" message, never 0 — an unfinished command must not look like
//!   a success.

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
    /// May precede or follow the verb (`claim --json add …` or `claim add … --json`).
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

    /// Run checks and report holds/drifted/unverifiable/broken.
    Check(CheckArgs),
    /// Filter claims by text, path, status, staleness, or supports.
    List(ListArgs),
    /// Show a claim's full history and evidence.
    Log(LogArgs),
    /// List drifted claims with what each supports.
    Drift(DriftArgs),
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
            Command::Check(_) => "check",
            Command::List(_) => "list",
            Command::Log(_) => "log",
            Command::Drift(_) => "drift",
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
    /// (e.g. `payments/libfoo-pin`).
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
    /// `<N>d` (e.g. `120d`).
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

/// The scriptable exit-code contract for `claim check`, shown under
/// `--help`/`--long-help` so a CI author sees it without reading the source.
const CHECK_EXIT_HELP: &str = "\
EXIT CODES (highest applicable wins, 2 > 1 > 0):
  0  every check held and every support resolved
  1  a drifted or unverifiable verdict, or an unresolved support (review needed)
  2  a broken check, an unloadable/duplicate-id claim file, or a tool error";

/// Arguments to `claim check`: the verify loop.
///
/// Selection is `--due` (default) or `--all`; they are mutually exclusive. By
/// default the verdict of every run is appended to the log; `--report-only`
/// suppresses every write (the fork-PR / CI-advisory mode) while still reporting
/// and still setting the exit code.
#[derive(Debug, clap::Args)]
#[command(after_long_help = CHECK_EXIT_HELP)]
pub struct CheckArgs {
    /// Run every claim's checks, ignoring whether they are currently due.
    ///
    /// Mutually exclusive with `--due`. When neither is given, `--due` is the
    /// default: only claims whose cadence has elapsed (or that have never run) are
    /// checked. Retired claims are never checked under either flag.
    #[arg(long, conflicts_with = "due")]
    pub all: bool,

    /// Run only the claims that are currently due (the default).
    ///
    /// A claim is due when any `on-change` check exists (always, in v1) or an
    /// `every Nd` check's interval has elapsed since its last run. Named explicitly
    /// so a script can state its intent; identical to passing nothing.
    #[arg(long, conflicts_with = "all")]
    pub due: bool,

    /// Run and report the checks but write nothing to the verdict log.
    ///
    /// The untrusted-runner mode (PRODUCT.md section 3: a fork PR's CI reports
    /// verdicts in its output only; trusted runs persist). The exit code is still
    /// set from the verdicts, so CI can gate on it — only the persistence is
    /// suppressed. Because nothing is written, no git identity or commit is needed.
    ///
    /// Note: writing nothing means this run does not advance a claim's due clock —
    /// an `every Nd` claim stays due until a *persisting* run records a verdict. A
    /// nightly report-only job therefore leaves everything perpetually due; pair it
    /// with a trusted persisting run to reset the cadence.
    #[arg(long)]
    pub report_only: bool,
}

/// Arguments to `claim list`: the store inventory with computed status.
///
/// The filters narrow the corpus; a bare positional term does a substring search
/// over id and statement. Filters combine with AND — every one given must hold —
/// so `--status drifted --path src/` is "drifted claims under src/".
#[derive(Debug, clap::Args)]
pub struct ListArgs {
    /// Keep only claims with this computed status: `verified`, `drifted`, `stale`,
    /// or `retired`.
    #[arg(long, value_name = "STATUS")]
    pub status: Option<String>,

    /// Keep only claims whose file, or one of whose watched paths, is under this
    /// path prefix.
    #[arg(long, value_name = "PREFIX")]
    pub path: Option<String>,

    /// Keep only claims that declare this `supports` target.
    #[arg(long, value_name = "TARGET")]
    pub supports: Option<String>,

    /// Keep only claims whose computed status is `stale` (overdue: never verified,
    /// or past `max-age`). Equivalent to `--status stale`; a shortcut for the
    /// common "what has gone unverified" query. Drift is a distinct status — use
    /// `claim drift` or `--status drifted` for that.
    #[arg(long)]
    pub stale: bool,

    /// Keep only claims with no passing verdict on record, or explicitly recorded
    /// unwitnessed: the acknowledged epistemic debt (soft-debt view).
    #[arg(long)]
    pub unverified: bool,

    /// A substring to match against each claim's id and statement (case-sensitive).
    #[arg(value_name = "TERM")]
    pub term: Option<String>,
}

/// Arguments to `claim log`: one claim's full definition and history.
#[derive(Debug, clap::Args)]
pub struct LogArgs {
    /// The id of the claim whose history to show.
    #[arg(value_name = "ID")]
    pub id: String,
}

/// The scriptable exit-code contract for `claim drift`, shown under `--help`.
const DRIFT_EXIT_HELP: &str = "\
EXIT CODES:
  0  no claim has drifted
  1  at least one claim has drifted (the review queue is non-empty)
  2  a tool error (unloadable store or claim file)";

/// Arguments to `claim drift`: the review queue of drifted claims.
#[derive(Debug, clap::Args)]
#[command(after_long_help = DRIFT_EXIT_HELP)]
pub struct DriftArgs {}
