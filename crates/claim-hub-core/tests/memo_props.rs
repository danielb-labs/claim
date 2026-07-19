//! Property tests for the memo: it invalidates on exactly the three causes, and a
//! discarded cache recomputes an identical answer.
//!
//! The load-bearing claim is that the memo is a *cache, never a store* (invariant
//! #3): for any sequence of reads, the answer the memo returns is exactly the answer
//! [`derive`] would return for the same inputs — the memo only changes how often the
//! work runs. These tests sample a spread of input transitions (a new event, a
//! registry bump, config change, and the clock crossing a horizon) and assert the
//! memoized answer equals the freshly-derived one at every step, and that a cleared
//! memo is indistinguishable from a warm one in its answers.

use claim_core::{parse_claim_file, Claim, Timestamp, Verdict};
use claim_hub_core::deriver::{derive, ClaimEntry, DeriverConfig, RegistrySnapshot, Standing};
use claim_hub_core::{check_digest, CheckRef, Event, EventKind, Memo, Producer};

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

fn claim_with(id: &str, hub: &str) -> Claim {
    let text = format!("---\nid: {id}\n{hub}checks:\n  - kind: cmd\n    run: \"true\"\n---\nS.\n");
    parse_claim_file(".claims/t.md", &text).expect("valid claim")
}

fn verdict_event(claim: &Claim, verdict: Verdict, at: &str) -> Event {
    let mut producer = serde_json::Map::new();
    producer.insert("run".into(), serde_json::json!("1"));
    Event {
        kind: EventKind::Verdict,
        claim: claim.id.as_str().to_owned(),
        check: CheckRef {
            index: 0,
            digest: check_digest(&claim.checks[0]),
        },
        verdict,
        evidence: None,
        commit: "abc".into(),
        store: "s".into(),
        producer: Producer(producer),
        reported_at: ts(at),
    }
}

fn registry(version: u64, claims: Vec<Claim>) -> RegistrySnapshot {
    RegistrySnapshot {
        version,
        claims: claims
            .iter()
            .map(|claim| ClaimEntry::from_claim("s", claim))
            .collect(),
    }
}

/// One transition in the memo-vs-derive sequence: the inputs to a single read
/// (registry, ledger slice, clock string, config).
type Step<'a> = (
    &'a RegistrySnapshot,
    &'a [(u64, Event)],
    &'a str,
    &'a DeriverConfig,
);

/// A read through the memo always equals a direct derivation, whatever the input
/// transition — the memo never changes the answer, only how often it is computed.
#[test]
fn a_memoized_read_always_equals_a_direct_derivation() {
    let claim = claim_with("t", "hub:\n  max-age: 30d\n");
    let hold = verdict_event(&claim, Verdict::Held, "2026-07-01T00:00:00Z");
    let drift = verdict_event(&claim, Verdict::Drifted, "2026-07-20T00:00:00Z");

    let reg1 = registry(1, vec![claim.clone()]);
    let reg2 = registry(2, vec![claim.clone()]); // a registry bump with the same live set
    let reg_empty = registry(3, vec![]); // a retirement

    let loose = DeriverConfig {
        default_max_age: Some("90d".parse().unwrap()),
        max_age_override: None,
    };
    let tight = DeriverConfig::default();

    // A sequence exercising all three invalidation causes and their combinations:
    // new event, registry bump (same and emptied), config change, and the clock
    // crossing the freshness horizon (2026-07-31) with no input change.
    let steps: &[Step] = &[
        (&reg1, &[(1, hold.clone())], "2026-07-10T00:00:00Z", &tight), // fresh
        (&reg1, &[(1, hold.clone())], "2026-07-31T00:00:00Z", &tight), // clock crossed horizon → stale
        (&reg2, &[(1, hold.clone())], "2026-07-31T00:00:00Z", &tight), // registry bump, still stale
        (&reg2, &[(1, hold.clone())], "2026-07-31T00:00:00Z", &loose), // config loosens → fresh again
        (
            &reg2,
            &[(1, hold.clone()), (2, drift.clone())],
            "2026-07-31T00:00:00Z",
            &loose,
        ), // new event: drift dominates
        (
            &reg_empty,
            &[(1, hold.clone()), (2, drift)],
            "2026-08-01T00:00:00Z",
            &loose,
        ), // retirement
    ];

    let memo = Memo::new();
    for (i, (reg, events, now, config)) in steps.iter().enumerate() {
        let now = ts(now);
        let memoized = memo.read(reg, events, now, config);
        let direct = derive(reg, events, now, config);
        assert_eq!(
            memoized, direct,
            "step {i}: memoized read must equal a direct derivation"
        );
        // A second read at the identical inputs (a cache hit) must also match.
        let cached = memo.read(reg, events, now, config);
        assert_eq!(cached, direct, "step {i}: a cache hit must equal derive()");
    }
}

/// Each of the three invalidation causes, applied in isolation to a warm cache,
/// changes the served answer; nothing else does.
#[test]
fn each_invalidation_cause_changes_the_served_answer_in_isolation() {
    let claim = claim_with("t", "hub:\n  max-age: 30d\n");
    let hold = verdict_event(&claim, Verdict::Held, "2026-07-01T00:00:00Z");
    let drift = verdict_event(&claim, Verdict::Drifted, "2026-07-02T00:00:00Z");
    let reg = registry(1, vec![claim.clone()]);
    let fresh_now = ts("2026-07-10T00:00:00Z");
    let config = DeriverConfig::default();

    // Warm the cache with a verified answer.
    let base_memo = || {
        let m = Memo::new();
        let warmed = m.read(&reg, &[(1, hold.clone())], fresh_now, &config);
        assert_eq!(
            warmed.standing("s", "t").unwrap().standing,
            Standing::Verified
        );
        m
    };

    // Cause 1: a new event (drift). Standing flips to drifted.
    let m = base_memo();
    let after_event = m.read(&reg, &[(1, hold.clone()), (2, drift)], fresh_now, &config);
    assert_eq!(
        after_event.standing("s", "t").unwrap().standing,
        Standing::Drifted,
        "a new event must change the served answer"
    );

    // Cause 2: a registry change (retirement via a version bump + empty live set).
    let m = base_memo();
    let after_registry = m.read(
        &registry(2, vec![]),
        &[(1, hold.clone())],
        fresh_now,
        &config,
    );
    assert_eq!(
        after_registry.standing("s", "t").unwrap().standing,
        Standing::Retired,
        "a registry change must change the served answer"
    );

    // Cause 3: the clock crosses the horizon (30-day window, read at expiry).
    let m = base_memo();
    let after_clock = m.read(
        &reg,
        &[(1, hold.clone())],
        ts("2026-07-31T00:00:00Z"),
        &config,
    );
    assert_eq!(
        after_clock.standing("s", "t").unwrap().standing,
        Standing::Stale,
        "the clock crossing the horizon must change the served answer"
    );

    // A no-op read (identical inputs, clock still short of the horizon) is served from
    // cache and is unchanged — the cache is not invalidated by anything else.
    let m = base_memo();
    let noop = m.read(&reg, &[(1, hold)], ts("2026-07-11T00:00:00Z"), &config);
    assert_eq!(
        noop.standing("s", "t").unwrap().standing,
        Standing::Verified,
        "an input-preserving read short of the horizon stays verified"
    );
}

/// A discarded cache recomputes an identical answer at every read: the memo holds
/// nothing the derivation does not (cache-never-truth).
#[test]
fn a_discarded_cache_recomputes_identically() {
    let claim = claim_with("t", "hub:\n  max-age: 30d\n");
    let hold = verdict_event(&claim, Verdict::Held, "2026-07-01T00:00:00Z");
    let reg = registry(1, vec![claim]);
    let events = [(1, hold)];
    let config = DeriverConfig::default();

    // Read at a spread of clock instants, clearing the cache before each. Each
    // cleared read must equal both a warm read and a direct derivation.
    for instant in [
        "2026-07-05T00:00:00Z",
        "2026-07-31T00:00:00Z",
        "2026-09-01T00:00:00Z",
    ] {
        let now = ts(instant);
        let direct = derive(&reg, &events, now, &config);

        let memo = Memo::new();
        let warm = memo.read(&reg, &events, now, &config);
        memo.clear();
        let recomputed = memo.read(&reg, &events, now, &config);

        assert_eq!(warm, direct, "warm read equals derive() at {instant}");
        assert_eq!(
            recomputed, direct,
            "a cleared cache recomputes identically at {instant}"
        );
    }
}
