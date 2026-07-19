//! The event envelope: one attested observation on the hub's ledger.
//!
//! This is the shape of a single event as HUB.md §2 fixes it — the hub's one
//! piece of primary state is an append-only log of these. In v1 the log carries two
//! kinds, and the grammar is deliberately wider: later kinds (a spot-audit result,
//! an acknowledgement) append to the same log with the same envelope, so the
//! [`EventKind`] enum is the seam for that growth.
//!
//! ## Two event kinds, one envelope
//!
//! A [`EventKind::Verdict`] is an attested observation from a CI producer — the
//! ingest gate's only output. A [`EventKind::Nag`] is the hub's own scheduling
//! telemetry (hub-11): a delivery mark the router appends when a claim *transitions*
//! into drifted, stale, or a lapsed skip, so "already nagged" is **derived** from the
//! ledger and never a mutable flag (invariant #3). A nag is *not* a verdict — it
//! carries no [`verdict`](Event::verdict) and no single [`check`](Event::check) — so
//! those two fields are [`Option`], `None` on a nag and `Some` on a verdict. That
//! makes invariant #4 structural: a nag cannot be rendered as a verdict because it
//! holds none, the same kind-filter the dossier applies. The nag's own payload (the
//! transition and the group it fired for) rides in the [`Producer`] block, the hub's
//! own principal (see [`crate::nag`]).
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
//! - **The producer block is kept whole.** The verified pipeline identity (issuer,
//!   repository, workflow, run) is retained as structured JSON — every claim the
//!   ingest gate verified, value-for-value, none distilled into named fields — so
//!   the trust judgment stays *re-derivable* later rather than made once at the
//!   door (HUB.md §4, invariant #3). It is preserved semantically, not byte-for-byte
//!   (see [`Producer`]).

use claim_core::{Timestamp, Verdict};
use serde::{Deserialize, Serialize};

/// One event on the ledger: a verified [`Verdict`] observation, or a hub-authored
/// [`nag`](EventKind::Nag) delivery mark.
///
/// The fields are HUB.md §2's set; the type carries `#[serde(deny_unknown_fields)]`
/// so a stored or received envelope with an unrecognized field is rejected naming it,
/// never half-read. Equality is structural and the (de)serialization is lossless, so
/// `to`/`from` JSON round-trips to an equal value — the property the ledger's integrity
/// rests on.
///
/// [`verdict`](Event::verdict) and [`check`](Event::check) are `Option` because a nag
/// event has neither: it is the hub's own scheduling telemetry, not a check result, and
/// carrying no verdict is what keeps invariant #4 structural (a nag cannot masquerade as
/// a verdict — there is nothing to render as one). A verdict event carries both `Some`;
/// a nag carries both `None` and puts its payload in [`producer`](Event::producer). Use
/// [`Event::verdict`] and [`Event::nag`] to construct the two so the invariant is upheld
/// at the constructor, not just by convention.
///
/// Dedup is not a field here: HUB.md §2's redelivery rule keys on
/// (`producer` run, `claim`, `check` identity), which the storage layer enforces
/// as a unique index over these fields, not something the envelope records about
/// itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    /// The event kind: [`EventKind::Verdict`] or [`EventKind::Nag`].
    pub kind: EventKind,
    /// The claim's id, as written in its store (e.g. `payments/libfoo-pin`). On a nag
    /// this is the claim the transition is about (a grouped nag names its primary
    /// claim; the whole group is in the producer payload).
    pub claim: String,
    /// Which check of the claim a *verdict* is about, by position and content-identity
    /// (see [`CheckRef`]). `None` on a nag, which is not about a single check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<CheckRef>,
    /// The verdict reported for that check. The shared [`claim_core::Verdict`], so
    /// `held`/`drifted`/`unverifiable`/`broken` mean exactly what the CLI meant.
    /// `None` on a nag, which reports no verdict (invariant #4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    /// The evidence the check recorded, if any — capped at ingest (see
    /// [`crate::cap_evidence`]) before an envelope is built, so what lands here is
    /// already bounded. `None` when the check recorded none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// The commit sha this event is anchored to: for a verdict, the sha the check was
    /// reported against; for a nag, the commit that groups the transition (one commit
    /// breaking N claims is one nag).
    pub commit: String,
    /// The connected store the claim lives in (e.g. `github.com/acme/payments`).
    pub store: String,
    /// The producer identity, kept whole. For a verdict, the verified pipeline
    /// identity; for a nag, the hub's own principal plus the nag payload (see
    /// [`Producer`] and [`crate::nag`]).
    pub producer: Producer,
    /// When this event was recorded (a UTC instant, RFC 3339). The shared
    /// [`claim_core::Timestamp`], so every hub timestamp round-trips losslessly and
    /// compares unambiguously.
    pub reported_at: Timestamp,
}

impl Event {
    /// Construct a verdict event: an attested check observation.
    ///
    /// The [`check`](Event::check) and [`verdict`](Event::verdict) are `Some`, so this
    /// is unambiguously a verdict. Evidence and the rest are set by the caller after.
    #[must_use]
    pub fn verdict(
        claim: impl Into<String>,
        check: CheckRef,
        verdict: Verdict,
        commit: impl Into<String>,
        store: impl Into<String>,
        producer: Producer,
        reported_at: Timestamp,
    ) -> Self {
        Self {
            kind: EventKind::Verdict,
            claim: claim.into(),
            check: Some(check),
            verdict: Some(verdict),
            evidence: None,
            commit: commit.into(),
            store: store.into(),
            producer,
            reported_at,
        }
    }

    /// Construct a nag event: the hub's own delivery mark for a transition.
    ///
    /// [`check`](Event::check) and [`verdict`](Event::verdict) are `None` — a nag is not
    /// a verdict — so this event can never be read as one (invariant #4). The `producer`
    /// carries the hub principal and the nag payload; `commit` is the transition's group
    /// commit; `claim` names the transition's primary claim.
    #[must_use]
    pub fn nag(
        claim: impl Into<String>,
        commit: impl Into<String>,
        store: impl Into<String>,
        producer: Producer,
        reported_at: Timestamp,
    ) -> Self {
        Self {
            kind: EventKind::Nag,
            claim: claim.into(),
            check: None,
            verdict: None,
            evidence: None,
            commit: commit.into(),
            store: store.into(),
            producer,
            reported_at,
        }
    }
}

/// The kind of an event on the ledger.
///
/// v1 has two kinds; the enum exists so later kinds (`audit`, `ack`) extend the
/// ledger grammar without a new store. `#[non_exhaustive]` reserves that growth: a
/// match on this enum in a consumer must stay total, so a new kind forces every
/// consumer to decide how to treat it rather than defaulting to a silent pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum EventKind {
    /// A reported check result: an attested observation from a CI producer.
    Verdict,
    /// A nag delivery mark: the hub's own record that it routed a transition (a claim
    /// entering drifted, crossing into stale, or a skip's `until` lapsing) to an owner.
    /// Not a verdict — it bears on the *schedule*, never on a claim's standing (invariant
    /// #4), so the deriver and the dossier skip it. Appended by the router (hub-11).
    Nag,
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

/// The verified producer identity behind an event, kept whole.
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
/// Held as a `serde_json::Map` so it is always a JSON object. The values are
/// preserved value-for-value and no key is added or dropped, so the trust judgment
/// re-derives from exactly the claims that were verified. It is *not* byte-faithful
/// to the received JSON: this workspace's `serde_json` has no `preserve_order`, so
/// the map is a `BTreeMap` that sorts keys and re-normalizes whitespace on
/// re-serialization — the semantics survive, the exact byte form does not. That
/// matters only for a future the deferred signed-attestation path opens (HUB.md
/// §4): if the hub ever verifies a signature over the *exact* producer JSON bytes,
/// it will need byte fidelity (e.g. enabling `serde_json/preserve_order`, or
/// retaining the raw bytes beside this structured view). A caveat for that item,
/// not a v1 change. The producer's *run* identifier — one of the keys here — is
/// part of the dedup key HUB.md §2 defines, read by the storage layer, not
/// enforced by this type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Producer(pub serde_json::Map<String, serde_json::Value>);

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully populated verdict event, for round-trip and field tests.
    fn sample_event() -> Event {
        let mut producer = serde_json::Map::new();
        producer.insert(
            "iss".into(),
            serde_json::json!("https://token.actions.githubusercontent.com"),
        );
        producer.insert("repository".into(), serde_json::json!("acme/payments"));
        producer.insert("workflow".into(), serde_json::json!("verify"));
        producer.insert("run".into(), serde_json::json!("1234567890"));
        let mut event = Event::verdict(
            "payments/libfoo-pin",
            CheckRef {
                index: 1,
                digest: "a".repeat(64),
            },
            Verdict::Held,
            "8f2c0a1",
            "github.com/acme/payments",
            Producer(producer),
            "2026-07-18T06:00:00Z".parse().unwrap(),
        );
        event.evidence = Some("libfoo==4.2".into());
        event
    }

    /// A nag event, carrying no verdict and no single check (invariant #4).
    fn sample_nag() -> Event {
        let mut producer = serde_json::Map::new();
        producer.insert("principal".into(), serde_json::json!("hub-router"));
        producer.insert("run".into(), serde_json::json!("drifted@8f2c0a1"));
        producer.insert("transition".into(), serde_json::json!("drifted"));
        Event::nag(
            "payments/libfoo-pin",
            "8f2c0a1",
            "github.com/acme/payments",
            Producer(producer),
            "2026-07-18T06:00:00Z".parse().unwrap(),
        )
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
    fn producer_claims_survive_round_trip_value_for_value() {
        // A producer with an unusual extra claim must survive round-trip with every
        // key and value intact: the trust judgment is re-derived from the whole
        // block, so nothing is dropped. (Key *order* and byte form are not promised —
        // this workspace's serde_json sorts keys — but no claim is lost.)
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
        assert_eq!(
            serde_json::to_string(&EventKind::Verdict).unwrap(),
            "\"verdict\""
        );
        assert_eq!(serde_json::to_string(&EventKind::Nag).unwrap(), "\"nag\"");
    }

    #[test]
    fn a_nag_event_round_trips_and_carries_no_verdict() {
        // A nag has no verdict and no check — invariant #4 made structural: it cannot be
        // read as a verdict because it holds none. Both fields are omitted from the wire.
        let nag = sample_nag();
        assert_eq!(nag.kind, EventKind::Nag);
        assert!(nag.verdict.is_none(), "a nag carries no verdict");
        assert!(nag.check.is_none(), "a nag is not about a single check");
        let json = serde_json::to_string(&nag).unwrap();
        assert!(
            !json.contains("verdict") && !json.contains("\"check\""),
            "a nag omits verdict and check on the wire: {json}"
        );
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(nag, back, "nag round-trips losslessly");
    }

    #[test]
    fn the_verdict_constructor_sets_check_and_verdict() {
        let event = sample_event();
        assert_eq!(event.kind, EventKind::Verdict);
        assert_eq!(event.verdict, Some(Verdict::Held));
        assert!(event.check.is_some(), "a verdict is about a check");
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
