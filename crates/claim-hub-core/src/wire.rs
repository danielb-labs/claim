//! Wire-format parsing of the CLI's `claim check --json` report.
//!
//! These types deserialize exactly what `claim check --json` emits, and they are
//! **deliberately independent** of the CLI crate's own serialize structs, never
//! imported from it (`HUB-IMPLEMENTATION.md` §1.7). The hub ingests reports from
//! many repos running many CLI versions: it must parse *what is on the wire*, and
//! a type shared with the in-tree CLI would only ever prove the CLI matches
//! itself — it could not catch a real producer drifting from the contract. The
//! workspace contract test (this crate's `tests/`) runs the built `claim` binary
//! and parses its real output through these types, which keeps the two ends honest
//! *without* coupling them.
//!
//! **These wire types are forward-compatible: an unknown field is tolerated, not
//! rejected.** They deliberately do *not* carry `#[serde(deny_unknown_fields)]`.
//! The obligation to reject an unknown field naming it is the *envelope's*, not
//! the wire report's (`HUB-IMPLEMENTATION.md` §1.7, §4.3–§4.4): the envelope is
//! the hub's own frozen shape, while the report is a foreign input from an
//! independently-versioned producer. If a newer CLI adds one field to its report,
//! an older hub must still read the fields it knows — rejecting the whole report
//! would age every claim in that repo into stale over a field the hub does not
//! even need, a self-inflicted outage and exactly the silent-staleness failure
//! invariant #6 forbids. Genuinely malformed input is still caught: serde rejects
//! a missing required field or a wrong-typed field by default, naming it, without
//! `deny_unknown_fields`.
//!
//! A check's `end` (a [`claim_core::ProcessEnd`], `#[non_exhaustive]`) is captured
//! as a tagged value that preserves the discriminator and its payload verbatim
//! without constraining the set of kinds, so a *newer* CLI's added end kind
//! round-trips through the hub — the verdict, not the end, is what the ledger
//! turns on.

use claim_core::Verdict;
use serde::Deserialize;

/// The top-level `claim check --json` object.
///
/// The report of one `claim check` run: an overall status and exit code, run
/// tallies, the per-claim results, and any per-file load errors. Parsing this
/// successfully means every field the hub relies on was present and well-typed;
/// an unknown field is tolerated (forward-compat — see the module docs), while a
/// missing required or wrong-typed field is rejected naming it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CheckReport {
    /// Always `"ok"` today: the verb ran. The findings live in `exit` and the
    /// per-claim results — a drift is a successful run that found a drift.
    pub status: String,
    /// The overall exit code (0 held / 1 review / 2 broken).
    pub exit: i32,
    /// How many claims were selected and evaluated.
    pub checked: usize,
    /// How many checks produced a verdict this run, across every claim. `0` means
    /// the run verified nothing — never to be read as "all held" (invariant #6).
    pub ran: usize,
    /// How many checks a declared skip suppressed this run, across every claim.
    pub skipped: usize,
    /// The per-claim results.
    pub claims: Vec<ClaimResult>,
    /// Per-file load errors (a malformed claim file, a duplicate id): reported,
    /// not fatal, and flooring `exit` at 2.
    pub errors: Vec<LoadError>,
}

/// One claim's result within a report: its checks' verdicts, its suppressed
/// skips, its supports' resolutions, and its exit contribution.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ClaimResult {
    /// The claim's id.
    pub id: String,
    /// The claim file's path relative to the store root.
    pub file: String,
    /// The verdict-bearing check results — **compacted**, not one per declared check: a
    /// check whose skip was in force is omitted here and appears in
    /// [`skipped`](ClaimResult::skipped) instead. So a result's position in this array is
    /// **not** the check's declared position once a skip precedes a run check. The
    /// declared position each result belongs to is carried on the result itself
    /// ([`CheckResult::index`]); the hub keys a verdict's check identity by that declared
    /// index against the registry, never by the array offset.
    pub checks: Vec<CheckResult>,
    /// Checks whose declared skip suppressed this run: reported, never a pass, and
    /// carrying no verdict. Each carries its own declared [`SkippedCheck::index`].
    pub skipped: Vec<SkippedCheck>,
    /// Each `supports` target's resolution.
    pub supports: Vec<SupportResult>,
    /// The per-claim exit contribution.
    pub exit: i32,
}

/// One check's verdict within a claim's result.
///
/// The `verdict` reuses [`claim_core::Verdict`] — the one honesty enum shared by
/// both ends of the wire — so `held`/`drifted`/`unverifiable`/`broken` cannot be
/// interpreted two ways. `end` is captured permissively (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CheckResult {
    /// The check's zero-based position in the claim's **declared** check list — the key
    /// the hub resolves a check's content digest by (issue #18). It is deliberately
    /// **required**, not `#[serde(default)]`: the array these results sit in is compacted
    /// past skipped checks, so a missing index is not a benign forward-compat gap but an
    /// unkeyable verdict. An older CLI that does not emit `index` (its report predates the
    /// field) is rejected naming the field — a loud refusal, never a verdict silently
    /// filed under check 0's identity (invariant #6, #18). This is the one required field
    /// the wire type gained; the module docs' forward-compat stance holds for every other.
    pub index: usize,
    /// The verdict the check reported.
    pub verdict: Verdict,
    /// The structured process end, preserved verbatim as a tagged value. Not
    /// constrained to a fixed set of kinds: [`claim_core::ProcessEnd`] is
    /// `#[non_exhaustive]`, so a newer CLI's added end kind must round-trip
    /// through the hub, not be rejected.
    pub end: ProcessEndWire,
    /// The human one-liner describing how the process ended (`exit 0`,
    /// `timed out after 60s`). Derived from `end`; the structured form is
    /// authoritative.
    pub detail: String,
    /// The evidence the check recorded, if any. Capped at ingest (see
    /// [`crate::cap_evidence`]) before it reaches the ledger.
    #[serde(default)]
    pub evidence: Option<String>,
    /// Why a declared skip did not apply this run, when worth reporting (a lapsed
    /// `until`, an `unless` that could not be evaluated). Absent on an ordinary run.
    #[serde(default)]
    pub note: Option<String>,
}

/// A check's structured process end, preserved verbatim.
///
/// Deliberately a captured tagged value rather than a mirror of
/// [`claim_core::ProcessEnd`]: it requires the `kind` discriminator (so a
/// malformed `end` with no kind is still rejected) but tolerates any kind and any
/// accompanying payload, because the enum is `#[non_exhaustive]` and the hub must
/// ingest a newer CLI's ends without a redeploy. The verbatim payload is retained
/// so a later hub version that *does* understand a new kind loses nothing.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProcessEndWire {
    /// The end's kind discriminator (`exited`, `timed-out`, `signalled`,
    /// `spawn-failed`, `not-executed`, or a future kind). Required — an `end`
    /// object with no `kind` is not a valid process end and is rejected.
    pub kind: String,
    /// The kind-specific payload (an `exited`'s `code`, a `timed-out`'s `after`),
    /// retained verbatim. Empty for a payloadless kind like `signalled`.
    #[serde(flatten)]
    pub payload: serde_json::Map<String, serde_json::Value>,
}

/// A check whose declared skip suppressed this run: reported so a skip is never
/// silent, and carrying no verdict — a skip is not a pass.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SkippedCheck {
    /// The check's zero-based position in the claim's **declared** check list. Unlike
    /// [`CheckResult::index`], this is `#[serde(default)]`: a skipped check produces no
    /// event, so the ingest gate never keys anything on it, and an older CLI's report
    /// that omits it must still parse (invariant #6). It is carried for a surface that
    /// wants to name exactly which declared check was skipped.
    #[serde(default)]
    pub index: usize,
    /// The author's justification, from the claim's `skip.reason`.
    pub reason: String,
    /// The skip's expiry, if it declared one (RFC 3339). Absent for an indefinite
    /// skip — surfaced plainly so an unbounded mute cannot hide.
    #[serde(default)]
    pub until: Option<String>,
}

/// One resolved (or unresolved) `supports` target within a claim's result.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SupportResult {
    /// The target as written in the claim's `supports` list.
    pub target: String,
    /// Whether it still resolves against the current tree and store.
    pub resolved: bool,
    /// When unresolved, why. Absent when resolved.
    #[serde(default)]
    pub reason: Option<String>,
}

/// A per-file load error the CLI reports without aborting the run: a malformed
/// claim file or a duplicate id. Reported, never fatal, and flooring the exit at
/// 2.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LoadError {
    /// The file that failed to load, relative to the store root.
    pub file: String,
    /// Why it failed, in the CLI's own words.
    pub message: String,
}

impl CheckReport {
    /// Parse a `claim check --json` report from its JSON bytes.
    ///
    /// This is the hub's ingest-side reader of the CLI's wire format. It succeeds
    /// on any report whose fields the hub relies on are present and well-typed,
    /// **tolerating unknown fields** so a newer CLI's added field does not make an
    /// older hub reject an otherwise-valid report (forward-compat; see the module
    /// docs). On a genuinely malformed report it returns the `serde_json` error,
    /// whose message names the offending field (`missing field \`exit\``,
    /// `invalid type` at a field), so a rejection tells a producer exactly what to
    /// fix (invariant #6: loud, never a silent drop).
    ///
    /// # Errors
    ///
    /// Returns the [`serde_json::Error`] for malformed JSON, a missing required
    /// field, or a wrong field type. An *unknown* field is not an error.
    pub fn from_json(bytes: &[u8]) -> serde_json::Result<Self> {
        serde_json::from_slice(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but complete report of one held cmd check, as the CLI emits it.
    fn held_report() -> &'static str {
        r#"{
          "status": "ok",
          "exit": 0,
          "checked": 1,
          "ran": 1,
          "skipped": 0,
          "claims": [
            {
              "id": "pin",
              "file": ".claims/pin.md",
              "checks": [
                { "index": 0, "verdict": "held", "end": { "kind": "exited", "code": 0 }, "detail": "exit 0" }
              ],
              "skipped": [],
              "supports": [],
              "exit": 0
            }
          ],
          "errors": []
        }"#
    }

    #[test]
    fn parses_a_real_held_report() {
        let report = CheckReport::from_json(held_report().as_bytes()).expect("valid report");
        assert_eq!(report.exit, 0);
        assert_eq!(report.claims.len(), 1);
        let check = &report.claims[0].checks[0];
        assert_eq!(check.index, 0);
        assert_eq!(check.verdict, Verdict::Held);
        assert_eq!(check.end.kind, "exited");
        assert_eq!(check.end.payload["code"], serde_json::json!(0));
        // Absent optionals default to None, not an error.
        assert!(check.evidence.is_none());
        assert!(check.note.is_none());
    }

    #[test]
    fn an_unknown_top_level_field_is_tolerated() {
        // Forward-compat: a newer CLI adding a report field must not make an older
        // hub reject the whole report (that would age the repo's claims into stale
        // over a field the hub does not need — invariant #6). The unknown field is
        // ignored and the known fields read through.
        let mut v: serde_json::Value = serde_json::from_str(held_report()).unwrap();
        v.as_object_mut()
            .unwrap()
            .insert("run_duration_ms".into(), serde_json::json!(1234));
        let report = CheckReport::from_json(v.to_string().as_bytes())
            .expect("an unknown top-level field is tolerated, not rejected");
        assert_eq!(report.exit, 0);
        assert_eq!(report.claims[0].checks[0].verdict, Verdict::Held);
    }

    #[test]
    fn an_unknown_field_on_a_check_is_tolerated() {
        // Same forward-compat at the nested level: an added per-check field (e.g. a
        // future `confidence`) is ignored, and the known fields still parse.
        let mut v: serde_json::Value = serde_json::from_str(held_report()).unwrap();
        v["claims"][0]["checks"][0]
            .as_object_mut()
            .unwrap()
            .insert("confidence".into(), serde_json::json!(0.9));
        let report = CheckReport::from_json(v.to_string().as_bytes())
            .expect("an unknown per-check field is tolerated, not rejected");
        assert_eq!(report.claims[0].checks[0].verdict, Verdict::Held);
        assert_eq!(report.claims[0].checks[0].end.kind, "exited");
    }

    #[test]
    fn a_missing_required_field_is_rejected_naming_it() {
        // Tolerating *unknown* fields must not weaken to tolerating a *missing*
        // required one: serde still rejects it by default, naming it.
        let mut v: serde_json::Value = serde_json::from_str(held_report()).unwrap();
        v.as_object_mut().unwrap().remove("exit");
        let err = CheckReport::from_json(v.to_string().as_bytes()).unwrap_err();
        assert!(
            err.to_string().contains("exit"),
            "the error names the missing field: {err}"
        );
    }

    #[test]
    fn a_wrong_typed_field_is_rejected_not_silently_accepted() {
        // A field present but of the wrong type is genuinely malformed, not a
        // forward-compat addition: dropping `deny_unknown_fields` must not weaken to
        // accepting garbage. Here `checked` (an integer count) carries a string, and
        // the parse fails with a diagnostic naming the expected type and location.
        // (serde_json reports a scalar type mismatch positionally, not by field name
        // — unlike a *missing* field, which it does name; that path is covered by
        // `a_missing_required_field_is_rejected_naming_it`.)
        let mut v: serde_json::Value = serde_json::from_str(held_report()).unwrap();
        v["checked"] = serde_json::json!("not a number");
        let err = CheckReport::from_json(v.to_string().as_bytes()).unwrap_err();
        assert!(
            err.to_string().contains("invalid type"),
            "a wrong-typed field is rejected with a diagnostic: {err}"
        );
    }

    #[test]
    fn a_future_process_end_kind_is_tolerated() {
        // `ProcessEnd` is non-exhaustive: a newer CLI may add an end kind, and the
        // hub must ingest it (the verdict, not the end, drives the ledger). The
        // unknown kind and its payload round-trip verbatim.
        let mut v: serde_json::Value = serde_json::from_str(held_report()).unwrap();
        v["claims"][0]["checks"][0]["end"] =
            serde_json::json!({ "kind": "quarantined", "detail": "sandbox refused" });
        let report = CheckReport::from_json(v.to_string().as_bytes())
            .expect("a future end kind must not be rejected");
        let end = &report.claims[0].checks[0].end;
        assert_eq!(end.kind, "quarantined");
        assert_eq!(end.payload["detail"], serde_json::json!("sandbox refused"));
    }

    #[test]
    fn an_end_with_no_kind_is_rejected() {
        // Tolerating new kinds must not weaken to tolerating a kindless end: `kind`
        // is the one required discriminator.
        let mut v: serde_json::Value = serde_json::from_str(held_report()).unwrap();
        v["claims"][0]["checks"][0]["end"] = serde_json::json!({ "code": 0 });
        let err = CheckReport::from_json(v.to_string().as_bytes()).unwrap_err();
        assert!(
            err.to_string().contains("kind"),
            "a process end must carry its kind: {err}"
        );
    }

    #[test]
    fn evidence_and_notes_parse_when_present() {
        let mut v: serde_json::Value = serde_json::from_str(held_report()).unwrap();
        v["claims"][0]["checks"][0]["evidence"] = serde_json::json!("libfoo==5.0");
        v["claims"][0]["checks"][0]["note"] = serde_json::json!("until lapsed");
        let report = CheckReport::from_json(v.to_string().as_bytes()).unwrap();
        let check = &report.claims[0].checks[0];
        assert_eq!(check.evidence.as_deref(), Some("libfoo==5.0"));
        assert_eq!(check.note.as_deref(), Some("until lapsed"));
    }

    #[test]
    fn a_drifted_report_parses_with_its_verdict_and_detail() {
        let json = r#"{
          "status": "ok", "exit": 1, "checked": 1, "ran": 1, "skipped": 0,
          "claims": [{
            "id": "pin", "file": ".claims/pin.md",
            "checks": [{ "index": 0, "verdict": "drifted", "end": { "kind": "exited", "code": 1 }, "detail": "exit 1" }],
            "skipped": [], "supports": [], "exit": 1
          }],
          "errors": []
        }"#;
        let report = CheckReport::from_json(json.as_bytes()).unwrap();
        assert_eq!(report.claims[0].checks[0].verdict, Verdict::Drifted);
    }

    #[test]
    fn a_check_result_carries_its_declared_index() {
        // The declared index is what the hub keys a check's identity on, so it must
        // survive parsing. A compacted `checks` array from a skip-then-run claim: the
        // surviving result carries declared index 1, not its array offset 0.
        let json = r#"{
          "status": "ok", "exit": 1, "checked": 1, "ran": 1, "skipped": 1,
          "claims": [{
            "id": "pin", "file": ".claims/pin.md",
            "checks": [{ "index": 1, "verdict": "drifted", "end": { "kind": "exited", "code": 1 }, "detail": "exit 1" }],
            "skipped": [{ "index": 0, "reason": "parked" }], "supports": [], "exit": 1
          }],
          "errors": []
        }"#;
        let report = CheckReport::from_json(json.as_bytes()).unwrap();
        assert_eq!(
            report.claims[0].checks[0].index, 1,
            "the surviving check's declared index is 1, not its array offset 0"
        );
        assert_eq!(report.claims[0].skipped[0].index, 0);
    }

    #[test]
    fn a_check_result_missing_its_index_is_rejected() {
        // `index` is the one required field the wire CheckResult gained: it is not
        // `#[serde(default)]`, so a report from a CLI predating the field is rejected
        // rather than have every check silently key to index 0 (invariant #6, #18).
        let json = r#"{
          "status": "ok", "exit": 0, "checked": 1, "ran": 1, "skipped": 0,
          "claims": [{
            "id": "pin", "file": ".claims/pin.md",
            "checks": [{ "verdict": "held", "end": { "kind": "exited", "code": 0 }, "detail": "exit 0" }],
            "skipped": [], "supports": [], "exit": 0
          }],
          "errors": []
        }"#;
        let err = CheckReport::from_json(json.as_bytes()).unwrap_err();
        assert!(
            err.to_string().contains("index"),
            "a check with no declared index is rejected naming it: {err}"
        );
    }
}
