//! The event envelope: one attested observation on the hub's ledger.
//!
//! This is the shape of a single event as HUB.md §2 fixes it — the hub's one
//! piece of primary state is an append-only log of these. In v1 every event is a
//! `verdict`, but the grammar is deliberately wider: later kinds (a delivered
//! `nag`, a spot-audit result, an acknowledgement) append to the same log with
//! the same envelope, so the [`EventKind`] enum is the seam for that growth.
//!
//! Three properties are load-bearing and tested:
//!
//! - **Lossless round-trip.** An envelope serialized to JSON and read back is
//!   equal to the original. The ledger stores exactly what was attested; a hub
//!   that silently reshaped an event on the way in or out could not honestly claim
//!   to preserve the evidence a standing derives from.
//! - **Shared honesty types, not re-declared ones.** The `verdict` is a
//!   [`claim_core::Verdict`] and `reported_at` is a [`claim_core::Timestamp`], the
//!   same types the CLI produces — so the two ends of the wire cannot disagree
//!   about what `held` means or how an instant is spelled.
//! - **The producer block is verbatim.** The verified pipeline identity (issuer,
//!   repository, workflow, run) is kept as structured JSON exactly as the ingest
//!   gate verified it, never distilled into named fields, so the trust judgment
//!   stays *re-derivable* later rather than made once at the door (HUB.md §4,
//!   invariant #3).

use claim_core::{Timestamp, Verdict};
use serde::{Deserialize, Serialize};

/// One attested observation on the ledger: a fact reported about a claim's check
/// at a moment, by a verified producer.
///
/// The fields are exactly HUB.md §2's set; the type carries
/// `#[serde(deny_unknown_fields)]` so a stored or received envelope with an
/// unrecognized field is rejected naming it, never half-read. Equality is
/// structural and the (de)serialization is lossless, so `to`/`from` JSON
/// round-trips to an equal value — the property the ledger's integrity rests on.
///
/// Dedup is not a field here: HUB.md §2's redelivery rule keys on
/// (`producer` run, `claim`, `check` identity), which the storage layer enforces
/// as a unique index over these fields, not something the envelope records about
/// itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    /// The event kind. `verdict` in v1; the enum is the seam for later kinds.
    pub kind: EventKind,
    /// The claim's id, as written in its store (e.g. `payments/libfoo-pin`).
    pub claim: String,
    /// Which check of the claim this observation is about, by position and
    /// content-identity. See [`CheckRef`].
    pub check: CheckRef,
    /// The verdict reported for that check. The shared [`claim_core::Verdict`], so
    /// `held`/`drifted`/`unverifiable`/`broken` mean exactly what the CLI meant.
    pub verdict: Verdict,
    /// The evidence the check recorded, if any — capped at ingest (see
    /// [`crate::cap_evidence`]) before an envelope is built, so what lands here is
    /// already bounded. `None` when the check recorded none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// The commit sha the check was reported against, tying the observation to a
    /// point in the store's history.
    pub commit: String,
    /// The connected store the claim lives in (e.g. `github.com/acme/payments`).
    pub store: String,
    /// The verified producer identity, kept verbatim. See [`Producer`].
    pub producer: Producer,
    /// When the producer reported this observation (a UTC instant, RFC 3339). The
    /// shared [`claim_core::Timestamp`], so every hub timestamp round-trips
    /// losslessly and compares unambiguously.
    pub reported_at: Timestamp,
}

/// The kind of an event on the ledger.
///
/// v1 has one kind, `verdict`; the enum exists so later kinds (`nag`, `audit`,
/// `ack`) extend the ledger grammar without a new store. `#[non_exhaustive]`
/// reserves that growth: a match on this enum in the deriver must stay total, so a
/// new kind forces every consumer to decide how to treat it rather than defaulting
/// to a silent pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum EventKind {
    /// A reported check result — the only kind in v1.
    Verdict,
}

/// Which check of a claim an event is about: its position *and* its content
/// identity.
///
/// The `index` alone is not identity — reorder a claim's checks and index 0 is a
/// different check — so the envelope carries the `digest` of the check's canonical
/// definition beside it ([`crate::check_digest`]). The digest is what stops a
/// shallow check's `held` from clearing a deep check's `drifted` (issue #18): the
/// ledger positions a verdict by content, and the index is retained only to name
/// the check back to a reader of the report it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRef {
    /// The check's zero-based position in the claim's declared check list, as the
    /// `--json` report ordered it. A locator back to the report, not an identity.
    pub index: usize,
    /// The canonical digest of the check's definition ([`crate::check_digest`]):
    /// the stable, reorder-proof identity the ledger keys a check's history on.
    pub digest: String,
}

/// The verified producer identity behind an event, kept verbatim.
///
/// The ingest gate verifies a pipeline's OIDC id-token and records the claims it
/// verified — issuer, repository, workflow, ref, run id — into this block *as it
/// verified them* (HUB.md §4). It is a JSON object, not a fixed struct, on
/// purpose: the trust judgment must be re-derivable from the raw verified claims
/// later, and different producers (and later, different identity providers) carry
/// different claim sets, so distilling them into named Rust fields now would
/// discard exactly the evidence a future audit needs. Provenance is derived from
/// this, never asserted by a claim file (invariant #3).
///
/// Held as a `serde_json::Map` so it is always a JSON object and round-trips
/// key-for-key. The producer's *run* identifier — one of the keys here — is part
/// of the dedup key HUB.md §2 defines, read by the storage layer, not enforced by
/// this type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Producer(pub serde_json::Map<String, serde_json::Value>);

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully populated event, for round-trip and field tests.
    fn sample_event() -> Event {
        let mut producer = serde_json::Map::new();
        producer.insert(
            "iss".into(),
            serde_json::json!("https://token.actions.githubusercontent.com"),
        );
        producer.insert("repository".into(), serde_json::json!("acme/payments"));
        producer.insert("workflow".into(), serde_json::json!("verify"));
        producer.insert("run".into(), serde_json::json!("1234567890"));
        Event {
            kind: EventKind::Verdict,
            claim: "payments/libfoo-pin".into(),
            check: CheckRef {
                index: 1,
                digest: "a".repeat(64),
            },
            verdict: Verdict::Held,
            evidence: Some("libfoo==4.2".into()),
            commit: "8f2c0a1".into(),
            store: "github.com/acme/payments".into(),
            producer: Producer(producer),
            reported_at: "2026-07-18T06:00:00Z".parse().unwrap(),
        }
    }

    #[test]
    fn event_round_trips_losslessly() {
        let event = sample_event();
        let json = serde_json::to_string(&event).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back, "value → JSON → value must be identity");
    }

    #[test]
    fn round_trip_holds_without_optional_evidence() {
        let mut event = sample_event();
        event.evidence = None;
        let json = serde_json::to_string(&event).unwrap();
        // Absent evidence is omitted from the wire, not serialized as null.
        assert!(!json.contains("evidence"), "no empty evidence key: {json}");
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn an_unknown_envelope_field_is_rejected_naming_it() {
        let mut v: serde_json::Value = serde_json::to_value(sample_event()).unwrap();
        v.as_object_mut()
            .unwrap()
            .insert("forged_status".into(), serde_json::json!("verified"));
        let err = serde_json::from_value::<Event>(v).unwrap_err();
        assert!(
            err.to_string().contains("forged_status"),
            "the error names the offending field: {err}"
        );
    }

    #[test]
    fn an_unknown_check_ref_field_is_rejected_naming_it() {
        let mut v: serde_json::Value = serde_json::to_value(sample_event()).unwrap();
        v["check"]
            .as_object_mut()
            .unwrap()
            .insert("trust_me".into(), serde_json::json!(true));
        let err = serde_json::from_value::<Event>(v).unwrap_err();
        assert!(
            err.to_string().contains("trust_me"),
            "a forged check identity field is rejected naming it: {err}"
        );
    }

    #[test]
    fn producer_is_preserved_verbatim_key_for_key() {
        // A producer with an unusual extra claim must survive round-trip untouched:
        // the trust judgment is re-derived from the raw block, so nothing is dropped.
        let mut event = sample_event();
        event.producer.0.insert(
            "job_workflow_ref".into(),
            serde_json::json!("acme/payments/.github/workflows/verify.yml@refs/heads/main"),
        );
        let json = serde_json::to_string(&event).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(event.producer, back.producer);
        assert_eq!(
            back.producer.0["job_workflow_ref"],
            serde_json::json!("acme/payments/.github/workflows/verify.yml@refs/heads/main")
        );
    }

    #[test]
    fn kind_serializes_as_kebab_case() {
        let json = serde_json::to_string(&EventKind::Verdict).unwrap();
        assert_eq!(json, "\"verdict\"");
    }

    #[test]
    fn reported_at_round_trips_as_rfc3339() {
        // The shared jiff Timestamp: the instant is preserved exactly, and the wire
        // form is RFC 3339.
        let event = sample_event();
        let v = serde_json::to_value(&event).unwrap();
        assert_eq!(v["reported_at"], serde_json::json!("2026-07-18T06:00:00Z"));
    }
}
