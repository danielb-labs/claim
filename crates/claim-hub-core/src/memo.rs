//! The deriver's memo: a cache of the last read model, never a store.
//!
//! A full [`derive()`] is milliseconds over thousands of
//! events at v1 volume, but successive reads at the same inputs should not repeat
//! it. The memo holds one slot — the most recent [`ReadModel`] and the key it was
//! computed under — behind an `RwLock`, and recomputes only when an input actually
//! changed.
//!
//! It is a **cache, never a store** (invariant #3): discard it and the next read
//! recomputes an identical answer from the ledger and registry, because the memo
//! holds nothing the derivation does not. There is no truth here to disagree with
//! the evidence. The property tests (`tests/memo_props.rs`) prove that a discarded
//! memo yields the same answers as a warm one.
//!
//! **The three invalidation causes** (HUB.md §2) are exactly:
//!
//! 1. **A new event** — the ledger head `seq` advanced. Keyed directly.
//! 2. **A registry change** — its `version` counter advanced. Keyed directly.
//! 3. **The clock crossing a threshold** — a claim aged into stale or came due with
//!    no new event. This one needs no timer: each [`ReadModel`] records the earliest
//!    future instant at which any of its answers changes
//!    ([`horizon`](crate::deriver::ReadModel::horizon)), and a read whose clock is at
//!    or past that horizon recomputes. Nothing runs for a claim to *become* stale;
//!    the next read reports it, the way a certificate expires.
//!
//! The first two are compared as an equality key (an internal `MemoKey`); the third
//! is a range check against the cached model's horizon. A read is served from cache
//! only when the key matches *and* the clock has not reached the horizon.

use std::sync::RwLock;

use claim_core::Timestamp;

use crate::deriver::{derive, DeriverConfig, ReadModel, RegistrySnapshot};
use crate::envelope::Event;

/// The equality part of the memo key: the two inputs whose change is a discrete
/// event, not a passage of time.
///
/// The clock is deliberately *not* here — it is not an equality input but a moving
/// one, handled by the horizon range check (see the module docs). Config enters the
/// key as a content hash so a config change invalidates like any other input change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MemoKey {
    /// The ledger head sequence the cached model was derived at; a new event bumps
    /// it. `None` for an empty ledger.
    ledger_head: Option<u64>,
    /// The registry version the cached model was derived at; a sync bumps it.
    registry_version: u64,
    /// A content hash of the config the cached model was derived under
    /// ([`DeriverConfig::hash`]).
    config_hash: u64,
}

/// One cached derivation and the key it is valid under.
struct Cached {
    key: MemoKey,
    model: ReadModel,
}

/// A single-slot memo over [`derive()`], invalidated by exactly the three causes.
///
/// Thread-safe via an `RwLock`: concurrent reads share a cached model, and a
/// recompute takes the write lock only to install a fresh result. There is no
/// dependency here — std suffices at v1 volume (`moka` is the named alternative if a
/// profile ever disagrees, adoptable behind this same type without changing callers).
///
/// **Safety precondition: one slot suffices only because a hub derives the *entire*
/// read model under one key.** [`derive()`] takes the whole registry and ledger and
/// returns every claim's standing at once, so successive reads at the same inputs
/// vary only in the clock — which the horizon handles — and the slot never contends
/// between differently-keyed answers. A caller that instead memoized finer-grained
/// slices through this one slot (per-claim, or per-query subsets of the ledger) would
/// *thrash* it: each differently-scoped read would evict the last. That is never a
/// wrong answer — a miss fails safe to a fresh [`derive()`] (the cache holds no truth,
/// only the last computation) — but it is a latency cliff, and such a caller wants a
/// keyed multi-entry cache (`moka`) behind the same [`read`](Memo::read) shape, not
/// this slot.
#[derive(Default)]
pub struct Memo {
    slot: RwLock<Option<Cached>>,
}

impl std::fmt::Debug for Memo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The `RwLock`'s contents are not `Debug`; report only whether the slot is
        // warm, which is all a diagnostic wants and avoids taking the lock's guard
        // type into the formatter.
        let warm = self.slot.read().map(|g| g.is_some()).unwrap_or(false);
        f.debug_struct("Memo").field("warm", &warm).finish()
    }
}

impl Memo {
    /// A fresh, cold memo.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the derived read model for these inputs, recomputing only when a cached
    /// answer would be wrong.
    ///
    /// A cached model is reused iff the ledger head, registry version, and config
    /// hash all match *and* `now` has not reached the cached model's horizon — the
    /// three invalidation causes, checked together. Otherwise the derivation runs and
    /// its result is cached under the new key. The returned model always carries the
    /// [`as_of`](ReadModel::as_of) it was derived at, so a caller can tell a cache hit
    /// from a miss by the answer's provenance, not by trusting the memo.
    ///
    /// The result is *identical* to calling [`derive()`] directly — the memo changes
    /// only how often the work runs, never the answer (proven in the property tests).
    /// `events` is the ledger in ascending `seq` order, as [`derive()`] expects.
    ///
    /// **Precondition: `max(seq)` must uniquely identify the event prefix.** The key
    /// summarizes the ledger by its head sequence alone (`events.iter().map(seq).max()`),
    /// which is sound only because a real ledger is append-only with a monotonic `seq`
    /// — so the same head means the same prefix, and a new event always raises the
    /// head. A caller that passed a slice whose `max(seq)` did not determine its
    /// contents (a mutated or reordered ledger, which the hub's `Ledger` trait makes
    /// unrepresentable) could get a stale cache hit; against the real append-only
    /// ledger this cannot happen.
    #[must_use]
    pub fn read(
        &self,
        registry: &RegistrySnapshot,
        events: &[(u64, Event)],
        now: Timestamp,
        config: &DeriverConfig,
    ) -> ReadModel {
        let key = MemoKey {
            ledger_head: events.iter().map(|(seq, _)| *seq).max(),
            registry_version: registry.version,
            config_hash: config.hash(),
        };

        // Fast path: a warm slot whose key matches and whose horizon the clock has
        // not reached. Cloning the cached model releases the read lock immediately,
        // so a recompute by another thread is never blocked on this reader.
        if let Ok(guard) = self.slot.read() {
            if let Some(cached) = guard.as_ref() {
                if cached.key == key && !horizon_reached(&cached.model, now) {
                    return cached.model.clone();
                }
            }
        }

        // Miss: recompute and install. The derivation is pure, so recomputing under
        // the same inputs is always safe; a race that recomputes twice wastes work
        // but cannot produce a wrong or divergent answer.
        let model = derive(registry, events, now, config);
        if let Ok(mut guard) = self.slot.write() {
            *guard = Some(Cached {
                key,
                model: model.clone(),
            });
        }
        model
    }

    /// Discard any cached model, forcing the next [`read`](Memo::read) to recompute.
    ///
    /// Because the memo is a cache and not a store, clearing it changes nothing a
    /// caller can observe except latency: the next read derives an identical answer.
    /// Exposed for tests and for an operator wanting to drop the cache.
    pub fn clear(&self) {
        if let Ok(mut guard) = self.slot.write() {
            *guard = None;
        }
    }
}

/// Whether `now` has reached the cached model's horizon — the instant at or past
/// which one of its answers changes by the clock alone.
///
/// A model with no horizon (nothing scheduled to change by the clock) is never
/// invalidated on this axis; only a discrete input change can retire it. The
/// comparison is inclusive at the horizon, matching the deriver's inclusive expiry
/// boundary, so the read exactly at an expiry recomputes and reports the new stale
/// standing rather than serving the last fresh one.
fn horizon_reached(model: &ReadModel, now: Timestamp) -> bool {
    model.horizon.is_some_and(|h| now >= h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deriver::{ClaimEntry, Standing};
    use claim_core::{parse_claim_file, Claim, Verdict};

    fn claim_of(yaml_body: &str) -> Claim {
        let text = format!("---\n{yaml_body}\n---\nStatement.\n");
        parse_claim_file(".claims/t.md", &text).expect("valid claim")
    }

    fn simple_claim(id: &str, hub: &str) -> Claim {
        claim_of(&format!(
            "id: {id}\n{hub}checks:\n  - kind: cmd\n    run: \"true\""
        ))
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn verdict_event(claim: &Claim, verdict: Verdict, at: &str) -> Event {
        let mut producer = serde_json::Map::new();
        producer.insert("run".into(), serde_json::json!("1"));
        Event {
            kind: crate::EventKind::Verdict,
            claim: claim.id.as_str().to_owned(),
            check: crate::CheckRef {
                index: 0,
                digest: crate::check_digest(&claim.checks[0]),
            },
            verdict,
            evidence: None,
            commit: "abc".into(),
            store: "s".into(),
            producer: crate::Producer(producer),
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

    #[test]
    fn a_warm_read_matches_a_cold_one() {
        let claim = simple_claim("t", "hub:\n  max-age: 30d\n");
        let ev = verdict_event(&claim, Verdict::Held, "2026-07-01T00:00:00Z");
        let reg = registry(1, vec![claim]);
        let events = [(1, ev)];
        let now = ts("2026-07-10T00:00:00Z");
        let config = DeriverConfig::default();

        let memo = Memo::new();
        let first = memo.read(&reg, &events, now, &config); // miss, installs
        let second = memo.read(&reg, &events, now, &config); // hit
        let direct = derive(&reg, &events, now, &config);
        assert_eq!(first, direct, "the first (computed) read matches derive()");
        assert_eq!(second, direct, "the cached read is identical");
    }

    #[test]
    fn a_new_event_invalidates_the_cache() {
        // A second event (new head seq) must recompute, or a fresh drift would be
        // served from a stale green cache.
        let claim = simple_claim("t", "");
        let hold = verdict_event(&claim, Verdict::Held, "2026-07-01T00:00:00Z");
        let drift = verdict_event(&claim, Verdict::Drifted, "2026-07-02T00:00:00Z");
        let reg = registry(1, vec![claim]);
        let now = ts("2026-07-03T00:00:00Z");
        let config = DeriverConfig::default();

        let memo = Memo::new();
        let first = memo.read(&reg, &[(1, hold.clone())], now, &config);
        assert_eq!(
            first.standing("s", "t").unwrap().standing,
            Standing::Verified
        );
        // A new event at seq 2 bumps the head; the drift must show.
        let second = memo.read(&reg, &[(1, hold), (2, drift)], now, &config);
        assert_eq!(
            second.standing("s", "t").unwrap().standing,
            Standing::Drifted
        );
    }

    #[test]
    fn a_registry_version_bump_invalidates_the_cache() {
        // The same ledger, but the registry version advanced (a claim was retired):
        // the cache must not serve the old live set.
        let claim = simple_claim("t", "");
        let ev = verdict_event(&claim, Verdict::Held, "2026-07-01T00:00:00Z");
        let events = [(1, ev)];
        let now = ts("2026-07-02T00:00:00Z");
        let config = DeriverConfig::default();

        let memo = Memo::new();
        let with_claim = registry(1, vec![claim]);
        let first = memo.read(&with_claim, &events, now, &config);
        assert_eq!(
            first.standing("s", "t").unwrap().standing,
            Standing::Verified
        );

        // Version 2 with an empty live set: the same head seq, but the registry
        // changed, so the claim is now retired.
        let empty = registry(2, vec![]);
        let second = memo.read(&empty, &events, now, &config);
        assert_eq!(
            second.standing("s", "t").unwrap().standing,
            Standing::Retired
        );
    }

    #[test]
    fn the_clock_crossing_the_horizon_invalidates_the_cache() {
        // No input changes at all — same ledger, same registry, same config — but the
        // clock advances past the freshness horizon. The cache must recompute and
        // report stale (invariant #6: a claim ages into stale with no new event).
        let claim = simple_claim("t", "hub:\n  max-age: 30d\n");
        let ev = verdict_event(&claim, Verdict::Held, "2026-07-01T00:00:00Z");
        let reg = registry(1, vec![claim]);
        let events = [(1, ev)];
        let config = DeriverConfig::default();

        let memo = Memo::new();
        let fresh = memo.read(&reg, &events, ts("2026-07-10T00:00:00Z"), &config);
        assert_eq!(
            fresh.standing("s", "t").unwrap().standing,
            Standing::Verified
        );
        assert_eq!(fresh.horizon, Some(ts("2026-07-31T00:00:00Z")));

        // Reading at the horizon (same inputs) must recompute to stale, not serve the
        // cached verified answer.
        let expired = memo.read(&reg, &events, ts("2026-07-31T00:00:00Z"), &config);
        assert_eq!(
            expired.standing("s", "t").unwrap().standing,
            Standing::Stale
        );
    }

    #[test]
    fn a_config_change_invalidates_the_cache() {
        // Same ledger and registry, but a tighter default max-age: the cache keyed on
        // the config hash must recompute.
        let claim = simple_claim("t", "");
        let ev = verdict_event(&claim, Verdict::Held, "2026-07-01T00:00:00Z");
        let reg = registry(1, vec![claim]);
        let events = [(1, ev)];
        let now = ts("2026-07-10T00:00:00Z");

        let memo = Memo::new();
        let loose = DeriverConfig {
            default_max_age: Some("30d".parse().unwrap()),
            max_age_override: None,
        };
        let first = memo.read(&reg, &events, now, &loose);
        assert_eq!(
            first.standing("s", "t").unwrap().standing,
            Standing::Verified
        );

        let tight = DeriverConfig {
            default_max_age: Some("5d".parse().unwrap()),
            max_age_override: None,
        };
        let second = memo.read(&reg, &events, now, &tight);
        assert_eq!(second.standing("s", "t").unwrap().standing, Standing::Stale);
    }

    #[test]
    fn a_cleared_memo_recomputes_an_identical_answer() {
        // The cache holds nothing the derivation does not: clear it and the next read
        // is byte-identical (invariant #3, the cache-never-truth property).
        let claim = simple_claim("t", "hub:\n  max-age: 30d\n");
        let ev = verdict_event(&claim, Verdict::Held, "2026-07-01T00:00:00Z");
        let reg = registry(1, vec![claim]);
        let events = [(1, ev)];
        let now = ts("2026-07-10T00:00:00Z");
        let config = DeriverConfig::default();

        let memo = Memo::new();
        let warm = memo.read(&reg, &events, now, &config);
        memo.clear();
        let after_clear = memo.read(&reg, &events, now, &config);
        assert_eq!(warm, after_clear);
    }
}
