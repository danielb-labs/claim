//! Rendering a claim's fields to the on-disk `.claims/*.md` text.
//!
//! Both front doors that author a claim — the CLI's `claim add` and the MCP `create`
//! tool — turn a small set of fields into frontmatter, then round-trip the text
//! through [`claim_core::parse_claim_file`] to validate it before writing. Keeping the
//! renderer here, once, means the two cannot drift: a `supports` target or a check
//! command renders byte-identically whichever door authored it.
//!
//! This is deliberately *not* a general YAML emitter. It emits exactly the v1 schema,
//! the minimal set of fields the tool understands, in a stable, predictable shape a
//! reviewer reads. The single-scalar fields an attacker could smuggle a newline
//! through — `id`, `max-age`, `when` — are rendered *quoted*, and
//! [`render_claim`] refuses any of them that carries a newline or control character
//! outright. Without that, a crafted value like `"30d\nsupports:\n  - injected"`
//! would render a claim with a phantom, unrequested `supports` edge that parses
//! cleanly and slips past a round-trip that only checks parse-validity, not
//! field-equality with the request.

/// A claim's fields could not be rendered because a single-scalar frontmatter value
/// carried a character that could inject an unrequested field.
///
/// The only render failure v1 has: `id`, `max-age`, and `when` are single-line
/// scalars, and a newline or control character in one of them is refused before any
/// text is produced (see [`render_claim`]). Everything else about a claim's shape is
/// validated by the caller's round-trip through [`claim_core::parse_claim_file`], not
/// here.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// A frontmatter scalar field (`id`, `max-age`, or `when`) contained a newline or
    /// control character — a value that could smuggle a phantom key into the file.
    #[error(
        "{field} must not contain a newline or control character; such a value could inject an \
         unrequested field into the claim file"
    )]
    UnsafeScalar {
        /// The offending field.
        field: &'static str,
    },
}

/// The fields to render into a claim file: the frontmatter plus the statement body.
///
/// A pre-validation view distinct from [`claim_core::Claim`] (which has no public
/// constructor — every `Claim` is proven valid by the parser): a caller gathers
/// raw-but-typed fields here, [`render_claim`] emits the text, and the parser produces
/// the real `Claim`. That is the single validation path — no caller hand-builds a
/// `Claim` that skipped the schema.
pub struct ClaimRender<'a> {
    /// The claim id, e.g. `payments/libfoo-pin`. Rendered quoted; a newline or
    /// control character is refused.
    pub id: &'a str,
    /// The freshness window, e.g. `120d`. Rendered quoted; a newline or control
    /// character is refused.
    pub max_age: &'a str,
    /// The single check to author.
    pub check: CheckRender<'a>,
    /// The trigger, e.g. `on-change` or `every 30d`. Rendered quoted; a newline or
    /// control character is refused.
    pub when: &'a str,
    /// The `supports` targets, in order. Each is rendered as a quoted-if-needed
    /// scalar.
    pub supports: &'a [String],
    /// The statement body.
    pub statement: &'a str,
}

/// A single check to render: one of the two kinds v1 authoring writes.
pub enum CheckRender<'a> {
    /// A `cmd` check: a shell command line, and whether to invert its verdict.
    Cmd {
        /// The command line.
        run: &'a str,
        /// Whether to render `negate: true`.
        negate: bool,
    },
    /// A `kind: agent` check: a natural-language instruction an agent runner
    /// executes.
    Agent {
        /// What the agent is asked to determine.
        instruction: &'a str,
    },
}

impl CheckRender<'_> {
    /// The `kind:` value for this check.
    fn kind(&self) -> &'static str {
        match self {
            CheckRender::Cmd { .. } => "cmd",
            CheckRender::Agent { .. } => "agent",
        }
    }
}

/// Render a claim's fields to the `.claims/*.md` text: `---`-fenced YAML frontmatter
/// followed by the statement body.
///
/// The output is minimal and canonical — only the fields the tool set, each on its
/// own line — so two claims authored the same way produce byte-identical files and a
/// reviewer reads a predictable shape. The caller round-trips the result through
/// [`claim_core::parse_claim_file`] so a rendering bug is caught before the file is
/// written, never after it is committed.
///
/// The single-scalar frontmatter fields (`id`, `max-age`, `when`) are rendered
/// double-quoted so a value like `payments/pin` needs no special-casing, and this
/// function *rejects* any of the three that contains a newline or an ASCII control
/// character. That refusal — not the quoting alone — is what closes the injection:
/// a round-trip validates that the text *parses*, not that its fields match the
/// request, so a value carrying `\n  key: value` could otherwise introduce a valid
/// but unrequested field. `run`/`instruction` and `supports` targets may legitimately
/// contain almost anything and are handled by escaping inside the quoted scalar (a
/// literal newline in a command becomes `\n`), so they need no such refusal.
///
/// # Errors
///
/// Returns [`RenderError::UnsafeScalar`] naming the field when `id`, `max-age`, or
/// `when` contains a newline or control character — an authoring input that could
/// smuggle a phantom key, refused before any text is produced.
pub fn render_claim(claim: &ClaimRender) -> Result<String, RenderError> {
    reject_injection("id", claim.id)?;
    reject_injection("max-age", claim.max_age)?;
    reject_injection("when", claim.when)?;

    let mut out = String::new();
    out.push_str("---\n");
    // Quoted so a control-free but otherwise special id (unlikely, but free to be
    // safe) cannot confuse the YAML scanner; the injection vector is closed by the
    // refusal above.
    out.push_str(&format!("id: {}\n", yaml_double_quoted(claim.id)));
    out.push_str("checks:\n");
    out.push_str(&format!("  - kind: {}\n", claim.check.kind()));
    match &claim.check {
        CheckRender::Cmd { run, negate } => {
            out.push_str(&format!("    run: {}\n", yaml_double_quoted(run)));
            if *negate {
                out.push_str("    negate: true\n");
            }
        }
        CheckRender::Agent { instruction } => {
            out.push_str(&format!(
                "    instruction: {}\n",
                yaml_double_quoted(instruction)
            ));
        }
    }
    out.push_str(&format!("    when: {}\n", yaml_double_quoted(claim.when)));
    out.push_str(&format!("max-age: {}\n", yaml_double_quoted(claim.max_age)));
    if !claim.supports.is_empty() {
        out.push_str("supports:\n");
        for target in claim.supports {
            out.push_str(&format!("  - {}\n", yaml_scalar(target)));
        }
    }
    out.push_str("---\n");
    out.push_str(claim.statement.trim());
    out.push('\n');
    Ok(out)
}

/// Refuse a single-scalar frontmatter value that carries a newline or control
/// character — the values through which a crafted input could inject a whole extra
/// YAML key past a parse-validity-only round-trip.
fn reject_injection(field: &'static str, value: &str) -> Result<(), RenderError> {
    if value.chars().any(|c| c == '\n' || c.is_control()) {
        return Err(RenderError::UnsafeScalar { field });
    }
    Ok(())
}

/// A YAML scalar safe unquoted when it has no special leading char or interior colon,
/// and double-quoted otherwise. Conservative: when in doubt, quote.
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

/// A YAML double-quoted scalar with `\` and `"` escaped, and newlines/tabs rendered as
/// their escape sequences. Used for the check payload and the single-scalar fields (a
/// command or an id is dense with characters YAML treats specially).
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

#[cfg(test)]
mod tests {
    use super::*;
    use claim_core::parse_claim_file;

    fn cmd_render<'a>(
        id: &'a str,
        max_age: &'a str,
        when: &'a str,
        run: &'a str,
    ) -> ClaimRender<'a> {
        ClaimRender {
            id,
            max_age,
            check: CheckRender::Cmd { run, negate: false },
            when,
            supports: &[],
            statement: "S.",
        }
    }

    #[test]
    fn a_cmd_claim_renders_and_parses() {
        let r = cmd_render("pin", "30d", "on-change", "grep -q 'libfoo==4.2' r.txt");
        let text = render_claim(&r).unwrap();
        let claim = parse_claim_file(".claims/pin.md", &text).unwrap();
        assert_eq!(claim.id.as_str(), "pin");
        assert_eq!(claim.max_age.to_string(), "30d");
    }

    #[test]
    fn an_agent_claim_renders_and_parses() {
        let r = ClaimRender {
            id: "a",
            max_age: "30d",
            check: CheckRender::Agent {
                instruction: "read the changelog",
            },
            when: "every 30d",
            supports: &[],
            statement: "S.",
        };
        let text = render_claim(&r).unwrap();
        let claim = parse_claim_file(".claims/a.md", &text).unwrap();
        assert_eq!(claim.checks.len(), 1);
    }

    #[test]
    fn a_newline_in_max_age_is_refused_not_injected() {
        // The injection vector: a max-age crafted to add a phantom `supports` edge.
        // Without the refusal it would render a claim that parses cleanly and carries
        // an unrequested support target.
        let r = cmd_render("pin", "30d\nsupports:\n  - injected", "on-change", "true");
        let err = render_claim(&r).unwrap_err();
        assert!(
            matches!(err, RenderError::UnsafeScalar { field: "max-age" }),
            "a newline-bearing max-age is refused: {err}"
        );
    }

    #[test]
    fn a_newline_in_id_or_when_is_refused() {
        let bad_id = cmd_render("pin\nmax-age: 9999d", "30d", "on-change", "true");
        assert!(render_claim(&bad_id).is_err(), "a newline in id is refused");
        let bad_when = cmd_render("pin", "30d", "on-change\nmax-age: 9999d", "true");
        assert!(
            render_claim(&bad_when).is_err(),
            "a newline in when is refused"
        );
    }

    #[test]
    fn a_control_character_in_a_scalar_field_is_refused() {
        let r = cmd_render("pin", "30d\u{7}", "on-change", "true");
        assert!(
            render_claim(&r).is_err(),
            "a control char in max-age is refused"
        );
    }

    #[test]
    fn a_run_with_yaml_metacharacters_round_trips() {
        // A command dense with YAML-special characters, including an embedded double
        // quote, must render and parse back to the exact command — the round-trip is
        // what proves the escaping is right.
        let run = "echo \"a: b # c\" | grep -q 'x' && test 1 = 1";
        let r = cmd_render("meta", "30d", "on-change", run);
        let text = render_claim(&r).unwrap();
        let claim = parse_claim_file(".claims/meta.md", &text).unwrap();
        match &claim.checks[0].kind {
            claim_core::CheckKind::Cmd { run: parsed, .. } => assert_eq!(parsed, run),
            other => panic!("expected a cmd check, got {other:?}"),
        }
    }

    #[test]
    fn supports_targets_are_quoted_when_special() {
        let supports = vec![
            "requirements.txt#libfoo".to_owned(),
            "other-claim".to_owned(),
        ];
        let r = ClaimRender {
            id: "s",
            max_age: "30d",
            check: CheckRender::Cmd {
                run: "true",
                negate: false,
            },
            when: "on-change",
            supports: &supports,
            statement: "S.",
        };
        let text = render_claim(&r).unwrap();
        assert!(
            text.contains("- \"requirements.txt#libfoo\""),
            "a #-ref is quoted: {text}"
        );
        assert!(
            text.contains("- other-claim"),
            "a bare id is unquoted: {text}"
        );
        let claim = parse_claim_file(".claims/s.md", &text).unwrap();
        assert_eq!(claim.supports.len(), 2);
    }
}
