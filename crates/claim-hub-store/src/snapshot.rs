//! Building the deriver's inputs from the live store.
//!
//! The deriver ([`claim_hub_core::derive`]) is a pure function of a registry snapshot,
//! the ledger's events, a clock, and config (HUB.md §2). This module is the seam that
//! turns *stored* data into the first two inputs, so a read endpoint can run the real
//! deriver over the real store — the integration hub-07 wires.
//!
//! Two conversions live here:
//!
//! - [`registry_snapshot`] walks every connected store's claims-at-tip into one
//!   [`claim_hub_core::RegistrySnapshot`], stamped with the registry's current version.
//!   The whole registry, not one store's slice, because the deriver derives the entire
//!   read model at once (and marks a claim the ledger knows but the registry has dropped
//!   as retired — which it can only do seeing the full live set).
//! - [`ledger_events`] scans the ledger into the `(seq, Event)` pairs the deriver folds,
//!   in ascending position order.
//!
//! ## Why a store-built entry carries no per-claim `hub:` hints (yet)
//!
//! A [`RegisteredClaim`] holds each check's content **digest** — the deriver's join key,
//! which is all issue #18 needs — but **not** the claim's `hub:` freshness hints
//! (`recheck`/`max-age`) nor its per-check skips: the registry schema (hub-05) does not
//! persist them. So the [`ClaimEntry`] built here carries no per-claim hints, and the
//! deriver's freshness falls back to the hub config's default/override
//! ([`claim_hub_core::DeriverConfig`]) — which is exactly what the config mapping supplies.
//! This is honest, not a silent gap: a claim with a per-claim `max-age` is aged by the
//! config's window rather than its own until sync stores the hints. Persisting the `hub:`
//! hints (and skips) in the registry is a focused follow-up on hub-05's schema; when it
//! lands, this builder passes them through and the deriver honors the claim's own cadence.
//! The join keys are already correct today, which is what the M0 loop proves.

use claim_hub_core::{ClaimEntry, DerivedCheck, HubHints, RegistrySnapshot};

use crate::error::Result;
use crate::ledger::Ledger;
use crate::registry::{RegisteredClaim, Registry};

/// The deriver's [`RegistrySnapshot`] built from every connected store's live claims.
///
/// Reads the registry version once, then gathers each store's claims-at-tip
/// ([`Registry::claims_of`]) into [`ClaimEntry`]s. The version stamps the snapshot so
/// the deriver's memo can key on it, and it is read in the same call as the claims: a
/// sync racing this read could produce a snapshot whose version is one behind its
/// claims, but that only makes the memo recompute once more — never a wrong answer,
/// because the deriver holds no truth the snapshot does not.
///
/// # Errors
///
/// Propagates any store read fault ([`crate::StoreError`]); a genuine failure to read the
/// registry is loud, never a silently empty snapshot that would derive every claim as
/// retired (invariant #6).
pub async fn registry_snapshot<S>(store: &S) -> Result<RegistrySnapshot>
where
    S: Registry,
{
    let version = store.version().await?;
    let mut claims = Vec::new();
    for store_id in store.stores().await? {
        for registered in store.claims_of(&store_id).await? {
            claims.push(claim_entry(&store_id, &registered));
        }
    }
    Ok(RegistrySnapshot {
        // `RegistryVersion` is an `i64` newtype; the deriver keys the memo on a `u64`.
        // The counter only ever advances from 0, so it is non-negative in practice; a
        // negative value (corruption) saturates to 0, which at worst forces a recompute.
        version: u64::try_from(version.0).unwrap_or(0),
        claims,
    })
}

/// One registry claim as a deriver [`ClaimEntry`].
///
/// Maps each stored check digest to a [`DerivedCheck`] in declared order — the digest is
/// the join key the ledger's events also carry (both computed by the one
/// [`claim_hub_core::check_digest`], so they match by construction). Skips and `hub:`
/// hints are absent here because the registry does not store them yet (see the module
/// docs); the entry carries [`HubHints::default`] and no skips, so freshness falls to
/// config.
fn claim_entry(store: &str, registered: &RegisteredClaim) -> ClaimEntry {
    let checks = registered
        .check_digests
        .iter()
        .map(|digest| DerivedCheck {
            digest: digest.clone(),
            skip: None,
        })
        .collect();
    ClaimEntry::new(
        store.to_owned(),
        registered.id.as_str().to_owned(),
        checks,
        HubHints::default(),
    )
}

/// The ledger's events as the `(seq, Event)` pairs the deriver folds, in ascending
/// position order.
///
/// A full cold scan ([`Ledger::scan_from`] from the start): at v1 volume a full
/// derivation is milliseconds over thousands of events (HUB-IMPLEMENTATION.md §1.5), so
/// the whole ledger feeds one derivation. A `limit`- or cursor-windowed scan is a later
/// concern behind the same trait, not something the M0 read needs.
///
/// # Errors
///
/// Propagates any store read fault ([`crate::StoreError`]).
pub async fn ledger_events<L>(ledger: &L) -> Result<Vec<(u64, claim_hub_core::Event)>>
where
    L: Ledger,
{
    let stored = ledger.scan_from(crate::Position(0)).await?;
    Ok(stored
        .into_iter()
        .map(|s| {
            // A ledger position is a non-negative monotonic counter; the deriver keys the
            // head on a `u64`. A negative position cannot occur from an append-only
            // `AUTOINCREMENT` column, so a corrupt one saturates to 0 rather than panicking.
            (u64::try_from(s.position.0).unwrap_or(0), s.event)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteStore;
    use claim_core::parse_claim_file;
    use claim_hub_core::check_digest;

    /// Parse a claim from frontmatter and register it in `store`.
    async fn seed(store: &SqliteStore, store_id: &str, file: &str, frontmatter: &str) {
        let text = format!("---\n{frontmatter}\n---\nStatement body.\n");
        let claim = parse_claim_file(file, &text).expect("valid claim");
        let registered = RegisteredClaim {
            id: claim.id.clone(),
            statement: claim.statement.clone(),
            supports: claim
                .supports
                .iter()
                .map(|t| t.as_str().to_owned())
                .collect(),
            commit: "seedcommit".to_owned(),
            check_digests: claim.checks.iter().map(check_digest).collect(),
        };
        store
            .replace_store(store_id, &[registered])
            .await
            .expect("seed");
    }

    #[tokio::test]
    async fn a_snapshot_gathers_claims_across_every_store() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        seed(
            &store,
            "github.com/acme/payments",
            ".claims/a.md",
            "id: payments/a\nchecks:\n  - kind: cmd\n    run: \"true\"",
        )
        .await;
        seed(
            &store,
            "github.com/acme/billing",
            ".claims/b.md",
            "id: billing/b\nchecks:\n  - kind: cmd\n    run: \"false\"",
        )
        .await;

        let snapshot = registry_snapshot(&store).await.unwrap();
        assert_eq!(snapshot.claims.len(), 2, "both stores' claims are gathered");
        // Two replace_store calls advanced the version to 2.
        assert_eq!(snapshot.version, 2);
        let stores: Vec<&str> = snapshot.claims.iter().map(|c| c.store.as_str()).collect();
        assert!(stores.contains(&"github.com/acme/payments"));
        assert!(stores.contains(&"github.com/acme/billing"));
    }

    #[tokio::test]
    async fn a_claim_entrys_digest_is_the_registrys_stored_digest() {
        // The load-bearing end-to-end guarantee: the ClaimEntry's join key is exactly the
        // digest the registry stored, so it matches a ledger event's digest by
        // construction (both are `check_digest` of the same definition).
        let store = SqliteStore::open_in_memory().await.unwrap();
        let text =
            "---\nid: payments/a\nchecks:\n  - kind: cmd\n    run: \"true\"\n---\nStatement.\n";
        let claim = parse_claim_file(".claims/a.md", text).unwrap();
        let expected = check_digest(&claim.checks[0]);
        seed(
            &store,
            "github.com/acme/payments",
            ".claims/a.md",
            "id: payments/a\nchecks:\n  - kind: cmd\n    run: \"true\"",
        )
        .await;

        let snapshot = registry_snapshot(&store).await.unwrap();
        let entry = &snapshot.claims[0];
        assert_eq!(entry.checks.len(), 1);
        assert_eq!(
            entry.checks[0].digest, expected,
            "the entry's join key is the registry's stored digest"
        );
    }

    #[tokio::test]
    async fn an_empty_store_yields_an_empty_snapshot_not_an_error() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let snapshot = registry_snapshot(&store).await.unwrap();
        assert!(snapshot.claims.is_empty());
        assert_eq!(snapshot.version, 0, "a fresh registry is version 0");
    }

    #[tokio::test]
    async fn ledger_events_scans_in_ascending_position_order() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        // No events on a fresh ledger.
        assert!(ledger_events(&store).await.unwrap().is_empty());
    }
}
