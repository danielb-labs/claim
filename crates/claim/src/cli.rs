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
//! - **One args struct per verb** ([`Command`]). The full v1 verb list is declared
//!   so `claim --help` shows the real surface, and each verb carries its own typed
//!   arguments; the whole v1 set — `amend`, `retire`, `stats` included — is now
//!   implemented in [`crate::commands`].

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

/// The `claim` verbs. The full v1 set (PRODUCT.md section 5).
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Scaffold a `.claims/` store in the current directory.
    Init(InitArgs),

    /// Create a claim: run its check now, require it passes, write the first log
    /// entry. `--witness-cmd` optionally witnesses the check going red in isolation.
    Add(AddArgs),

    /// Run checks and report holds/drifted/unverifiable/broken.
    Check(CheckArgs),
    /// Filter claims by text, path, status, staleness, or supports.
    List(ListArgs),
    /// Show a claim's full history and evidence.
    Log(LogArgs),
    /// List drifted claims with what each supports.
    Drift(DriftArgs),
    /// Resolve drift by fixing a claim's statement and check, keeping its history.
    Amend(AmendArgs),
    /// Close a claim on purpose with a note; its status becomes `retired`.
    Retire(RetireArgs),
    /// Pilot metrics: status and verdict counts, drifts caught, staleness.
    Stats(StatsArgs),
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
/// falls back to prompting. The default path runs the check once, requires `Held`,
/// and writes — never touching the working tree. [`AddArgs::witness_cmd`] is the
/// optional extra-confidence path (invariant #5); see the command's module docs.
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

    /// Optional. A command that makes the fact false, to witness the check going red
    /// for extra confidence that it discriminates.
    ///
    /// Not required: a passing check already verifies the fact (invariant #5). When
    /// given, the tool applies this command in a *throwaway git worktree* detached at
    /// HEAD, runs the check there expecting `Drifted`, and tears the worktree down —
    /// so your working tree is never touched and no clean tree is required. The
    /// observed red is recorded as evidence on the establishing verdict. Needs a
    /// commit to check out, so it is refused on an unborn HEAD (commit first, or drop
    /// the flag).
    #[arg(long, value_name = "CMD")]
    pub witness_cmd: Option<String>,
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

    /// Keep only claims with no passing verdict on record: never genuinely verified
    /// (hand-committed with no log, or only ever broken/drifted/unverifiable). A
    /// single passing check clears it.
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

/// Arguments to `claim retire <id>`: close a claim on purpose.
///
/// Retirement is a deliberate lifecycle decision, not a check result, so the only
/// inputs are the claim to close and *why*. The note is required: a retirement
/// with no reason is the silent closure the tool exists to prevent (invariant #6).
#[derive(Debug, clap::Args)]
pub struct RetireArgs {
    /// The id of the claim to retire.
    #[arg(value_name = "ID")]
    pub id: String,

    /// Why the claim is being closed: the world changed and the decision was
    /// re-reviewed, or the fact became a real test and this says where. Required.
    #[arg(long, value_name = "WHY")]
    pub note: String,
}

/// Arguments to `claim amend <id>`: fix a claim's statement and/or check in place,
/// keeping its verdict history.
///
/// Every field is an *overlay*: a flag left off keeps the claim's current value, so
/// `claim amend pin --run '<new cmd>'` changes only the check. The id is not
/// amendable — changing the id would be a new claim, not an amendment — so there is
/// deliberately no `--id` flag; passing one is a usage error clap rejects.
///
/// At least one field must be supplied and must actually change something, or the
/// amend is a no-op error. The amended check is then run and must report `Held`
/// before anything is written: an amend cannot turn a drifted claim green unless the
/// new fact actually holds now.
#[derive(Debug, clap::Args)]
pub struct AmendArgs {
    /// The id of the claim to amend. Not itself amendable.
    #[arg(value_name = "ID")]
    pub id: String,

    /// The new plain-language statement.
    #[arg(long)]
    pub statement: Option<String>,

    /// The new `cmd` check command line. Exit 0 means the fact holds, exit 1 means
    /// it drifted (unless `--negate` inverts).
    #[arg(long, value_name = "CMD")]
    pub run: Option<String>,

    /// The new trigger: `on-change` or `every <N>d`.
    #[arg(long, value_name = "TRIGGER")]
    pub when: Option<String>,

    /// Invert the amended check's `Held`/`Drifted` sense. Only meaningful together
    /// with `--run`: negation is a property of the check, so it is set when the
    /// command is replaced and otherwise left exactly as the existing check declares
    /// it. Requiring `--run` means an amend that does not touch the check can never
    /// silently un-negate a negated one. See [`crate::commands::amend`].
    #[arg(long, requires = "run")]
    pub negate: bool,

    /// The new freshness window, as `<N>d` (e.g. `120d`).
    #[arg(long, value_name = "DAYS")]
    pub max_age: Option<String>,

    /// Replace the `supports` targets with exactly this set. Repeatable; passing it
    /// with no value is not possible (clap requires a value), so to *clear* supports
    /// there is no flag — amend never silently drops edges it was not told to.
    #[arg(long = "supports", value_name = "TARGET")]
    pub supports: Vec<String>,
}

/// Arguments to `claim stats`: the pilot instrumentation (PRODUCT.md section 9).
///
/// A read-only rollup over the whole store; no selection flags in v1, because the
/// pilot wants the corpus-wide picture. `--json` (global) is the structured form.
#[derive(Debug, clap::Args)]
pub struct StatsArgs {}
