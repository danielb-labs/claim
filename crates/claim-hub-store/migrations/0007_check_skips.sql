-- Per-check skip data, persisted so the router (hub-11) can detect a lapsed skip `until`.
--
-- A skip defers a check with a reason and an optional `until` expiry. When that `until`
-- lapses, the deferred check is due again — a transition the router routes (a lapsed skip
-- fires a nag). The deriver already surfaces lapsed skips from its `ClaimStanding.skips`,
-- but only if the skip data reaches it: registry sync parses the claim and has each check's
-- skip in hand, so it records it here beside the check's digest.
--
-- Stored on `check_digests` (one row per declared check already), nullable so a check with
-- no skip stores NULL for both. `skip_until` is the canonical RFC 3339 date/instant the
-- claim file declared (or NULL for an indefinite skip); the deriver compares it to the
-- clock. Rebuilt with the claim's snapshot: these columns live on `check_digests`, which a
-- snapshot replace cascades away and re-inserts, so an edited or removed skip never lingers
-- past the next sync.
ALTER TABLE check_digests ADD COLUMN skip_reason TEXT; -- the skip's reason, or NULL if no skip.
ALTER TABLE check_digests ADD COLUMN skip_until TEXT;  -- RFC 3339 expiry, NULL if indefinite or no skip.
