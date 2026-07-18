//! Rendering a claim to the on-disk `.claims/*.md` format.
//!
//! `claim add` builds a claim's frontmatter and body as text, then hands that
//! exact text to [`claim_core::parse_claim_file`] to validate it before writing —
//! so the file the tool commits is proven to parse, not merely assembled and
//! hoped-correct. Keeping the rendering here (rather than pulling a YAML serializer
//! into the CLI) means the tool writes only the small, fixed set of fields it
//! understands, in a stable shape a human will read in review.

use claim_core::{Check, CheckKind, Claim};

/// Render a claim's fields to the `.claims/*.md` text: `---`-fenced YAML
/// frontmatter followed by the statement body.
///
/// The output is deliberately minimal and canonical — only the fields the tool set,
/// each on its own line — so two claims authored the same way produce byte-identical
/// files and a reviewer reads a predictable shape. It is *not* a general YAML
/// emitter; it emits exactly the v1 schema. `run` is quoted so shell metacharacters
/// in a command (`|`, `#`, `:`) cannot break the YAML, and any embedded quote is
/// escaped.
///
/// The rendered text is round-tripped through [`claim_core::parse_claim_file`] by
/// the caller ([`render_and_validate`]) so a rendering bug is caught before the
/// file is written, never after it is committed.
#[must_use]
pub fn render(claim: &ClaimDraft) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("id: {}\n", claim.id));
    out.push_str("checks:\n");
    for check in &claim.checks {
        render_check(&mut out, check);
    }
    out.push_str(&format!("max-age: {}\n", claim.max_age));
    if !claim.supports.is_empty() {
        out.push_str("supports:\n");
        for target in &claim.supports {
            out.push_str(&format!("  - {}\n", yaml_scalar(target)));
        }
    }
    out.push_str("---\n");
    out.push_str(claim.statement.trim());
    out.push('\n');
    out
}

/// Render one check as a YAML list item under `checks:`.
fn render_check(out: &mut String, check: &CheckDraft) {
    out.push_str(&format!("  - kind: {}\n", check.kind_name()));
    match &check.kind {
        CheckDraftKind::Cmd { run, negate } => {
            out.push_str(&format!("    run: {}\n", yaml_double_quoted(run)));
            if *negate {
                out.push_str("    negate: true\n");
            }
        }
    }
    out.push_str(&format!("    when: {}\n", check.when));
}

/// A YAML scalar that is safe unquoted when it has no special leading char or
/// interior colon, and double-quoted otherwise. Conservative: when in doubt, quote.
fn yaml_scalar(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.contains([':', '#', '"', '\'', '\n'])
        || s.starts_with([
            ' ', '-', '?', '[', ']', '{', '}', '&', '*', '!', '|', '>', '@', '`',
        ])
        || s.ends_with(' ');
    if needs_quote {
        yaml_double_quoted(s)
    } else {
        s.to_owned()
    }
}

/// A YAML double-quoted scalar with `\` and `"` escaped. Used for the check `run`
/// unconditionally, since a command is dense with characters YAML treats specially.
fn yaml_double_quoted(s: &str) -> String {
    let mut q = String::with_capacity(s.len() + 2);
    q.push('"');
    for c in s.chars() {
        match c {
            '\\' => q.push_str("\\\\"),
            '"' => q.push_str("\\\""),
            '\n' => q.push_str("\\n"),
            '\t' => q.push_str("\\t"),
            other => q.push(other),
        }
    }
    q.push('"');
    q
}

/// The claim fields `claim add` collected, before rendering and validation.
///
/// A pre-validation draft distinct from [`claim_core::Claim`]: the latter has no
/// public constructor (every `Claim` is proven valid by the parser), so the CLI
/// gathers raw-but-typed fields here, renders them, and lets the parser produce the
/// real `Claim`. That is the single validation path — the CLI never hand-builds a
/// `Claim` that skipped the schema.
pub struct ClaimDraft {
    /// The validated id, as a string for rendering.
    pub id: String,
    /// The freshness window, e.g. `120d`.
    pub max_age: String,
    /// The checks, in order.
    pub checks: Vec<CheckDraft>,
    /// The `supports` targets, in order.
    pub supports: Vec<String>,
    /// The statement body.
    pub statement: String,
}

/// One drafted check.
pub struct CheckDraft {
    /// The check's kind and payload. v1 `claim add` only authors `cmd` checks.
    pub kind: CheckDraftKind,
    /// The trigger string, e.g. `on-change` or `every 30d`.
    pub when: String,
}

impl CheckDraft {
    fn kind_name(&self) -> &'static str {
        match self.kind {
            CheckDraftKind::Cmd { .. } => "cmd",
        }
    }
}

/// The kind of a drafted check. Only `cmd` is authored by `claim add` in v1; the
/// enum leaves room for `agent`/`human` authoring later without reshaping callers.
pub enum CheckDraftKind {
    /// A command check.
    Cmd {
        /// The command line.
        run: String,
        /// Whether to invert the verdict.
        negate: bool,
    },
}

/// Render a draft and validate it by parsing the rendered bytes, returning the
/// parsed [`Claim`] and the text that produced it.
///
/// The returned text is exactly what will be written to disk, and the returned
/// `Claim` is proof it parses — the same file, validated. Any schema violation
/// (a bad id, an empty statement, a malformed trigger) surfaces here as a
/// [`claim_core::Error`] naming the field, before anything is written.
///
/// # Errors
///
/// Returns the parser's error if the rendered claim does not satisfy the schema.
pub fn render_and_validate(draft: &ClaimDraft, path: &str) -> claim_core::Result<(Claim, String)> {
    let text = render(draft);
    let claim = claim_core::parse_claim_file(path, &text)?;
    Ok((claim, text))
}

/// The primary `cmd` check of a parsed claim, for the witnessed-red runs.
///
/// `claim add` authors exactly one check in v1, so this returns the first check
/// with its kind narrowed for execution. Returns `None` only for the (unreachable
/// in v1) case of a non-cmd first check, letting the caller decide rather than
/// panicking.
#[must_use]
pub fn primary_cmd_check(claim: &Claim) -> Option<&Check> {
    claim
        .checks
        .first()
        .filter(|c| matches!(c.kind, CheckKind::Cmd { .. }))
}
