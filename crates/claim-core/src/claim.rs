//! The claim format and its parser.
//!
//! A claim is a plain-language statement bound to a machine-readable policy: how
//! to re-verify the statement, and what it supports. This module turns bytes on
//! disk into a validated [`Claim`], or fails with an error that names the file
//! and the exact field the author must fix. It does not execute checks or resolve
//! `supports` targets; those are later concerns kept deliberately out of the
//! parser so that "is this file well-formed" stays separable from "is this fact
//! true".
//!
//! Scheduling is not a property a claim asserts about itself. Whether a fact is
//! re-checked on a code change or on a clock is *orchestration* — a CI step or the
//! hub's scheduler decides it — so there is no `when`/trigger field. A cadence
//! *hint* and a freshness window live under an optional [`Hub`] subfield the
//! parser validates syntactically but the CLI never acts on: they are consumed by
//! the hub that ingests the reported verdict stream, not by this stateless
//! verifier. See `docs/design/CLI-HUB-BOUNDARY.md`.
//!
//! Two host formats carry the same schema. A standalone `.claims/**/*.md` file
//! puts the YAML in a `---`-fenced frontmatter block with the statement as its
//! markdown body ([`parse_claim_file`]). Any other text file may embed claims in
//! `<!-- claim ... -->` comment blocks, each preceded by its statement
//! ([`extract_embedded_claims`]). Both funnel through one validator, so the
//! schema is defined once and cannot drift between the two.
//!
//! Validation is intentionally strict and loud, per the product's failure mode:
//! a malformed claim is rejected at parse time rather than silently degraded,
//! because a claim the tool cannot understand is a claim it cannot honestly
//! check.

use std::num::NonZeroU32;
use std::str::FromStr;

use jiff::Timestamp;
use serde_norway::Value;

use crate::error::{Error, Result};

/// A fully validated claim: a human statement, the checks that re-verify it, its
/// `supports` graph edges, and any hub hints.
///
/// A `Claim` is only ever produced by this module's parsers ([`parse_claim_file`]
/// and [`extract_embedded_claims`]), which is why it has no public constructor:
/// there is no way to build one that skips validation, so every `Claim` a caller
/// holds already satisfies the schema (a non-empty check list, a well-formed and
/// non-empty statement, well-formed hub hints). The fields are public for reading;
/// the `#[non_exhaustive]` attribute reserves the right to add fields without a
/// breaking change, so callers must not construct or exhaustively destructure it.
///
/// Identity and provenance are deliberately absent. Who authored or reviewed a
/// claim is derived from git and the forge at read time, never stored here,
/// because anything a file asserts about itself can be forged. Likewise there is
/// no `status` field: the CLI reports a check's current verdict and stores nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Claim {
    /// The claim's stable identity: a kebab-case slug, optionally namespaced with
    /// `/`, unique within a store. See [`ClaimId`].
    pub id: ClaimId,
    /// The human-and-agent-readable prose. For a standalone file this is the
    /// markdown body; for an embedded claim it is the non-blank text immediately
    /// preceding the comment block. The statement is the real source of truth a
    /// check only approximates.
    pub statement: String,
    /// How to re-verify the statement. Guaranteed non-empty: a claim with no
    /// check is a claim nothing can re-verify, and is rejected at parse time.
    pub checks: Vec<Check>,
    /// Decision refs or claim ids this claim justifies (the `supports` edge).
    /// Kept as validated strings here and resolved later; an empty list means the
    /// claim stands alone. See [`SupportTarget`].
    pub supports: Vec<SupportTarget>,
    /// Hints for the hub that ingests this claim's reported verdicts: a cadence
    /// and a freshness window. Co-located with the claim so they are reviewed in
    /// the same PR, but validated only for shape here — the CLI never acts on
    /// them. See [`Hub`].
    pub hub: Hub,
    /// `[[wiki-link]]` slugs harvested from the statement body, in first-seen
    /// order with duplicates removed. Navigation edges only: parsed, never
    /// resolved, and carrying no verification consequences.
    pub wiki_links: Vec<WikiLink>,
    /// Where this claim was found, for diagnostics and later tooling.
    pub source: Source,
}

/// The `hub:` subfield: scheduling hints for the hub that stores this claim's
/// verdict stream, validated by the CLI but never acted on by it.
///
/// These are demoted from the authoritative fields on purpose (see
/// `docs/design/CLI-HUB-BOUNDARY.md`): a QA hub and a production hub track the
/// same claim on different cadences and different histories, which no single
/// committed field can encode. The CLI validates the *shape* (a malformed
/// `recheck` or `max-age` is a loud parse error, so a hub never ingests garbage)
/// and stops there; the hub reads these as defaults and may override them in its
/// own config. Both are optional — a claim with no `hub:` block is valid, and its
/// cadence is entirely the hub's to decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Hub {
    /// The cadence hint: how often the hub scheduler should re-check this claim,
    /// as a `<N>d` day count. `None` when unspecified.
    pub recheck: Option<Days>,
    /// The freshness-window hint: how long a passing check keeps the claim fresh
    /// before the hub should treat it as stale, as a `<N>d` day count. `None` when
    /// unspecified. This is the v1 `max-age`, demoted to a hub hint.
    pub max_age: Option<Days>,
}

/// A claim's identity: a lowercase kebab-case slug that may use `/` as a
/// namespace separator, for example `payments/libfoo-pin`.
///
/// The shape is validated but uniqueness is not — that is a store-level property
/// checked when claims are collected, not something a single file can know.
/// Permitted characters are `a`–`z`, `0`–`9`, `-`, and `/`. Segments between
/// slashes must be non-empty and may not start or end with a hyphen, so the id
/// reads as a clean path of clean slugs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClaimId(String);

impl ClaimId {
    /// Validate and wrap a raw id string, returning the reason on failure.
    ///
    /// Returns a bare reason (not a full [`Error`]) so the frontmatter parser can
    /// prepend the field path and file. The public entry point is
    /// [`FromStr`](ClaimId::from_str), for downstream code that must validate a
    /// bare id — a `claim add` argument, an id naming a claim file — with no file
    /// to name.
    fn validate(raw: &str) -> std::result::Result<Self, String> {
        if raw.is_empty() {
            return Err("id must not be empty".to_owned());
        }
        for segment in raw.split('/') {
            if segment.is_empty() {
                return Err(format!(
                    "id '{raw}' has an empty path segment; use single '/' separators \
                     with no leading, trailing, or doubled slashes"
                ));
            }
            if segment.starts_with('-') || segment.ends_with('-') {
                return Err(format!(
                    "id segment '{segment}' must not start or end with a hyphen"
                ));
            }
        }
        if let Some(bad) = raw
            .chars()
            .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '/'))
        {
            return Err(format!(
                "id '{raw}' contains '{bad}'; ids use only lowercase letters, digits, \
                 hyphens, and '/' as a namespace separator"
            ));
        }
        Ok(Self(raw.to_owned()))
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ClaimId {
    type Err = Error;

    /// Validate a bare id string outside any file, for callers that hold an id
    /// without a claim file — the id naming `.claims/<id>.md` and the `claim add`
    /// id argument. The error's reason is self-contained (it quotes the offending
    /// id), so the `id` context in the path position is only a label.
    fn from_str(raw: &str) -> Result<Self> {
        ClaimId::validate(raw).map_err(|reason| Error::parse("id", reason))
    }
}

impl std::fmt::Display for ClaimId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One way to re-verify a claim.
///
/// A claim may carry several checks at different depths — a cheap command, an
/// expensive agent investigation. The [`kind`](Check::kind) carries the
/// check-type-specific payload. There is no trigger here: when a check runs is
/// orchestration a CI step or the hub decides, not a property the claim asserts
/// (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Check {
    /// The check's mechanism and its payload.
    pub kind: CheckKind,
    /// An optional declared skip: conditions under which this check is deliberately
    /// not run. `None` — the common case — means the check always runs.
    pub skip: Option<Skip>,
}

/// A declared, justified reason not to run a check in some environments.
///
/// A skip is an *acknowledged, bounded debt* — never a pass. A skipped check records
/// no verdict and is reported as skipped, so the claim honestly reads as unverified
/// where the skip applies while the skip explains why; the build stays green only
/// where the skip legitimately applies. This is the single most dangerous thing a
/// verification tool can offer (it is exactly the stale-green-light the tool exists
/// to prevent), so the fields are shaped to keep it honest:
///
/// - [`reason`](Skip::reason) is mandatory — a silent skip is refused at parse time.
/// - [`unless`](Skip::unless) *cancels* the skip and runs the check when a condition
///   holds, so an environment that can verify does. A condition that cannot be
///   evaluated runs the check rather than silently muting it.
/// - [`until`](Skip::until) expires the skip so the debt is eventually called.
///
/// Who added a skip, and when, is git's to say (invariant #3), never the file's.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Skip {
    /// Why this check is skipped. Required; a skip with no reason is refused at parse.
    pub reason: String,
    /// A shell command whose success (exit 0) *cancels* the skip and runs the check;
    /// a failure (exit 1) leaves the skip in force. A command that cannot be
    /// evaluated (any other exit, a spawn failure, a timeout) runs the check — a
    /// broken condition must never silently mute a check.
    pub unless: Option<String>,
    /// The instant the skip expires. On or after it the check runs regardless of
    /// `unless`, and the lapse is reported. `None` means the skip has no time bound.
    pub until: Option<Timestamp>,
}

/// A check's mechanism, with the fields that mechanism needs.
///
/// v1 only *executes* [`Cmd`](CheckKind::Cmd) checks, but the parser accepts and
/// round-trips all three so that files authored for a later version remain valid
/// today. This enum is deliberately *not* `#[non_exhaustive]`: the workspace
/// crates version together, and an exhaustive `match` here is the mechanism that
/// forces every consumer — above all check execution — to handle a new kind the
/// moment one is added, rather than silently skipping it. That compile error is
/// the point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckKind {
    /// A command line the tool runs, mapping its exit code to a verdict. The tool
    /// owns that mapping and owns negation; see [`crate::verdict::Verdict`].
    Cmd {
        /// The command line to execute.
        run: String,
        /// Whether to invert `Held`/`Drifted`. The tool performs the inversion
        /// internally — never by asking a shell to interpret `!` — so a missing
        /// binary stays `Broken` instead of inverting into a false pass.
        negate: bool,
    },
    /// An investigation carried out by an agent against a natural-language
    /// instruction. Not executed in v1.
    Agent {
        /// What the agent is asked to determine.
        instruction: String,
    },
    /// A scheduled human look. Not executed in v1.
    Human {
        /// An optional prompt shown to the person doing the check.
        prompt: Option<String>,
    },
}

/// A duration in whole days.
///
/// A newtype rather than a bare integer so a day count cannot be confused with
/// any other number, and so a hub hint carries meaning at the type level. Always
/// positive: a zero-day window would mean a claim is stale the instant it is
/// verified, which is never what an author intends, so it is rejected at parse
/// time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Days(NonZeroU32);

impl Days {
    /// The number of days, guaranteed positive. Returned as a plain `u32` so call
    /// sites doing arithmetic need not unwrap a second time; the positivity
    /// guarantee is upheld at construction, not at every read.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0.get()
    }

    /// Wrap a positive day count already known at the type level.
    ///
    /// `const` so callers can build compile-time day constants without routing
    /// through the string parser and an `unwrap`. Takes a [`NonZeroU32`] so the
    /// positivity guarantee is discharged by the type, not re-checked here.
    #[must_use]
    pub const fn from_nonzero(days: NonZeroU32) -> Self {
        Days(days)
    }
}

impl FromStr for Days {
    type Err = Error;

    /// Validate a bare `<N>d` day count outside any file, for callers that hold a
    /// duration string without a claim file. The error's reason quotes the input,
    /// so the `days` context in the path position is only a label.
    fn from_str(raw: &str) -> Result<Self> {
        parse_day_count(raw)
            .map(Days)
            .map_err(|reason| Error::parse("days", reason))
    }
}

impl std::fmt::Display for Days {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}d", self.0)
    }
}

/// Parse an `<N>d` day count, e.g. `120d`, into a positive count.
///
/// Shared by [`Days`] and the `hub:` day-count hints. The number must be a bare
/// canonical decimal — no sign, no leading zero, no surrounding or embedded
/// whitespace — so that `+30d`, `030d`, and ` 30d` are rejected as firmly as
/// `12.5d` and `12days`. Returns a bare reason on failure, and gives an
/// out-of-range count a distinct message from a non-numeric one. Callers frame
/// the reason with a field path.
fn parse_day_count(raw: &str) -> std::result::Result<NonZeroU32, String> {
    let digits = raw
        .strip_suffix('d')
        .ok_or_else(|| format!("'{raw}' must be a day count ending in 'd', for example '120d'"))?;
    if digits.is_empty() {
        return Err(format!("'{raw}' has no number before 'd'"));
    }
    // Only a canonical `[1-9][0-9]*` (or a lone `0`, which fails the positivity
    // check below) is a number here. Rejecting non-canonical spellings up front
    // keeps `u32::from_str`'s lenient acceptance of signs and whitespace from
    // leaking in.
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "'{raw}' has a non-numeric day count; write a plain number like '120d'"
        ));
    }
    // A leading zero on a non-zero number is non-canonical. An all-zero count
    // (`0d`, `000d`) is instead reported as non-positive below, which is the more
    // useful message.
    let significant = digits.trim_start_matches('0');
    if digits.len() > 1 && digits.starts_with('0') && !significant.is_empty() {
        return Err(format!(
            "'{raw}' has a leading zero; write the day count as '{significant}d'"
        ));
    }
    let n: u32 = digits
        .parse()
        .map_err(|_| format!("'{raw}' is too large; the day count must fit in 32 bits"))?;
    NonZeroU32::new(n).ok_or_else(|| format!("'{raw}' must be a positive number of days, not 0"))
}

/// A target a claim justifies via its `supports` edge: a decision ref like
/// `requirements.txt#libfoo`, or a bare claim id.
///
/// Kept as a validated string in v1. Resolution — confirming the decision or
/// claim actually exists — is a later item; a `supports` target that fails to
/// resolve is meant to make the claim go loud, not to fail parsing. Validation
/// here is only that the target is non-empty, since an empty edge is a certain
/// authoring mistake.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SupportTarget(String);

impl SupportTarget {
    /// The target as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SupportTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A `[[wiki-link]]` slug harvested from a statement body.
///
/// The inner text is stored trimmed and without its brackets. These are casual
/// navigation edges by design: no schema, no review, no resolution here.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WikiLink(String);

impl WikiLink {
    /// The link target as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WikiLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a claim was found.
///
/// Both variants name the file so any downstream diagnostic can point a human at
/// it; the embedded variant additionally records the byte offset of its
/// `<!-- claim` block so a host file with several claims can distinguish them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A standalone `.claims/**/*.md` file whose entire content is one claim.
    File {
        /// The file's path, as passed to the parser.
        path: String,
    },
    /// A claim embedded in a `<!-- claim ... -->` block inside a host file.
    Embedded {
        /// The host file's path, as passed to the parser.
        path: String,
        /// The byte offset of the `<!-- claim` opener within the host file,
        /// distinguishing multiple claims in one file.
        byte_offset: usize,
    },
}

impl Source {
    /// The path of the file the claim came from, regardless of host format.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Source::File { path } | Source::Embedded { path, .. } => path,
        }
    }
}

/// Parse a standalone claim file: `---`-fenced YAML frontmatter followed by the
/// statement as the markdown body.
///
/// `path` is used only for error messages and the returned [`Source`]; the file
/// is not read from disk here, so callers control I/O and this stays pure and
/// testable. `contents` is the file's full text.
///
/// # Errors
///
/// Returns [`Error::Parse`] naming `path` and the specific problem when the
/// frontmatter fences are missing or unterminated, the YAML is malformed, or any
/// field violates the schema.
pub fn parse_claim_file(path: &str, contents: &str) -> Result<Claim> {
    let (yaml, body) = split_frontmatter(path, contents)?;
    let source = Source::File {
        path: path.to_owned(),
    };
    build_claim(path, yaml, body, source)
}

/// Whether a file's text *intends* to be a standalone claim: it opens with a
/// `---` frontmatter fence (after an optional UTF-8 BOM).
///
/// This is the discriminator a store scanner uses to tell a claim file from a
/// plain document living beside it (a `README.md` in `.claims/`). A file that
/// opens with the fence is treated as a claim and parsed — so malformed YAML
/// under an opening fence is still a *loud* [`parse_claim_file`] error, never
/// silently skipped (invariant #6: a file that means to be a claim must nag when
/// it is broken). A file that does *not* open with the fence is a plain document
/// the scanner may skip.
///
/// This is the parser's opening-fence *acceptance* condition, and only that: it
/// returns `true` for exactly the first lines [`parse_claim_file`] accepts as a
/// fence. The parser is additionally stricter — it also requires a following
/// newline and a body to proceed — so a file this accepts can still fail to
/// parse (a bare `---` with no body is one such case). The divergence is
/// one-directional and that is the load-bearing property: any file the parser
/// would accept as a claim, this accepts too, so the scanner never skips a real
/// claim; when the two differ, it is only the parser being louder, never the
/// scanner being quieter. It inspects only the first line, so a caller may pass
/// just that line rather than a whole file.
#[must_use]
pub fn has_frontmatter_fence(contents: &str) -> bool {
    let text = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    // The parser's opening-fence acceptance: the first line must *start* with `---`
    // (no leading whitespace — an indented `---` is not a fence) and carry nothing
    // but whitespace after it. The parser then also needs a newline and a body, which
    // this does not require — the accepted-as-fence set is a superset, so divergence
    // only ever makes the parser louder.
    text.strip_prefix(FRONTMATTER_FENCE)
        .map(|rest| rest.split_once('\n').map_or(rest, |(head, _)| head))
        .is_some_and(|line_rest| line_rest.trim().is_empty())
}

/// Extract every `<!-- claim ... -->` block embedded in a host file.
///
/// A host file (CLAUDE.md, AGENTS.md, or any text file) may carry several claims.
/// Each block's statement is the non-blank text immediately preceding its
/// opener. Blocks are returned in file order. A file with no claim blocks yields
/// an empty vector, which is not an error — most files have none.
///
/// Both fences are structural, not textual: the `<!-- claim` opener is
/// recognized only when it stands alone on its line (as the whole line, followed
/// by a word boundary so `<!-- claims ... -->` is an ordinary comment), and the
/// `-->` closer only when it stands alone on its line. A `-->` appearing inside a
/// YAML value therefore does not terminate the block — otherwise an arrow in a
/// string would silently truncate the claim and drop later checks.
///
/// `path` is used for error messages and each returned [`Source::Embedded`].
///
/// # Errors
///
/// Returns [`Error::Parse`] naming `path` and the offending block's location
/// when a `<!-- claim` opener is never closed by a `-->` on its own line, its
/// YAML is malformed, or any field violates the schema. One bad block fails the
/// whole extraction, because a host file that silently drops a claim it clearly
/// meant to declare is the kind of quiet failure this tool exists to prevent.
pub fn extract_embedded_claims(path: &str, contents: &str) -> Result<Vec<Claim>> {
    let mut claims = Vec::new();
    let mut search_from = 0;
    // The start of the line `search_from` sits on, so the backward scan for an
    // opener's line start never rescans past content already consumed — bounding
    // total work at O(n) even for a newline-free file dense with `<!-- claim`.
    let mut region_line_start = 0;
    while let Some(rel) = contents[search_from..].find(EMBED_OPEN) {
        let open_at = search_from + rel;
        let after_open = open_at + EMBED_OPEN.len();

        // The keyword must end at a boundary; `<!-- claimant` and `<!-- claims`
        // are ordinary comments, not claim blocks.
        let next = contents[after_open..].chars().next();
        let is_keyword = matches!(next, None | Some(' ' | '\t' | '\r' | '\n'))
            || contents[after_open..].starts_with(EMBED_CLOSE);
        // The opener must be the whole line up to here: only whitespace may
        // precede it on its line, or it is prose that merely mentions the marker.
        let line_start = contents[region_line_start..open_at]
            .rfind('\n')
            .map_or(region_line_start, |nl| region_line_start + nl + 1);
        let opener_alone = contents[line_start..open_at].trim().is_empty();

        if !is_keyword || !opener_alone {
            search_from = after_open;
            region_line_start = line_start;
            continue;
        }

        let (yaml, close_end) = embedded_yaml(path, contents, open_at, after_open)?;
        let statement = preceding_statement(&contents[..line_start]);
        let source = Source::Embedded {
            path: path.to_owned(),
            byte_offset: open_at,
        };
        claims.push(build_claim(path, yaml, &statement, source)?);
        search_from = close_end;
        region_line_start = contents[..close_end].rfind('\n').map_or(0, |nl| nl + 1);
    }
    Ok(claims)
}

/// Locate the YAML payload of an embedded block and the byte just past its
/// closing fence.
///
/// The closer is the first line after the opener that is exactly `-->`, allowing
/// only trailing whitespace. Leading whitespace disqualifies it: a `-->` indented
/// beneath a mapping key is block-scalar content, not a fence, so an arrow inside
/// a multi-line value cannot silently truncate the block and drop later checks.
/// Returns the YAML slice between the fences and the offset one past the closer.
fn embedded_yaml<'a>(
    path: &str,
    contents: &'a str,
    open_at: usize,
    after_open: usize,
) -> Result<(&'a str, usize)> {
    // Scan line by line from just after the opener for a line that is the closing
    // fence alone. `split_inclusive` keeps line terminators so byte offsets stay
    // exact across CRLF.
    let mut offset = after_open;
    for line in contents[after_open..].split_inclusive('\n') {
        // `trim_end` (not `trim`) so an indented `-->` stays block-scalar content,
        // matching the unindented-closing-fence rule in `split_frontmatter`.
        if line.trim_end() == EMBED_CLOSE {
            let yaml = &contents[after_open..offset];
            return Ok((yaml, offset + line.len()));
        }
        offset += line.len();
    }
    Err(Error::parse(
        path,
        format!(
            "unterminated '<!-- claim' block at byte {open_at}; it must be closed by a '-->' \
             alone on its own line"
        ),
    ))
}

const FRONTMATTER_FENCE: &str = "---";
const EMBED_OPEN: &str = "<!-- claim";
const EMBED_CLOSE: &str = "-->";

/// Split a standalone file into its frontmatter YAML and markdown body.
///
/// The frontmatter must open with a `---` fence on the first line and close with
/// an *unindented* `---` line. The closing fence is matched structurally rather
/// than by a trimmed compare, so a `---` line that is indented (and therefore
/// part of a YAML block scalar) does not prematurely terminate the frontmatter.
fn split_frontmatter<'a>(path: &str, contents: &'a str) -> Result<(&'a str, &'a str)> {
    // A UTF-8 BOM ahead of the fence would otherwise defeat the prefix check and
    // produce a baffling "missing frontmatter" error on a file that looks correct.
    let text = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    let after_open = text
        .strip_prefix(FRONTMATTER_FENCE)
        .and_then(|rest| {
            // The opening fence must be alone on the first line; `---foo` is not a
            // fence. Accept an immediate newline or end of that line's whitespace.
            let line_rest = rest.split_once('\n').map_or(rest, |(head, _)| head);
            line_rest.trim().is_empty().then_some(())?;
            rest.split_once('\n').map(|(_, body)| body)
        })
        .ok_or_else(|| {
            Error::parse(
                path,
                "missing YAML frontmatter; a claim file must begin with a '---' fence on its \
                 own line",
            )
        })?;

    // The closing fence is the first unindented line equal to exactly `---`.
    // Leading whitespace disqualifies it, because an indented `---` inside a block
    // scalar is data, not a fence; only trailing spaces and the line terminator
    // are tolerated.
    let mut offset = 0;
    for line in after_open.split_inclusive('\n') {
        let content = line.trim_end_matches(['\r', '\n']);
        if content.trim_end() == FRONTMATTER_FENCE {
            let yaml = &after_open[..offset];
            let body = &after_open[offset + line.len()..];
            return Ok((yaml, body));
        }
        offset += line.len();
    }
    Err(Error::parse(
        path,
        "unterminated YAML frontmatter; the opening '---' fence has no matching unindented '---'",
    ))
}

/// The statement for an embedded claim: the last non-blank run of text before
/// its block.
///
/// "Immediately preceding" means the contiguous non-blank lines ending at the
/// block, so a paragraph of statement is captured whole while earlier unrelated
/// prose (separated by a blank line) is not.
fn preceding_statement(before: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in before.lines().rev() {
        if line.trim().is_empty() {
            if lines.is_empty() {
                // Skip blank lines directly above the block before the statement.
                continue;
            }
            break;
        }
        lines.push(line);
    }
    lines.reverse();
    lines
        .iter()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// Validate parsed YAML and a statement into a [`Claim`].
///
/// The single choke point both host formats pass through, so the schema is
/// enforced in exactly one place. Every error names the field path and file.
fn build_claim(path: &str, yaml: &str, statement: &str, source: Source) -> Result<Claim> {
    let statement = statement.trim();
    if statement.is_empty() {
        // The statement is the fact a human reads when drift routes to them; a
        // claim without one is a nag-worthy error, not a valid claim. The two host
        // formats fail it for the same reason but need different guidance.
        let reason = match &source {
            Source::File { .. } => {
                "the claim has no statement; write the fact in the markdown body after the \
                 closing '---' fence"
            }
            Source::Embedded { .. } => {
                "the claim has no statement; put the fact on the non-blank line immediately \
                 before the '<!-- claim' block"
            }
        };
        return Err(Error::parse(path, reason));
    }

    let value: Value = serde_norway::from_str(yaml)
        .map_err(|e| Error::parse(path, format!("invalid YAML: {e}")))?;

    let map = match &value {
        Value::Mapping(m) => m,
        // An empty frontmatter deserializes to null; treat it as the missing
        // required fields it is, rather than a cryptic type error.
        Value::Null => {
            return Err(Error::parse(
                path,
                "claim has no fields; 'id' and 'checks' are required",
            ));
        }
        other => {
            return Err(Error::parse(
                path,
                format!(
                    "claim must be a YAML mapping of fields, found {}",
                    value_kind(other)
                ),
            ));
        }
    };

    reject_unknown_fields(path, map)?;

    let id_raw = require_str(path, map, "id")?;
    let id = ClaimId::validate(id_raw).map_err(|reason| Error::parse(path, reason))?;

    let checks = parse_checks(path, map)?;
    let supports = parse_supports(path, map)?;
    let hub = parse_hub(path, map)?;
    let wiki_links = harvest_wiki_links(statement);

    Ok(Claim {
        id,
        statement: statement.to_owned(),
        checks,
        supports,
        hub,
        wiki_links,
        source,
    })
}

/// The recognized top-level claim fields. Anything else is flagged so a typo
/// like `check:` or `maxage:` fails loudly instead of being silently ignored.
///
/// `max-age` is deliberately *not* here: it moved under `hub:`. A top-level
/// `max-age` from a v1 file is caught as an unknown field with guidance to move
/// it (see [`reject_unknown_fields`]).
const KNOWN_FIELDS: &[&str] = &["id", "checks", "supports", "hub"];

fn reject_unknown_fields(path: &str, map: &serde_norway::Mapping) -> Result<()> {
    for key in map.keys() {
        let name = key.as_str().ok_or_else(|| {
            Error::parse(
                path,
                "claim has a non-string field name; keys must be strings",
            )
        })?;
        if !KNOWN_FIELDS.contains(&name) {
            // A v1 top-level `max-age` is a common migration mistake worth naming
            // precisely: it is now a hub hint under `hub:`.
            if name == "max-age" {
                return Err(Error::parse(
                    path,
                    "max-age is no longer a top-level field; move it under 'hub:' as \
                     'hub:\\n  max-age: <N>d' — it is a hub hint the CLI validates but \
                     does not act on",
                ));
            }
            // A v1 `when:` on the claim is likewise a migration mistake: triggers
            // are gone (scheduling is the CI/hub's job, not the claim's).
            if name == "when" {
                return Err(Error::parse(
                    path,
                    "'when' is not a claim field; a check's trigger is orchestration a CI \
                     step or the hub decides, not a property the claim asserts. Remove it; \
                     a cadence hint lives under 'hub:' as 'recheck: <N>d'",
                ));
            }
            return Err(Error::parse(
                path,
                format!(
                    "unknown field '{name}'; claim fields are {}",
                    KNOWN_FIELDS.join(", ")
                ),
            ));
        }
    }
    Ok(())
}

fn parse_checks(path: &str, map: &serde_norway::Mapping) -> Result<Vec<Check>> {
    let checks_val = map.get("checks").ok_or_else(|| {
        Error::parse(
            path,
            "missing required field 'checks'; a claim needs at least one check",
        )
    })?;
    let seq = match checks_val {
        Value::Sequence(s) => s,
        other => {
            return Err(Error::parse(
                path,
                format!(
                    "checks: expected a list of checks, found {}",
                    value_kind(other)
                ),
            ));
        }
    };
    if seq.is_empty() {
        return Err(Error::parse(
            path,
            "checks: the list is empty; a claim needs at least one check to be verifiable",
        ));
    }
    seq.iter()
        .enumerate()
        .map(|(i, v)| parse_check(path, i, v))
        .collect()
}

fn parse_check(path: &str, index: usize, value: &Value) -> Result<Check> {
    let map = match value {
        Value::Mapping(m) => m,
        other => {
            return Err(Error::parse(
                path,
                format!(
                    "checks[{index}]: expected a mapping, found {}",
                    value_kind(other)
                ),
            ));
        }
    };

    let field = |name: &str| format!("checks[{index}].{name}");

    let kind_raw = map
        .get("kind")
        .ok_or_else(|| {
            Error::parse(
                path,
                format!(
                    "{}: missing 'kind'; one of cmd, agent, human",
                    field("kind")
                ),
            )
        })?
        .as_str()
        .ok_or_else(|| Error::parse(path, format!("{}: expected a string", field("kind"))))?;

    // The fields each kind permits, always including the shared `kind`. A field
    // outside this set is rejected, mirroring top-level `reject_unknown_fields`: a
    // typo like `negated:` — or a leftover v1 `when:` — must fail loudly, never be
    // silently ignored into a wrong-sensed check.
    let (kind, allowed): (CheckKind, &[&str]) = match kind_raw {
        "cmd" => {
            let run = require_check_str(path, map, index, "run")?;
            if run.trim().is_empty() {
                // A blank or whitespace-only `run` runs as `sh -c ""`, exits 0,
                // and reports Held forever — a check that can never go red, the
                // exact vacuous pass this tool exists to prevent. Rejected at parse
                // time alongside the other empty-string checks (id, statement,
                // supports).
                return Err(Error::parse(
                    path,
                    format!(
                        "{}: must not be empty; a check with no command can never fail",
                        field("run")
                    ),
                ));
            }
            let negate = match map.get("negate") {
                None => false,
                Some(Value::Bool(b)) => *b,
                Some(other) => {
                    return Err(Error::parse(
                        path,
                        format!(
                            "{}: expected true or false, found {}",
                            field("negate"),
                            value_kind(other)
                        ),
                    ));
                }
            };
            (
                CheckKind::Cmd {
                    run: run.to_owned(),
                    negate,
                },
                &["kind", "run", "negate", "skip"],
            )
        }
        "agent" => {
            let instruction = require_check_str(path, map, index, "instruction")?;
            (
                CheckKind::Agent {
                    instruction: instruction.to_owned(),
                },
                &["kind", "instruction", "skip"],
            )
        }
        "human" => {
            let prompt = match map.get("prompt") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(other) => {
                    return Err(Error::parse(
                        path,
                        format!(
                            "{}: expected a string, found {}",
                            field("prompt"),
                            value_kind(other)
                        ),
                    ));
                }
            };
            (CheckKind::Human { prompt }, &["kind", "prompt", "skip"])
        }
        other => {
            return Err(Error::parse(
                path,
                format!(
                    "{}: unknown check kind '{other}'; expected one of cmd, agent, human",
                    field("kind")
                ),
            ));
        }
    };

    reject_unknown_check_fields(path, map, index, kind_raw, allowed)?;

    let skip = parse_skip(path, map, index)?;

    Ok(Check { kind, skip })
}

/// Parse an optional `skip:` block on a check.
///
/// Absent or null means no skip. A present skip must be a mapping with a non-empty
/// `reason` (the mandatory justification — a silent skip is exactly the stale green
/// this tool refuses); `unless` (an optional non-empty command) and `until` (an
/// optional date or RFC 3339 instant) are the guards that keep a skip from becoming a
/// permanent mute. Any other field is rejected, like every other over-specified
/// check field.
fn parse_skip(path: &str, map: &serde_norway::Mapping, index: usize) -> Result<Option<Skip>> {
    let field = |name: &str| format!("checks[{index}].skip.{name}");
    let skip_map = match map.get("skip") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Mapping(m)) => m,
        Some(other) => {
            return Err(Error::parse(
                path,
                format!(
                    "checks[{index}].skip: expected a mapping, found {}",
                    value_kind(other)
                ),
            ));
        }
    };

    let reason = match skip_map.get("reason") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        Some(Value::String(_)) => {
            return Err(Error::parse(
                path,
                format!(
                    "{}: must not be empty; a skip must say why it is not verifying",
                    field("reason")
                ),
            ));
        }
        Some(other) => {
            return Err(Error::parse(
                path,
                format!(
                    "{}: expected a string, found {}",
                    field("reason"),
                    value_kind(other)
                ),
            ));
        }
        None => {
            return Err(Error::parse(
                path,
                format!(
                    "checks[{index}].skip: missing 'reason'; a skip is never silent — it must say \
                     why the check is not run"
                ),
            ));
        }
    };

    let unless = match skip_map.get("unless") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
        Some(Value::String(_)) => {
            return Err(Error::parse(
                path,
                format!(
                    "{}: must not be empty; omit it for an unconditional skip",
                    field("unless")
                ),
            ));
        }
        Some(other) => {
            return Err(Error::parse(
                path,
                format!(
                    "{}: expected a command string, found {}",
                    field("unless"),
                    value_kind(other)
                ),
            ));
        }
    };

    let until = match skip_map.get("until") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(
            parse_until(s)
                .map_err(|reason| Error::parse(path, format!("{}: {reason}", field("until"))))?,
        ),
        Some(other) => {
            return Err(Error::parse(
                path,
                format!(
                    "{}: expected a date 'YYYY-MM-DD' or an RFC 3339 instant, found {}",
                    field("until"),
                    value_kind(other)
                ),
            ));
        }
    };

    for key in skip_map.keys() {
        let name = key.as_str().unwrap_or_default();
        if !["reason", "unless", "until"].contains(&name) {
            return Err(Error::parse(
                path,
                format!(
                    "checks[{index}].skip: unknown field '{name}'; allowed are reason, unless, until"
                ),
            ));
        }
    }

    Ok(Some(Skip {
        reason,
        unless,
        until,
    }))
}

/// Parse a skip's `until` as a full RFC 3339 instant, or a bare `YYYY-MM-DD` date
/// taken as the start of that day in UTC. A date is the friendly common form; the
/// instant form is there when a finer bound is wanted.
fn parse_until(raw: &str) -> std::result::Result<Timestamp, String> {
    let trimmed = raw.trim();
    if let Ok(ts) = trimmed.parse::<Timestamp>() {
        return Ok(ts);
    }
    format!("{trimmed}T00:00:00Z")
        .parse::<Timestamp>()
        .map_err(|_| format!("expected a date 'YYYY-MM-DD' or an RFC 3339 instant, found '{raw}'"))
}

/// Reject any field on a check outside the set its kind permits.
fn reject_unknown_check_fields(
    path: &str,
    map: &serde_norway::Mapping,
    index: usize,
    kind: &str,
    allowed: &[&str],
) -> Result<()> {
    for key in map.keys() {
        let name = key.as_str().ok_or_else(|| {
            Error::parse(
                path,
                format!("checks[{index}]: has a non-string field name; keys must be strings"),
            )
        })?;
        if !allowed.contains(&name) {
            // A leftover v1 `when:` on a check is a common migration mistake worth
            // naming precisely rather than as a generic unknown field.
            if name == "when" {
                return Err(Error::parse(
                    path,
                    format!(
                        "checks[{index}]: 'when' is no longer a check field; a trigger is \
                         orchestration a CI step or the hub decides, not a property the check \
                         asserts. Remove it; a cadence hint lives under the claim's 'hub:' as \
                         'recheck: <N>d'"
                    ),
                ));
            }
            return Err(Error::parse(
                path,
                format!(
                    "checks[{index}]: unknown field '{name}' on a '{kind}' check; allowed fields \
                     are {}",
                    allowed.join(", ")
                ),
            ));
        }
    }
    Ok(())
}

/// Require a string field on a check, with a field-pathed error on absence or
/// wrong type.
fn require_check_str<'a>(
    path: &str,
    map: &'a serde_norway::Mapping,
    index: usize,
    name: &str,
) -> Result<&'a str> {
    match map.get(name) {
        None => Err(Error::parse(
            path,
            format!("checks[{index}].{name}: missing required field"),
        )),
        Some(Value::String(s)) => Ok(s),
        Some(other) => Err(Error::parse(
            path,
            format!(
                "checks[{index}].{name}: expected a string, found {}",
                value_kind(other)
            ),
        )),
    }
}

/// Parse the optional `hub:` subfield — scheduling hints validated for shape but
/// never acted on by the CLI.
///
/// Absent or null means an empty [`Hub`] (a claim with no hints, valid — its
/// cadence is entirely the hub's to decide). A present block must be a mapping;
/// its `recheck` and `max-age` keys, when given, are `<N>d` day counts validated
/// through the same [`parse_day_count`] the top-level `max-age` once used, so a
/// malformed hint is a loud parse error a hub never ingests. Any other key is
/// rejected, mirroring [`reject_unknown_fields`]: a typo like `rechek:` must fail
/// loudly, not be silently ignored into a lost hint.
fn parse_hub(path: &str, map: &serde_norway::Mapping) -> Result<Hub> {
    let hub_map = match map.get("hub") {
        None | Some(Value::Null) => return Ok(Hub::default()),
        Some(Value::Mapping(m)) => m,
        Some(other) => {
            return Err(Error::parse(
                path,
                format!(
                    "hub: expected a mapping of scheduling hints, found {}",
                    value_kind(other)
                ),
            ));
        }
    };

    for key in hub_map.keys() {
        let name = key.as_str().unwrap_or_default();
        if !["recheck", "max-age"].contains(&name) {
            return Err(Error::parse(
                path,
                format!("hub: unknown field '{name}'; allowed hints are recheck, max-age"),
            ));
        }
    }

    let recheck = parse_hub_days(path, hub_map, "recheck")?;
    let max_age = parse_hub_days(path, hub_map, "max-age")?;
    Ok(Hub { recheck, max_age })
}

/// Parse one optional `<N>d` day-count hint under `hub:`, naming the field on
/// failure. Accepts the string form (`120d`); a bare integer is a common mistake
/// named precisely, as the top-level `max-age` once did.
fn parse_hub_days(path: &str, hub_map: &serde_norway::Mapping, name: &str) -> Result<Option<Days>> {
    match hub_map.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => parse_day_count(s)
            .map(|d| Some(Days(d)))
            .map_err(|reason| Error::parse(path, format!("hub.{name}: {reason}"))),
        Some(Value::Number(_)) => Err(Error::parse(
            path,
            format!(
                "hub.{name}: write the day count with a 'd' suffix, e.g. '120d', not a bare number"
            ),
        )),
        Some(other) => Err(Error::parse(
            path,
            format!(
                "hub.{name}: expected a duration like '120d', found {}",
                value_kind(other)
            ),
        )),
    }
}

fn parse_supports(path: &str, map: &serde_norway::Mapping) -> Result<Vec<SupportTarget>> {
    let Some(value) = map.get("supports") else {
        return Ok(Vec::new());
    };
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Sequence(seq) => seq
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let s = v.as_str().ok_or_else(|| {
                    Error::parse(
                        path,
                        format!("supports[{i}]: expected a string, found {}", value_kind(v)),
                    )
                })?;
                if s.trim().is_empty() {
                    return Err(Error::parse(
                        path,
                        format!("supports[{i}]: target must not be empty"),
                    ));
                }
                Ok(SupportTarget(s.to_owned()))
            })
            .collect(),
        other => Err(Error::parse(
            path,
            format!(
                "supports: expected a list of strings, found {}",
                value_kind(other)
            ),
        )),
    }
}

/// Require a top-level string field, with a precise error on absence or wrong
/// type.
fn require_str<'a>(path: &str, map: &'a serde_norway::Mapping, name: &str) -> Result<&'a str> {
    match map.get(name) {
        None => Err(Error::parse(
            path,
            format!("missing required field '{name}'"),
        )),
        Some(Value::String(s)) => Ok(s),
        Some(other) => Err(Error::parse(
            path,
            format!("{name}: expected a string, found {}", value_kind(other)),
        )),
    }
}

/// Harvest `[[wiki-link]]` targets from a statement body, de-duplicated and in
/// first-seen order.
///
/// A link is the shortest `[[`…`]]` span that contains no newline and no further
/// `[`, so `[[a]] [[b]]` yields two links, `[[[x]]]` yields `x` (the innermost
/// pair), and a `[[` that never closes on its line captures nothing. Empty or
/// whitespace-only brackets are ignored — `[[]]` is not a link.
fn harvest_wiki_links(statement: &str) -> Vec<WikiLink> {
    let mut links: Vec<WikiLink> = Vec::new();
    let mut i = 0;
    while let Some(open_rel) = statement[i..].find("[[") {
        let mut open = i + open_rel + 2;
        // Align to the innermost opener so `[[[x]]]` yields `x`, not `[x`.
        while statement[open..].starts_with('[') {
            open += 1;
        }
        let Some(close_rel) = statement[open..].find("]]") else {
            break;
        };
        let span = &statement[open..open + close_rel];
        // A link never spans a line break, and never contains a further `[[`
        // (which would mean the true opener is later). In either case this is a
        // false opener; restart the scan just inside it.
        if span.contains('\n') || span.contains("[[") {
            i = open;
            continue;
        }
        let inner = span.trim();
        if !inner.is_empty() {
            let link = WikiLink(inner.to_owned());
            if !links.contains(&link) {
                links.push(link);
            }
        }
        i = open + close_rel + 2;
    }
    links
}

/// A human-readable name for a YAML value's type, for error messages.
fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Sequence(_) => "a list",
        Value::Mapping(_) => "a mapping",
        Value::Tagged(_) => "a tagged value",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reason text of a [`Error::Parse`], for asserting on error content.
    /// Panics if the error is not a parse error, since every parser failure in
    /// this module is one.
    fn parse_reason(err: &Error) -> &str {
        match err {
            Error::Parse { reason, .. } => reason,
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    /// The path a [`Error::Parse`] names, to prove errors point at the file.
    fn parse_path(err: &Error) -> &str {
        match err {
            Error::Parse { path, .. } => path,
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    fn nz(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }

    const FULL_CLAIM: &str = r#"---
id: payments/libfoo-pin
checks:
  - kind: cmd
    run: "grep -q 'libfoo==4.2' requirements.txt"
  - kind: agent
    instruction: Check the changelog since 5.0 for a CJK fix.
supports:
  - requirements.txt#libfoo
  - other-claim
hub:
  recheck: 30d
  max-age: 120d
---
We pin libfoo at 4.2. Versions 5.x corrupt PDF export for CJK fonts.
See [[libfoo-cjk-repro]] for the reproduction.
"#;

    #[test]
    fn parses_a_full_frontmatter_claim() {
        let claim = parse_claim_file("payments/.claims/libfoo.md", FULL_CLAIM).unwrap();

        assert_eq!(claim.id.as_str(), "payments/libfoo-pin");
        assert_eq!(claim.hub.recheck, Some(Days(nz(30))));
        assert_eq!(claim.hub.max_age, Some(Days(nz(120))));
        assert_eq!(claim.checks.len(), 2);
        assert_eq!(
            claim.checks[0].kind,
            CheckKind::Cmd {
                run: "grep -q 'libfoo==4.2' requirements.txt".to_owned(),
                negate: false,
            }
        );
        assert_eq!(
            claim.checks[1].kind,
            CheckKind::Agent {
                instruction: "Check the changelog since 5.0 for a CJK fix.".to_owned(),
            }
        );
        assert_eq!(
            claim
                .supports
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>(),
            ["requirements.txt#libfoo", "other-claim"]
        );
        assert_eq!(
            claim
                .wiki_links
                .iter()
                .map(|w| w.as_str())
                .collect::<Vec<_>>(),
            ["libfoo-cjk-repro"]
        );
        assert!(claim.statement.starts_with("We pin libfoo at 4.2."));
        assert!(claim.statement.contains("[[libfoo-cjk-repro]]"));
        assert_eq!(
            claim.source,
            Source::File {
                path: "payments/.claims/libfoo.md".to_owned()
            }
        );
    }

    #[test]
    fn parses_a_minimal_claim_with_no_hub() {
        // A claim with no `hub:` block is valid: its cadence is the hub's to decide.
        let text = "---\nid: minimal\nchecks:\n  - kind: cmd\n    run: \"true\"\n---\nStatement.\n";
        let claim = parse_claim_file("m.md", text).unwrap();
        assert_eq!(claim.id.as_str(), "minimal");
        assert!(claim.supports.is_empty());
        assert!(claim.wiki_links.is_empty());
        assert_eq!(claim.hub, Hub::default());
        assert_eq!(claim.hub.recheck, None);
        assert_eq!(claim.hub.max_age, None);
    }

    #[test]
    fn a_hub_block_validates_its_day_counts_but_is_not_acted_on() {
        let text =
            "---\nid: c\nchecks:\n  - kind: cmd\n    run: \"true\"\nhub:\n  recheck: 7d\n---\nS.\n";
        let claim = parse_claim_file("c.md", text).unwrap();
        assert_eq!(claim.hub.recheck, Some(Days(nz(7))));
        assert_eq!(claim.hub.max_age, None);
    }

    #[test]
    fn a_malformed_hub_recheck_is_a_loud_parse_error_naming_the_field() {
        // A hub never ingests garbage: a bad `recheck` is a parse error at authoring
        // time, not a silently dropped hint.
        let text = "---\nid: c\nchecks:\n  - kind: cmd\n    run: \"true\"\nhub:\n  recheck: soon\n---\nS.\n";
        let err = parse_claim_file("c.md", text).unwrap_err();
        assert!(parse_reason(&err).contains("hub.recheck"), "{err:?}");
    }

    #[test]
    fn a_bare_integer_hub_max_age_is_named_precisely() {
        let text = "---\nid: c\nchecks:\n  - kind: cmd\n    run: \"true\"\nhub:\n  max-age: 120\n---\nS.\n";
        let err = parse_claim_file("c.md", text).unwrap_err();
        assert!(parse_reason(&err).contains("hub.max-age"), "{err:?}");
        assert!(parse_reason(&err).contains("'d' suffix"), "{err:?}");
    }

    #[test]
    fn an_unknown_hub_field_is_rejected() {
        let text =
            "---\nid: c\nchecks:\n  - kind: cmd\n    run: \"true\"\nhub:\n  rechek: 7d\n---\nS.\n";
        let err = parse_claim_file("c.md", text).unwrap_err();
        assert!(
            parse_reason(&err).contains("hub: unknown field 'rechek'"),
            "{err:?}"
        );
    }

    #[test]
    fn a_top_level_max_age_is_rejected_with_migration_guidance() {
        // The v1 top-level field is now a hub hint; the error tells the author to move
        // it rather than reporting a cryptic unknown field.
        let text =
            "---\nid: c\nchecks:\n  - kind: cmd\n    run: \"true\"\nmax-age: 120d\n---\nS.\n";
        let err = parse_claim_file("c.md", text).unwrap_err();
        assert!(parse_reason(&err).contains("hub"), "{err:?}");
        assert!(parse_reason(&err).contains("max-age"), "{err:?}");
    }

    #[test]
    fn a_when_on_the_claim_is_rejected_with_guidance() {
        let text =
            "---\nid: c\nchecks:\n  - kind: cmd\n    run: \"true\"\nwhen: on-change\n---\nS.\n";
        let err = parse_claim_file("c.md", text).unwrap_err();
        assert!(parse_reason(&err).contains("'when'"), "{err:?}");
    }

    #[test]
    fn a_when_on_a_check_is_rejected_with_guidance() {
        // A leftover v1 `when:` on a check is named precisely: triggers are gone.
        let text =
            "---\nid: c\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: on-change\n---\nS.\n";
        let err = parse_claim_file("c.md", text).unwrap_err();
        assert!(parse_reason(&err).contains("when"), "{err:?}");
        assert!(parse_reason(&err).contains("hub"), "{err:?}");
    }

    #[test]
    fn parses_a_single_embedded_claim() {
        let host = "# Project notes\n\nSome unrelated prose here.\n\nWe require TLS 1.3 on the ingress.\n<!-- claim\nid: tls13-ingress\nchecks:\n  - kind: cmd\n    run: \"grep -q tls1.3 ingress.conf\"\n-->\n\nMore prose after.\n";
        let claims = extract_embedded_claims("CLAUDE.md", host).unwrap();
        assert_eq!(claims.len(), 1);
        let c = &claims[0];
        assert_eq!(c.id.as_str(), "tls13-ingress");
        assert_eq!(c.statement, "We require TLS 1.3 on the ingress.");
        match &c.source {
            Source::Embedded { path, byte_offset } => {
                assert_eq!(path, "CLAUDE.md");
                assert_eq!(
                    &host[*byte_offset..*byte_offset + EMBED_OPEN.len()],
                    EMBED_OPEN
                );
            }
            other => panic!("expected an embedded source, got {other:?}"),
        }
    }

    #[test]
    fn extracts_multiple_embedded_claims_in_one_file() {
        let host = "First fact.\n<!-- claim\nid: first\nchecks:\n  - kind: cmd\n    run: \"a\"\n-->\n\nSecond fact.\n<!-- claim\nid: second\nchecks:\n  - kind: human\n    prompt: Look at it.\n-->\n";
        let claims = extract_embedded_claims("AGENTS.md", host).unwrap();
        assert_eq!(claims.len(), 2);
        assert_eq!(claims[0].id.as_str(), "first");
        assert_eq!(claims[0].statement, "First fact.");
        assert_eq!(claims[1].id.as_str(), "second");
        assert_eq!(claims[1].statement, "Second fact.");
        assert_eq!(
            claims[1].checks[0].kind,
            CheckKind::Human {
                prompt: Some("Look at it.".to_owned())
            }
        );
        assert!(claims[0].source != claims[1].source);
    }

    #[test]
    fn file_with_no_embedded_claims_yields_empty() {
        let host = "Just a normal file.\nIt mentions <!-- not a claim --> in passing.\n";
        let claims = extract_embedded_claims("README.md", host).unwrap();
        assert!(claims.is_empty());
    }

    #[test]
    fn embedded_opener_not_on_own_line_is_ignored() {
        // Prose that mentions the marker mid-line must not be treated as a block.
        let host = "The literal string `<!-- claim` is how you open a block.\n";
        let claims = extract_embedded_claims("doc.md", host).unwrap();
        assert!(claims.is_empty());
    }

    #[test]
    fn a_missing_id_is_rejected() {
        let text = "---\nchecks:\n  - kind: cmd\n    run: \"true\"\n---\nS.\n";
        let err = parse_claim_file("c.md", text).unwrap_err();
        assert!(parse_reason(&err).contains("id"), "{err:?}");
        assert_eq!(parse_path(&err), "c.md");
    }

    #[test]
    fn a_missing_checks_field_is_rejected() {
        let text = "---\nid: c\n---\nS.\n";
        let err = parse_claim_file("c.md", text).unwrap_err();
        assert!(parse_reason(&err).contains("checks"), "{err:?}");
    }

    #[test]
    fn an_empty_checks_list_is_rejected() {
        let text = "---\nid: c\nchecks: []\n---\nS.\n";
        let err = parse_claim_file("c.md", text).unwrap_err();
        assert!(parse_reason(&err).contains("empty"), "{err:?}");
    }

    #[test]
    fn a_blank_cmd_run_is_rejected() {
        let text = "---\nid: c\nchecks:\n  - kind: cmd\n    run: \"  \"\n---\nS.\n";
        let err = parse_claim_file("c.md", text).unwrap_err();
        assert!(parse_reason(&err).contains("must not be empty"), "{err:?}");
    }

    #[test]
    fn a_missing_statement_is_rejected() {
        let text = "---\nid: c\nchecks:\n  - kind: cmd\n    run: \"true\"\n---\n\n";
        let err = parse_claim_file("c.md", text).unwrap_err();
        assert!(parse_reason(&err).contains("statement"), "{err:?}");
    }

    #[test]
    fn an_unknown_top_level_field_is_rejected() {
        let text = "---\nid: c\nchecks:\n  - kind: cmd\n    run: \"true\"\nnonsense: 1\n---\nS.\n";
        let err = parse_claim_file("c.md", text).unwrap_err();
        assert!(
            parse_reason(&err).contains("unknown field 'nonsense'"),
            "{err:?}"
        );
    }

    #[test]
    fn negate_is_parsed_on_a_cmd_check() {
        let text =
            "---\nid: c\nchecks:\n  - kind: cmd\n    run: \"true\"\n    negate: true\n---\nS.\n";
        let claim = parse_claim_file("c.md", text).unwrap();
        assert_eq!(
            claim.checks[0].kind,
            CheckKind::Cmd {
                run: "true".to_owned(),
                negate: true,
            }
        );
    }

    #[test]
    fn a_skip_block_parses_with_reason_unless_and_until() {
        let text = "---\nid: c\nchecks:\n  - kind: cmd\n    run: \"true\"\n    skip:\n      reason: no runner here\n      unless: 'test -n \"$X\"'\n      until: 2027-01-01\n---\nS.\n";
        let claim = parse_claim_file("c.md", text).unwrap();
        let skip = claim.checks[0].skip.as_ref().unwrap();
        assert_eq!(skip.reason, "no runner here");
        assert_eq!(skip.unless.as_deref(), Some("test -n \"$X\""));
        assert_eq!(skip.until, Some("2027-01-01T00:00:00Z".parse().unwrap()));
    }

    #[test]
    fn a_skip_with_no_reason_is_rejected() {
        let text = "---\nid: c\nchecks:\n  - kind: cmd\n    run: \"true\"\n    skip:\n      unless: 'true'\n---\nS.\n";
        let err = parse_claim_file("c.md", text).unwrap_err();
        assert!(parse_reason(&err).contains("reason"), "{err:?}");
    }

    #[test]
    fn day_count_parsing_rejects_non_canonical_and_zero() {
        assert!("0d".parse::<Days>().is_err());
        assert!("030d".parse::<Days>().is_err());
        assert!(" 30d".parse::<Days>().is_err());
        assert!("12.5d".parse::<Days>().is_err());
        assert!("30".parse::<Days>().is_err());
        assert_eq!("120d".parse::<Days>().unwrap(), Days(nz(120)));
    }

    #[test]
    fn days_display_round_trips() {
        assert_eq!(Days(nz(30)).to_string(), "30d");
    }

    /// Parse a standalone claim, expecting failure, and return its error.
    fn expect_err(text: &str) -> Error {
        parse_claim_file("the/file.md", text).expect_err("expected a parse error")
    }

    #[test]
    fn embedded_arrow_inside_a_value_does_not_truncate_the_block() {
        // A `-->` inside a YAML string must not be mistaken for the closing fence:
        // truncating here would silently drop the second check with no error, the
        // exact "nag never a lie" break the fence-on-own-line rule prevents.
        let host = "A fact.\n<!-- claim\nid: silent\nchecks:\n  - kind: agent\n    instruction: A --> B\n  - kind: cmd\n    run: second\n-->\n";
        let claims = extract_embedded_claims("host.md", host).unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].checks.len(), 2);
        assert_eq!(
            claims[0].checks[0].kind,
            CheckKind::Agent {
                instruction: "A --> B".to_owned()
            }
        );
        assert_eq!(
            claims[0].checks[1].kind,
            CheckKind::Cmd {
                run: "second".to_owned(),
                negate: false
            }
        );
    }

    #[test]
    fn indented_arrow_alone_in_block_scalar_does_not_truncate_the_block() {
        // A `-->` alone on an indented line is block-scalar content, not the fence.
        // Only an unindented `-->` closes the block. Matching on a trimmed line
        // (leading whitespace stripped) would truncate here and silently drop the
        // second check -- the same "nag never a lie" break as an inline arrow, via
        // an indented one.
        let host = "Fact.\n<!-- claim\nid: x\nchecks:\n  - kind: agent\n    instruction: |\n      line one\n      -->\n      line three\n  - kind: cmd\n    run: second\n-->\n";
        let claims = extract_embedded_claims("host.md", host).unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].checks.len(), 2);
        assert_eq!(
            claims[0].checks[0].kind,
            CheckKind::Agent {
                instruction: "line one\n-->\nline three\n".to_owned()
            }
        );
    }

    #[test]
    fn embedded_closer_must_be_alone_on_its_line() {
        // A line that starts with `-->` but has trailing content is not a closer;
        // only a `-->` alone on its line terminates the block. Here the arrow-led
        // line lives inside a block scalar and must be carried into the value.
        let host = "A fact.\n<!-- claim\nid: x\nchecks:\n  - kind: agent\n    instruction: |\n      step one\n      --> keep scanning\n-->\n";
        let claims = extract_embedded_claims("host.md", host).unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(
            claims[0].checks[0].kind,
            CheckKind::Agent {
                instruction: "step one\n--> keep scanning\n".to_owned()
            }
        );
    }

    #[test]
    fn indented_dashes_in_a_block_scalar_are_not_the_closing_fence() {
        // A `---` line inside a `|` block scalar is indented data, not the fence;
        // a trimmed compare would truncate the frontmatter here and then reject it.
        let text = "---\nid: a\nchecks:\n  - kind: agent\n    instruction: |\n      first line\n      ---\n      third line\n---\nStatement.\n";
        let claim = parse_claim_file("f.md", text).unwrap();
        assert_eq!(
            claim.checks[0].kind,
            CheckKind::Agent {
                instruction: "first line\n---\nthird line\n".to_owned()
            }
        );
        assert_eq!(claim.statement, "Statement.");
    }

    #[test]
    fn embedded_keyword_needs_a_boundary() {
        // `<!-- claims ...` and `<!-- claimant` are ordinary comments, not blocks,
        // so an innocent comment is not turned into a whole-file parse error.
        let host = "See the docs.\n<!-- claims are documented in docs/claims.md -->\nMore text.\n";
        let claims = extract_embedded_claims("README.md", host).unwrap();
        assert!(claims.is_empty());

        let host2 = "<!-- claimant details below -->\n";
        assert!(extract_embedded_claims("f.md", host2).unwrap().is_empty());
    }

    #[test]
    fn dense_openers_without_newlines_are_bounded_and_correct() {
        // Many `<!-- claim` substrings on a single newline-free line, none of them
        // real openers (each followed by non-boundary text), must all be skipped
        // and yield no claims — exercising the bounded backward scan.
        let host = "<!-- claimA <!-- claimB <!-- claimC ".repeat(50);
        assert!(extract_embedded_claims("f.md", &host).unwrap().is_empty());
    }

    #[test]
    fn unterminated_embedded_block_fails() {
        // A block the author opened but never closed with a `-->` on its own line is
        // a loud error, never a silently dropped claim (invariant #6).
        let host = "A fact.\n<!-- claim\nid: x\nchecks:\n  - kind: cmd\n    run: a\n";
        let err = extract_embedded_claims("host.md", host).expect_err("expected error");
        let r = parse_reason(&err);
        assert!(r.contains("unterminated") && r.contains("claim"), "{r}");
        assert_eq!(parse_path(&err), "host.md");
    }

    #[test]
    fn one_bad_embedded_block_fails_whole_extraction() {
        // A malformed block must fail extraction, not be silently skipped: a host
        // file dropping a claim it meant to declare is the quiet failure we forbid.
        // The bad block uses a space in its id, which the id validator rejects.
        let host = "Good.\n<!-- claim\nid: ok\nchecks:\n  - kind: cmd\n    run: a\n-->\n\nBad.\n<!-- claim\nid: bad id\nchecks:\n  - kind: cmd\n    run: a\n-->\n";
        let err = extract_embedded_claims("host.md", host).expect_err("expected error");
        let r = parse_reason(&err);
        assert!(r.contains("bad id") && r.contains("contains ' '"), "{r}");
    }

    #[test]
    fn harvests_wiki_links_deduped_and_ordered() {
        let s = "See [[alpha]] and [[beta]], then [[alpha]] again, and [[  spaced  ]].";
        let links = harvest_wiki_links(s);
        assert_eq!(
            links.iter().map(|w| w.as_str()).collect::<Vec<_>>(),
            ["alpha", "beta", "spaced"]
        );
    }

    #[test]
    fn empty_wiki_link_brackets_are_not_links() {
        assert!(harvest_wiki_links("nothing [[]] here and [[   ]] either").is_empty());
    }

    #[test]
    fn unbalanced_wiki_brackets_do_not_capture_greedily() {
        // A stray `[[` before a real link must not swallow the real target.
        let links = harvest_wiki_links("[[ open without close and [[real]]");
        assert_eq!(
            links.iter().map(|w| w.as_str()).collect::<Vec<_>>(),
            ["real"]
        );
    }

    #[test]
    fn wiki_links_do_not_span_a_newline() {
        // A `[[` whose `]]` is on a later line is not a link.
        let links = harvest_wiki_links("open [[start\nend]] and a real [[link]] here");
        assert_eq!(
            links.iter().map(|w| w.as_str()).collect::<Vec<_>>(),
            ["link"]
        );
    }

    #[test]
    fn wiki_link_triple_brackets_takes_innermost() {
        // `[[[x]]]` yields `x`, not `[x` with a stray leading bracket.
        let links = harvest_wiki_links("nested [[[x]]] here");
        assert_eq!(links.iter().map(|w| w.as_str()).collect::<Vec<_>>(), ["x"]);
    }

    #[test]
    fn day_count_boundary_and_overflow() {
        assert_eq!(parse_day_count("1d").unwrap(), nz(1));
        assert_eq!(
            parse_day_count(&format!("{}d", u32::MAX)).unwrap(),
            nz(u32::MAX)
        );
        // One past u32 is rejected with a distinct "too large" message, not folded
        // into the generic non-numeric error and not silently truncated.
        let err = parse_day_count("4294967296d").unwrap_err();
        assert!(err.contains("too large"), "{err}");
    }

    #[test]
    fn day_count_rejects_noncanonical_numbers() {
        // Signs, leading zeros, and surrounding or embedded whitespace are
        // rejected as firmly as `12.5d`, so `u32::from_str`'s leniency never leaks.
        for bad in ["+30d", "-30d", "030d", " 30d", "\t30d", "3 0d", "30 d"] {
            assert!(parse_day_count(bad).is_err(), "expected '{bad}' rejected");
        }
        // The leading-zero message points at the canonical spelling.
        assert!(parse_day_count("007d")
            .unwrap_err()
            .contains("leading zero"));
        // An all-zero count is reported as non-positive, not as a leading zero.
        assert!(parse_day_count("000d").unwrap_err().contains("positive"));
    }

    #[test]
    fn misspelled_negate_is_rejected_not_silently_ignored() {
        // `negated:` must not parse as `negate: false` and silently flip the
        // check's sense; an unknown field on a check is an error naming the field.
        let err = expect_err(
            "---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    negated: true\n---\nS.\n",
        );
        let r = parse_reason(&err);
        assert!(
            r.contains("checks[0]") && r.contains("unknown field 'negated'"),
            "{r}"
        );
    }

    #[test]
    fn field_belonging_to_another_kind_is_rejected() {
        // `run` on a human check, `instruction` on a cmd check: each names the
        // stray field and the kind it does not belong to.
        let human_run = expect_err("---\nid: a\nchecks:\n  - kind: human\n    run: x\n---\nS.\n");
        let r = parse_reason(&human_run);
        assert!(
            r.contains("unknown field 'run'") && r.contains("'human'"),
            "{r}"
        );

        let cmd_instruction = expect_err(
            "---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    instruction: y\n---\nS.\n",
        );
        let r = parse_reason(&cmd_instruction);
        assert!(
            r.contains("unknown field 'instruction'") && r.contains("'cmd'"),
            "{r}"
        );
    }

    #[test]
    fn malformed_yaml_fails_with_yaml_reason() {
        let err = expect_err("---\nid: a\nchecks: [unclosed\n---\nS.\n");
        assert!(
            parse_reason(&err).contains("invalid YAML"),
            "{}",
            parse_reason(&err)
        );
    }

    #[test]
    fn id_wrong_type_fails() {
        let err = expect_err("---\nid: 42\nchecks:\n  - kind: cmd\n    run: x\n---\nS.\n");
        let r = parse_reason(&err);
        assert!(
            r.starts_with("id:") && r.contains("expected a string"),
            "{r}"
        );
    }

    #[test]
    fn non_string_top_level_key_fails() {
        let err = expect_err("---\n42: x\nid: a\nchecks:\n  - kind: cmd\n    run: x\n---\nS.\n");
        assert!(
            parse_reason(&err).contains("non-string field name"),
            "{}",
            parse_reason(&err)
        );
    }

    #[test]
    fn frontmatter_that_is_a_sequence_or_scalar_fails() {
        let seq = expect_err("---\n- a\n- b\n---\nS.\n");
        let r = parse_reason(&seq);
        assert!(
            r.contains("must be a YAML mapping") && r.contains("a list"),
            "{r}"
        );

        let scalar = expect_err("---\njust a string\n---\nS.\n");
        let r = parse_reason(&scalar);
        assert!(
            r.contains("must be a YAML mapping") && r.contains("a string"),
            "{r}"
        );
    }

    #[test]
    fn check_element_not_a_mapping_fails() {
        let err = expect_err("---\nid: a\nchecks:\n  - just-a-string\n---\nS.\n");
        let r = parse_reason(&err);
        assert!(
            r.starts_with("checks[0]:") && r.contains("expected a mapping"),
            "{r}"
        );
    }

    #[test]
    fn kind_non_string_fails() {
        let err = expect_err("---\nid: a\nchecks:\n  - kind: [cmd]\n---\nS.\n");
        let r = parse_reason(&err);
        assert!(
            r.contains("checks[0].kind") && r.contains("expected a string"),
            "{r}"
        );
    }

    #[test]
    fn human_prompt_wrong_type_fails() {
        let err = expect_err("---\nid: a\nchecks:\n  - kind: human\n    prompt: [a, b]\n---\nS.\n");
        let r = parse_reason(&err);
        assert!(
            r.contains("checks[0].prompt") && r.contains("expected a string"),
            "{r}"
        );
    }

    #[test]
    fn frontmatter_with_crlf_line_endings_parses() {
        let text =
            "---\r\nid: a\r\nchecks:\r\n  - kind: cmd\r\n    run: x\r\n---\r\nStatement.\r\n";
        let claim = parse_claim_file("f.md", text).unwrap();
        assert_eq!(claim.id.as_str(), "a");
    }

    #[test]
    fn leading_bom_before_fence_parses() {
        let text = "\u{feff}---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n---\nS.\n";
        let claim = parse_claim_file("f.md", text).unwrap();
        assert_eq!(claim.id.as_str(), "a");
    }
}
