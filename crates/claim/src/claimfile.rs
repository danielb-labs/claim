//! Gathering `claim add`'s fields and rendering them to the on-disk `.claims/*.md`
//! format.
//!
//! `claim add` builds a claim's frontmatter and body as text via the shared
//! [`claim_store::render_claim`], then hands that exact text to
//! [`claim_core::parse_claim_file`] to validate it before writing — so the file the
//! tool commits is proven to parse, not merely assembled and hoped-correct. The
//! renderer lives in `claim-store`, shared with the MCP `create` tool, so both front
//! doors emit byte-identical files and the frontmatter's injection-hardening lives in
//! exactly one place. This module keeps only the CLI's own gather structures and the
//! draft→render→validate glue.

use claim_core::{Check, CheckKind, Claim};
use claim_store::{render_claim, CheckRender, ClaimRender, RenderError};

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

/// Why a draft could not be turned into a validated claim: either the shared renderer
/// refused an injection-prone scalar, or the parser rejected the schema.
///
/// Both are surfaced to the user as "the claim you described is not valid: …" with the
/// specific reason, so the two failure classes read the same to a caller while staying
/// distinct in the type.
#[derive(Debug)]
pub enum DraftError {
    /// A frontmatter scalar (`id`/`max-age`/`when`) carried a newline or control
    /// character — refused by the renderer before any text was produced.
    Render(RenderError),
    /// The rendered text did not satisfy the claim schema.
    Parse(claim_core::Error),
}

impl std::fmt::Display for DraftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DraftError::Render(e) => e.fmt(f),
            DraftError::Parse(e) => e.fmt(f),
        }
    }
}

/// Render a draft and validate it by parsing the rendered bytes, returning the
/// parsed [`Claim`] and the text that produced it.
///
/// The returned text is exactly what will be written to disk, and the returned
/// `Claim` is proof it parses — the same file, validated. The renderer refuses a
/// newline or control character in `id`/`max-age`/`when` (so a crafted value cannot
/// inject a phantom field past this round-trip), and any remaining schema violation
/// (a bad id, an empty statement, a malformed trigger) surfaces from the parser
/// naming the field, before anything is written.
///
/// # Errors
///
/// Returns [`DraftError::Render`] if the renderer refuses an injection-prone scalar,
/// or [`DraftError::Parse`] if the rendered claim does not satisfy the schema.
pub fn render_and_validate(draft: &ClaimDraft, path: &str) -> Result<(Claim, String), DraftError> {
    let render = draft.as_render();
    let text = render_claim(&render).map_err(DraftError::Render)?;
    let claim = claim_core::parse_claim_file(path, &text).map_err(DraftError::Parse)?;
    Ok((claim, text))
}

impl ClaimDraft {
    /// Borrow this draft as the shared renderer's input. `claim add` authors exactly
    /// one check in v1, so the first (and only) check is rendered; a draft with no
    /// check is impossible from the CLI's gather path.
    fn as_render(&self) -> ClaimRender<'_> {
        let check = match self.checks.first() {
            Some(CheckDraft {
                kind: CheckDraftKind::Cmd { run, negate },
                ..
            }) => CheckRender::Cmd {
                run,
                negate: *negate,
            },
            None => unreachable!("claim add always gathers exactly one check"),
        };
        let when = self.checks.first().map_or("on-change", |c| c.when.as_str());
        ClaimRender {
            id: &self.id,
            max_age: &self.max_age,
            check,
            when,
            supports: &self.supports,
            statement: &self.statement,
        }
    }
}

/// The primary `cmd` check of a parsed claim, for the establishing and witness runs.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn full_draft() -> ClaimDraft {
        ClaimDraft {
            id: "payments/libfoo-pin".to_owned(),
            max_age: "120d".to_owned(),
            checks: vec![CheckDraft {
                kind: CheckDraftKind::Cmd {
                    run: "grep -q 'libfoo==4.2' requirements.txt".to_owned(),
                    negate: true,
                },
                when: "every 30d".to_owned(),
            }],
            supports: vec![
                "requirements.txt#libfoo".to_owned(),
                "other-claim".to_owned(),
            ],
            statement: "We pin libfoo at 4.2.".to_owned(),
        }
    }

    #[test]
    fn rendered_claim_file_is_stable() {
        // A snapshot of the rendered file (CLAUDE.md's insta obligation), on a
        // deliberately dynamic-content-free surface: no timestamps, no temp paths,
        // so the snapshot is stable and any format change is a reviewable diff. The
        // `#`-bearing supports target and the quoted command exercise the YAML
        // quoting rules.
        let text = render_claim(&full_draft().as_render()).unwrap();
        insta::assert_snapshot!(text);
    }

    #[test]
    fn rendered_claim_round_trips_through_the_parser() {
        // The bytes we write must parse back — the single validation path.
        let draft = full_draft();
        let (claim, text) =
            render_and_validate(&draft, ".claims/payments/libfoo-pin.md").expect("valid claim");
        assert_eq!(claim.id.as_str(), "payments/libfoo-pin");
        assert_eq!(text, render_claim(&draft.as_render()).unwrap());
    }
}
