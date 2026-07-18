//! The claim format and its parser.
//!
//! A claim is a plain-language statement bound to a machine-readable policy: how
//! to re-verify the statement, on what trigger, and how long a passing check
//! keeps it fresh. This module turns bytes on disk into a validated [`Claim`],
//! or fails with an error that names the file and the exact field the author
//! must fix. It does not execute checks, resolve `supports` targets, or read the
//! verdict log; those are later concerns kept deliberately out of the parser so
//! that "is this file well-formed" stays separable from "is this fact true".
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

use serde_norway::Value;

use crate::error::{Error, Result};

/// A fully validated claim: a human statement, the checks that re-verify it, its
/// freshness policy, and where it came from.
///
/// A `Claim` is only ever produced by this module's parsers ([`parse_claim_file`]
/// and [`extract_embedded_claims`]), which is why it has no public constructor:
/// there is no way to build one that skips validation, so every `Claim` a caller
/// holds already satisfies the schema (a non-empty check list, a well-formed and
/// non-empty statement, a positive `max_age`). The fields are public for reading;
/// the `#[non_exhaustive]` attribute reserves the right to add fields without a
/// breaking change, so callers must not construct or exhaustively destructure it.
///
/// Identity and provenance are deliberately absent. Who authored or reviewed a
/// claim is derived from git and the forge at read time, never stored here,
/// because anything a file asserts about itself can be forged. Likewise there is
/// no `status` field: status is computed from the verdict log and `max_age`.
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
    /// check is a claim nothing can keep fresh, and is rejected at parse time.
    pub checks: Vec<Check>,
    /// The dead-man's switch. A passing check renews freshness for this long;
    /// once it lapses without a pass, the claim goes stale and a human is nagged.
    pub max_age: Days,
    /// Decision refs or claim ids this claim justifies (the `supports` edge).
    /// Kept as validated strings here and resolved later; an empty list means the
    /// claim stands alone. See [`SupportTarget`].
    pub supports: Vec<SupportTarget>,
    /// `[[wiki-link]]` slugs harvested from the statement body, in first-seen
    /// order with duplicates removed. Navigation edges only: parsed, never
    /// resolved, and carrying no verification consequences.
    pub wiki_links: Vec<WikiLink>,
    /// Where this claim was found, for diagnostics and later tooling.
    pub source: Source,
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
    /// bare id — a verdict-log path, a `claim add` argument — with no file to name.
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
    /// without a claim file — the verdict-log path `.claims/log/<id>/` and the
    /// `claim add` id argument. The error's reason is self-contained (it quotes
    /// the offending id), so the `id` context in the path position is only a
    /// label.
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
/// A claim may carry several checks at different speeds — a cheap command on
/// every change, an expensive agent investigation on a slow clock. The [`kind`]
/// carries the check-type-specific payload; [`when`] carries the trigger shared
/// by all kinds.
///
/// [`kind`]: Check::kind
/// [`when`]: Check::when
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Check {
    /// The check's mechanism and its payload.
    pub kind: CheckKind,
    /// When this check should run.
    pub when: Trigger,
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

/// When a check should run.
///
/// v1 recognizes exactly two triggers. Anything else is rejected rather than
/// guessed at, because a trigger the tool misreads is a check that runs on the
/// wrong clock. Like [`CheckKind`] this is intentionally not `#[non_exhaustive]`,
/// so a future trigger form forces every scheduler consumer to handle it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// Run whenever a file the check watches changes. In v1 this means the check
    /// runs on every pull request.
    OnChange,
    /// Run on a fixed cadence, every `days` days.
    Every {
        /// The interval in days; always positive.
        days: NonZeroU32,
    },
}

impl Trigger {
    /// Parse a `when:` value: the literal `on-change`, or `every <N>d` with `N` a
    /// positive integer.
    ///
    /// Returns a bare reason on failure (not a full [`Error`], and without a field
    /// prefix), so the caller frames it as `checks[i].when: {reason}` in its own
    /// `field: message` style.
    fn parse(raw: &str) -> std::result::Result<Self, String> {
        if raw == "on-change" {
            return Ok(Trigger::OnChange);
        }
        // The interval must follow `every` separated by exactly one space, so a
        // typo like `every30d` or padded forms like `every  30d` are clear errors
        // rather than being silently normalized. `parse_day_count` then rejects
        // any surrounding or embedded whitespace in the count itself.
        if let Some(interval) = raw.strip_prefix("every ") {
            let days = parse_day_count(interval).map_err(|reason| {
                format!("'{raw}' is malformed ({reason}); write 'every <N>d', e.g. 'every 30d'")
            })?;
            return Ok(Trigger::Every { days });
        }
        Err(format!(
            "'{raw}' is not a recognized trigger; use 'on-change' or 'every <N>d'"
        ))
    }
}

/// A duration in whole days.
///
/// A newtype rather than a bare integer so a day count cannot be confused with
/// any other number, and so `max-age` carries meaning at the type level. Always
/// positive: a zero-day freshness window would mean a claim is stale the instant
/// it is verified, which is never what an author intends, so it is rejected at
/// parse time. The verdict log turns a day count into instant arithmetic
/// (`crate::log`), treating a day as an unambiguous 24 hours against a UTC
/// instant.
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
    /// `const` so callers can build compile-time day constants (a policy default,
    /// a grace window) without routing through the string parser and an
    /// `unwrap`. Takes a [`NonZeroU32`] so the positivity guarantee is discharged
    /// by the type, not re-checked here.
    #[must_use]
    pub const fn from_nonzero(days: NonZeroU32) -> Self {
        Days(days)
    }
}

impl FromStr for Days {
    type Err = Error;

    /// Validate a bare `<N>d` day count outside any file, for callers that hold a
    /// duration string without a claim file. The error's reason quotes the input,
    /// so the `max-age` context in the path position is only a label.
    fn from_str(raw: &str) -> Result<Self> {
        parse_day_count(raw)
            .map(Days)
            .map_err(|reason| Error::parse("max-age", reason))
    }
}

impl std::fmt::Display for Days {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}d", self.0)
    }
}

/// Parse an `<N>d` day count, e.g. `120d`, into a positive count.
///
/// Shared by [`Days`] and the `every <N>d` trigger. The number must be a bare
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
                "claim has no fields; 'id', 'checks', and 'max-age' are required",
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
    let max_age = parse_max_age(path, map)?;
    let supports = parse_supports(path, map)?;
    let wiki_links = harvest_wiki_links(statement);

    Ok(Claim {
        id,
        statement: statement.to_owned(),
        checks,
        max_age,
        supports,
        wiki_links,
        source,
    })
}

/// The recognized top-level claim fields. Anything else is flagged so a typo
/// like `check:` or `maxage:` fails loudly instead of being silently ignored.
const KNOWN_FIELDS: &[&str] = &["id", "checks", "max-age", "supports"];

fn reject_unknown_fields(path: &str, map: &serde_norway::Mapping) -> Result<()> {
    for key in map.keys() {
        let name = key.as_str().ok_or_else(|| {
            Error::parse(
                path,
                "claim has a non-string field name; keys must be strings",
            )
        })?;
        if !KNOWN_FIELDS.contains(&name) {
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

    // The fields each kind permits, always including the shared `kind` and
    // `when`. A field outside this set is rejected, mirroring top-level
    // `reject_unknown_fields`: a typo like `negated:` must fail loudly, never be
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
                &["kind", "run", "negate", "when"],
            )
        }
        "agent" => {
            let instruction = require_check_str(path, map, index, "instruction")?;
            (
                CheckKind::Agent {
                    instruction: instruction.to_owned(),
                },
                &["kind", "instruction", "when"],
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
            (CheckKind::Human { prompt }, &["kind", "prompt", "when"])
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

    let when_raw = map
        .get("when")
        .ok_or_else(|| {
            Error::parse(
                path,
                format!(
                    "{}: missing 'when'; use 'on-change' or 'every <N>d'",
                    field("when")
                ),
            )
        })?
        .as_str()
        .ok_or_else(|| {
            Error::parse(
                path,
                format!("{}: expected a string trigger", field("when")),
            )
        })?;
    let when = Trigger::parse(when_raw)
        .map_err(|reason| Error::parse(path, format!("{}: {reason}", field("when"))))?;

    Ok(Check { kind, when })
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

fn parse_max_age(path: &str, map: &serde_norway::Mapping) -> Result<Days> {
    let raw = map.get("max-age").ok_or_else(|| {
        Error::parse(
            path,
            "missing required field 'max-age'; write it as '<N>d', e.g. '120d'",
        )
    })?;
    // Accept `120d` as a string. A bare integer in YAML (`max-age: 120`) is a
    // common mistake worth naming precisely rather than reporting as a type error.
    let text = match raw {
        Value::String(s) => s.as_str(),
        Value::Number(_) => {
            return Err(Error::parse(
                path,
                "max-age: write the day count with a 'd' suffix, e.g. '120d', not a bare number",
            ));
        }
        other => {
            return Err(Error::parse(
                path,
                format!(
                    "max-age: expected a duration like '120d', found {}",
                    value_kind(other)
                ),
            ));
        }
    };
    let days =
        parse_day_count(text).map_err(|reason| Error::parse(path, format!("max-age: {reason}")))?;
    Ok(Days(days))
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
    when: on-change
  - kind: agent
    instruction: Check the changelog since 5.0 for a CJK fix.
    when: every 30d
max-age: 120d
supports:
  - requirements.txt#libfoo
  - other-claim
---
We pin libfoo at 4.2. Versions 5.x corrupt PDF export for CJK fonts.
See [[libfoo-cjk-repro]] for the reproduction.
"#;

    #[test]
    fn parses_a_full_frontmatter_claim() {
        let claim = parse_claim_file("payments/.claims/libfoo.md", FULL_CLAIM).unwrap();

        assert_eq!(claim.id.as_str(), "payments/libfoo-pin");
        assert_eq!(claim.max_age, Days(nz(120)));
        assert_eq!(claim.checks.len(), 2);
        assert_eq!(
            claim.checks[0].kind,
            CheckKind::Cmd {
                run: "grep -q 'libfoo==4.2' requirements.txt".to_owned(),
                negate: false,
            }
        );
        assert_eq!(claim.checks[0].when, Trigger::OnChange);
        assert_eq!(
            claim.checks[1].kind,
            CheckKind::Agent {
                instruction: "Check the changelog since 5.0 for a CJK fix.".to_owned(),
            }
        );
        assert_eq!(claim.checks[1].when, Trigger::Every { days: nz(30) });
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
    fn parses_a_minimal_claim_with_defaults() {
        let text = "---\nid: minimal\nchecks:\n  - kind: cmd\n    run: \"true\"\n    when: on-change\nmax-age: 7d\n---\nStatement.\n";
        let claim = parse_claim_file("m.md", text).unwrap();
        assert_eq!(claim.id.as_str(), "minimal");
        assert!(claim.supports.is_empty());
        assert!(claim.wiki_links.is_empty());
        assert_eq!(claim.max_age, Days(nz(7)));
    }

    #[test]
    fn parses_a_single_embedded_claim() {
        let host = "# Project notes\n\nSome unrelated prose here.\n\nWe require TLS 1.3 on the ingress.\n<!-- claim\nid: tls13-ingress\nchecks:\n  - kind: cmd\n    run: \"grep -q tls1.3 ingress.conf\"\n    when: on-change\nmax-age: 90d\n-->\n\nMore prose after.\n";
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
        let host = "First fact.\n<!-- claim\nid: first\nchecks:\n  - kind: cmd\n    run: \"a\"\n    when: on-change\nmax-age: 1d\n-->\n\nSecond fact.\n<!-- claim\nid: second\nchecks:\n  - kind: human\n    prompt: Look at it.\n    when: every 14d\nmax-age: 30d\n-->\n";
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
        let claims = extract_embedded_claims("docs.md", host).unwrap();
        assert!(claims.is_empty());
    }

    #[test]
    fn embedded_statement_is_only_the_adjacent_paragraph() {
        let host = "Earlier unrelated paragraph.\n\nThe real statement.\n<!-- claim\nid: x\nchecks:\n  - kind: cmd\n    run: \"a\"\n    when: on-change\nmax-age: 1d\n-->\n";
        let claims = extract_embedded_claims("f.md", host).unwrap();
        assert_eq!(claims[0].statement, "The real statement.");
    }

    #[test]
    fn all_three_trigger_forms_parse() {
        assert_eq!(Trigger::parse("on-change").unwrap(), Trigger::OnChange);
        assert_eq!(
            Trigger::parse("every 30d").unwrap(),
            Trigger::Every { days: nz(30) }
        );
        assert_eq!(
            Trigger::parse("every 1d").unwrap(),
            Trigger::Every { days: nz(1) }
        );
    }

    #[test]
    fn trigger_rejects_noncanonical_spacing() {
        // The trigger is a fixed syntax, not free-form: padded or doubled spaces
        // are errors, so a misread trigger cannot run a check on the wrong clock.
        for bad in [" every 30d", "every  30d", "every 30d ", "on-change "] {
            assert!(
                Trigger::parse(bad).is_err(),
                "expected '{bad}' to be rejected"
            );
        }
    }

    #[test]
    fn max_age_parses_various_positive_days() {
        for (text, days) in [("1d", 1u32), ("30d", 30), ("120d", 120), ("3650d", 3650)] {
            let src = format!(
                "---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: {text}\n---\nS.\n"
            );
            let claim = parse_claim_file("f.md", &src).unwrap();
            assert_eq!(claim.max_age, Days(nz(days)));
        }
    }

    #[test]
    fn negate_defaults_to_false_and_reads_explicit() {
        let base = |neg: &str| {
            format!("---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\n{neg}max-age: 1d\n---\nS.\n")
        };
        let default = parse_claim_file("f.md", &base("")).unwrap();
        assert_eq!(
            default.checks[0].kind,
            CheckKind::Cmd {
                run: "x".to_owned(),
                negate: false
            }
        );
        let explicit_true = parse_claim_file("f.md", &base("    negate: true\n")).unwrap();
        assert_eq!(
            explicit_true.checks[0].kind,
            CheckKind::Cmd {
                run: "x".to_owned(),
                negate: true
            }
        );
        let explicit_false = parse_claim_file("f.md", &base("    negate: false\n")).unwrap();
        assert_eq!(
            explicit_false.checks[0].kind,
            CheckKind::Cmd {
                run: "x".to_owned(),
                negate: false
            }
        );
    }

    #[test]
    fn empty_or_blank_run_is_rejected() {
        // A blank `run` would execute as `sh -c ""`, exit 0, and report Held
        // forever — a vacuous pass that can never go red. It is rejected at parse
        // time alongside empty id/statement/supports. A comment-only command is
        // blank once the shell strips it, but the parser cannot know that; those
        // are caught defensively at execution (see check.rs), so here only the
        // syntactically-empty forms must fail.
        for bad in ["\"\"", "\"   \"", "\"\\t\""] {
            let src = format!(
                "---\nid: a\nchecks:\n  - kind: cmd\n    run: {bad}\n    when: on-change\nmax-age: 1d\n---\nS.\n"
            );
            let err = expect_err(&src);
            assert!(
                parse_reason(&err).contains("must not be empty"),
                "run {bad} should be rejected: {}",
                parse_reason(&err)
            );
        }
    }

    #[test]
    fn all_three_check_kinds_round_trip() {
        let text = "---\nid: kinds\nchecks:\n  - kind: cmd\n    run: run-me\n    when: on-change\n  - kind: agent\n    instruction: investigate\n    when: every 30d\n  - kind: human\n    prompt: eyeball it\n    when: every 90d\nmax-age: 1d\n---\nS.\n";
        let claim = parse_claim_file("f.md", text).unwrap();
        assert_eq!(
            claim.checks[0].kind,
            CheckKind::Cmd {
                run: "run-me".to_owned(),
                negate: false
            }
        );
        assert_eq!(
            claim.checks[1].kind,
            CheckKind::Agent {
                instruction: "investigate".to_owned()
            }
        );
        assert_eq!(
            claim.checks[2].kind,
            CheckKind::Human {
                prompt: Some("eyeball it".to_owned())
            }
        );
    }

    #[test]
    fn human_check_prompt_is_optional() {
        let text =
            "---\nid: h\nchecks:\n  - kind: human\n    when: every 90d\nmax-age: 1d\n---\nS.\n";
        let claim = parse_claim_file("f.md", text).unwrap();
        assert_eq!(claim.checks[0].kind, CheckKind::Human { prompt: None });
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
    fn frontmatter_fence_detection_accepts_exactly_the_parser_opening_fence() {
        // A store scanner uses this to tell a claim from a plain doc. It must accept
        // every first line the parser accepts as a fence, or a fenced-but-broken
        // claim could be skipped instead of erroring loudly; it may accept *more*
        // (the parser is additionally stricter), which only ever makes the parser
        // louder, never the scanner quieter.
        assert!(has_frontmatter_fence("---\nid: a\n---\nS.\n"));
        assert!(has_frontmatter_fence("---   \nid: a\n---\nS.\n")); // trailing space is fine
        assert!(has_frontmatter_fence("\u{feff}---\nid: a\n")); // a leading BOM is tolerated
                                                                // A bare `---` with no body is accepted as a fence here but the parser rejects
                                                                // it (no body) — the safe divergence: the scanner keeps it, the parser is loud.
        assert!(has_frontmatter_fence("---"));
        assert!(parse_claim_file("f.md", "---").is_err());

        assert!(!has_frontmatter_fence("# README\n\nnot a claim\n"));
        assert!(!has_frontmatter_fence("")); // empty file
        assert!(!has_frontmatter_fence("----\nid: a\n")); // four dashes is not the fence
        assert!(!has_frontmatter_fence("---foo\nid: a\n")); // fence must be alone on its line
        assert!(!has_frontmatter_fence(" ---\nid: a\n")); // leading space disqualifies it
    }

    // --- Negative paths: every validation must fail loudly and name the fix. ---

    /// Parse a standalone claim, expecting failure, and return its error.
    fn expect_err(text: &str) -> Error {
        parse_claim_file("the/file.md", text).expect_err("expected a parse error")
    }

    #[test]
    fn missing_frontmatter_fences_fail() {
        let err = expect_err("id: a\nchecks: []\nmax-age: 1d\nNo fences at all.\n");
        assert_eq!(parse_path(&err), "the/file.md");
        assert!(
            parse_reason(&err).contains("frontmatter"),
            "{}",
            parse_reason(&err)
        );
    }

    #[test]
    fn unterminated_frontmatter_fails() {
        let err = expect_err("---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: 1d\nStatement but no closing fence.\n");
        assert!(
            parse_reason(&err).contains("unterminated"),
            "{}",
            parse_reason(&err)
        );
    }

    #[test]
    fn malformed_yaml_fails_with_yaml_reason() {
        let err = expect_err("---\nid: a\nchecks: [unclosed\nmax-age: 1d\n---\nS.\n");
        assert!(
            parse_reason(&err).contains("invalid YAML"),
            "{}",
            parse_reason(&err)
        );
    }

    #[test]
    fn missing_id_fails() {
        let err = expect_err(
            "---\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: 1d\n---\nS.\n",
        );
        assert!(
            parse_reason(&err).contains("'id'"),
            "{}",
            parse_reason(&err)
        );
    }

    #[test]
    fn empty_id_fails() {
        let err = expect_err("---\nid: \"\"\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: 1d\n---\nS.\n");
        assert!(
            parse_reason(&err).contains("id must not be empty"),
            "{}",
            parse_reason(&err)
        );
    }

    #[test]
    fn malformed_id_uppercase_fails() {
        let err = expect_err("---\nid: LibFoo\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: 1d\n---\nS.\n");
        let r = parse_reason(&err);
        assert!(
            r.contains("lowercase") || r.contains("only lowercase"),
            "{r}"
        );
    }

    #[test]
    fn malformed_id_bad_char_fails() {
        let err = expect_err("---\nid: lib_foo\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: 1d\n---\nS.\n");
        assert!(parse_reason(&err).contains('_'), "{}", parse_reason(&err));
    }

    #[test]
    fn malformed_id_empty_segment_fails() {
        for bad in ["/leading", "trailing/", "double//slash"] {
            let src = format!("---\nid: {bad}\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: 1d\n---\nS.\n");
            let err = expect_err(&src);
            assert!(
                parse_reason(&err).contains("empty path segment"),
                "{bad}: {}",
                parse_reason(&err)
            );
        }
    }

    #[test]
    fn id_segment_with_edge_hyphen_fails() {
        let err = expect_err("---\nid: a/-bad\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: 1d\n---\nS.\n");
        assert!(
            parse_reason(&err).contains("hyphen"),
            "{}",
            parse_reason(&err)
        );
    }

    #[test]
    fn missing_checks_fails() {
        let err = expect_err("---\nid: a\nmax-age: 1d\n---\nS.\n");
        assert!(
            parse_reason(&err).contains("'checks'"),
            "{}",
            parse_reason(&err)
        );
    }

    #[test]
    fn empty_checks_list_fails() {
        let err = expect_err("---\nid: a\nchecks: []\nmax-age: 1d\n---\nS.\n");
        let r = parse_reason(&err);
        assert!(
            r.starts_with("checks:") && r.contains("the list is empty"),
            "{r}"
        );
    }

    #[test]
    fn checks_not_a_list_fails() {
        let err = expect_err("---\nid: a\nchecks: nope\nmax-age: 1d\n---\nS.\n");
        let r = parse_reason(&err);
        assert!(
            r.starts_with("checks:") && r.contains("expected a list"),
            "{r}"
        );
    }

    #[test]
    fn unknown_check_kind_fails() {
        let err = expect_err(
            "---\nid: a\nchecks:\n  - kind: webhook\n    when: on-change\nmax-age: 1d\n---\nS.\n",
        );
        let r = parse_reason(&err);
        assert!(r.contains("checks[0].kind"), "{r}");
        assert!(r.contains("webhook"), "{r}");
    }

    #[test]
    fn check_missing_kind_fails() {
        let err = expect_err(
            "---\nid: a\nchecks:\n  - run: x\n    when: on-change\nmax-age: 1d\n---\nS.\n",
        );
        assert!(
            parse_reason(&err).contains("checks[0].kind"),
            "{}",
            parse_reason(&err)
        );
    }

    #[test]
    fn cmd_without_run_fails() {
        let err = expect_err(
            "---\nid: a\nchecks:\n  - kind: cmd\n    when: on-change\nmax-age: 1d\n---\nS.\n",
        );
        assert_eq!(parse_reason(&err), "checks[0].run: missing required field");
    }

    #[test]
    fn cmd_run_wrong_type_fails() {
        let err = expect_err("---\nid: a\nchecks:\n  - kind: cmd\n    run: [not, a, string]\n    when: on-change\nmax-age: 1d\n---\nS.\n");
        let r = parse_reason(&err);
        assert!(
            r.contains("checks[0].run") && r.contains("expected a string"),
            "{r}"
        );
    }

    #[test]
    fn agent_without_instruction_fails() {
        let err = expect_err(
            "---\nid: a\nchecks:\n  - kind: agent\n    when: every 30d\nmax-age: 1d\n---\nS.\n",
        );
        assert_eq!(
            parse_reason(&err),
            "checks[0].instruction: missing required field"
        );
    }

    #[test]
    fn negate_wrong_type_fails() {
        let err = expect_err("---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    negate: yes-please\n    when: on-change\nmax-age: 1d\n---\nS.\n");
        assert!(
            parse_reason(&err).contains("checks[0].negate"),
            "{}",
            parse_reason(&err)
        );
    }

    #[test]
    fn missing_when_fails() {
        let err =
            expect_err("---\nid: a\nchecks:\n  - kind: cmd\n    run: x\nmax-age: 1d\n---\nS.\n");
        assert!(
            parse_reason(&err).contains("checks[0].when"),
            "{}",
            parse_reason(&err)
        );
    }

    #[test]
    fn malformed_when_unknown_form_fails() {
        let err = expect_err("---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: sometimes\nmax-age: 1d\n---\nS.\n");
        let r = parse_reason(&err);
        assert!(
            r.contains("checks[0].when") && r.contains("sometimes"),
            "{r}"
        );
    }

    #[test]
    fn malformed_when_every_variants_fail() {
        for bad in [
            "every 30",
            "every 0d",
            "every -5d",
            "every30d",
            "every d",
            "every xd",
        ] {
            let src = format!("---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: \"{bad}\"\nmax-age: 1d\n---\nS.\n");
            let err = expect_err(&src);
            assert!(
                parse_reason(&err).contains("checks[0].when"),
                "{bad}: {}",
                parse_reason(&err)
            );
        }
    }

    #[test]
    fn missing_max_age_fails() {
        let err = expect_err(
            "---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\n---\nS.\n",
        );
        assert!(
            parse_reason(&err).contains("'max-age'"),
            "{}",
            parse_reason(&err)
        );
    }

    #[test]
    fn zero_max_age_fails() {
        let err = expect_err("---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: 0d\n---\nS.\n");
        let r = parse_reason(&err);
        assert!(r.contains("max-age") && r.contains("positive"), "{r}");
    }

    #[test]
    fn malformed_max_age_variants_fail() {
        for bad in ["120", "d", "12days", "abc", "12.5d"] {
            let src = format!("---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: \"{bad}\"\n---\nS.\n");
            let err = expect_err(&src);
            assert!(
                parse_reason(&err).contains("max-age"),
                "{bad}: {}",
                parse_reason(&err)
            );
        }
    }

    #[test]
    fn bare_number_max_age_is_named_precisely() {
        let err = expect_err("---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: 120\n---\nS.\n");
        assert!(
            parse_reason(&err).contains("'d' suffix"),
            "{}",
            parse_reason(&err)
        );
    }

    #[test]
    fn unknown_top_level_field_fails() {
        let err = expect_err("---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: 1d\nowner: alice\n---\nS.\n");
        let r = parse_reason(&err);
        assert!(r.contains("unknown field 'owner'"), "{r}");
    }

    #[test]
    fn supports_wrong_element_type_fails() {
        let err = expect_err("---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: 1d\nsupports:\n  - 42\n---\nS.\n");
        let r = parse_reason(&err);
        assert!(
            r.contains("supports[0]") && r.contains("expected a string"),
            "{r}"
        );
    }

    #[test]
    fn empty_support_target_fails() {
        let err = expect_err("---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: 1d\nsupports:\n  - \"\"\n---\nS.\n");
        assert!(
            parse_reason(&err).contains("supports[0]"),
            "{}",
            parse_reason(&err)
        );
    }

    #[test]
    fn empty_frontmatter_names_required_fields() {
        let err = expect_err("---\n---\nS.\n");
        assert!(
            parse_reason(&err).contains("required"),
            "{}",
            parse_reason(&err)
        );
    }

    #[test]
    fn unterminated_embedded_block_fails() {
        let host = "A fact.\n<!-- claim\nid: x\nchecks:\n  - kind: cmd\n    run: a\n    when: on-change\nmax-age: 1d\n";
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
        let host = "Good.\n<!-- claim\nid: ok\nchecks:\n  - kind: cmd\n    run: a\n    when: on-change\nmax-age: 1d\n-->\n\nBad.\n<!-- claim\nid: bad id\nchecks:\n  - kind: cmd\n    run: a\n    when: on-change\nmax-age: 1d\n-->\n";
        let err = extract_embedded_claims("host.md", host).expect_err("expected error");
        let r = parse_reason(&err);
        assert!(r.contains("bad id") && r.contains("contains ' '"), "{r}");
    }

    #[test]
    fn frontmatter_with_crlf_line_endings_parses() {
        let text = "---\r\nid: a\r\nchecks:\r\n  - kind: cmd\r\n    run: x\r\n    when: on-change\r\nmax-age: 1d\r\n---\r\nStatement.\r\n";
        let claim = parse_claim_file("f.md", text).unwrap();
        assert_eq!(claim.id.as_str(), "a");
    }

    #[test]
    fn leading_bom_before_fence_parses() {
        let text = "\u{feff}---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: 1d\n---\nS.\n";
        let claim = parse_claim_file("f.md", text).unwrap();
        assert_eq!(claim.id.as_str(), "a");
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
    fn max_age_and_every_share_strict_numeric_rules() {
        // The same non-canonical spellings are rejected in both fields.
        for bad in ["+30", "030", "30 "] {
            let ma = format!("---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: \"{bad}d\"\n---\nS.\n");
            assert!(
                expect_err(&ma).to_string().contains("max-age"),
                "max-age {bad}"
            );
            assert!(
                Trigger::parse(&format!("every {bad}d")).is_err(),
                "every {bad}d"
            );
        }
    }

    #[test]
    fn embedded_claim_with_no_preceding_prose_fails() {
        // The statement is required in both host formats; an embedded block with
        // no prose above it is an error that points the author at the fix.
        let host = "<!-- claim\nid: x\nchecks:\n  - kind: cmd\n    run: a\n    when: on-change\nmax-age: 1d\n-->\n";
        let err = extract_embedded_claims("f.md", host).expect_err("expected an error");
        let r = parse_reason(&err);
        assert!(
            r.contains("no statement") && r.contains("before the"),
            "{r}"
        );
    }

    #[test]
    fn file_claim_with_no_body_fails() {
        let text = "---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: 1d\n---\n\n   \n";
        let err = expect_err(text);
        let r = parse_reason(&err);
        assert!(
            r.contains("no statement") && r.contains("markdown body"),
            "{r}"
        );
    }

    #[test]
    fn display_impls_round_trip_readable_forms() {
        assert_eq!(Days(nz(120)).to_string(), "120d");
        let claim = parse_claim_file("f.md", FULL_CLAIM).unwrap();
        assert_eq!(claim.id.to_string(), "payments/libfoo-pin");
        assert_eq!(claim.wiki_links[0].to_string(), "libfoo-cjk-repro");
        assert_eq!(claim.supports[0].to_string(), "requirements.txt#libfoo");
    }

    #[test]
    fn embedded_arrow_inside_a_value_does_not_truncate_the_block() {
        // A `-->` inside a YAML string must not be mistaken for the closing fence:
        // truncating here would silently drop the second check with no error, the
        // exact "nag never a lie" break the fence-on-own-line rule prevents.
        let host = "A fact.\n<!-- claim\nid: silent\nmax-age: 1d\nchecks:\n  - kind: agent\n    when: on-change\n    instruction: A --> B\n  - kind: cmd\n    run: second\n    when: on-change\n-->\n";
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
        let host = "Fact.\n<!-- claim\nid: x\nmax-age: 1d\nchecks:\n  - kind: agent\n    when: on-change\n    instruction: |\n      line one\n      -->\n      line three\n  - kind: cmd\n    run: second\n    when: on-change\n-->\n";
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
        let host = "A fact.\n<!-- claim\nid: x\nchecks:\n  - kind: agent\n    when: on-change\n    instruction: |\n      step one\n      --> keep scanning\nmax-age: 1d\n-->\n";
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
        let text = "---\nid: a\nchecks:\n  - kind: agent\n    when: every 30d\n    instruction: |\n      first line\n      ---\n      third line\nmax-age: 1d\n---\nStatement.\n";
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
    fn misspelled_negate_is_rejected_not_silently_ignored() {
        // `negated:` must not parse as `negate: false` and silently flip the
        // check's sense; an unknown field on a check is an error naming the field.
        let err = expect_err("---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    negated: true\n    when: on-change\nmax-age: 1d\n---\nS.\n");
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
        let human_run = expect_err("---\nid: a\nchecks:\n  - kind: human\n    run: x\n    when: on-change\nmax-age: 1d\n---\nS.\n");
        let r = parse_reason(&human_run);
        assert!(
            r.contains("unknown field 'run'") && r.contains("'human'"),
            "{r}"
        );

        let cmd_instruction = expect_err("---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    instruction: y\n    when: on-change\nmax-age: 1d\n---\nS.\n");
        let r = parse_reason(&cmd_instruction);
        assert!(
            r.contains("unknown field 'instruction'") && r.contains("'cmd'"),
            "{r}"
        );
    }

    #[test]
    fn id_wrong_type_fails() {
        let err = expect_err("---\nid: 42\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: 1d\n---\nS.\n");
        let r = parse_reason(&err);
        assert!(
            r.starts_with("id:") && r.contains("expected a string"),
            "{r}"
        );
    }

    #[test]
    fn non_string_top_level_key_fails() {
        let err = expect_err("---\n42: x\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: 1d\n---\nS.\n");
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
        let err = expect_err("---\nid: a\nchecks:\n  - just-a-string\nmax-age: 1d\n---\nS.\n");
        let r = parse_reason(&err);
        assert!(
            r.starts_with("checks[0]:") && r.contains("expected a mapping"),
            "{r}"
        );
    }

    #[test]
    fn kind_non_string_fails() {
        let err = expect_err(
            "---\nid: a\nchecks:\n  - kind: [cmd]\n    when: on-change\nmax-age: 1d\n---\nS.\n",
        );
        let r = parse_reason(&err);
        assert!(
            r.contains("checks[0].kind") && r.contains("expected a string"),
            "{r}"
        );
    }

    #[test]
    fn when_non_string_fails() {
        let err = expect_err("---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: [on-change]\nmax-age: 1d\n---\nS.\n");
        let r = parse_reason(&err);
        assert!(
            r.contains("checks[0].when") && r.contains("expected a string"),
            "{r}"
        );
    }

    #[test]
    fn human_prompt_wrong_type_fails() {
        let err = expect_err("---\nid: a\nchecks:\n  - kind: human\n    prompt: [a, b]\n    when: on-change\nmax-age: 1d\n---\nS.\n");
        let r = parse_reason(&err);
        assert!(
            r.contains("checks[0].prompt") && r.contains("expected a string"),
            "{r}"
        );
    }

    #[test]
    fn max_age_as_a_list_fails() {
        let err = expect_err("---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: [1d]\n---\nS.\n");
        let r = parse_reason(&err);
        assert!(r.starts_with("max-age:") && r.contains("a list"), "{r}");
    }

    #[test]
    fn supports_not_a_list_fails() {
        let err = expect_err("---\nid: a\nchecks:\n  - kind: cmd\n    run: x\n    when: on-change\nmax-age: 1d\nsupports: requirements.txt#libfoo\n---\nS.\n");
        let r = parse_reason(&err);
        assert!(
            r.starts_with("supports:") && r.contains("expected a list"),
            "{r}"
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
    fn claim_id_from_str_validates_bare_ids() {
        assert_eq!(
            "payments/libfoo-pin".parse::<ClaimId>().unwrap().as_str(),
            "payments/libfoo-pin"
        );
        // The public entry point rejects the same shapes the file parser does, and
        // its error reason quotes the offending id.
        let err = "Bad Id".parse::<ClaimId>().unwrap_err();
        assert!(
            parse_reason(&err).contains("Bad Id"),
            "{}",
            parse_reason(&err)
        );
        assert!("trailing/".parse::<ClaimId>().is_err());
    }

    #[test]
    fn days_from_str_validates_bare_durations() {
        assert_eq!("120d".parse::<Days>().unwrap(), Days(nz(120)));
        assert!("0d".parse::<Days>().is_err());
        assert!("120".parse::<Days>().is_err());
        assert!("+5d".parse::<Days>().is_err());
    }

    #[test]
    fn days_get_returns_plain_u32() {
        let d: Days = "30d".parse().unwrap();
        // No second unwrap needed at call sites doing arithmetic.
        assert_eq!(d.get(), 30u32);
        assert_eq!(d.get() * 2, 60);
    }

    #[test]
    fn dense_openers_without_newlines_are_bounded_and_correct() {
        // Many `<!-- claim` substrings on a single newline-free line, none of them
        // real openers (each followed by non-boundary text), must all be skipped
        // and yield no claims — exercising the bounded backward scan.
        let host = "<!-- claimA <!-- claimB <!-- claimC ".repeat(50);
        assert!(extract_embedded_claims("f.md", &host).unwrap().is_empty());
    }
}
