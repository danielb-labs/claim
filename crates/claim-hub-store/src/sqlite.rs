//! The SQLite implementation of [`Ledger`] and [`Registry`], via `sqlx`.
//!
//! One WAL-mode SQLite file holds both the append-only ledger and the
//! rebuildable registry (HUB-IMPLEMENTATION.md §1.4): export is `cp`, delete is
//! `rm`, and there is no database server the product operates on the customer's
//! behalf. The schema, the dedup index, and the append-only triggers live in the
//! embedded migration ([`MIGRATOR`]); this module is the typed Rust over it.
//!
//! Queries use sqlx's compile-time-checked `query!`/`query_as!` macros against the
//! schema, with the metadata cache committed under `.sqlx/` so the workspace builds
//! offline with no `DATABASE_URL` (the gate and CI have no database). A typo'd
//! column or a type mismatch is a build failure, not a wrong answer at read time.

use crate::error::{Result, StoreError};
use crate::ledger::{Appended, Ledger, Position, StoredEvent};
use crate::registry::{RegisteredClaim, Registry, RegistryVersion, SupportsEdge};
use claim_core::{ClaimId, Timestamp, Verdict};
use claim_hub_core::{CheckRef, Event, EventKind, Producer};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use std::path::Path;
use std::str::FromStr;

/// The embedded migrations that create and upgrade the schema from an empty file.
///
/// `sqlx::migrate!` compiles the `migrations/` SQL into the binary, so the hub
/// creates its own database on first boot with no external tooling
/// (HUB-IMPLEMENTATION.md §1.4). Exposed so a caller can also run migrations against
/// a pool it built itself, though [`SqliteStore::open`] runs them for the common case.
///
/// # Offline query cache (`.sqlx/`)
///
/// The `query!`/`query_as!` macros in this module are checked at compile time against
/// the schema. The committed `.sqlx/` directory is what lets `cargo build`/`clippy`/
/// `test` succeed with **no** `DATABASE_URL` — the gate and CI have no database.
/// After changing any query, the schema, or a migration, regenerate it:
///
/// ```text
/// # from crates/claim-hub-store, against a throwaway migrated db:
/// DATABASE_URL="sqlite:///tmp/prepare.db?mode=rwc" sqlx migrate run --source ./migrations
/// DATABASE_URL="sqlite:///tmp/prepare.db?mode=rwc" cargo sqlx prepare
/// ```
///
/// and commit the updated `.sqlx/`. Skipping this makes the offline build fail with a
/// confusing "no cached data for this query" error rather than a schema mismatch.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// The SQLite-backed store, implementing both [`Ledger`] and [`Registry`] over one
/// connection pool to one database file.
///
/// Cloning is cheap — the inner [`SqlitePool`] is reference-counted — so the hub
/// shares one `SqliteStore` across its axum handlers. WAL mode (set at open) lets
/// concurrent readers proceed against the single writer, which is the concurrency
/// shape a hub needs (HUB-IMPLEMENTATION.md §1.4).
#[derive(Clone, Debug)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open (creating if absent) the database at `path`, enable WAL, foreign keys,
    /// and recursive triggers, and run the embedded migrations, so the returned
    /// store is ready to use.
    ///
    /// Creating the file if it does not exist and migrating it is the self-host
    /// first-boot story: point the hub at an empty directory and it stands up its
    /// own schema. Foreign keys are enabled so the registry's
    /// `ON DELETE CASCADE` retirements actually cascade (SQLite defaults them off).
    /// Running migrations here is idempotent: a second open of an already-migrated
    /// file applies nothing.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        Self::from_options(hardened(options)).await
    }

    /// Open an in-memory database, for tests that want no file at all.
    ///
    /// A single shared in-memory database backed by one connection, so every query
    /// sees the same schema and data within the test. Not for production — an
    /// in-memory database is not customer-owned durable storage — but it keeps the
    /// trait tests free of even a temp file where they do not need one.
    pub async fn open_in_memory() -> Result<Self> {
        let options =
            hardened(SqliteConnectOptions::from_str("sqlite::memory:").map_err(StoreError::Sql)?);
        // One connection: distinct connections to `:memory:` are distinct databases,
        // so the pool must not hand out a second, empty one.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let store = Self { pool };
        MIGRATOR.run(&store.pool).await?;
        Ok(store)
    }

    async fn from_options(options: SqliteConnectOptions) -> Result<Self> {
        let pool = SqlitePool::connect_with(options).await?;
        let store = Self { pool };
        MIGRATOR.run(&store.pool).await?;
        Ok(store)
    }
}

/// Apply the connection-level hardening every `SqliteStore` connection needs.
///
/// Two pragmas are load-bearing for the honesty invariants and cannot be pinned in a
/// migration (they are per-connection):
///
/// - `foreign_keys = ON` so the registry's `ON DELETE CASCADE` retirements actually
///   cascade (SQLite defaults foreign-key enforcement off).
/// - `recursive_triggers = ON` so the append-only `BEFORE DELETE` trigger *also*
///   fires for the implicit delete that `INSERT OR REPLACE` / `REPLACE INTO` perform
///   on a conflict. Without it those statements silently delete-and-reinsert a
///   conflicting row — rewriting history and changing its `seq` with no error — the
///   exact silent rewrite invariants #4 and #6 forbid. It is set on the pool's
///   connect options so *every* connection the pool hands out enforces it, not just
///   the first.
fn hardened(options: SqliteConnectOptions) -> SqliteConnectOptions {
    options
        .foreign_keys(true)
        .pragma("recursive_triggers", "ON")
}

/// The wire-string form of a [`Verdict`], matching its kebab-case JSON, for the
/// `verdict` TEXT column.
///
/// Derived from serde so the column form cannot drift from the shared type's
/// spelling — the string stored is exactly the one the CLI and the envelope use.
fn verdict_to_text(v: Verdict) -> Result<String> {
    json_string_of(&v, "verdict")
}

/// Parse a `verdict` column back into the shared [`Verdict`]. A value no writer of
/// this crate produced (corruption, a foreign writer) is loud, not coerced.
fn verdict_from_text(s: &str) -> Result<Verdict> {
    json_string_into(s, "verdict")
}

/// The wire-string form of an [`EventKind`], matching its kebab-case JSON.
fn kind_to_text(k: EventKind) -> Result<String> {
    json_string_of(&k, "kind")
}

/// Parse a `kind` column back into an [`EventKind`].
fn kind_from_text(s: &str) -> Result<EventKind> {
    json_string_into(s, "kind")
}

/// Serialize a value that serde encodes as a JSON *string* into that bare string
/// (without the surrounding quotes), for a TEXT column.
fn json_string_of<T: serde::Serialize>(value: &T, context: &str) -> Result<String> {
    let json = serde_json::to_value(value).map_err(|source| StoreError::Json {
        context: context.to_owned(),
        source,
    })?;
    match json {
        serde_json::Value::String(s) => Ok(s),
        other => Err(StoreError::Corrupt {
            context: format!("{context}: expected a JSON string encoding"),
            value: other.to_string(),
        }),
    }
}

/// Parse a bare TEXT value back into a type serde decodes from a JSON string.
fn json_string_into<T: serde::de::DeserializeOwned>(s: &str, context: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(s.to_owned())).map_err(|_| {
        StoreError::Corrupt {
            context: format!("{context}: not a recognized value"),
            value: s.to_owned(),
        }
    })
}

/// The producer's run identifier, extracted from the producer block for the dedup
/// key, or `None` when the block carries no usable run.
///
/// The run is one value inside the producer JSON object (HUB.md §4's `producer.run`);
/// pulling it into its own indexed column is what lets the UNIQUE dedup index key on
/// it without SQLite JSON-path indexing. Returns `None` for an absent run and for an
/// empty-string run: both are unattributable, and [`SqliteStore::append`] rejects a
/// verdict event with no run rather than bucketing it into an empty-string collision
/// class where every run-less observation for one (store, claim, digest) would
/// collapse regardless of verdict (invariant #6 — make it loud).
///
/// Forward note for hub-11: internal event kinds like `nag` are appended by the hub's
/// own principal, which must supply a non-empty run of its own (e.g. the router tick's
/// identity) so those events dedup and attribute the same way; this function reads the
/// same `run` value, so the hub's principal block carries one.
fn producer_run(producer: &Producer) -> Option<String> {
    let run = match producer.0.get("run") {
        Some(serde_json::Value::String(s)) => s.clone(),
        // A non-string run (a JSON number, say) still keys the index by its canonical
        // text, so redelivery is caught regardless of the producer's JSON typing.
        Some(other) => other.to_string(),
        None => return None,
    };
    if run.is_empty() {
        return None;
    }
    Some(run)
}

impl Ledger for SqliteStore {
    async fn append(&self, event: &Event) -> Result<Appended> {
        let kind = kind_to_text(event.kind)?;
        let verdict = verdict_to_text(event.verdict)?;
        let producer_json =
            serde_json::to_string(&event.producer).map_err(|source| StoreError::Json {
                context: "producer".to_owned(),
                source,
            })?;
        // A verdict with no usable producer run is unattributable and cannot dedup
        // safely; reject it loudly rather than bucket it into an empty-run collision
        // class (invariant #6). The run is also the run component of the dedup key.
        let dedup_run = producer_run(&event.producer).ok_or(StoreError::MissingProducerRun)?;
        let reported_at = event.reported_at.to_string();
        let check_index = i64::try_from(event.check.index).map_err(|_| StoreError::Corrupt {
            context: "check.index too large for the ledger".to_owned(),
            value: event.check.index.to_string(),
        })?;

        // A dedup collision is not an error: it is the idempotent-redelivery success
        // of HUB.md §2. `ON CONFLICT DO NOTHING` leaves the original row untouched;
        // the follow-up read reports whether this call inserted or was absorbed, and
        // returns the position either way. The conflict target lists the dedup key's
        // columns (store, run, claim, digest) in the order the UNIQUE index declares.
        let inserted = sqlx::query!(
            r#"
            INSERT INTO events
                (kind, claim_id, check_index, check_digest, verdict,
                 evidence, "commit", store, producer, reported_at, dedup_run)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (store, dedup_run, claim_id, check_digest) DO NOTHING
            RETURNING seq AS "seq!: i64"
            "#,
            kind,
            event.claim,
            check_index,
            event.check.digest,
            verdict,
            event.evidence,
            event.commit,
            event.store,
            producer_json,
            reported_at,
            dedup_run,
        )
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = inserted {
            return Ok(Appended::New(Position(row.seq)));
        }

        // Absorbed by the dedup index: return the original's position. The original
        // is uniquely identified by the same key the index enforces.
        let existing = sqlx::query!(
            r#"
            SELECT seq AS "seq!: i64" FROM events
            WHERE store = ? AND dedup_run = ? AND claim_id = ? AND check_digest = ?
            "#,
            event.store,
            dedup_run,
            event.claim,
            event.check.digest,
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(Appended::Duplicate(Position(existing.seq)))
    }

    async fn scan_from(&self, cursor: Position) -> Result<Vec<StoredEvent>> {
        let after = cursor.0;
        let rows = sqlx::query!(
            r#"
            SELECT seq AS "seq!: i64", kind, claim_id, check_index, check_digest, verdict,
                   evidence, "commit" AS commit_sha, store, producer, reported_at
            FROM events
            WHERE seq > ?
            ORDER BY seq ASC
            "#,
            after,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let producer: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&row.producer).map_err(|source| StoreError::Json {
                    context: "producer".to_owned(),
                    source,
                })?;
            let index = usize::try_from(row.check_index).map_err(|_| StoreError::Corrupt {
                context: "check_index out of range".to_owned(),
                value: row.check_index.to_string(),
            })?;
            let reported_at =
                Timestamp::from_str(&row.reported_at).map_err(|_| StoreError::Corrupt {
                    context: "reported_at is not an RFC 3339 instant".to_owned(),
                    value: row.reported_at.clone(),
                })?;
            events.push(StoredEvent {
                position: Position(row.seq),
                event: Event {
                    kind: kind_from_text(&row.kind)?,
                    claim: row.claim_id,
                    check: CheckRef {
                        index,
                        digest: row.check_digest,
                    },
                    verdict: verdict_from_text(&row.verdict)?,
                    evidence: row.evidence,
                    commit: row.commit_sha,
                    store: row.store,
                    producer: Producer(producer),
                    reported_at,
                },
            });
        }
        Ok(events)
    }

    async fn head(&self) -> Result<Position> {
        // COALESCE to 0 so an empty ledger's head is Position(0) — before the first
        // event — matching scan_from(Position(0)) returning everything.
        let row = sqlx::query!(r#"SELECT COALESCE(MAX(seq), 0) AS "head!: i64" FROM events"#)
            .fetch_one(&self.pool)
            .await?;
        Ok(Position(row.head))
    }
}

impl Registry for SqliteStore {
    async fn replace_store(
        &self,
        store: &str,
        claims: &[RegisteredClaim],
    ) -> Result<RegistryVersion> {
        let mut tx = self.pool.begin().await?;

        // Ensure the store row exists (idempotent), then wipe its claims. The
        // supports edges cascade from claims_at_tip's ON DELETE CASCADE, so deleting
        // the claims clears the edges too.
        sqlx::query!("INSERT OR IGNORE INTO stores (store) VALUES (?)", store)
            .execute(&mut *tx)
            .await?;
        sqlx::query!("DELETE FROM claims_at_tip WHERE store = ?", store)
            .execute(&mut *tx)
            .await?;

        for claim in claims {
            let id = claim.id.as_str();
            sqlx::query!(
                r#"
                INSERT INTO claims_at_tip (store, claim_id, statement, "commit")
                VALUES (?, ?, ?, ?)
                "#,
                store,
                id,
                claim.statement,
                claim.commit,
            )
            .execute(&mut *tx)
            .await?;

            for target in &claim.supports {
                sqlx::query!(
                    r#"
                    INSERT OR IGNORE INTO supports_edges (store, claim_id, target)
                    VALUES (?, ?, ?)
                    "#,
                    store,
                    id,
                    target,
                )
                .execute(&mut *tx)
                .await?;
            }
        }

        // Advance the version counter as part of the same transaction, so a sync and
        // its version bump are atomic: a reader never sees new claims at an old
        // version or vice versa.
        let row = sqlx::query!(
            "UPDATE registry_version SET version = version + 1 WHERE id = 0 RETURNING version"
        )
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(RegistryVersion(row.version))
    }

    async fn version(&self) -> Result<RegistryVersion> {
        let row = sqlx::query!("SELECT version FROM registry_version WHERE id = 0")
            .fetch_one(&self.pool)
            .await?;
        Ok(RegistryVersion(row.version))
    }

    async fn claims_of(&self, store: &str) -> Result<Vec<RegisteredClaim>> {
        let rows = sqlx::query!(
            r#"
            SELECT claim_id, statement, "commit" AS commit_sha
            FROM claims_at_tip
            WHERE store = ?
            ORDER BY claim_id ASC
            "#,
            store,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut claims = Vec::with_capacity(rows.len());
        for row in rows {
            let id = parse_claim_id(&row.claim_id)?;
            let supports = self.supports_targets_of(store, &id).await?;
            claims.push(RegisteredClaim {
                id,
                statement: row.statement,
                supports,
                commit: row.commit_sha,
            });
        }
        Ok(claims)
    }

    async fn claim(&self, store: &str, id: &ClaimId) -> Result<Option<RegisteredClaim>> {
        let id_str = id.as_str();
        let row = sqlx::query!(
            r#"
            SELECT statement, "commit" AS commit_sha
            FROM claims_at_tip
            WHERE store = ? AND claim_id = ?
            "#,
            store,
            id_str,
        )
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        let supports = self.supports_targets_of(store, id).await?;
        Ok(Some(RegisteredClaim {
            id: id.clone(),
            statement: row.statement,
            supports,
            commit: row.commit_sha,
        }))
    }

    async fn supports_targets_of(&self, store: &str, id: &ClaimId) -> Result<Vec<String>> {
        let id_str = id.as_str();
        let rows = sqlx::query!(
            r#"
            SELECT target FROM supports_edges
            WHERE store = ? AND claim_id = ?
            ORDER BY target ASC
            "#,
            store,
            id_str,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.target).collect())
    }

    async fn claims_supporting(&self, target: &str) -> Result<Vec<SupportsEdge>> {
        let rows = sqlx::query!(
            r#"
            SELECT store, claim_id FROM supports_edges
            WHERE target = ?
            ORDER BY store ASC, claim_id ASC
            "#,
            target,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(SupportsEdge {
                    store: r.store,
                    claim_id: parse_claim_id(&r.claim_id)?,
                    target: target.to_owned(),
                })
            })
            .collect()
    }
}

/// Parse a stored claim id back into a validated [`ClaimId`]. A stored id the parser
/// rejects means a foreign writer or corruption — loud, not coerced.
fn parse_claim_id(s: &str) -> Result<ClaimId> {
    ClaimId::from_str(s).map_err(|_| StoreError::Corrupt {
        context: "claim_id is not a valid claim id".to_owned(),
        value: s.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_text_round_trips_every_variant() {
        // The column form is the shared type's own kebab-case JSON, so a stored
        // verdict and the enum can never disagree about what `held` means.
        for v in [
            Verdict::Held,
            Verdict::Drifted,
            Verdict::Unverifiable,
            Verdict::Broken,
        ] {
            let text = verdict_to_text(v).unwrap();
            assert_eq!(verdict_from_text(&text).unwrap(), v, "round-trip {v:?}");
        }
        assert_eq!(verdict_to_text(Verdict::Held).unwrap(), "held");
    }

    #[test]
    fn kind_text_round_trips() {
        let text = kind_to_text(EventKind::Verdict).unwrap();
        assert_eq!(text, "verdict");
        assert_eq!(kind_from_text(&text).unwrap(), EventKind::Verdict);
    }

    #[test]
    fn an_unrecognized_verdict_string_is_loud_not_coerced() {
        // A stored value no writer of this crate produced is a corruption signal,
        // never silently mapped to a pass.
        let err = verdict_from_text("green").unwrap_err();
        assert!(matches!(err, StoreError::Corrupt { .. }), "{err}");
    }

    #[test]
    fn producer_run_reads_a_string_run() {
        let mut p = serde_json::Map::new();
        p.insert("run".into(), serde_json::json!("1234567890"));
        assert_eq!(producer_run(&Producer(p)), Some("1234567890".to_owned()));
    }

    #[test]
    fn producer_run_is_none_when_absent() {
        // No run is unattributable: append rejects it rather than bucketing it into
        // the empty-run collision class.
        let p = serde_json::Map::new();
        assert_eq!(producer_run(&Producer(p)), None);
    }

    #[test]
    fn producer_run_is_none_when_empty() {
        // An empty-string run is as unattributable as an absent one, and must not
        // become a shared dedup bucket.
        let mut p = serde_json::Map::new();
        p.insert("run".into(), serde_json::json!(""));
        assert_eq!(producer_run(&Producer(p)), None);
    }

    #[test]
    fn producer_run_of_a_non_string_run_is_its_canonical_text() {
        // A numeric run (an unusual but possible producer typing) still yields a
        // stable dedup key, so redelivery is caught regardless of JSON typing.
        let mut p = serde_json::Map::new();
        p.insert("run".into(), serde_json::json!(42));
        assert_eq!(producer_run(&Producer(p)), Some("42".to_owned()));
    }
}
