-- The ingest rejection counter: how many pushes the ingest gate refused.
--
-- A rejected push writes no event (invariant #4: a forged or malformed push never
-- becomes telemetry) — but it must not vanish either. A hub that silently drops
-- telemetry ages the affected claims into staleness with nobody told why (invariant
-- #6, HUB.md §3), so every rejection is counted here and surfaced at `/status`. The
-- count is the machine-readable signal a monitor watches: a rising rejection count
-- means a producer's pushes are being turned away (a misconfigured audience, an
-- unconnected repository, an expired token) while the claims those pushes would have
-- refreshed quietly go stale.
--
-- Held as a single monotonic counter in a one-row table, incremented in its own
-- committed statement so a rejection is durably recorded before the 4xx is returned.
-- It is *not* on the append-only events ledger: a rejection is the absence of an
-- event, operational health the hub owns, not an attested observation on the log —
-- and it is a mutable count, which the events table forbids by trigger. A future item
-- that wants per-reason breakdowns or a rejection *feed* (who was turned away, when,
-- why) adds a table beside this; the aggregate count is what v1's `/status` needs.
CREATE TABLE ingest_rejections (
    id    INTEGER PRIMARY KEY CHECK (id = 0), -- exactly one row, id 0.
    count INTEGER NOT NULL
);
INSERT INTO ingest_rejections (id, count) VALUES (0, 0);
