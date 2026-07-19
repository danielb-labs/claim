//! Evidence capping at ingest.
//!
//! A verdict event carries the check's `evidence` — the captured output or an
//! agent's note. The CLI already caps a single check's evidence
//! ([`claim_core::DEFAULT_OUTPUT_CAP`], 8 KiB), but the hub cannot trust that: it
//! ingests reports from many CLI versions and any repo's CI, so a hostile or
//! buggy producer could push megabytes. The ledger is append-only and
//! customer-owned storage, so unbounded evidence is a durable cost the hub must
//! bound at the door.
//!
//! The cap is applied by **truncating with a recorded marker, never dropping
//! silently** (invariant #6: the failure mode is a nag, never a lie). A reader of
//! a capped event sees exactly where the evidence was cut and why, so the hub
//! never presents a partial record as if it were whole.

/// The ingest cap on a single event's evidence, in bytes.
///
/// A few KB: enough for the diff, the failing line, or an agent's short note —
/// the evidence a human or agent actually reads when a claim drifts — while
/// bounding what one event can commit to the append-only ledger. Larger than the
/// CLI's per-check `DEFAULT_OUTPUT_CAP` is unnecessary (the CLI already trimmed to
/// 8 KiB), and smaller would clip legitimate evidence, so this sits at the same
/// order of magnitude, chosen for the hub's own storage discipline rather than
/// inherited from the CLI's.
pub const EVIDENCE_CAP: usize = 8 * 1024;

/// The marker appended when evidence is truncated at the cap, so a cut is always
/// visible. Names the cap and states that more existed — never a silent clip.
const TRUNCATION_MARKER: &str = "\n[evidence truncated at ingest]";

/// Cap `evidence` at [`EVIDENCE_CAP`] bytes, truncating with a recorded marker.
///
/// Returns the evidence unchanged when it is within the cap. When it exceeds the
/// cap, returns the retained prefix (cut on a UTF-8 char boundary, so the result
/// is always valid UTF-8) followed by a truncation marker naming the cut, so a
/// reader can never mistake truncated evidence for the whole of it. The returned
/// string is therefore at most [`EVIDENCE_CAP`] plus the marker's length.
///
/// The cap is measured against the *original* evidence: the marker is additive, so
/// a producer cannot dodge the cap, and re-capping already-capped evidence (whose
/// prefix is under the cap) is a no-op.
#[must_use]
pub fn cap_evidence(evidence: &str) -> String {
    if evidence.len() <= EVIDENCE_CAP {
        return evidence.to_owned();
    }
    let mut end = EVIDENCE_CAP;
    while end > 0 && !evidence.is_char_boundary(end) {
        end -= 1;
    }
    let mut capped = String::with_capacity(end + TRUNCATION_MARKER.len());
    capped.push_str(&evidence[..end]);
    capped.push_str(TRUNCATION_MARKER);
    capped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_under_the_cap_is_untouched() {
        let small = "the failing line: libfoo==5.0";
        assert_eq!(cap_evidence(small), small);
        assert!(!cap_evidence(small).contains("truncated"));
    }

    #[test]
    fn evidence_exactly_at_the_cap_is_untouched() {
        let exact = "x".repeat(EVIDENCE_CAP);
        assert_eq!(cap_evidence(&exact), exact);
    }

    #[test]
    fn evidence_over_the_cap_is_truncated_with_a_marker() {
        let big = "y".repeat(EVIDENCE_CAP + 4096);
        let capped = cap_evidence(&big);
        assert!(capped.len() < big.len(), "it shrank");
        assert!(
            capped.ends_with(TRUNCATION_MARKER),
            "the cut is marked, never silent (invariant #6): {}",
            &capped[capped.len().saturating_sub(40)..]
        );
        // The retained content is the original's prefix, and no more than the cap.
        let retained = capped.strip_suffix(TRUNCATION_MARKER).unwrap();
        assert!(retained.len() <= EVIDENCE_CAP);
        assert!(big.starts_with(retained));
    }

    #[test]
    fn truncation_never_splits_a_multibyte_char() {
        // A run of 3-byte chars straddling the cap boundary: the result must stay
        // valid UTF-8 (the type guarantees it, but the boundary walk is the
        // load-bearing part, so pin it).
        let big = "€".repeat(EVIDENCE_CAP); // 3 bytes each → well over the cap
        let capped = cap_evidence(&big);
        let retained = capped.strip_suffix(TRUNCATION_MARKER).unwrap();
        assert!(retained.len() <= EVIDENCE_CAP);
        // Every retained char is a whole `€`; `chars()` would panic on a split one.
        assert!(retained.chars().all(|c| c == '€'));
    }
}
