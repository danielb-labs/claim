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
use crate::findings::{Findings, SyncFinding};
use crate::ledger::{Appended, Ledger, Position, StoredEvent};
use crate::registry::{RegisteredClaim, Registry, RegistryVersion, SupportsEdge};
use crate::rejections::Rejections;
use claim_core::{ClaimId, Days, Timestamp, Verdict};
use claim_hub_core::{CheckRef, Event, EventKind, HubHints, Producer};
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

    /// Write a consistent, self-contained backup of the whole database to `dest`,
    /// safe to run against a live hub.
    ///
    /// This is SQLite's online backup (`VACUUM INTO`), not a file copy: it reads a
    /// transactionally-consistent snapshot through a connection and writes **one** new
    /// database file with no `-wal`/`-shm` sidecars — the WAL is already folded in. That
    /// is why it is the export a running hub must use. A live `cp hub.db` is a *hot* copy
    /// that races the WAL: a checkpoint interleaving the copy can yield a file that passes
    /// `PRAGMA integrity_check` yet has dropped the ledger tail, silently losing committed
    /// verdicts (invariants #4 and #6). `VACUUM INTO` cannot lose them — it snapshots
    /// under a read transaction — so a restore is a single-file copy of `dest` with no
    /// sidecars to carry.
    ///
    /// `dest` must not already exist; SQLite refuses to overwrite, so a backup never
    /// clobbers a prior one silently. The path is bound as a value (not string-formatted
    /// into the SQL) so a path with a quote cannot break out of the statement.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Sql`] if `dest` cannot be written (it exists, its directory
    /// is missing, or the disk is full) or the snapshot read fails.
    pub async fn backup(&self, dest: impl AsRef<Path>) -> Result<()> {
        let dest = dest.as_ref();
        let dest_str = dest.to_str().ok_or_else(|| StoreError::Corrupt {
            context: "backup destination path is not valid UTF-8".to_owned(),
            value: dest.display().to_string(),
        })?;
        sqlx::query("VACUUM INTO ?")
            .bind(dest_str)
            .execute(&self.pool)
            .await
            .map_err(StoreError::Sql)?;
        Ok(())
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

/// The value stored in the `check_digest` column — the dedup key's digest component — for
/// an event of either kind.
///
/// A verdict carries its check's content digest; a nag carries its [`FireKey`](claim_hub_core::FireKey) in the
/// producer block, and that fire key IS the digest the dedup index keys on — so the ledger
/// enforces fire-once (one nag per fire key per store+run+claim) beneath the router's
/// derived ledger-diff. A nag with no fire key, or a verdict-kind event with no check, is
/// ill-formed: it is rejected loudly rather than stored with a NULL digest that would never
/// dedup and could double-fire (invariant #6).
///
/// A `Verdict`-kind event must also carry a `verdict`: the columns became nullable for nags
/// (hub-11), but a verdict row with a NULL verdict is unreadable — every later `scan_from`
/// would fail on "a verdict row has no verdict", bricking the whole hub with no recovery on
/// an append-only ledger. So the write boundary rejects it here, symmetrically with the
/// missing-check guard, rather than storing an unrecoverable NULL (invariant #6).
fn dedup_digest(event: &Event) -> Result<String> {
    match event.kind {
        EventKind::Verdict => {
            if event.verdict.is_none() {
                return Err(StoreError::Corrupt {
                    context: "a verdict event carries no verdict".to_owned(),
                    value: event.claim.clone(),
                });
            }
            event
                .check
                .as_ref()
                .map(|c| c.digest.clone())
                .ok_or_else(|| StoreError::Corrupt {
                    context: "a verdict event carries no check digest".to_owned(),
                    value: event.claim.clone(),
                })
        }
        EventKind::Nag => claim_hub_core::fire_key_of(event)
            .map(|k| k.as_str().to_owned())
            .ok_or_else(|| StoreError::Corrupt {
                context: "a nag event carries no fire key in its producer block".to_owned(),
                value: event.claim.clone(),
            }),
        // `EventKind` is `#[non_exhaustive]`: a future kind has no dedup-digest convention
        // yet, so it is refused loudly rather than filed under a guessed identity (invariant
        // #6). The item that adds a kind adds its arm here.
        _ => Err(StoreError::Corrupt {
            context: "an event of an unrecognized kind has no dedup identity".to_owned(),
            value: event.claim.clone(),
        }),
    }
}

impl Ledger for SqliteStore {
    async fn append(&self, event: &Event) -> Result<Appended> {
        let kind = kind_to_text(event.kind)?;
        // `verdict` and `check` are `None` on a nag (invariant #4: a nag reports no
        // verdict and is about no single check), so both columns are nullable and the
        // nag row honestly stores NULL for verdict/index.
        let verdict = event.verdict.map(verdict_to_text).transpose()?;
        let check_index = event
            .check
            .as_ref()
            .map(|c| {
                i64::try_from(c.index).map_err(|_| StoreError::Corrupt {
                    context: "check.index too large for the ledger".to_owned(),
                    value: c.index.to_string(),
                })
            })
            .transpose()?;
        // The dedup `check_digest` column: for a verdict, the check's content digest; for
        // a nag, the fire key its producer block carries, so the dedup index gives
        // fire-once a second line of defense beneath the router's ledger-diff. A nag with
        // no fire key is ill-formed telemetry and is rejected loudly rather than filed
        // with a NULL digest that would never dedup (invariant #6).
        let check_digest = dedup_digest(event)?;
        let producer_json =
            serde_json::to_string(&event.producer).map_err(|source| StoreError::Json {
                context: "producer".to_owned(),
                source,
            })?;
        // An event with no usable producer run is unattributable and cannot dedup
        // safely; reject it loudly rather than bucket it into an empty-run collision
        // class (invariant #6). The run is also the run component of the dedup key. A nag
        // carries the fire key as its run, so it always satisfies this.
        let dedup_run = producer_run(&event.producer).ok_or(StoreError::MissingProducerRun)?;
        let reported_at = event.reported_at.to_string();

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
            check_digest,
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
            check_digest,
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
            let reported_at =
                Timestamp::from_str(&row.reported_at).map_err(|_| StoreError::Corrupt {
                    context: "reported_at is not an RFC 3339 instant".to_owned(),
                    value: row.reported_at.clone(),
                })?;
            let kind = kind_from_text(&row.kind)?;
            // A verdict reconstructs its `check` and `verdict`; a nag has neither (invariant
            // #4). The `check_digest` column holds the check's content digest for a verdict
            // and the nag's fire key for a nag — the nag's fire key stays in the producer
            // block on the reconstructed event, so `check` is `None` and the event cannot be
            // read as a verdict. A match makes a future kind decide explicitly.
            let (check, verdict) = match kind {
                EventKind::Verdict => {
                    let index = row
                        .check_index
                        .map(|i| {
                            usize::try_from(i).map_err(|_| StoreError::Corrupt {
                                context: "check_index out of range".to_owned(),
                                value: i.to_string(),
                            })
                        })
                        .transpose()?
                        .ok_or_else(|| StoreError::Corrupt {
                            context: "a verdict row has no check_index".to_owned(),
                            value: row.claim_id.clone(),
                        })?;
                    let digest = row
                        .check_digest
                        .clone()
                        .ok_or_else(|| StoreError::Corrupt {
                            context: "a verdict row has no check_digest".to_owned(),
                            value: row.claim_id.clone(),
                        })?;
                    let verdict_text =
                        row.verdict.as_deref().ok_or_else(|| StoreError::Corrupt {
                            context: "a verdict row has no verdict".to_owned(),
                            value: row.claim_id.clone(),
                        })?;
                    (
                        Some(CheckRef { index, digest }),
                        Some(verdict_from_text(verdict_text)?),
                    )
                }
                EventKind::Nag => (None, None),
                // A future kind stored by a newer hub has no reconstruction rule here yet;
                // reading it back is a loud corruption signal, never a coerced verdict.
                _ => {
                    return Err(StoreError::Corrupt {
                        context: "a stored event has an unrecognized kind".to_owned(),
                        value: row.kind.clone(),
                    })
                }
            };
            events.push(StoredEvent {
                position: Position(row.seq),
                event: Event {
                    kind,
                    claim: row.claim_id,
                    check,
                    verdict,
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

/// A live SQLite transaction, the type the snapshot-write helpers execute against.
///
/// Aliased so the helpers read cleanly and the `sqlx::Transaction` type stays an
/// implementation detail of this module — it never appears on the `Registry`/
/// `Findings` trait surface, so the seam stays clean for the Postgres impl.
type Tx<'a> = sqlx::Transaction<'a, sqlx::Sqlite>;

/// Replace a store's claims and supports edges within an open transaction.
///
/// Ensures the store row exists (idempotent), wipes the store's claims — the supports
/// edges cascade from `claims_at_tip`'s `ON DELETE CASCADE` — and re-inserts the
/// snapshot. Factored out so `replace_store` and the atomic `replace_store_snapshot`
/// share one definition and cannot drift.
async fn write_claims(tx: &mut Tx<'_>, store: &str, claims: &[RegisteredClaim]) -> Result<()> {
    sqlx::query!("INSERT OR IGNORE INTO stores (store) VALUES (?)", store)
        .execute(&mut **tx)
        .await?;
    sqlx::query!("DELETE FROM claims_at_tip WHERE store = ?", store)
        .execute(&mut **tx)
        .await?;

    for claim in claims {
        let id = claim.id.as_str();
        // The claim's `hub:` hints as their canonical `<N>d` strings (or NULL), so the
        // deriver ages the claim on its own declared cadence. `Days`'s Display is the
        // file's exact spelling, round-tripped losslessly by its FromStr on read.
        let hub_max_age = claim.hub.max_age.map(|d| d.to_string());
        let hub_recheck = claim.hub.recheck.map(|d| d.to_string());
        sqlx::query!(
            r#"
            INSERT INTO claims_at_tip (store, claim_id, statement, "commit", hub_max_age, hub_recheck, path)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
            store,
            id,
            claim.statement,
            claim.commit,
            hub_max_age,
            hub_recheck,
            claim.path,
        )
        .execute(&mut **tx)
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
            .execute(&mut **tx)
            .await?;
        }

        // The per-check digests, keyed by the check's declared position, so the ingest
        // gate maps a positional CLI report onto content identities (issue #18) without
        // re-parsing, plus each check's declared skip (reason + until) so the router
        // detects a lapsed skip (hub-11). Cascades away with the claim on the next snapshot
        // replace.
        for (index, digest) in claim.check_digests.iter().enumerate() {
            let check_index = i64::try_from(index).map_err(|_| StoreError::Corrupt {
                context: "check index too large for the registry".to_owned(),
                value: index.to_string(),
            })?;
            // The skip at this index, if the claim carried one. `check_skips` is parallel to
            // `check_digests` (both in declared order), so a shorter or absent vector just
            // means no skip — never a mismatch that could file a skip against the wrong check.
            let skip = claim.check_skips.get(index).and_then(|s| s.as_ref());
            let skip_reason = skip.map(|s| s.reason.clone());
            let skip_until = skip.and_then(|s| s.until).map(|u| u.to_string());
            sqlx::query!(
                r#"
                INSERT INTO check_digests (store, claim_id, check_index, digest, skip_reason, skip_until)
                VALUES (?, ?, ?, ?, ?, ?)
                "#,
                store,
                id,
                check_index,
                digest,
                skip_reason,
                skip_until,
            )
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

/// Replace a store's sync findings within an open transaction.
///
/// Wipes the store's findings and re-inserts `findings`, so a file fixed at the new
/// tip no longer nags. The store row is assumed to exist (a snapshot write inserts it
/// via [`write_claims`] first); the standalone [`Findings::replace_store_findings`]
/// inserts it itself for independent use.
async fn write_findings(tx: &mut Tx<'_>, store: &str, findings: &[SyncFinding]) -> Result<()> {
    sqlx::query!("DELETE FROM sync_findings WHERE store = ?", store)
        .execute(&mut **tx)
        .await?;
    for f in findings {
        sqlx::query!(
            r#"
            INSERT INTO sync_findings (store, file, "commit", reason)
            VALUES (?, ?, ?, ?)
            "#,
            store,
            f.file,
            f.commit,
            f.reason,
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Advance the single registry version counter within an open transaction, returning
/// the new value.
///
/// In the same transaction as the snapshot write, so a sync and its version bump are
/// atomic: a reader never sees new claims at an old version or vice versa.
async fn bump_version(tx: &mut Tx<'_>) -> Result<RegistryVersion> {
    let row = sqlx::query!(
        "UPDATE registry_version SET version = version + 1 WHERE id = 0 RETURNING version"
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok(RegistryVersion(row.version))
}

impl Registry for SqliteStore {
    async fn replace_store(
        &self,
        store: &str,
        claims: &[RegisteredClaim],
    ) -> Result<RegistryVersion> {
        let mut tx = self.pool.begin().await?;
        write_claims(&mut tx, store, claims).await?;
        let version = bump_version(&mut tx).await?;
        tx.commit().await?;
        Ok(version)
    }

    async fn replace_store_snapshot(
        &self,
        store: &str,
        claims: &[RegisteredClaim],
        findings: &[SyncFinding],
    ) -> Result<RegistryVersion> {
        // Claims, supports edges, findings, and the version bump in one transaction.
        // If any step faults, the whole snapshot rolls back — the registry and its
        // findings can never skew, so a malformed file is never indexed-away with its
        // nag lost (invariant #6). See the trait doc.
        let mut tx = self.pool.begin().await?;
        write_claims(&mut tx, store, claims).await?;
        write_findings(&mut tx, store, findings).await?;
        let version = bump_version(&mut tx).await?;
        tx.commit().await?;
        Ok(version)
    }

    async fn version(&self) -> Result<RegistryVersion> {
        let row = sqlx::query!("SELECT version FROM registry_version WHERE id = 0")
            .fetch_one(&self.pool)
            .await?;
        Ok(RegistryVersion(row.version))
    }

    async fn stores(&self) -> Result<Vec<String>> {
        // `store` is a TEXT PRIMARY KEY, which sqlite reports as nullable in query
        // metadata; the `!` asserts the non-null the schema guarantees so the column
        // maps to `String`, not `Option<String>`.
        let rows = sqlx::query!(r#"SELECT store AS "store!" FROM stores ORDER BY store ASC"#)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|r| r.store).collect())
    }

    async fn claims_of(&self, store: &str) -> Result<Vec<RegisteredClaim>> {
        let rows = sqlx::query!(
            r#"
            SELECT claim_id, statement, "commit" AS commit_sha, hub_max_age, hub_recheck, path
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
            let (check_digests, check_skips) = self.checks_of(store, &id).await?;
            claims.push(RegisteredClaim {
                id,
                statement: row.statement,
                supports,
                commit: row.commit_sha,
                // A NULL path (a pre-hub-11 row) reads as empty; the router treats an empty
                // path as "no path" and dead-letters rather than routing to a guessed owner.
                path: row.path.unwrap_or_default(),
                check_digests,
                hub: hub_hints(row.hub_max_age.as_deref(), row.hub_recheck.as_deref())?,
                check_skips,
            });
        }
        Ok(claims)
    }

    async fn claim(&self, store: &str, id: &ClaimId) -> Result<Option<RegisteredClaim>> {
        let id_str = id.as_str();
        let row = sqlx::query!(
            r#"
            SELECT statement, "commit" AS commit_sha, hub_max_age, hub_recheck, path
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
        let (check_digests, check_skips) = self.checks_of(store, id).await?;
        Ok(Some(RegisteredClaim {
            id: id.clone(),
            statement: row.statement,
            supports,
            commit: row.commit_sha,
            // A NULL path (a pre-hub-11 row) reads as empty; the router dead-letters on an
            // empty path rather than routing to a guessed owner.
            path: row.path.unwrap_or_default(),
            check_digests,
            hub: hub_hints(row.hub_max_age.as_deref(), row.hub_recheck.as_deref())?,
            check_skips,
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

    async fn check_digest(
        &self,
        store: &str,
        claim: &ClaimId,
        index: usize,
    ) -> Result<Option<String>> {
        // A `usize` index that overflows `i64` cannot match any stored row (the write
        // side rejects such indices), so it is a plain "not in the registry" — return
        // `None`, letting the gate reject the push loudly rather than error.
        let Ok(check_index) = i64::try_from(index) else {
            return Ok(None);
        };
        let id = claim.as_str();
        let row = sqlx::query!(
            r#"
            SELECT digest FROM check_digests
            WHERE store = ? AND claim_id = ? AND check_index = ?
            "#,
            store,
            id,
            check_index,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.digest))
    }
}

impl SqliteStore {
    /// Every check's digest **and** declared skip for `id` in `store`, ordered by the
    /// check's declared index, so the two returned vectors are `RegisteredClaim`'s
    /// `check_digests` and `check_skips` — `[i]` is the digest and skip of check `i`. Empty
    /// vectors mean the claim declares no checks (or is not at tip).
    ///
    /// The skip's `until` is parsed back from its stored RFC 3339 text; a stored value the
    /// timestamp parser rejects is loud corruption ([`StoreError::Corrupt`]), never coerced
    /// to a silent `None` that would hide a lapsed skip (invariant #6).
    #[allow(clippy::type_complexity)]
    async fn checks_of(
        &self,
        store: &str,
        id: &ClaimId,
    ) -> Result<(Vec<String>, Vec<Option<claim_hub_core::DerivedSkip>>)> {
        let id_str = id.as_str();
        let rows = sqlx::query!(
            r#"
            SELECT digest, skip_reason, skip_until FROM check_digests
            WHERE store = ? AND claim_id = ?
            ORDER BY check_index ASC
            "#,
            store,
            id_str,
        )
        .fetch_all(&self.pool)
        .await?;
        let mut digests = Vec::with_capacity(rows.len());
        let mut skips = Vec::with_capacity(rows.len());
        for row in rows {
            digests.push(row.digest);
            // A skip exists iff a reason was stored; the `until` is optional even then.
            let skip = match row.skip_reason {
                Some(reason) => {
                    let until = row
                        .skip_until
                        .map(|raw| {
                            Timestamp::from_str(&raw).map_err(|_| StoreError::Corrupt {
                                context: "skip_until is not an RFC 3339 instant".to_owned(),
                                value: raw,
                            })
                        })
                        .transpose()?;
                    Some(claim_hub_core::DerivedSkip { reason, until })
                }
                None => None,
            };
            skips.push(skip);
        }
        Ok((digests, skips))
    }
}

impl Rejections for SqliteStore {
    async fn record_rejection(&self) -> Result<i64> {
        let row = sqlx::query!(
            "UPDATE ingest_rejections SET count = count + 1 WHERE id = 0 RETURNING count"
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.count)
    }

    async fn rejection_count(&self) -> Result<i64> {
        let row = sqlx::query!("SELECT count FROM ingest_rejections WHERE id = 0")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.count)
    }
}

impl Findings for SqliteStore {
    async fn replace_store_findings(&self, store: &str, findings: &[SyncFinding]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        // The store row must exist for the finding's foreign key. A snapshot write
        // inserts it via `write_claims`; this standalone path inserts it itself so the
        // method is safe to call independently (in isolation tests, say).
        sqlx::query!("INSERT OR IGNORE INTO stores (store) VALUES (?)", store)
            .execute(&mut *tx)
            .await?;
        write_findings(&mut tx, store, findings).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn findings(&self) -> Result<Vec<SyncFinding>> {
        let rows = sqlx::query!(
            r#"
            SELECT store, file, "commit" AS commit_sha, reason
            FROM sync_findings
            ORDER BY store ASC, file ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| SyncFinding {
                store: r.store,
                file: r.file,
                commit: r.commit_sha,
                reason: r.reason,
            })
            .collect())
    }

    async fn findings_of(&self, store: &str) -> Result<Vec<SyncFinding>> {
        let rows = sqlx::query!(
            r#"
            SELECT file, "commit" AS commit_sha, reason
            FROM sync_findings
            WHERE store = ?
            ORDER BY file ASC
            "#,
            store,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| SyncFinding {
                store: store.to_owned(),
                file: r.file,
                commit: r.commit_sha,
                reason: r.reason,
            })
            .collect())
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

/// Build [`HubHints`] from the stored `<N>d` hint columns, or fail loudly on a value the
/// day-count parser rejects.
///
/// `None` in (the column was NULL — the claim declared no such hint) yields `None` out.
/// A present value is parsed with [`Days`]'s `FromStr`; a stored string it rejects is a
/// foreign writer or corruption, surfaced as [`StoreError::Corrupt`] rather than coerced
/// to a silent `None` — which would drop a real freshness window and let a stale claim
/// read green (invariant #6). The value written was `Days::to_string`, so a round-trip is
/// lossless and this only fails on genuine corruption.
fn hub_hints(max_age: Option<&str>, recheck: Option<&str>) -> Result<HubHints> {
    Ok(HubHints {
        max_age: parse_days_column("hub_max_age", max_age)?,
        recheck: parse_days_column("hub_recheck", recheck)?,
    })
}

/// Parse one stored `<N>d` hint column into `Option<Days>`, naming the column on
/// corruption.
fn parse_days_column(column: &str, value: Option<&str>) -> Result<Option<Days>> {
    value
        .map(|raw| {
            Days::from_str(raw).map_err(|_| StoreError::Corrupt {
                context: format!("{column} is not a valid day count"),
                value: raw.to_owned(),
            })
        })
        .transpose()
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
