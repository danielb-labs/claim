-- The hub's storage schema, v1: one append-only event ledger and a
-- wipe-and-rebuild registry, in a single SQLite file (HUB-IMPLEMENTATION.md §1.4).
--
-- Two disciplines are enforced *in the schema*, below the Rust traits, so a bug
-- reaching around the trait still cannot break them:
--   * the events table is append-only — triggers RAISE on any UPDATE or DELETE;
--   * redelivery of the same observation is deduplicated by a UNIQUE index on
--     (producer run, claim, check identity), per HUB.md §2.

-- The append-only event ledger: the hub's one piece of primary state.
--
-- `seq` is the monotonic ledger cursor (INTEGER PRIMARY KEY AUTOINCREMENT, so a
-- reused rowid can never hand a later event an earlier cursor). A reader scans
-- from a cursor and the head is `MAX(seq)`. Every other column is the event
-- envelope (claim-hub-core's `Event`) stored field-for-field; `producer` and the
-- honesty-typed columns keep the trust judgment re-derivable (invariant #3).
CREATE TABLE events (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    kind         TEXT    NOT NULL,
    claim_id     TEXT    NOT NULL,
    check_index  INTEGER NOT NULL,
    check_digest TEXT    NOT NULL,
    verdict      TEXT    NOT NULL,
    evidence     TEXT,            -- NULL when the check recorded none.
    "commit"     TEXT    NOT NULL,
    store        TEXT    NOT NULL,
    -- The verified producer identity block, serialized as JSON verbatim so the
    -- trust judgment re-derives from exactly what the ingest gate verified.
    producer     TEXT    NOT NULL,
    reported_at  TEXT    NOT NULL, -- RFC 3339 UTC instant (claim_core::Timestamp).
    -- The producer's run id, extracted from the producer JSON at append time so
    -- the dedup index below can key on it: the run is one value inside the
    -- producer object, and lifting it into its own column keys the index without
    -- SQLite JSON-path indexing. Empty string when the producer carries no run.
    dedup_run    TEXT    NOT NULL
);

-- The dedup key of HUB.md §2: a retried push carrying the same observation —
-- same producer *run*, same claim, same check *identity* (digest) — hits this
-- index and is absorbed, so an observation is never double-counted. Store, check
-- index, and verdict are deliberately *not* part of the key: the same run
-- re-reporting the same check is the same observation regardless of a
-- re-serialized producer block or a changed verdict, and letting a changed
-- verdict slip past the index would be a silent double-count.
CREATE UNIQUE INDEX events_dedup ON events (dedup_run, claim_id, check_digest);

-- Scans by claim are the deriver's common read (a claim's verdict history), so
-- index the claim id; `seq` order within a claim falls out of the primary key.
CREATE INDEX events_by_claim ON events (claim_id);

-- Defense in depth (HUB-IMPLEMENTATION.md §1.4): the ledger is append-only, and
-- these triggers make UPDATE and DELETE against it fail even if a future bug or a
-- raw SQL path reaches past the `Ledger` trait, which has no update or delete.
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

-- The registry: the hub's mirror of git, derived data rebuilt by re-scanning.
--
-- `version` is a single monotonic counter advanced once per sync (a store
-- snapshot replace), so a reader can tell whether the registry changed under it
-- and the deriver's memo can key on it. Held in a one-row table rather than a
-- column so it advances atomically with a snapshot replace inside one
-- transaction.
CREATE TABLE registry_version (
    id      INTEGER PRIMARY KEY CHECK (id = 0), -- exactly one row, id 0.
    version INTEGER NOT NULL
);
INSERT INTO registry_version (id, version) VALUES (0, 0);

-- Each connected store, by its canonical id (e.g. github.com/acme/payments).
CREATE TABLE stores (
    store TEXT PRIMARY KEY
);

-- Each claim at its store's default-branch tip, with the commit it was read at.
-- A snapshot replace for a store deletes this store's rows and re-inserts them,
-- so a claim absent at the new tip is dropped (a retirement, HUB.md §3).
CREATE TABLE claims_at_tip (
    store      TEXT NOT NULL REFERENCES stores (store) ON DELETE CASCADE,
    claim_id   TEXT NOT NULL,
    statement  TEXT NOT NULL,
    "commit"   TEXT NOT NULL, -- the sha this claim was read at.
    PRIMARY KEY (store, claim_id)
);

-- The cross-store `supports` index: one row per (claim, target) edge, the
-- substrate cross-repo routing (#10) keys on. Rebuilt with its claim's snapshot.
CREATE TABLE supports_edges (
    store    TEXT NOT NULL,
    claim_id TEXT NOT NULL,
    target   TEXT NOT NULL,
    PRIMARY KEY (store, claim_id, target),
    FOREIGN KEY (store, claim_id) REFERENCES claims_at_tip (store, claim_id) ON DELETE CASCADE
);
