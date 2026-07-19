//! The command-line surface: the `clap` derive types for `claim` and its verbs.
//!
//! This module is *only* the shape of the CLI — the parsed arguments — with no
//! behavior, so the grammar of the tool can be read in one place and each verb's
//! logic lives in [`crate::commands`]. Two conventions are wired here for every
//! verb, present and future:
//!
//! - **A global `--json` flag** ([`Cli::json`]). Agents are the heaviest readers
//!   (docs/design/PRODUCT.md section 5), so every command owes a stable machine form. The flag
//!   is parsed once at the top level and threaded to each command.
//! - **One args struct per verb** ([`Command`]). The verb list is declared so
//!   `claim --help` shows the real surface, and each verb carries its own typed
//!   arguments.

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

/// The `claim` verbs.
///
/// The CLI is a stateless runtime verifier: it reads claim files, runs their
/// checks, and reports `held`/`drifted`/`broken` right now. It stores no verdicts
/// and computes no staleness — that ledger belongs to the hub that ingests the
/// reported stream (see `docs/design/CLI-HUB-BOUNDARY.md`).
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Scaffold a `.claims/` store in the current directory.
    Init(InitArgs),

    /// Create a claim: run its check now and require it holds, then write the file.
    /// `--witness-cmd` optionally witnesses the check going red in isolation.
    Add(AddArgs),

    /// Run every claim's checks and report holds/drifted/unverifiable/broken.
    Check(CheckArgs),
    /// List the store's claims (id, statement, file, supports count).
    List(ListArgs),
    /// Print one claim's full definition — statement, checks, supports, hub hints.
    /// Runs nothing (the static counterpart to `claim check <id>`).
    Show(ShowArgs),
    /// Run checks and show the drifted claims with what each supports.
    Drift(DriftArgs),
    /// Resolve drift by fixing a claim's statement and check; require it holds again.
    Amend(AmendArgs),
    /// Close a claim on purpose with a note: remove its file (a git rm).
    Retire(RetireArgs),

    /// Render the `supports` graph: each claim and the decisions or claims it backs
    /// (`--backers` flips it to each target and the claims backing it).
    Graph(GraphArgs),

    /// Open the product documentation bundled into this binary.
    Docs(DocsArgs),
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
/// non-interactively — the default: a missing required flag is an error naming it,
/// never a prompt. [`AddArgs::interactive`] (`-i`) opts into prompting for omitted
/// fields when authoring by hand. The default path runs the check once, requires
/// `Held`, and writes the claim file — never touching the working tree otherwise, and
/// never writing a verdict. [`AddArgs::witness_cmd`] is the optional extra-confidence
/// path (invariant #5); see the command's module docs.
#[derive(Debug, clap::Args)]
pub struct AddArgs {
    /// The claim's id: a kebab-case slug, optionally namespaced with `/`
    /// (e.g. `payments/libfoo-pin`). Required: pass the flag, or run with
    /// `--interactive` to be prompted.
    #[arg(long)]
    pub id: Option<String>,

    /// The plain-language statement — the fact the claim records. Required: pass
    /// the flag, or run with `--interactive` to be prompted.
    #[arg(long)]
    pub statement: Option<String>,

    /// The `cmd` check's command line. Runs through the shell; exit 0 means the
    /// fact holds, exit 1 means it drifted (unless `--negate` inverts). Required:
    /// pass the flag, or run with `--interactive` to be prompted.
    #[arg(long, value_name = "CMD")]
    pub run: Option<String>,

    /// Invert the check's `Held`/`Drifted` sense (the tool owns the inversion; it
    /// never wraps the command in a shell `!`).
    #[arg(long)]
    pub negate: bool,

    /// Prompt for any required field left unset (`--id`, `--statement`, `--run`)
    /// instead of erroring.
    ///
    /// Off by default: `claim` is headless-first, so a missing required field is a
    /// machine-actionable error naming the flag, never a prompt that could hang an
    /// agent running under a pseudo-terminal. Pass this when authoring a claim by
    /// hand and you would rather be walked through the fields.
    #[arg(long, short = 'i')]
    pub interactive: bool,

    /// An optional hub freshness-window hint, as `<N>d` (e.g. `120d`), written under
    /// the claim's `hub:` subfield.
    ///
    /// Optional: it is a hint the CLI validates but never acts on — how long a
    /// passing check keeps the claim fresh is a property of the hub's schedule, not
    /// of this stateless verifier. Omitted, the claim carries no `hub:` block and the
    /// hub decides its cadence. There is no default and none is invented.
    #[arg(long, value_name = "DAYS")]
    pub max_age: Option<String>,

    /// A `supports` target this claim justifies — a decision ref
    /// (`requirements.txt#libfoo`) or a bare claim id. Repeatable.
    ///
    /// A decision ref `path#anchor` resolves when `path` exists and the text
    /// `anchor` occurs in it as a case-sensitive word-boundary text scan — not a
    /// Markdown heading slug. `CLAUDE.md#approved-dependencies` looks for the exact
    /// text `approved-dependencies`, so to point at a heading like "## Approved
    /// dependencies" use the words as written and matching case (`CLAUDE.md#Approved
    /// dependencies`), not the GitHub anchor slug; `#approved` will not match
    /// `Approved`. A bare target (no `#`) resolves when it is an existing file or a
    /// known claim id. `add` runs this check now and prints a warning for each
    /// target that does not resolve (a forward reference is allowed, so it is a
    /// warning, not a hard failure); `check` later reports an unresolved target as
    /// review-needed.
    #[arg(long = "supports", value_name = "TARGET")]
    pub supports: Vec<String>,

    /// Optional. A command that makes the fact false, to witness the check going red
    /// for extra confidence that it discriminates.
    ///
    /// Not required: a passing check already verifies the fact (invariant #5). When
    /// given, the tool applies this command in a *throwaway git worktree* detached at
    /// HEAD, runs the check there expecting `Drifted`, and tears the worktree down —
    /// so your working tree is never touched and no clean tree is required. Needs a
    /// commit to check out, so it is refused on an unborn HEAD (commit first, or drop
    /// the flag).
    #[arg(long, value_name = "CMD")]
    pub witness_cmd: Option<String>,
}

/// The scriptable exit-code contract and the agent-runner env var for
/// `claim check`, shown under `--help`/`--long-help` so a CI author sees both
/// without reading the source.
const CHECK_EXIT_HELP: &str = "\
EXIT CODES (highest applicable wins, 2 > 1 > 0):
  0  every check held and every support resolved
  1  a drifted or unverifiable verdict, or an unresolved support (review needed)
  2  a broken check, an unloadable/duplicate-id claim file, or a tool error

ENVIRONMENT:
  CLAIM_AGENT_CMD  Opt in to executing 'kind: agent' checks. Set it to a shell
                   command that receives the verdict prompt on stdin and prints a
                   single JSON object on stdout:
                     {\"verdict\":\"held\"|\"drifted\"|\"unverifiable\",
                      \"evidence\":\"...\",\"citations\":[\"...\"]}
                   A crash, timeout, non-zero exit, or malformed output is Broken,
                   never a pass. Unset (the default), agent checks are Unverifiable
                   and no runner is spawned. The runner is yours to provide (e.g. a
                   wrapper around a model CLI); its cost and budget are your
                   responsibility. See docs/agent-checks.md.";

/// Arguments to `claim check`: the runtime verifier.
///
/// `check` runs every claim's checks and reports the current verdict — it stores
/// nothing. Two selectors narrow which claims run, so a CI step can verify a cheap
/// subset on a PR and leave the rest to a scheduled run: positional [`ids`](CheckArgs::ids)
/// name claims exactly, and [`path`](CheckArgs::path) selects by repo path. Given
/// neither, `check` runs the whole store (unchanged). Given both, the selected set is
/// their **union** — a claim runs if its id was named *or* it matches the path.
#[derive(Debug, clap::Args)]
#[command(after_long_help = CHECK_EXIT_HELP)]
pub struct CheckArgs {
    /// Claim ids to check, e.g. `claim check auth/no-cycles billing/tax`. Repeatable.
    ///
    /// Each id must resolve to a real claim in the store: a named id asserts "this
    /// claim exists," so an unknown id is a usage error (exit 2) naming the
    /// unresolved id(s), never a silent no-op. Combined with `--path`, the selected
    /// set is the union of the two. Given no ids and no `--path`, every claim runs.
    #[arg(value_name = "ID")]
    pub ids: Vec<String>,

    /// Check only claims whose file, or one of whose `supports` paths, is under this
    /// repo path prefix (the same match `claim list --path` uses).
    ///
    /// Unlike a named id, a path is a filter, not an existence assertion: a prefix
    /// that matches no claim is not an error — it exits 0 and the report says plainly
    /// that no claims matched (never "all held"). Combined with positional ids, the
    /// selected set is the union of the two.
    #[arg(long, value_name = "PREFIX")]
    pub path: Option<String>,
}

/// Arguments to `claim list`: the store inventory.
///
/// A plain inventory — id, statement, file, supports count — with no status: the
/// CLI stores no verdicts, so there is nothing to compute a status from here. The
/// filters narrow the corpus; a bare positional term does a substring search over id
/// and statement. Filters combine with AND — every one given must hold — so
/// `--path src/ --supports x` is "claims under src/ that support x".
#[derive(Debug, clap::Args)]
pub struct ListArgs {
    /// Keep only claims whose file, or one of whose watched paths, is under this
    /// path prefix.
    #[arg(long, value_name = "PREFIX")]
    pub path: Option<String>,

    /// Keep only claims that declare this `supports` target.
    #[arg(long, value_name = "TARGET")]
    pub supports: Option<String>,

    /// A substring to match against each claim's id and statement (case-sensitive).
    #[arg(value_name = "TERM")]
    pub term: Option<String>,
}

/// Arguments to `claim show <id>`: print one claim's full definition.
///
/// The static counterpart to `claim check <id>`: it reads the claim and prints
/// everything the file holds — the statement, each check, the `supports` targets,
/// and any `hub:` hints — but runs *nothing*. No check executes, no verdict is
/// produced. A single id, not a list: `show` is about one claim, so an unknown id
/// is a loud error (exit 2), never an empty success.
#[derive(Debug, clap::Args)]
pub struct ShowArgs {
    /// The id of the claim to print. Unknown → exit 2 naming the id; a target whose
    /// file failed to parse surfaces that parse error instead, so a broken claim is
    /// never shown as a silent blank.
    #[arg(value_name = "ID")]
    pub id: String,
}

/// The scriptable exit-code contract for `claim drift`, shown under `--help`.
const DRIFT_EXIT_HELP: &str = "\
EXIT CODES:
  0  no claim's check reported drifted
  1  at least one claim drifted (the review queue is non-empty)
  2  a broken check, or an unloadable store or claim file";

/// Arguments to `claim drift`: run checks and show the drifted claims.
#[derive(Debug, clap::Args)]
#[command(after_long_help = DRIFT_EXIT_HELP)]
pub struct DriftArgs {}

/// Arguments to `claim graph`: render the store's `supports` graph.
///
/// It reads the whole store from the current directory and prints the edges. The
/// default ASCII view groups by claim — each claim, then the targets it supports, a
/// target that is itself a claim tagged `[claim]`; `--backers` flips to the inverse
/// (each target, then the claims backing it). `--json` emits a direction-agnostic
/// node/edge list unaffected by `--backers`. Richer views — filtering, transitive
/// support, real visualization — belong to the hub, not here.
#[derive(Debug, clap::Args)]
pub struct GraphArgs {
    /// Group by target instead of by claim: show each decision or claim and the claims
    /// that back it ("who backs this?"), the inverse of the default grouped-by-claim
    /// view. Ignored under `--json`, whose node/edge list is direction-agnostic.
    #[arg(long)]
    pub backers: bool,
}

/// Arguments to `claim retire <id>`: close a claim on purpose by removing its file.
///
/// Retirement is a deliberate lifecycle decision, so the only inputs are the claim
/// to close and *why*. The note is required: a retirement with no reason is the
/// silent closure the tool exists to prevent (invariant #6). The changelog is git
/// history — `git log .claims/` — so the note rides in the commit message the caller
/// makes, not in a stored event.
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

/// Arguments to `claim amend <id>`: fix a claim's statement and/or check in place.
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

    /// Invert the amended check's `Held`/`Drifted` sense. Only meaningful together
    /// with `--run`: negation is a property of the check, so it is set when the
    /// command is replaced and otherwise left exactly as the existing check declares
    /// it. Requiring `--run` means an amend that does not touch the check can never
    /// silently un-negate a negated one. See [`crate::commands::amend`].
    #[arg(long, requires = "run")]
    pub negate: bool,

    /// The new hub freshness-window hint, as `<N>d` (e.g. `120d`), under `hub:`.
    #[arg(long, value_name = "DAYS")]
    pub max_age: Option<String>,

    /// Replace the `supports` targets with exactly this set. Repeatable; passing it
    /// with no value is not possible (clap requires a value), so to *clear* supports
    /// there is no flag — amend never silently drops edges it was not told to.
    #[arg(long = "supports", value_name = "TARGET")]
    pub supports: Vec<String>,
}

/// Help text for `claim docs`, naming the topics and the two shipping modes so a
/// user sees the whole surface under `--help` without reading the source.
const DOCS_HELP: &str = "\
TOPICS (the page `claim docs <topic>` selects; default is the overview):
  ci             the two CI lanes, exit codes, and the renderer
  agent-checks   the CLAIM_AGENT_CMD runner contract and verdict mapping
  dogfooding     how this repo verifies its own decisions with claim

The site is bundled into this binary, so it always matches the version you ran.
`claim docs` writes it to a per-version cache directory and prints the path — only
the path, on stdout, so `open \"$(claim docs)\"` composes. Pass `--open` to also
launch a browser; on a box with no opener it still just prints the path and exits 0.

ENVIRONMENT:
  CLAIM_DOCS_CACHE_DIR  Override the base directory the site is written under,
                        ahead of the platform default ($XDG_CACHE_HOME or
                        ~/.cache on Linux, ~/Library/Caches on macOS,
                        %LOCALAPPDATA% on Windows). The content is reproducible
                        from the binary, so relocating or losing this cache costs
                        only a rewrite on the next run.";

/// Arguments to `claim docs`: locate (or open) the version-locked documentation site.
///
/// The docs travel *inside* the binary (see [`crate::commands::docs`]), so an
/// installed user with no repository can still read the docs for exactly the tool
/// they have. An optional positional `TOPIC` picks a deeper page; by default the
/// path is printed for headless and scripting use, and `--open` launches a browser.
#[derive(Debug, clap::Args)]
#[command(after_long_help = DOCS_HELP)]
pub struct DocsArgs {
    /// The topic page: `ci`, `agent-checks`, or `dogfooding`. Omitted, the overview
    /// (`index.html`).
    #[arg(value_name = "TOPIC")]
    pub topic: Option<String>,

    /// Open the page in a browser instead of just printing its path.
    ///
    /// Off by default: `claim` is headless-first, so `claim docs` writes the bundled
    /// site and prints only the path, on stdout, so `open "$(claim docs)"` composes.
    /// Pass `--open` to also launch the platform opener; on a box with no opener it
    /// still just prints the path and exits 0.
    #[arg(long)]
    pub open: bool,
}
