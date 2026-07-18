//! The `report` tool's pure logic: append one verdict an agent reached in the
//! course of its work.
//!
//! This is the write side of the agent surface, and every honesty rule the
//! product rests on is enforced here, not in the protocol shell:
//!
//! - **Evidence is required.** An unsubstantiated verdict is worthless, so an
//!   empty or whitespace-only `evidence` is rejected before anything is written
//!   ([`ReportError::EmptyEvidence`]). A `report` cannot record a bare "it's
//!   fine".
//! - **The id must exist.** Reporting against an unknown id is rejected loudly
//!   ([`ReportError::UnknownId`]) — a verdict with no claim to attach to is a
//!   typo or a stale reference, never a silent no-op. Existence is checked against
//!   the loaded store, so a malformed sibling cannot mask a real id.
//! - **The verdict is attributed.** The commit and actor come from git
//!   ([`resolve_commit`]/[`resolve_actor`]) — the agent's own identity — never
//!   from the request, so a reported verdict is auditable as a commit and cannot
//!   be forged in the payload (invariant #3).
//! - **The server does not commit.** [`run_report`] appends the entry to the
//!   working-tree verdict log via [`claim_core::append_entry`] and returns its
//!   path; committing it is the caller's job (invariant #4, a write to the truth
//!   is a commit). The server has no side channel to the truth.
//!
//! `report` deliberately accepts only conclusive verdicts an agent can actually
//! reach — `held`, `drifted`, `unverifiable` — never `broken`: `broken` means "the
//! check could not run", which is a fact about a mechanical check, not a finding
//! an agent reports about the world.

use claim_core::{append_entry, ClaimId, Event, LogEntry, Timestamp, Verdict};
use claim_store::git::{resolve_actor, resolve_commit, short_commit};
use claim_store::Store;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `report` tool's inputs.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReportRequest {
    /// The id of the claim this verdict is about. Must name a claim that already
    /// exists in the store; an unknown id is rejected.
    pub id: String,
    /// The verdict reached: `held` (the fact still holds), `drifted` (the fact no
    /// longer holds), or `unverifiable` (the agent could not reach a conclusion).
    /// `broken` is deliberately not accepted — that is a mechanical check failing
    /// to run, not a finding to report.
    pub verdict: String,
    /// The evidence behind the verdict — a changelog line, a command's output, a
    /// link, a paragraph of reasoning. Required and non-empty: an unsubstantiated
    /// verdict is not recorded.
    pub evidence: String,
}

/// The `report` tool's structured output: what was written, and what to commit.
///
/// The `commit_hint` names the file the caller must `git add` and commit, because
/// the server does not commit — a write to the truth is a commit the caller makes,
/// with the agent's identity, so it stays auditable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ReportResponse {
    /// The claim the verdict was recorded against.
    pub id: String,
    /// The verdict recorded, echoed back in its canonical kebab-case form
    /// (`held`/`drifted`/`unverifiable`). A string (not the enum) so this crate
    /// advertises a schema without pulling `schemars` into `claim-core`.
    pub verdict: String,
    /// The commit sha the verdict was attributed to (git `HEAD`, or the unborn
    /// sentinel), abbreviated for display.
    pub commit: String,
    /// The actor the verdict was attributed to, `Name <email>` from git config.
    pub actor: String,
    /// The verdict-log file that was written, relative to the store root. Left in
    /// the working tree for the caller to commit — the server never commits.
    pub log_file: String,
    /// A reminder that the write is uncommitted: the exact `git` command that
    /// records it. The server does not run this; a write to the truth is a commit
    /// the caller makes.
    pub commit_hint: String,
}

/// A `report` that could not be recorded, each variant a distinct, loud refusal.
///
/// Every variant degrades toward *not writing a verdict* rather than writing a
/// dubious one: an empty evidence, an unknown id, an unattributable verdict — all
/// are rejected before or instead of an append, never papered over.
#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    /// The `verdict` field was not one of the accepted conclusive verdicts.
    #[error("unknown verdict '{0}'; use one of: held, drifted, unverifiable")]
    UnknownVerdict(String),

    /// The `evidence` field was empty or whitespace-only. An unsubstantiated
    /// verdict is not recorded.
    #[error(
        "evidence is required and must not be empty; a verdict with no evidence is not recorded"
    )]
    EmptyEvidence,

    /// The `id` did not name a claim in the store.
    #[error("no claim with id '{0}' exists in this store; report is only for a claim that already exists")]
    UnknownId(String),

    /// The `id` was not a syntactically valid claim id.
    #[error("invalid claim id '{id}': {reason}")]
    InvalidId {
        /// The rejected id.
        id: String,
        /// Why it is not a valid id.
        reason: String,
    },

    /// Git provenance could not be resolved — no repository, unset identity — so
    /// the verdict would be unattributable and is not written.
    #[error("could not attribute the verdict: {0}")]
    Provenance(#[from] claim_store::GitError),

    /// The append to the verdict log failed (an I/O fault, or core rejected the
    /// entry).
    #[error("could not write the verdict: {0}")]
    Write(#[from] claim_core::Error),
}

/// Append one verdict to a claim's log, with git-resolved provenance, without
/// committing.
///
/// Enforces every honesty rule in order, refusing before it writes: the verdict
/// must parse, the evidence must be non-empty, the id must be valid and must name
/// an existing claim (checked against `load`), and git must yield a commit and
/// actor. Only then is exactly one [`LogEntry`] appended to the working-tree log.
/// `now` is a parameter so the recorded instant is deterministic under test.
///
/// `load_ids` is the set of claim ids the store currently holds (from
/// [`Store::load_all`]), passed in so the existence check and the query share one
/// load rather than each re-walking the store, and so a caller controls exactly
/// which corpus "exists" means against.
///
/// # Errors
///
/// Returns the matching [`ReportError`] for each rule it fails, and never writes a
/// verdict when it returns `Err`. On success returns the written entry's path and
/// the resolved provenance.
pub fn run_report(
    store: &Store,
    request: &ReportRequest,
    load_ids: &[ClaimId],
    now: Timestamp,
) -> Result<ReportResponse, ReportError> {
    let verdict = parse_verdict(&request.verdict)?;

    let evidence = request.evidence.trim();
    if evidence.is_empty() {
        return Err(ReportError::EmptyEvidence);
    }

    let id = request
        .id
        .parse::<ClaimId>()
        .map_err(|e| ReportError::InvalidId {
            id: request.id.clone(),
            reason: e.to_string(),
        })?;

    if !load_ids.contains(&id) {
        return Err(ReportError::UnknownId(request.id.clone()));
    }

    // Provenance first, so an unattributable verdict fails before any write. The
    // commit and actor are the agent's own git identity — never the request's — so
    // the verdict is auditable as a commit and cannot be forged in the payload.
    let commit = resolve_commit(store.root())?;
    let actor = resolve_actor(store.root())?;

    let entry = LogEntry {
        at: now,
        commit: commit.clone(),
        actor: actor.clone(),
        event: Event::Verification {
            verdict,
            evidence: Some(evidence.to_owned()),
        },
    };

    let written = append_entry(&store.log_dir(), &id, &entry)?;
    let log_file = written
        .strip_prefix(store.root())
        .unwrap_or(&written)
        .display()
        .to_string();

    Ok(ReportResponse {
        id: request.id.clone(),
        verdict: verdict_word(verdict).to_owned(),
        commit: short_commit(&commit),
        actor,
        // The store root is single-quoted so a path with spaces still yields a runnable
        // `git -C` command; the log path and id are tool-controlled and carry no spaces.
        commit_hint: format!(
            "git -C '{}' add {log_file} && git commit -m \"claim: record {} verdict for {}\"",
            store.root().display(),
            verdict_word(verdict),
            request.id,
        ),
        log_file,
    })
}

/// Parse the `verdict` field into a [`Verdict`], accepting only the conclusive
/// verdicts an agent can report and rejecting `broken`.
fn parse_verdict(raw: &str) -> Result<Verdict, ReportError> {
    match raw {
        "held" => Ok(Verdict::Held),
        "drifted" => Ok(Verdict::Drifted),
        "unverifiable" => Ok(Verdict::Unverifiable),
        other => Err(ReportError::UnknownVerdict(other.to_owned())),
    }
}

/// The lowercase word for a verdict, for the commit-message hint.
fn verdict_word(v: Verdict) -> &'static str {
    match v {
        Verdict::Held => "held",
        Verdict::Drifted => "drifted",
        Verdict::Unverifiable => "unverifiable",
        Verdict::Broken => "broken",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestStore;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    /// A store holding one claim `pin`, with its id in the loaded-ids set.
    fn with_one_claim() -> (TestStore, Vec<ClaimId>) {
        let s = TestStore::new();
        s.write_claim(
            "pin",
            &TestStore::claim_text("pin", "We pin libfoo at 4.2.", &[]),
        );
        let ids = vec!["pin".parse::<ClaimId>().unwrap()];
        (s, ids)
    }

    fn request(id: &str, verdict: &str, evidence: &str) -> ReportRequest {
        ReportRequest {
            id: id.to_owned(),
            verdict: verdict.to_owned(),
            evidence: evidence.to_owned(),
        }
    }

    #[test]
    fn appends_exactly_one_verdict_with_the_right_verdict_evidence_and_provenance() {
        let (s, ids) = with_one_claim();
        let now = ts("2026-07-17T12:00:00Z");
        let resp = run_report(
            &s.store,
            &request("pin", "held", "grep confirmed libfoo==4.2"),
            &ids,
            now,
        )
        .unwrap();

        assert_eq!(resp.id, "pin");
        assert_eq!(resp.verdict, "held");
        // Provenance is the store's git identity, not anything from the request.
        assert_eq!(resp.actor, "Test Agent <agent@example.com>");

        // Exactly one entry was written, carrying the verdict, the evidence, and
        // the resolved commit and actor.
        assert_eq!(s.log_count("pin"), 1);
        let entries = s.log_entries("pin");
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e["at"], "2026-07-17T12:00:00Z");
        assert_eq!(e["event"]["type"], "verification");
        assert_eq!(e["event"]["verdict"], "held");
        assert_eq!(e["event"]["evidence"], "grep confirmed libfoo==4.2");
        assert_eq!(e["actor"], "Test Agent <agent@example.com>");
        // The commit is a real 40-char sha (HEAD), not a placeholder.
        assert_eq!(e["commit"].as_str().unwrap().len(), 40);
    }

    #[test]
    fn report_writes_to_the_working_tree_and_does_not_commit() {
        let (s, ids) = with_one_claim();
        // A clean tree before the report (the claim file is committed by the
        // helper? no — write_claim writes an uncommitted file). Commit everything
        // first so the only post-report change is the verdict itself.
        std::process::Command::new("git")
            .arg("-C")
            .arg(s.root())
            .args(["add", "-A"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(s.root())
            .args(["commit", "-q", "-m", "add claim"])
            .status()
            .unwrap();
        assert!(!s.working_tree_has_changes(), "clean before report");

        let resp = run_report(
            &s.store,
            &request("pin", "held", "evidence here"),
            &ids,
            ts("2026-07-17T12:00:00Z"),
        )
        .unwrap();

        // The verdict is on disk but uncommitted: the server does not commit.
        assert!(
            s.working_tree_has_changes(),
            "the verdict is left in the working tree, uncommitted"
        );
        // And the response tells the caller exactly what to commit, and that it is
        // the caller's job.
        assert!(resp.log_file.starts_with(".claims/log/pin/"));
        assert!(resp.commit_hint.contains("git commit"));
    }

    #[test]
    fn empty_or_whitespace_evidence_is_rejected_and_writes_nothing() {
        let (s, ids) = with_one_claim();
        let now = ts("2026-07-17T12:00:00Z");
        for blank in ["", "   ", "\t\n"] {
            let err = run_report(&s.store, &request("pin", "held", blank), &ids, now).unwrap_err();
            assert!(
                matches!(err, ReportError::EmptyEvidence),
                "blank {blank:?} must be rejected as empty evidence"
            );
        }
        assert_eq!(
            s.log_count("pin"),
            0,
            "nothing was written for any rejection"
        );
    }

    #[test]
    fn an_unknown_id_is_rejected_loudly_and_writes_nothing() {
        let (s, ids) = with_one_claim();
        let err = run_report(
            &s.store,
            &request("does-not-exist", "held", "evidence"),
            &ids,
            ts("2026-07-17T12:00:00Z"),
        )
        .unwrap_err();
        assert!(matches!(err, ReportError::UnknownId(id) if id == "does-not-exist"));
        assert_eq!(s.log_count("does-not-exist"), 0);
    }

    #[test]
    fn an_invalid_id_is_rejected_before_any_write() {
        let (s, ids) = with_one_claim();
        let err = run_report(
            &s.store,
            &request("Not A Valid Id", "held", "evidence"),
            &ids,
            ts("2026-07-17T12:00:00Z"),
        )
        .unwrap_err();
        assert!(matches!(err, ReportError::InvalidId { .. }));
    }

    #[test]
    fn an_unknown_verdict_is_rejected() {
        let (s, ids) = with_one_claim();
        let err = run_report(
            &s.store,
            &request("pin", "sortof", "evidence"),
            &ids,
            ts("2026-07-17T12:00:00Z"),
        )
        .unwrap_err();
        assert!(matches!(err, ReportError::UnknownVerdict(v) if v == "sortof"));
    }

    #[test]
    fn broken_cannot_be_reported() {
        // `broken` is a mechanical check failing to run, not a finding an agent
        // reports; report accepts only conclusive verdicts.
        let (s, ids) = with_one_claim();
        let err = run_report(
            &s.store,
            &request("pin", "broken", "evidence"),
            &ids,
            ts("2026-07-17T12:00:00Z"),
        )
        .unwrap_err();
        assert!(matches!(err, ReportError::UnknownVerdict(v) if v == "broken"));
        assert_eq!(s.log_count("pin"), 0);
    }

    #[test]
    fn a_drifted_verdict_is_recorded_faithfully() {
        let (s, ids) = with_one_claim();
        let resp = run_report(
            &s.store,
            &request(
                "pin",
                "drifted",
                "libfoo 5.1 fixed the CJK bug; the pin is obsolete",
            ),
            &ids,
            ts("2026-07-17T12:00:00Z"),
        )
        .unwrap();
        assert_eq!(resp.verdict, "drifted");
        let entries = s.log_entries("pin");
        assert_eq!(entries[0]["event"]["verdict"], "drifted");
    }

    #[test]
    fn a_second_report_appends_rather_than_overwriting() {
        let (s, ids) = with_one_claim();
        run_report(
            &s.store,
            &request("pin", "held", "first"),
            &ids,
            ts("2026-07-17T12:00:00Z"),
        )
        .unwrap();
        run_report(
            &s.store,
            &request("pin", "drifted", "second"),
            &ids,
            ts("2026-07-18T12:00:00Z"),
        )
        .unwrap();
        assert_eq!(s.log_count("pin"), 2, "append-only: both verdicts survive");
        // Both entries carry their own distinct content — verdict, timestamp, and
        // evidence — so writing one entry twice (or the second clobbering the
        // first) could not pass. The helper returns them in filename order, which
        // is chronological.
        let entries = s.log_entries("pin");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["at"], "2026-07-17T12:00:00Z");
        assert_eq!(entries[0]["event"]["verdict"], "held");
        assert_eq!(entries[0]["event"]["evidence"], "first");
        assert_eq!(entries[1]["at"], "2026-07-18T12:00:00Z");
        assert_eq!(entries[1]["event"]["verdict"], "drifted");
        assert_eq!(entries[1]["event"]["evidence"], "second");
    }
}
