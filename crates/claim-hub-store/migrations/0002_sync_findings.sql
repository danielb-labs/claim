-- Sync findings: a malformed claim file observed at a store's tip, recorded
-- rather than dropped so it nags (invariant #6, HUB.md §3). Added by hub-05.
--
-- Findings are derived data on the same replace-per-sync discipline as the
-- registry: each sync of a store deletes that store's findings and re-inserts the
-- ones seen at the new tip, so a file fixed at the new tip drops its finding
-- automatically. The cascade off `stores` means disconnecting a store clears its
-- findings too.
CREATE TABLE sync_findings (
    store    TEXT NOT NULL REFERENCES stores (store) ON DELETE CASCADE,
    file     TEXT NOT NULL, -- path relative to the store root, e.g. .claims/x.md.
    "commit" TEXT NOT NULL, -- the tip sha the file was read at.
    reason   TEXT NOT NULL, -- the parser's reason, naming the field to fix.
    -- One finding per (store, file): a tip has at most one parse outcome per file,
    -- and the replace-per-sync write rebuilds the set anyway, so the file is the key.
    PRIMARY KEY (store, file)
);
