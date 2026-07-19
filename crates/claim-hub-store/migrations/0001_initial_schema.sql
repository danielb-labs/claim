-- The hub's storage schema, v1: one append-only event ledger and a
-- wipe-and-rebuild registry, in a single SQLite file (HUB-IMPLEMENTATION.md §1.4).
--
-- Two disciplines are enforced *in the schema*, below the Rust traits, so a bug
-- reaching around the trait still cannot break them:
--   * the events table is append-only — triggers RAISE on any UPDATE or DELETE.
--     For the triggers to also catch the implicit DELETE that
--     INSERT OR REPLACE / REPLACE INTO perform on a conflict, connections open
--     with PRAGMA recursive_triggers = ON (set in `SqliteStore::open`, since a
--     migration cannot pin per-connection pragmas); without it a BEFORE DELETE
--     trigger does not fire for that implicit delete and history could be rewritten.
--   * redelivery of the same observation is deduplicated by a UNIQUE index on
--     (store, producer run, claim, check identity) — see the index comment below.

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

-- The dedup key: a retried push carrying the same observation hits this index and
-- is absorbed, so an observation is never double-counted (HUB.md §2). The key is
-- (store, producer run, claim, check identity), and every component earns its place:
--   * `store` — a GitHub Actions run id is unique per repository, not globally
--     (HUB.md §4's identity is (repository, run)), and `check_digest` is
--     content-based and deliberately stable across repos, so without `store` two
--     genuinely distinct observations from two stores sharing a run id + claim id +
--     digest would collapse, silently dropping one.
--   * `dedup_run` — the producer's run distinguishes one CI run's report from the
--     next; a non-empty run is required at append (a run-less verdict is
--     unattributable and rejected, not bucketed).
--   * `claim_id` — the digest is a property of the check's definition alone and is
--     claim-independent, so two distinct claims with identical checks would collide
--     without it.
--   * `check_digest` — the check's content identity, so a shallow check's pass never
--     lands on a deep check's ledger slot (#18).
-- Deliberately *not* in the key: `check_index` and `verdict`. The same run
-- re-reporting the same check is the same observation regardless of a re-serialized
-- producer block or a changed verdict, and letting a changed verdict slip past the
-- index would be a silent double-count.
CREATE UNIQUE INDEX events_dedup ON events (store, dedup_run, claim_id, check_digest);

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

-- The reverse-routing query `claims_supporting(target)` (the #10 substrate) filters
-- on `target`, which is the *last* column of the primary key and so cannot use it as
-- a prefix — an index on `target` turns that table scan into a seek.
CREATE INDEX supports_edges_by_target ON supports_edges (target);
