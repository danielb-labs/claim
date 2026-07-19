//! Integration tests for the [`Registry`] over the SQLite store.
//!
//! The registry is derived data (HUB.md §2): a store's snapshot is *replaced*, not
//! merged, so a claim absent at the new tip is retired; wipe-plus-resnapshot rebuilds
//! it identically; and the version counter advances per sync. Each test migrates a
//! fresh temp file and drives the trait — no shared state, no network.

use claim_core::ClaimId;
use claim_hub_core::HubHints;
use claim_hub_store::{RegisteredClaim, Registry, RegistryVersion, SqliteStore, SupportsEdge};
use std::str::FromStr;
use tempfile::TempDir;

async fn fresh_store() -> (SqliteStore, TempDir) {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("hub.db");
    let store = SqliteStore::open(&path).await.expect("open + migrate");
    (store, dir)
}

fn claim(id: &str, statement: &str, supports: &[&str], commit: &str) -> RegisteredClaim {
    RegisteredClaim {
        id: ClaimId::from_str(id).expect("valid id"),
        statement: statement.into(),
        supports: supports.iter().map(|s| (*s).to_owned()).collect(),
        commit: commit.into(),
        // Most registry tests exercise the claim/statement/supports/version paths; a
        // claim with no checks is the simplest fixture. The digest read/write path has
        // its own dedicated test (`check_digests_round_trip`), and the hub-hint
        // round-trip has `hub_hints_round_trip`.
        check_digests: Vec::new(),
        hub: Default::default(),
    }
}

const STORE: &str = "github.com/acme/payments";

#[tokio::test]
async fn replace_store_stores_claims_and_supports_with_the_read_commit() {
    let (store, _dir) = fresh_store().await;
    assert_eq!(store.version().await.unwrap(), RegistryVersion(0));

    let snapshot = vec![
        claim(
            "payments/libfoo-pin",
            "libfoo is pinned to 4.2",
            &["requirements.txt#libfoo", "decisions/pin-libfoo"],
            "8f2c0a1",
        ),
        claim("payments/tls-required", "TLS is required", &[], "8f2c0a1"),
    ];
    let v = store.replace_store(STORE, &snapshot).await.unwrap();
    assert_eq!(
        v,
        RegistryVersion(1),
        "the first sync advances to version 1"
    );
    assert_eq!(store.version().await.unwrap(), RegistryVersion(1));

    // claims_of returns both, in ascending id order, with their read commit and edges.
    let claims = store.claims_of(STORE).await.unwrap();
    assert_eq!(claims.len(), 2);
    assert_eq!(claims[0].id.as_str(), "payments/libfoo-pin");
    assert_eq!(claims[0].commit, "8f2c0a1");
    assert_eq!(
        claims[0].supports,
        vec![
            "decisions/pin-libfoo".to_owned(),
            "requirements.txt#libfoo".to_owned()
        ],
        "supports come back in ascending order"
    );
    assert_eq!(claims[1].id.as_str(), "payments/tls-required");
    assert!(claims[1].supports.is_empty());

    // The single-claim query agrees, and a missing claim is None.
    let one = store
        .claim(STORE, &ClaimId::from_str("payments/libfoo-pin").unwrap())
        .await
        .unwrap();
    assert_eq!(one, Some(claims[0].clone()));
    let missing = store
        .claim(STORE, &ClaimId::from_str("payments/ghost").unwrap())
        .await
        .unwrap();
    assert_eq!(missing, None);

    // The reverse supports index finds the supporting claim by target.
    let supporters = store
        .claims_supporting("decisions/pin-libfoo")
        .await
        .unwrap();
    assert_eq!(
        supporters,
        vec![SupportsEdge {
            store: STORE.to_owned(),
            claim_id: ClaimId::from_str("payments/libfoo-pin").unwrap(),
            target: "decisions/pin-libfoo".to_owned(),
        }]
    );
}

#[tokio::test]
async fn replace_is_a_wipe_not_a_merge_so_an_absent_claim_is_retired() {
    let (store, _dir) = fresh_store().await;
    store
        .replace_store(
            STORE,
            &[
                claim("payments/a", "A", &["decisions/x"], "c1"),
                claim("payments/b", "B", &[], "c1"),
            ],
        )
        .await
        .unwrap();

    // Resnapshot without `payments/a`: it is retired, and its supports edge with it.
    let v = store
        .replace_store(STORE, &[claim("payments/b", "B", &[], "c2")])
        .await
        .unwrap();
    assert_eq!(
        v,
        RegistryVersion(2),
        "the second sync advances the counter"
    );

    let claims = store.claims_of(STORE).await.unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].id.as_str(), "payments/b");
    assert_eq!(claims[0].commit, "c2", "the survivor's commit updated");

    // The retired claim is gone from both directions of the index.
    assert!(store
        .claim(STORE, &ClaimId::from_str("payments/a").unwrap())
        .await
        .unwrap()
        .is_none());
    assert!(store
        .claims_supporting("decisions/x")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn an_empty_snapshot_retires_every_claim_but_keeps_the_store() {
    let (store, _dir) = fresh_store().await;
    store
        .replace_store(STORE, &[claim("payments/a", "A", &[], "c1")])
        .await
        .unwrap();
    let v = store.replace_store(STORE, &[]).await.unwrap();
    assert_eq!(v, RegistryVersion(2));
    assert!(store.claims_of(STORE).await.unwrap().is_empty());
}

#[tokio::test]
async fn wipe_plus_resnapshot_rebuilds_identically() {
    let (store, _dir) = fresh_store().await;
    let snapshot = vec![
        claim(
            "payments/a",
            "A",
            &["decisions/x", "requirements.txt#a"],
            "c1",
        ),
        claim("payments/b", "B", &["decisions/y"], "c1"),
    ];
    store.replace_store(STORE, &snapshot).await.unwrap();
    let before = store.claims_of(STORE).await.unwrap();

    // Wipe (empty snapshot) then resnapshot the same claims: the read model is
    // identical, which is what makes the registry safely rebuildable derived data.
    store.replace_store(STORE, &[]).await.unwrap();
    assert!(store.claims_of(STORE).await.unwrap().is_empty());
    store.replace_store(STORE, &snapshot).await.unwrap();
    let after = store.claims_of(STORE).await.unwrap();

    assert_eq!(before, after, "resnapshot reproduces the registry exactly");
}

#[tokio::test]
async fn the_version_counter_advances_once_per_sync_even_with_identical_content() {
    let (store, _dir) = fresh_store().await;
    let snapshot = [claim("payments/a", "A", &[], "c1")];
    let v1 = store.replace_store(STORE, &snapshot).await.unwrap();
    let v2 = store.replace_store(STORE, &snapshot).await.unwrap();
    let v3 = store.replace_store(STORE, &snapshot).await.unwrap();
    assert_eq!(
        (v1, v2, v3),
        (RegistryVersion(1), RegistryVersion(2), RegistryVersion(3)),
        "each sync advances the counter, so a reader can tell a sync happened"
    );
    // Contents stayed idempotent even though the counter did not.
    assert_eq!(store.claims_of(STORE).await.unwrap().len(), 1);
}

#[tokio::test]
async fn supports_index_spans_stores() {
    let (store, _dir) = fresh_store().await;
    let other = "github.com/acme/web";
    store
        .replace_store(
            STORE,
            &[claim("payments/a", "A", &["decisions/shared"], "c1")],
        )
        .await
        .unwrap();
    store
        .replace_store(other, &[claim("web/b", "B", &["decisions/shared"], "c1")])
        .await
        .unwrap();

    // The reverse index returns supporters across both stores, ordered by (store, id):
    // "…/payments" sorts before "…/web" ('p' < 'w').
    let supporters = store.claims_supporting("decisions/shared").await.unwrap();
    assert_eq!(supporters.len(), 2);
    assert_eq!(supporters[0].store, STORE, "payments sorts before web");
    assert_eq!(supporters[1].store, other);
}

#[tokio::test]
async fn claims_of_an_unknown_store_is_empty() {
    let (store, _dir) = fresh_store().await;
    assert!(store
        .claims_of("github.com/acme/nonexistent")
        .await
        .unwrap()
        .is_empty());
}

/// A registered claim carrying two check digests, for the digest read/write tests.
fn claim_with_digests(id: &str, digests: &[&str]) -> RegisteredClaim {
    RegisteredClaim {
        id: ClaimId::from_str(id).expect("valid id"),
        statement: "S".into(),
        supports: vec![],
        commit: "c1".into(),
        check_digests: digests.iter().map(|d| (*d).to_owned()).collect(),
        hub: Default::default(),
    }
}

#[tokio::test]
async fn check_digests_round_trip_by_index_and_read_back_in_order() {
    // The ingest gate's bridge: a claim's per-check digests are stored by position and
    // read back by (store, claim, index) and as an ordered vector.
    let (store, _dir) = fresh_store().await;
    let digest0 = "a".repeat(64);
    let digest1 = "b".repeat(64);
    store
        .replace_store(STORE, &[claim_with_digests("pin", &[&digest0, &digest1])])
        .await
        .unwrap();

    let id = ClaimId::from_str("pin").unwrap();
    assert_eq!(
        store.check_digest(STORE, &id, 0).await.unwrap(),
        Some(digest0.clone())
    );
    assert_eq!(
        store.check_digest(STORE, &id, 1).await.unwrap(),
        Some(digest1.clone())
    );
    // The whole claim reads its digests back in declared order.
    let claim = store.claim(STORE, &id).await.unwrap().unwrap();
    assert_eq!(claim.check_digests, vec![digest0, digest1]);
}

#[tokio::test]
async fn check_digest_is_none_for_an_unknown_claim_or_out_of_range_index() {
    // Both "the registry never synced this claim" and "the index is past the claim's
    // checks" return `None` — the ingest gate's reject-loudly signal, distinct from an
    // error.
    let (store, _dir) = fresh_store().await;
    store
        .replace_store(STORE, &[claim_with_digests("pin", &[&"c".repeat(64)])])
        .await
        .unwrap();
    let id = ClaimId::from_str("pin").unwrap();
    let unknown = ClaimId::from_str("never-synced").unwrap();

    assert_eq!(store.check_digest(STORE, &unknown, 0).await.unwrap(), None);
    assert_eq!(
        store.check_digest(STORE, &id, 1).await.unwrap(),
        None,
        "index 1 is past the single check"
    );
    assert_eq!(
        store
            .check_digest("github.com/acme/other", &id, 0)
            .await
            .unwrap(),
        None,
        "a different store does not see this claim's digest"
    );
}

#[tokio::test]
async fn a_snapshot_replace_retires_a_removed_claims_check_digests() {
    // Digests cascade with their claim: a claim absent at the new tip drops its digests,
    // so a retired check's identity never lingers to mis-key a stale verdict.
    let (store, _dir) = fresh_store().await;
    let id = ClaimId::from_str("pin").unwrap();
    store
        .replace_store(STORE, &[claim_with_digests("pin", &[&"a".repeat(64)])])
        .await
        .unwrap();
    assert!(store.check_digest(STORE, &id, 0).await.unwrap().is_some());

    // Re-snapshot the store with the claim gone (a retirement).
    store.replace_store(STORE, &[]).await.unwrap();
    assert_eq!(
        store.check_digest(STORE, &id, 0).await.unwrap(),
        None,
        "the retired claim's digest is gone"
    );
}

#[tokio::test]
async fn hub_hints_round_trip_through_the_registry() {
    // A claim's own `hub:` hints are persisted and read back verbatim, so the deriver ages
    // it on its own declared cadence — not just a config default. Without this a claim
    // declaring `max-age: 30d` would read verified forever under no config window
    // (invariant #6).
    let (store, _dir) = fresh_store().await;
    let hinted = RegisteredClaim {
        id: ClaimId::from_str("pin").unwrap(),
        statement: "S".into(),
        supports: vec![],
        commit: "c1".into(),
        check_digests: vec![],
        hub: HubHints {
            max_age: Some("30d".parse().unwrap()),
            recheck: Some("7d".parse().unwrap()),
        },
    };
    store.replace_store(STORE, &[hinted]).await.unwrap();

    let id = ClaimId::from_str("pin").unwrap();
    let read = store.claim(STORE, &id).await.unwrap().unwrap();
    assert_eq!(read.hub.max_age, Some("30d".parse().unwrap()));
    assert_eq!(read.hub.recheck, Some("7d".parse().unwrap()));

    // `claims_of` reads the same hints (the deriver-snapshot path uses it).
    let all = store.claims_of(STORE).await.unwrap();
    assert_eq!(all[0].hub.max_age, Some("30d".parse().unwrap()));
    assert_eq!(all[0].hub.recheck, Some("7d".parse().unwrap()));
}

#[tokio::test]
async fn a_claim_with_no_hub_hints_reads_none_not_a_fabricated_window() {
    // A claim declaring no `hub:` reads back with None hints, so freshness falls to config
    // rather than a window nobody set.
    let (store, _dir) = fresh_store().await;
    store
        .replace_store(STORE, &[claim("pin", "S", &[], "c1")])
        .await
        .unwrap();
    let read = store
        .claim(STORE, &ClaimId::from_str("pin").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.hub, HubHints::default());
}

#[tokio::test]
async fn stores_lists_connected_stores_in_ascending_order() {
    // The deriver-snapshot builder walks every connected store; `stores()` is that list,
    // ascending for a deterministic snapshot build.
    let (store, _dir) = fresh_store().await;
    // Insert out of order to prove the ordering is the query's, not the insertion's.
    store
        .replace_store("github.com/acme/payments", &[claim("a", "S", &[], "c1")])
        .await
        .unwrap();
    store
        .replace_store("github.com/acme/billing", &[claim("b", "S", &[], "c1")])
        .await
        .unwrap();

    assert_eq!(
        store.stores().await.unwrap(),
        vec![
            "github.com/acme/billing".to_owned(),
            "github.com/acme/payments".to_owned(),
        ],
        "connected stores list ascending"
    );
}

#[tokio::test]
async fn an_emptied_store_stays_connected_so_its_ledger_only_claims_can_retire() {
    // A store synced to empty — every claim deleted from git — must still appear in
    // `stores()`. The deriver needs the store present to derive its ledger-only claims as
    // `Retired` rather than have them vanish; a connected-but-empty store is load-bearing.
    let (store, _dir) = fresh_store().await;
    store
        .replace_store(STORE, &[claim("pin", "S", &[], "c1")])
        .await
        .unwrap();
    // Re-sync the store to empty (all claims removed upstream).
    store.replace_store(STORE, &[]).await.unwrap();

    assert_eq!(
        store.stores().await.unwrap(),
        vec![STORE.to_owned()],
        "an emptied store stays connected"
    );
    assert!(
        store.claims_of(STORE).await.unwrap().is_empty(),
        "but it has no live claims"
    );
}
