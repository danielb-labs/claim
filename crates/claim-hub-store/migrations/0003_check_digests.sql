-- The per-check content digests of each registered claim, indexed by the check's
-- declared position, so the ingest gate can turn a CLI report's *positional* check
-- result into the check's stable content identity.
--
-- Why the registry holds this, and why by position:
--   The CLI's `claim check --json` report identifies each check only by its slot in
--   the claim's declared check list (index 0, 1, ...) — it carries no digest. The
--   hub's ledger, by contrast, keys a check's history on its content digest
--   (`check_digest`, issue #18), so a shallow check's pass can never clear a deep
--   check's drift. Bridging the two needs the check *definition* the digest is
--   computed from, which the report does not carry. Registry sync already parses the
--   claim at tip and has every check in hand, so it computes each check's canonical
--   digest there (one `claim-core` grammar, one digest function) and records it here
--   keyed by (store, claim, index). Ingest then reads the digest by position — a pure
--   read of the registry, no re-parsing at the door.
--
-- Rebuilt with its claim's snapshot: a snapshot replace for a store cascades these
-- away with `claims_at_tip` and re-inserts the tip's digests, so an edited or
-- retired check's digest never lingers. A claim (or check index) absent here at
-- ingest time is a claim the registry has not synced: ingest rejects that push loudly
-- rather than fabricate an identity (invariant #6), because a wrong digest would file
-- a verdict under the wrong check.
CREATE TABLE check_digests (
    store       TEXT    NOT NULL,
    claim_id    TEXT    NOT NULL,
    check_index INTEGER NOT NULL, -- the check's zero-based position in the claim.
    digest      TEXT    NOT NULL, -- the canonical SHA-256 of the check's definition.
    PRIMARY KEY (store, claim_id, check_index),
    FOREIGN KEY (store, claim_id) REFERENCES claims_at_tip (store, claim_id) ON DELETE CASCADE
);
