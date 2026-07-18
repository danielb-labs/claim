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

use serde_norway::Value;

use crate::error::{Error, Result};

/// A fully validated claim: a human statement, the checks that re-verify it, its
/// freshness policy, and where it came from.
///
/// Constructing a `Claim` outside this module's parsers is possible but means
/// bypassing validation; prefer [`parse_claim_file`] and
/// [`extract_embedded_claims`], which guarantee every invariant the rest of the
/// system relies on (a non-empty check list, a well-formed id, a positive
/// `max_age`). The fields are public so downstream items can read them; treat a
/// value obtained from a parser as already-validated and a hand-built one as
/// suspect.
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
    /// Validate and wrap a raw id string.
    ///
    /// Returns the reason the id is malformed (never a full [`Error`], so callers
    /// can prepend the field path and file), phrased for the author to fix.
    fn parse(raw: &str) -> std::result::Result<Self, String> {
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
/// today. Modelling this as an enum makes the exhaustive `match` in later check
/// execution a compile-time obligation: a new kind cannot be added without every
/// consumer being forced to handle it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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
/// wrong clock.
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
    /// Returns the reason on failure (not a full [`Error`]) so the caller can
    /// attach the field path and file.
    fn parse(raw: &str) -> std::result::Result<Self, String> {
        let trimmed = raw.trim();
        if trimmed == "on-change" {
            return Ok(Trigger::OnChange);
        }
        if let Some(rest) = trimmed.strip_prefix("every") {
            // Require whitespace between the keyword and the interval so that a
            // typo like `every30d` is a clear error rather than being silently
            // accepted.
            let interval = rest.trim_start();
            if interval.len() == rest.len() {
                return Err(format!(
                    "when '{raw}' is malformed; write 'every <N>d', for example 'every 30d'"
                ));
            }
            let days = parse_day_count(interval).map_err(|reason| {
                format!("when '{raw}' is malformed: {reason}; write 'every <N>d', e.g. 'every 30d'")
            })?;
            return Ok(Trigger::Every { days });
        }
        Err(format!(
            "when '{raw}' is not a recognized trigger; use 'on-change' or 'every <N>d'"
        ))
    }
}

/// A duration in whole days.
///
/// A newtype rather than a bare integer so a day count cannot be confused with
/// any other number, and so `max-age` carries meaning at the type level. Always
/// positive: a zero-day freshness window would mean a claim is stale the instant
/// it is verified, which is never what an author intends, so it is rejected at
/// parse time. This crate deliberately avoids a datetime library — turning a day
/// count into calendar arithmetic is a later item's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Days(NonZeroU32);

impl Days {
    /// The number of days, guaranteed positive.
    #[must_use]
    pub fn get(self) -> NonZeroU32 {
        self.0
    }
}

impl std::fmt::Display for Days {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}d", self.0)
    }
}

/// Parse an `<N>d` day count, e.g. `120d`, into a positive count.
///
/// Shared by [`Days`] parsing and the `every <N>d` trigger. Returns the reason
/// on failure so callers can frame it with a field path.
fn parse_day_count(raw: &str) -> std::result::Result<NonZeroU32, String> {
    let trimmed = raw.trim();
    let digits = trimmed.strip_suffix('d').ok_or_else(|| {
        format!("'{trimmed}' must be a day count ending in 'd', for example '120d'")
    })?;
    if digits.is_empty() {
        return Err(format!("'{trimmed}' has no number before 'd'"));
    }
    let n: u32 = digits
        .parse()
        .map_err(|_| format!("'{trimmed}' has a non-numeric or out-of-range day count"))?;
    NonZeroU32::new(n)
        .ok_or_else(|| format!("'{trimmed}' must be a positive number of days, not 0"))
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

/// Extract every `<!-- claim ... -->` block embedded in a host file.
///
/// A host file (CLAUDE.md, AGENTS.md, or any text file) may carry several claims.
/// Each block's statement is the non-blank text immediately preceding its
/// opener. Blocks are returned in file order. A file with no claim blocks yields
/// an empty vector, which is not an error — most files have none.
///
/// `path` is used for error messages and each returned [`Source::Embedded`].
///
/// # Errors
///
/// Returns [`Error::Parse`] naming `path` and the offending block's location
/// when a `<!-- claim` opener is never closed, its YAML is malformed, or any
/// field violates the schema. One bad block fails the whole extraction, because
/// a host file that silently drops a claim it clearly meant to declare is the
/// kind of quiet failure this tool exists to prevent.
pub fn extract_embedded_claims(path: &str, contents: &str) -> Result<Vec<Claim>> {
    let mut claims = Vec::new();
    let mut search_from = 0;
    while let Some(rel) = contents[search_from..].find(EMBED_OPEN) {
        let open_at = search_from + rel;
        // The opener must sit on its own line, per the format: text before it on
        // the same line (other than whitespace) means this is prose that merely
        // mentions the marker, not a claim block.
        let line_start = contents[..open_at].rfind('\n').map_or(0, |nl| nl + 1);
        if !contents[line_start..open_at].trim().is_empty() {
            search_from = open_at + EMBED_OPEN.len();
            continue;
        }

        let after_open = open_at + EMBED_OPEN.len();
        let close_rel = contents[after_open..].find(EMBED_CLOSE).ok_or_else(|| {
            Error::parse(
                path,
                format!(
                    "unterminated '<!-- claim' block at byte {open_at}; it must be closed with \
                     '-->'"
                ),
            )
        })?;
        let yaml = &contents[after_open..after_open + close_rel];
        let close_end = after_open + close_rel + EMBED_CLOSE.len();

        let statement = preceding_statement(&contents[..line_start]);
        let source = Source::Embedded {
            path: path.to_owned(),
            byte_offset: open_at,
        };
        claims.push(build_claim(path, yaml, &statement, source)?);
        search_from = close_end;
    }
    Ok(claims)
}

const FRONTMATTER_FENCE: &str = "---";
const EMBED_OPEN: &str = "<!-- claim";
const EMBED_CLOSE: &str = "-->";

/// Split a standalone file into its frontmatter YAML and markdown body.
///
/// The frontmatter must open with a `---` fence on the first line and close with
/// a `---` fence on its own line. Everything after the closing fence is the body.
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

    // The closing fence is the first line consisting solely of `---`.
    let mut offset = 0;
    for line in after_open.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']).trim() == FRONTMATTER_FENCE {
            let yaml = &after_open[..offset];
            let body = &after_open[offset + line.len()..];
            return Ok((yaml, body));
        }
        offset += line.len();
    }
    Err(Error::parse(
        path,
        "unterminated YAML frontmatter; the opening '---' fence has no matching closing '---'",
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
    let id = ClaimId::parse(id_raw).map_err(|reason| Error::parse(path, reason))?;

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

    let kind = match kind_raw {
        "cmd" => {
            let run = require_check_str(path, map, index, "run")?;
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
            CheckKind::Cmd {
                run: run.to_owned(),
                negate,
            }
        }
        "agent" => {
            let instruction = require_check_str(path, map, index, "instruction")?;
            CheckKind::Agent {
                instruction: instruction.to_owned(),
            }
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
            CheckKind::Human { prompt }
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
        .map_err(|reason| Error::parse(path, format!("checks[{index}].{reason}")))?;

    Ok(Check { kind, when })
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
/// Empty or whitespace-only brackets are ignored — `[[]]` is not a link. Nested
/// or unbalanced brackets are handled by taking the shortest `[[`…`]]` span, so
/// `[[a]] [[b]]` yields two links rather than one greedy match.
fn harvest_wiki_links(statement: &str) -> Vec<WikiLink> {
    let mut links: Vec<WikiLink> = Vec::new();
    let mut i = 0;
    while let Some(open_rel) = statement[i..].find("[[") {
        let open = i + open_rel + 2;
        let Some(close_rel) = statement[open..].find("]]") else {
            break;
        };
        let inner = statement[open..open + close_rel].trim();
        // A nested `[[` inside the span means the true opener is later; skip past
        // this false opener rather than capturing the outer, greedy match.
        if inner.contains("[[") {
            i = open;
            continue;
        }
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
        // Surrounding whitespace is tolerated; YAML folding can introduce it.
        assert_eq!(
            Trigger::parse("  every   7d  ").unwrap(),
            Trigger::Every { days: nz(7) }
        );
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
        assert!(
            parse_reason(&err).contains("empty"),
            "{}",
            parse_reason(&err)
        );
    }

    #[test]
    fn checks_not_a_list_fails() {
        let err = expect_err("---\nid: a\nchecks: nope\nmax-age: 1d\n---\nS.\n");
        assert!(
            parse_reason(&err).contains("checks:"),
            "{}",
            parse_reason(&err)
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
        let host = "Good.\n<!-- claim\nid: ok\nchecks:\n  - kind: cmd\n    run: a\n    when: on-change\nmax-age: 1d\n-->\n\nBad.\n<!-- claim\nid: BAD ID\nchecks:\n  - kind: cmd\n    run: a\n    when: on-change\nmax-age: 1d\n-->\n";
        let err = extract_embedded_claims("host.md", host).expect_err("expected error");
        assert!(
            parse_reason(&err).contains("BAD ID") || parse_reason(&err).contains("contains"),
            "{}",
            parse_reason(&err)
        );
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
        // One past u32 must be rejected, not silently truncated.
        assert!(parse_day_count("4294967296d").is_err());
    }

    #[test]
    fn embedded_claim_at_start_of_file_has_empty_statement() {
        // No preceding prose is a valid, if unusual, shape: an empty statement.
        let host = "<!-- claim\nid: x\nchecks:\n  - kind: cmd\n    run: a\n    when: on-change\nmax-age: 1d\n-->\n";
        let claims = extract_embedded_claims("f.md", host).unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].statement, "");
    }

    #[test]
    fn display_impls_round_trip_readable_forms() {
        assert_eq!(Days(nz(120)).to_string(), "120d");
        let claim = parse_claim_file("f.md", FULL_CLAIM).unwrap();
        assert_eq!(claim.id.to_string(), "payments/libfoo-pin");
        assert_eq!(claim.wiki_links[0].to_string(), "libfoo-cjk-repro");
        assert_eq!(claim.supports[0].to_string(), "requirements.txt#libfoo");
    }
}
