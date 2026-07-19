-- Nag events on the one append-only ledger (hub-11).
--
-- The router records a delivered nag as an `EventKind::Nag` event on the SAME events
-- table the verdicts live on (HUB.md §2's wider event grammar): "already nagged" is then
-- DERIVED by diffing the current transitions against these events, never a mutable
-- "notified" flag (invariant #3). A nag is not a verdict, so it carries no verdict and no
-- single check — the two columns the verdict envelope filled become nullable here so a nag
-- row can honestly hold NULL for both. A verdict row still fills them (the ingest gate
-- always supplies them), so no verdict loses its check identity.
--
-- SQLite cannot ALTER a column's NOT NULL in place, so the events table is rebuilt: a new
-- table with the relaxed constraints, the rows copied over, the old table dropped, and the
-- append-only triggers and indexes recreated against the new table. This runs once, at
-- migration time, on a schema that (for any existing hub) holds only verdict rows, so the
-- copy preserves every attested observation verbatim, `seq` included (seq is copied
-- explicitly so a cursor a consumer already holds still points at the same event).
--
-- The dedup index is unchanged in shape (store, dedup_run, claim_id, check_digest): a nag
-- reuses check_digest to carry its FIRE KEY, so the DB index gives fire-once a second line
-- of defense beneath the ledger-diff the router derives. A nag's dedup_run is the router
-- principal's run, non-empty like any attributable event.

-- Rebuild `events` with verdict/check nullable. `PRAGMA foreign_keys` is per-connection
-- and off inside the migrator's transaction, so no FK cascade fires during the swap.
CREATE TABLE events_new (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind         TEXT    NOT NULL,
    claim_id     TEXT    NOT NULL,
    check_index  INTEGER,          -- NULL on a nag (not about one check); set on a verdict.
    check_digest TEXT,             -- NULL on a nag's... no: a nag stores its fire key here.
    verdict      TEXT,             -- NULL on a nag (invariant #4: a nag reports no verdict).
    evidence     TEXT,
    "commit"     TEXT    NOT NULL,
    store        TEXT    NOT NULL,
    producer     TEXT    NOT NULL,
    reported_at  TEXT    NOT NULL,
    dedup_run    TEXT    NOT NULL
);

-- Copy every existing row verbatim, seq preserved, so no attested observation moves.
INSERT INTO events_new
    (seq, kind, claim_id, check_index, check_digest, verdict, evidence,
     "commit", store, producer, reported_at, dedup_run)
SELECT
    seq, kind, claim_id, check_index, check_digest, verdict, evidence,
    "commit", store, producer, reported_at, dedup_run
FROM events;

DROP TABLE events;
ALTER TABLE events_new RENAME TO events;

-- Recreate the dedup index. check_digest can now be NULL (an ill-formed row), and SQLite
-- treats NULLs as distinct in a UNIQUE index, so a NULL digest never collides — but every
-- row this crate writes supplies a non-NULL check_digest (a verdict its check's digest, a
-- nag its fire key), so the index enforces dedup for both kinds.
CREATE UNIQUE INDEX events_dedup ON events (store, dedup_run, claim_id, check_digest);
CREATE INDEX events_by_claim ON events (claim_id);
-- Scanning the ledger for `nag` events (the router's fired-set diff) filters on kind, so
-- index it; the router reads only the nag rows to rebuild what has already fired.
CREATE INDEX events_by_kind ON events (kind);

-- Recreate the append-only triggers against the rebuilt table.
CREATE TRIGGER events_no_update
BEFORE UPDATE ON events
BEGIN
    SELECT RAISE(ABORT, 'events is append-only: UPDATE is forbidden');
END;

CREATE TRIGGER events_no_delete
BEFORE DELETE ON events
BEGIN
    SELECT RAISE(ABORT, 'events is append-only: DELETE is forbidden');
END;
