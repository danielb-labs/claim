//! Online-backup safety against a **live** WAL-mode hub.
//!
//! The self-host promise is "back up by taking the file" (HUB.md §1), but a naive
//! `cp hub.db` against a *running* hub is a hot copy that races the WAL: appends live
//! in `hub.db-wal` until a checkpoint folds them into `hub.db`, and a checkpoint that
//! interleaves the copy can produce a file that passes `PRAGMA integrity_check` yet has
//! **dropped the ledger tail** — silently losing committed verdicts (invariants #4 and
//! #6). These tests pin the fix: [`SqliteStore::backup`] uses SQLite's online backup
//! (`VACUUM INTO`), which snapshots under a read transaction into one self-contained
//! file with no sidecars, and so cannot lose a committed event.
//!
//! Two properties are proven:
//! 1. `backup` captures events that are still only in the WAL (uncheckpointed), while a
//!    bare `cp hub.db` at the same instant misses them — the old approach failing, the
//!    new one holding, deterministically (no checkpoint has run, so the split is exact).
//! 2. With a concurrent writer and a `wal_checkpoint(TRUNCATE)` interleaved during the
//!    backup, the restored single-file copy's ledger head equals the source head — the
//!    race the finding proved, now survived.

use claim_core::Verdict;
use claim_hub_core::{CheckRef, Event, EventKind, Producer};
use claim_hub_store::{Ledger, Position, SqliteStore};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{ConnectOptions, Connection};
use std::path::Path;
use tempfile::TempDir;

/// A verdict event with a caller-chosen run id, so each append is a distinct ledger row
/// (the dedup key is (store, run, claim, digest), and the run varies).
fn event(run: &str) -> Event {
    let mut producer = serde_json::Map::new();
    producer.insert("repository".into(), serde_json::json!("acme/payments"));
    producer.insert("run".into(), serde_json::json!(run));
    Event {
        kind: EventKind::Verdict,
        claim: "payments/pin".into(),
        check: CheckRef {
            index: 0,
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

/// Open a raw WAL-mode connection to the file, so a test can issue the `PRAGMA
/// wal_checkpoint(TRUNCATE)` and the bare appends the trait does not expose. Matches the
/// store's own journal mode so the connection joins the same WAL.
async fn raw_connection(path: &Path) -> sqlx::SqliteConnection {
    SqliteConnectOptions::new()
        .filename(path)
        .journal_mode(SqliteJournalMode::Wal)
        .connect()
        .await
        .expect("open a raw WAL connection to the hub file")
}

/// Open the restored single-file backup as a fresh store and read its ledger head. The
/// restore is a plain file copy of the one backup file — no `-wal`/`-shm` to carry,
/// because `VACUUM INTO` already folded everything in.
async fn restored_head(backup_file: &Path, into: &Path) -> Position {
    std::fs::copy(backup_file, into).expect("copy the single backup file");
    // Belt-and-suspenders: a restore must never inherit a stale sidecar from a prior
    // hub at the destination, which SQLite could otherwise "recover" into a
    // wrong-but-consistent state. The backup carries none; assert the destination has
    // none either.
    for suffix in ["-wal", "-shm"] {
        let mut name = into.as_os_str().to_owned();
        name.push(suffix);
        assert!(
            !Path::new(&name).exists(),
            "a restore must have no {suffix} sidecar beside it"
        );
    }
    let store = SqliteStore::open(into)
        .await
        .expect("open the restored store");
    store.head().await.expect("read the restored head")
}

#[tokio::test]
async fn backup_captures_uncheckpointed_writes_that_a_bare_copy_drops() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("hub.db");
    let store = SqliteStore::open(&db).await.unwrap();

    // Append events, then take NO checkpoint: in WAL mode they live in `hub.db-wal`,
    // not yet folded into `hub.db`. This is the exact state a live hub sits in between
    // checkpoints — the state a naive `cp hub.db` copies incompletely.
    for i in 0..20 {
        store.append(&event(&format!("run-{i}"))).await.unwrap();
    }
    let source_head = store.head().await.unwrap();
    assert_eq!(source_head, Position(20), "twenty events appended");

    // The correct backup: an online `VACUUM INTO` snapshot to one self-contained file.
    let backup_file = dir.path().join("backup.db");
    store.backup(&backup_file).await.unwrap();
    assert!(
        !dir.path().join("backup.db-wal").exists(),
        "VACUUM INTO produces no WAL sidecar"
    );

    // The online backup carries every committed event.
    let restored = restored_head(&backup_file, &dir.path().join("restored.db")).await;
    assert_eq!(
        restored, source_head,
        "the online backup restored the full ledger, losing nothing"
    );

    // The OLD approach, at the same uncheckpointed instant: copy only `hub.db`. Because
    // the appends are still in the WAL, the bare main file is missing them — the exact
    // silent data loss the finding proved. Opening it reports a shorter (or empty)
    // ledger, never the full one.
    let bare = dir.path().join("bare-hub.db");
    std::fs::copy(&db, &bare).expect("bare copy of just hub.db");
    let bare_store = SqliteStore::open(&bare).await.unwrap();
    let bare_head = bare_store.head().await.unwrap();
    assert!(
        bare_head < source_head,
        "a bare `cp hub.db` at an uncheckpointed instant drops the ledger tail: \
         bare head {bare_head:?} < source head {source_head:?}"
    );
}

#[tokio::test]
async fn backup_survives_a_checkpoint_racing_a_concurrent_writer() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("hub.db");
    let store = SqliteStore::open(&db).await.unwrap();

    // A base of committed events, then a concurrent writer appends more while the backup
    // runs and a TRUNCATE checkpoint interleaves — the precise interleaving the finding
    // proved silently corrupts a file copy.
    for i in 0..10 {
        store.append(&event(&format!("base-{i}"))).await.unwrap();
    }

    let writer_store = store.clone();
    let writer = tokio::spawn(async move {
        for i in 0..40 {
            writer_store
                .append(&event(&format!("racing-{i}")))
                .await
                .unwrap();
        }
    });

    // Interleave a TRUNCATE checkpoint through a second connection while writes land —
    // this is what folds (and truncates) the WAL mid-flight, the operation that races a
    // hot `cp` and drops the tail.
    let mut conn = raw_connection(&db).await;
    for _ in 0..5 {
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&mut conn)
            .await
            .expect("checkpoint");
        tokio::task::yield_now().await;
    }

    writer.await.expect("the concurrent writer finished");
    conn.close().await.expect("close the raw connection");

    // The backup is taken after the writer has committed all its events, against the
    // still-open live store — a `VACUUM INTO` snapshot that cannot lose a committed row
    // to the checkpoints that just ran.
    let source_head = store.head().await.unwrap();
    assert_eq!(source_head, Position(50), "10 base + 40 racing events");
    let backup_file = dir.path().join("backup.db");
    store.backup(&backup_file).await.unwrap();

    let restored = restored_head(&backup_file, &dir.path().join("restored.db")).await;
    assert_eq!(
        restored, source_head,
        "every committed event survived the backup despite the interleaved checkpoints"
    );
}
