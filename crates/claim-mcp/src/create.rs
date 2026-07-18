//! The `create` tool's pure logic: record a claim an agent just established.
//!
//! This is the second write verb on the agent surface, and the heavier one:
//! `report` appends a verdict to an *existing* claim, while `create` brings a new
//! claim into being. Every honesty rule authoring rests on is enforced by the shared
//! [`claim_store::author_claim`] this delegates to — not re-implemented here — so the
//! MCP tool and the CLI's `claim add` cannot disagree about what it takes to record a
//! fact:
//!
//! - **The check must hold now.** `create` runs the claim's single check against the
//!   current tree and records it only on [`Verdict::Held`]. A [`Verdict::Drifted`]
//!   (the fact is already false) or [`Verdict::Broken`] (the check cannot run) is
//!   refused with the observed evidence, writing nothing — a claim whose check did
//!   not hold is never created. An `agent` check with no runner configured is
//!   [`Verdict::Unverifiable`], which is not `Held`, so it too is refused: an agent
//!   cannot establish a claim it cannot verify.
//! - **The id must be new and valid.** A malformed id, a duplicate id, an empty
//!   statement, or a bad `max-age` is rejected loudly before any check runs — reusing
//!   claim-core's own validators via the round-trip through
//!   [`claim_core::parse_claim_file`].
//! - **The verdict is attributed to git.** Provenance is the agent's own git
//!   identity (invariant #3), resolved inside `author_claim`, never taken from the
//!   request.
//! - **The server does not commit.** The claim file and its birth verdict are left in
//!   the working tree with a `commit_hint` naming what to commit (invariant #4, a
//!   write to the truth is a commit the caller makes). The created claim is
//!   *unreviewed* until the caller commits it, and a human reviews that commit — the
//!   tool description says so.
//!
//! An unresolvable `supports` target does not fail the create (a forward reference is
//! legitimate); it is surfaced as a warning in the response, exactly as `claim add`
//! warns.

use claim_core::{
    parse_claim_file, resolve_supports, CheckContext, Claim, ClaimId, Timestamp, Verdict,
};
use claim_store::git::short_commit;
use claim_store::{author_claim, AuthorError, Store, StoreLoad};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `create` tool's inputs: enough to build a single-check claim.
///
/// Exactly one of `run` (a `cmd` check) or `instruction` (a `kind: agent` check)
/// must be given; supplying both, or neither, is rejected. `negate` applies only to a
/// `cmd` check (an agent check has no exit code to invert). `when` defaults to
/// `on-change`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CreateRequest {
    /// The claim's id: a kebab-case slug, optionally namespaced with `/`
    /// (e.g. `payments/libfoo-pin`). Must be new to the store; a duplicate is
    /// rejected.
    pub id: String,
    /// The plain-language statement — the fact this claim records. Required and
    /// non-empty.
    pub statement: String,
    /// A `cmd` check's command line: exit 0 means the fact holds, exit 1 means it
    /// drifted (unless `negate` inverts). Give this *or* `instruction`, not both.
    #[serde(default)]
    pub run: Option<String>,
    /// A `kind: agent` check's natural-language instruction: what the agent runner is
    /// asked to determine. Give this *or* `run`, not both. Establishing an agent
    /// check requires a runner (`CLAIM_AGENT_CMD`); with none it cannot be verified
    /// and the create is refused.
    #[serde(default)]
    pub instruction: Option<String>,
    /// When the check runs: `on-change` or `every <N>d` (e.g. `every 30d`). Defaults
    /// to `on-change`.
    #[serde(default)]
    pub when: Option<String>,
    /// Invert a `cmd` check's `Held`/`Drifted` sense (the tool owns the inversion; it
    /// never wraps the command in a shell `!`). Ignored for an `instruction` check.
    #[serde(default)]
    pub negate: bool,
    /// The dead-man's switch: how long a passing check keeps the claim fresh, as
    /// `<N>d` (e.g. `120d`). Required.
    pub max_age: String,
    /// The decisions or claim ids this claim justifies (its `supports` edge). An
    /// unresolvable target does not fail the create — it is surfaced as a warning.
    #[serde(default)]
    pub supports: Vec<String>,
}

/// The `create` tool's structured output: what was written, and what to commit.
///
/// The `commit_hint` names the file and the birth verdict the caller must `git add`
/// and commit, because the server does not commit — a write to the truth is a commit
/// the caller makes, under the agent's identity, so the new claim is reviewed as part
/// of that commit or PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CreateResponse {
    /// The id of the created claim.
    pub id: String,
    /// The establishing verdict, always `held` — a claim is created only when its
    /// check holds now.
    pub verdict: String,
    /// The commit sha the establishing verdict was attributed to (git `HEAD`, or the
    /// unborn sentinel), abbreviated for display.
    pub commit: String,
    /// The actor the verdict was attributed to, `Name <email>` from git config.
    pub actor: String,
    /// The claim file that was written, relative to the store root. Left in the
    /// working tree for the caller to commit.
    pub claim_file: String,
    /// The establishing verdict-log file that was written, relative to the store
    /// root. Left in the working tree for the caller to commit.
    pub log_file: String,
    /// The exact `git` command that records the new claim. The server does not run
    /// this; a write to the truth is a commit the caller makes, and until it does the
    /// claim is unreviewed.
    pub commit_hint: String,
    /// One warning per `supports` target that does not resolve now. Non-fatal: the
    /// claim was created, but a human should fix a typo'd anchor. Empty when every
    /// target resolves.
    pub warnings: Vec<String>,
}

/// A `create` that could not be recorded, each variant a distinct, loud refusal that
/// writes nothing.
///
/// The protocol shell maps a *caller-fixable* mistake (bad input, a check that did
/// not hold, a duplicate id, a missing agent runner) to `invalid_params`, and an
/// *environment* fault (provenance or I/O) to `internal_error`, without matching on
/// prose — the split is on the variant.
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    /// Neither `run` nor `instruction` was given, or both were. A claim needs exactly
    /// one check to author.
    #[error(
        "give exactly one of `run` (a cmd check) or `instruction` (an agent check); \
         {found} were supplied"
    )]
    CheckKind {
        /// How many of the two were supplied (`neither` or `both`).
        found: &'static str,
    },

    /// A field failed claim-core's schema — a malformed id, an empty statement, a bad
    /// `max-age`, or an invalid trigger. Carries the parser's own message, which names
    /// the field and the fix.
    #[error("the claim you described is not valid: {0}")]
    Invalid(String),

    /// The establishing check did not hold against the current tree, so there is no
    /// true fact to record. Carries the observed verdict and its evidence so the agent
    /// sees *why* it was refused and can fix the fact or the check.
    #[error("the check did not hold ({status}); nothing was created. {guidance}")]
    NotHeld {
        /// The observed verdict (`drifted`, `broken`, or `unverifiable`).
        verdict: String,
        /// The human one-liner for how the check ended.
        status: String,
        /// Guidance specific to the observed verdict.
        guidance: String,
        /// The check's evidence, if any, so the caller can see what was observed.
        evidence: Option<String>,
    },

    /// The id already names a claim in the store. Carries the shared authoring
    /// error's message, which names where the conflict lives.
    #[error("{0}")]
    Duplicate(String),

    /// Git provenance could not be resolved — no repository, unset identity — so the
    /// verdict would be unattributable and is not written.
    #[error("could not attribute the establishing verdict: {0}")]
    Provenance(String),

    /// The claim file could not be written, or the verdict could not be appended.
    #[error("could not write the claim: {0}")]
    Write(String),
}

/// Create a claim an agent established: validate the fields, run the establishing
/// check requiring [`Verdict::Held`], and — only then — write the claim file and its
/// birth verdict to the working tree with git provenance, without committing.
///
/// The honesty gate lives in [`claim_store::author_claim`], which this delegates to;
/// this function's own job is to turn the request into a validated [`Claim`] plus its
/// exact file text (round-tripped through [`parse_claim_file`], the single validation
/// path), attach the caller-supplied `ctx` (whose agent runner, if any, comes from
/// `CLAIM_AGENT_CMD`), and shape the result — including the `supports` warnings and
/// the `commit_hint` — for the protocol shell.
///
/// `load` is the loaded corpus (from [`Store::load_all`]), passed in so the
/// duplicate-id scan and the `supports` resolution share one load. `now` is a
/// parameter so the recorded instant is deterministic under test.
///
/// # Errors
///
/// Returns the matching [`CreateError`] for each rule it fails, and never writes a
/// claim when it returns `Err`. A [`CreateError::NotHeld`] carries the refused verdict
/// and its evidence; a [`CreateError::CheckKind`]/[`CreateError::Invalid`]/
/// [`CreateError::Duplicate`] is a caller mistake; a [`CreateError::Provenance`]/
/// [`CreateError::Write`] is an environment fault.
pub fn run_create(
    store: &Store,
    request: &CreateRequest,
    load: &StoreLoad,
    ctx: &CheckContext,
    now: Timestamp,
) -> Result<CreateResponse, CreateError> {
    let (claim, file_text) = build_claim(store, request)?;

    let authored =
        author_claim(store, &claim, &file_text, load, ctx, now, None).map_err(|e| match e {
            AuthorError::DuplicateId { .. } | AuthorError::IdAlreadyDeclared { .. } => {
                CreateError::Duplicate(e.to_string())
            }
            AuthorError::NotHeld {
                verdict,
                status,
                evidence,
            } => CreateError::NotHeld {
                verdict: verdict_word(verdict).to_owned(),
                status,
                guidance: not_held_guidance(verdict),
                evidence,
            },
            AuthorError::Provenance(g) => CreateError::Provenance(g.to_string()),
            AuthorError::Write(w) => CreateError::Write(w),
        })?;

    let claim_file = rel(store, &authored.claim_file);
    let log_file = rel(store, &authored.log_file);
    let warnings = supports_warnings(store, load, &claim);

    Ok(CreateResponse {
        id: claim.id.to_string(),
        verdict: verdict_word(authored.establishing.verdict).to_owned(),
        commit: short_commit(&authored.provenance.commit),
        actor: authored.provenance.actor,
        commit_hint: format!(
            "git -C {} add {claim_file} {log_file} && git commit -m \"Add claim {}\"",
            store.root().display(),
            claim.id,
        ),
        claim_file,
        log_file,
        warnings,
    })
}

/// Turn a request into a validated [`Claim`] and the exact file text to write.
///
/// The exactly-one-of-`run`/`instruction` rule is enforced here, before rendering.
/// The rendered text is validated by parsing it back through
/// [`parse_claim_file`] — the same single validation path `claim add` uses — so a
/// malformed id, an empty statement, a bad `max-age`, or an invalid trigger surfaces
/// as the parser's own field-named error, never a hand-rolled check that could drift
/// from the schema.
fn build_claim(store: &Store, request: &CreateRequest) -> Result<(Claim, String), CreateError> {
    let when = request.when.as_deref().unwrap_or("on-change");
    let check = match (&request.run, &request.instruction) {
        (Some(run), None) => CheckText::cmd(run, request.negate),
        (None, Some(instruction)) => CheckText::agent(instruction),
        (Some(_), Some(_)) => return Err(CreateError::CheckKind { found: "both" }),
        (None, None) => return Err(CreateError::CheckKind { found: "neither" }),
    };

    let text = render_claim(request, &check, when);
    // The id must parse before it can name the file path the parser error will cite.
    // Its validity is re-checked by parse_claim_file anyway; parsing it here only
    // picks a sensible path for the error message, falling back to a placeholder.
    let file_rel = request
        .id
        .parse::<ClaimId>()
        .map(|id| store.claim_file_relative(&id))
        .unwrap_or_else(|_| ".claims/<id>.md".to_owned());
    let claim =
        parse_claim_file(&file_rel, &text).map_err(|e| CreateError::Invalid(e.to_string()))?;
    Ok((claim, text))
}

/// A single check's YAML lines, already quoted, for one of the two authored kinds.
struct CheckText {
    kind: &'static str,
    /// The `run`/`instruction` payload line (`run: "…"` or `instruction: "…"`).
    payload: String,
    /// A `negate: true` line for a cmd check, else empty.
    negate: String,
}

impl CheckText {
    fn cmd(run: &str, negate: bool) -> Self {
        CheckText {
            kind: "cmd",
            payload: format!("    run: {}\n", yaml_double_quoted(run)),
            negate: if negate {
                "    negate: true\n".to_owned()
            } else {
                String::new()
            },
        }
    }

    fn agent(instruction: &str) -> Self {
        CheckText {
            kind: "agent",
            payload: format!("    instruction: {}\n", yaml_double_quoted(instruction)),
            negate: String::new(),
        }
    }
}

/// Render the claim's `.claims/*.md` text: `---`-fenced YAML frontmatter, then the
/// statement body. Minimal and canonical, mirroring the CLI's renderer; the result is
/// validated by the caller's round-trip through the parser, so a rendering slip is
/// caught before anything is written, never after.
fn render_claim(request: &CreateRequest, check: &CheckText, when: &str) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("id: {}\n", request.id));
    out.push_str("checks:\n");
    out.push_str(&format!("  - kind: {}\n", check.kind));
    out.push_str(&check.payload);
    out.push_str(&check.negate);
    out.push_str(&format!("    when: {when}\n"));
    out.push_str(&format!("max-age: {}\n", request.max_age));
    if !request.supports.is_empty() {
        out.push_str("supports:\n");
        for target in &request.supports {
            out.push_str(&format!("  - {}\n", yaml_scalar(target)));
        }
    }
    out.push_str("---\n");
    out.push_str(request.statement.trim());
    out.push('\n');
    out
}

/// A YAML scalar safe unquoted when it has no special leading char or interior colon,
/// and double-quoted otherwise. Conservative: when in doubt, quote. Mirrors the CLI's
/// `yaml_scalar` so `supports` targets render identically across the two front doors.
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

/// A YAML double-quoted scalar with `\` and `"` escaped. Used for the check payload
/// (a command or instruction is dense with characters YAML treats specially).
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

/// Warn for each `supports` target that does not resolve now, phrased for the agent
/// to fix. Reuses claim-core's [`resolve_supports`] against the same store root and
/// known-id set `check` uses, so `create` and `check` agree on what resolves.
fn supports_warnings(store: &Store, load: &StoreLoad, claim: &Claim) -> Vec<String> {
    if claim.supports.is_empty() {
        return Vec::new();
    }
    let known_ids: Vec<ClaimId> = load.claims.iter().map(|c| c.claim.id.clone()).collect();
    resolve_supports(&claim.supports, store.root(), &known_ids)
        .into_iter()
        .filter(|r| !r.resolved)
        .map(|r| {
            let reason = r.reason.as_deref().unwrap_or("does not resolve");
            format!(
                "supports target '{}' does not resolve: {reason}. The claim was created, but \
                 `claim check` will flag this as an unresolved support until it resolves. A \
                 `#anchor` is a case-sensitive text scan, not a GitHub slug — use the words as \
                 written (`#Approved dependencies`, not `#approved-dependencies`).",
                r.target
            )
        })
        .collect()
}

/// Guidance for a refused establish, specific to the verdict observed. An agent check
/// that came back `Unverifiable` almost always means no runner is configured, so the
/// guidance names `CLAIM_AGENT_CMD`.
fn not_held_guidance(verdict: Verdict) -> String {
    match verdict {
        Verdict::Drifted => "The fact is already false against the current tree, so there is \
             nothing true to record. Fix the fact or the check first."
            .to_owned(),
        Verdict::Broken => "The check could not run, so it cannot be trusted. Fix the command \
             first."
            .to_owned(),
        Verdict::Unverifiable => "The check could not be verified — an agent check needs a \
             runner. Set CLAIM_AGENT_CMD to a runner that reads the prompt on stdin and prints \
             the verdict JSON on stdout, then retry. A claim you cannot verify cannot be created."
            .to_owned(),
        // author_claim never returns NotHeld for Held; kept total so a future verdict
        // forces a decision here rather than defaulting to a silent empty string.
        Verdict::Held => String::new(),
    }
}

/// The lowercase wire word for a verdict.
fn verdict_word(v: Verdict) -> &'static str {
    match v {
        Verdict::Held => "held",
        Verdict::Drifted => "drifted",
        Verdict::Broken => "broken",
        Verdict::Unverifiable => "unverifiable",
    }
}

/// Render an absolute store path relative to the store root, falling back to the full
/// path (which should never happen for a path the store itself produced).
fn rel(store: &Store, path: &std::path::Path) -> String {
    path.strip_prefix(store.root())
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestStore;
    use claim_core::AgentRunner;

    fn ts() -> Timestamp {
        "2026-07-18T12:00:00Z".parse().unwrap()
    }

    fn req(
        id: &str,
        statement: &str,
        run: Option<&str>,
        instruction: Option<&str>,
    ) -> CreateRequest {
        CreateRequest {
            id: id.to_owned(),
            statement: statement.to_owned(),
            run: run.map(ToOwned::to_owned),
            instruction: instruction.map(ToOwned::to_owned),
            when: None,
            negate: false,
            max_age: "30d".to_owned(),
            supports: Vec::new(),
        }
    }

    /// A context rooted at the store, with no agent runner (the default).
    fn ctx(s: &TestStore) -> CheckContext {
        CheckContext::new(s.store.root())
    }

    #[test]
    fn a_holding_cmd_check_creates_the_claim_and_a_held_verdict() {
        let s = TestStore::new();
        let load = s.store.load_all().unwrap();
        let request = req(
            "pin",
            "We pin libfoo at 4.2.",
            Some("grep -q 'libfoo==4.2' requirements.txt"),
            None,
        );

        let resp = run_create(&s.store, &request, &load, &ctx(&s), ts()).unwrap();

        assert_eq!(resp.id, "pin");
        assert_eq!(resp.verdict, "held");
        assert_eq!(resp.actor, "Test Agent <agent@example.com>");
        assert_eq!(resp.claim_file, ".claims/pin.md");
        assert!(resp.log_file.starts_with(".claims/log/pin/"));
        assert!(resp.commit_hint.contains("git commit"));
        assert!(resp.warnings.is_empty());

        // The claim file and exactly one Held verdict are on disk.
        assert!(s.root().join(".claims/pin.md").exists());
        assert_eq!(s.log_count("pin"), 1);
        let entries = s.log_entries("pin");
        assert_eq!(entries[0]["event"]["verdict"], "held");
        assert_eq!(entries[0]["actor"], "Test Agent <agent@example.com>");
        assert_eq!(entries[0]["commit"].as_str().unwrap().len(), 40);
    }

    #[test]
    fn the_created_claim_checks_green() {
        // A claim created via `create` must itself pass `claim check`: it is a valid,
        // holding, parseable claim, not just bytes that happened to write.
        let s = TestStore::new();
        let load = s.store.load_all().unwrap();
        let request = req(
            "pin",
            "We pin libfoo at 4.2.",
            Some("grep -q 'libfoo==4.2' requirements.txt"),
            None,
        );
        run_create(&s.store, &request, &load, &ctx(&s), ts()).unwrap();

        // Re-load and run the claim's check the way `claim check` does.
        let reload = s.store.load_all().unwrap();
        let created = reload
            .claims
            .iter()
            .find(|c| c.claim.id.as_str() == "pin")
            .unwrap();
        let outcome = claim_core::run_check(&created.claim.checks[0], &ctx(&s));
        assert_eq!(
            outcome.verdict,
            Verdict::Held,
            "the created claim checks green"
        );
    }

    #[test]
    fn a_drifted_check_creates_nothing() {
        let s = TestStore::new();
        let load = s.store.load_all().unwrap();
        // A grep for a pin that is not present: the fact is already false.
        let request = req(
            "x",
            "S.",
            Some("grep -q 'libfoo==9.9' requirements.txt"),
            None,
        );

        let err = run_create(&s.store, &request, &load, &ctx(&s), ts()).unwrap_err();
        assert!(
            matches!(&err, CreateError::NotHeld { verdict, .. } if verdict == "drifted"),
            "a drifted check is NotHeld, got {err:?}"
        );
        assert!(!s.root().join(".claims/x.md").exists(), "nothing written");
        assert_eq!(s.log_count("x"), 0);
    }

    #[test]
    fn a_broken_check_creates_nothing() {
        let s = TestStore::new();
        let load = s.store.load_all().unwrap();
        let request = req("x", "S.", Some("this-binary-does-not-exist-anywhere"), None);

        let err = run_create(&s.store, &request, &load, &ctx(&s), ts()).unwrap_err();
        assert!(
            matches!(&err, CreateError::NotHeld { verdict, .. } if verdict == "broken"),
            "a broken check is NotHeld, got {err:?}"
        );
        assert!(!s.root().join(".claims/x.md").exists());
    }

    #[test]
    fn a_duplicate_id_creates_nothing() {
        let s = TestStore::new();
        let load = s.store.load_all().unwrap();
        run_create(
            &s.store,
            &req("dup", "S.", Some("true"), None),
            &load,
            &ctx(&s),
            ts(),
        )
        .unwrap();

        let reload = s.store.load_all().unwrap();
        let err = run_create(
            &s.store,
            &req("dup", "S2.", Some("true"), None),
            &reload,
            &ctx(&s),
            ts(),
        )
        .unwrap_err();
        assert!(matches!(err, CreateError::Duplicate(_)), "got {err:?}");
    }

    #[test]
    fn a_malformed_id_is_rejected_before_any_write() {
        let s = TestStore::new();
        let load = s.store.load_all().unwrap();
        let err = run_create(
            &s.store,
            &req("Bad_Id", "S.", Some("true"), None),
            &load,
            &ctx(&s),
            ts(),
        )
        .unwrap_err();
        assert!(matches!(err, CreateError::Invalid(_)), "got {err:?}");
    }

    #[test]
    fn a_bad_max_age_is_rejected() {
        let s = TestStore::new();
        let load = s.store.load_all().unwrap();
        let mut request = req("ok", "S.", Some("true"), None);
        request.max_age = "banana".to_owned();
        let err = run_create(&s.store, &request, &load, &ctx(&s), ts()).unwrap_err();
        assert!(
            matches!(&err, CreateError::Invalid(m) if m.contains("day count")),
            "got {err:?}"
        );
    }

    #[test]
    fn an_empty_statement_is_rejected() {
        let s = TestStore::new();
        let load = s.store.load_all().unwrap();
        let err = run_create(
            &s.store,
            &req("ok", "   ", Some("true"), None),
            &load,
            &ctx(&s),
            ts(),
        )
        .unwrap_err();
        assert!(matches!(err, CreateError::Invalid(_)), "got {err:?}");
    }

    #[test]
    fn both_run_and_instruction_is_rejected() {
        let s = TestStore::new();
        let load = s.store.load_all().unwrap();
        let err = run_create(
            &s.store,
            &req("ok", "S.", Some("true"), Some("investigate")),
            &load,
            &ctx(&s),
            ts(),
        )
        .unwrap_err();
        assert!(
            matches!(err, CreateError::CheckKind { found: "both" }),
            "got {err:?}"
        );
        assert!(!s.root().join(".claims/ok.md").exists());
    }

    #[test]
    fn neither_run_nor_instruction_is_rejected() {
        let s = TestStore::new();
        let load = s.store.load_all().unwrap();
        let err = run_create(
            &s.store,
            &req("ok", "S.", None, None),
            &load,
            &ctx(&s),
            ts(),
        )
        .unwrap_err();
        assert!(
            matches!(err, CreateError::CheckKind { found: "neither" }),
            "got {err:?}"
        );
    }

    #[test]
    fn an_agent_check_with_a_holding_runner_is_created() {
        let s = TestStore::new();
        let load = s.store.load_all().unwrap();
        let request = req(
            "a",
            "The changelog fix shipped.",
            None,
            Some("read the changelog"),
        );
        // A mock runner that always answers held — no real API.
        let runner = AgentRunner::Shell(
            "printf '{\"verdict\":\"held\",\"evidence\":\"the fix is in 5.1\"}'".to_owned(),
        );
        let context = CheckContext::new(s.store.root()).with_agent_runner(Some(runner));

        let resp = run_create(&s.store, &request, &load, &context, ts()).unwrap();
        assert_eq!(resp.verdict, "held");
        assert_eq!(s.log_count("a"), 1);
        // The agent's evidence is recorded on the birth verdict.
        let entries = s.log_entries("a");
        assert!(entries[0]["event"]["evidence"]
            .as_str()
            .unwrap()
            .contains("the fix is in 5.1"));
    }

    #[test]
    fn an_agent_check_with_no_runner_is_refused_naming_the_runner() {
        // With no runner in the context an agent check is Unverifiable, which is not
        // Held: a claim you cannot verify cannot be created. Nothing is written, and
        // the error names CLAIM_AGENT_CMD so the caller knows the fix.
        let s = TestStore::new();
        let load = s.store.load_all().unwrap();
        let request = req("a", "S.", None, Some("investigate"));

        let err = run_create(&s.store, &request, &load, &ctx(&s), ts()).unwrap_err();
        match err {
            CreateError::NotHeld {
                verdict, guidance, ..
            } => {
                assert_eq!(verdict, "unverifiable");
                assert!(
                    guidance.contains("CLAIM_AGENT_CMD"),
                    "the refusal names the runner: {guidance}"
                );
            }
            other => panic!("expected NotHeld(unverifiable), got {other:?}"),
        }
        assert!(!s.root().join(".claims/a.md").exists());
        assert_eq!(s.log_count("a"), 0);
    }

    #[test]
    fn an_unresolvable_supports_warns_but_still_creates() {
        let s = TestStore::new();
        let load = s.store.load_all().unwrap();
        let mut request = req("dep", "S.", Some("true"), None);
        // A path#anchor that does not resolve (no such file).
        request.supports = vec!["MISSING.md#nope".to_owned()];

        let resp = run_create(&s.store, &request, &load, &ctx(&s), ts()).unwrap();
        assert_eq!(resp.verdict, "held");
        assert!(
            s.root().join(".claims/dep.md").exists(),
            "the claim is still created"
        );
        assert_eq!(resp.warnings.len(), 1, "the unresolvable support is warned");
        assert!(resp.warnings[0].contains("MISSING.md#nope"));
    }

    #[test]
    fn create_writes_to_the_working_tree_and_does_not_commit() {
        let s = TestStore::new();
        // Commit everything first so the only post-create change is the new claim.
        std::process::Command::new("git")
            .arg("-C")
            .arg(s.root())
            .args(["add", "-A"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(s.root())
            .args(["commit", "-q", "-m", "baseline"])
            .status()
            .unwrap();
        assert!(!s.working_tree_has_changes(), "clean before create");

        let load = s.store.load_all().unwrap();
        run_create(
            &s.store,
            &req("pin", "S.", Some("true"), None),
            &load,
            &ctx(&s),
            ts(),
        )
        .unwrap();

        // The claim and verdict are on disk but uncommitted: the server does not
        // commit (invariant #4).
        assert!(
            s.working_tree_has_changes(),
            "the created claim is left in the working tree, uncommitted"
        );
    }

    #[test]
    fn a_cmd_check_with_metacharacters_round_trips() {
        // A command dense with YAML-special characters (`:`, `#`, quotes, a pipe) must
        // render and parse back so the claim holds — the round-trip through the parser
        // is what proves the quoting is right.
        let s = TestStore::new();
        let load = s.store.load_all().unwrap();
        let request = req(
            "meta",
            "S.",
            Some("grep -q 'libfoo==4.2' requirements.txt || echo 'x: y # z'"),
            None,
        );
        let resp = run_create(&s.store, &request, &load, &ctx(&s), ts()).unwrap();
        assert_eq!(resp.verdict, "held");
    }
}
