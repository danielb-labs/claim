# The hub — self-host quickstart

The **hub** is the per-environment service that turns `claim`'s reported verdicts
into the thing the stateless CLI cannot: staleness derived over time, and a nag when
a fact goes unchecked. The CLI reads claims and reports whether each holds *right
now*; the hub ingests those reports, mirrors the claims in git, and derives standing,
freshness, and due-ness from the verdict stream it stores (see the
[CLI/hub boundary](design/CLI-HUB-BOUNDARY.md) and [HUB.md](design/HUB.md)). It is a
single binary plus one SQLite file the customer owns — export is `cp`, delete is
`rm`, and there is no database server the product runs on your behalf.

> **This is v1, still growing.** The hub today ships the application shell (config,
> the HTTP app, `/status`, tracing, boot), **registry sync** (mirroring your git
> stores), and the **ingest gate** (the single OIDC-authenticated verdict write path,
> `POST /ingest`). The **JSON API**, the **hub MCP**, and the **web UI** arrive in
> later items and mount into this same shell. A freshly booted hub reports a truthful
> *empty* position — head 0, version 0 — until a sync populates the registry and a CI
> lane starts pushing verdicts through the ingest gate.

## Running the binary

The hub reads one TOML config file (default `hub.toml` in the working directory) and
serves. Point it at an empty directory and it creates and migrates its own database
on first boot:

```sh
# Build the hub binary.
source "$HOME/.cargo/env"
cargo build -p claim-hub

# A minimal config: where to listen, and where the database file lives.
cat > hub.toml <<'TOML'
listen = "127.0.0.1:8080"
database = "hub.db"
TOML

# Boot. Creates and migrates hub.db if it does not exist, then serves.
./target/debug/claim-hub --config hub.toml
```

`--config` (or `-c`) names the file; with no flag the binary reads `hub.toml` in the
working directory. A missing or invalid config fails loudly before anything binds,
naming the file and pointing at the offending line — never a silent default. A typo'd
`listen` address reports:

```text
error: config `hub.toml`: TOML parse error at line 1, column 10
  |
1 | listen = "not-an-address"
  |          ^^^^^^^^^^^^^^^^
invalid socket address syntax
```

and an unknown key names it and lists the fields it expected.

### Configuration

The config is one TOML file plus a few environment overrides. v1 fields:

| Field | Meaning | Default |
|---|---|---|
| `listen` | The address the HTTP listener binds. | `127.0.0.1:8080` (loopback — an unconfigured hub is not exposed off the host) |
| `database` | The SQLite file the ledger and registry live in; created and migrated on first boot. | `hub.db` |

The `[oidc]` section configures the **ingest gate** (see below): its `audience` is
the identifier the hub verifies a producer's OIDC token against, and `repositories`
is the set of connected repositories a token may come from. **With no `[oidc]`
section the ingest route is not mounted** — a hub that cannot authenticate producers
exposes no write path at all, rather than one that rejects everything:

```toml
[oidc]
audience = "https://hub.acme.example"   # what the hub identifies itself as
repositories = ["acme/payments"]         # connected repos a token may come from
```

Other sections a later item consumes are already accepted so an operator's file
keeps working as the hub grows: `[[stores]]` (connected git stores, syncing),
`[hub_overrides]` (per-hub `hub:` cadence overrides), and `[read_auth]` (read-auth
policy, secure-by-default).

Two environment variables override the file's fields per instance, so a shared
config can be pointed at different addresses or database paths without editing it:

- `CLAIM_HUB_LISTEN` — overrides `listen` (e.g. `0.0.0.0:8080`).
- `CLAIM_HUB_DATABASE` — overrides `database`.

A malformed override is as loud as a malformed file field, naming the variable.

Logging is structured via `tracing`; set `RUST_LOG` to tune verbosity (e.g.
`RUST_LOG=claim_hub=debug`), defaulting to `info`.

## What `/status` reports

`/status` is the hub's machine-readable health-and-position endpoint. It reports
truthfully against a real, possibly empty store — an empty database is head 0 /
version 0 / never synced, not an error and never a fabricated "healthy":

```sh
curl -s http://127.0.0.1:8080/status
```

```json
{
  "ledger_head": 0,
  "registry_version": 0,
  "rejection_count": 0
}
```

| Field | Meaning | Source |
|---|---|---|
| `ledger_head` | The position of the most recent event on the append-only ledger; `0` on an empty ledger. Advances as verdicts are ingested. | `Ledger::head` |
| `registry_version` | The number of store syncs applied; `0` before the first sync. | `Registry::version` |
| `last_sync` | When the registry was last synced (RFC 3339). Omitted until registry sync records one. | registry sync |
| `rejection_count` | How many ingests the hub rejected — a quiet source of staleness a monitor must be able to see. Increments on every refused push (a forged token, a wrong audience, a malformed envelope); a rising count means telemetry is being turned away while the claims it would refresh go stale. | the ingest gate's rejection counter |

`last_sync` reports `omitted` until a sync records one. `rejection_count` is live:
watch it — a climbing count is the hub telling you telemetry is being dropped, which
is exactly the invisible staleness the tool exists to prevent.

## The ingest gate (`POST /ingest`)

Ingest is the hub's **single telemetry write path**. A CI lane runs `claim check
--json` and POSTs the report to `/ingest`, authenticated by the runner's GitHub
Actions OIDC id-token in an `Authorization: Bearer <token>` header. There is no other
way in — no backfill endpoint, no manual verdict entry, no static ingest token. A
developer's local `claim check` is a terminal report, never hub telemetry.

The gate authenticates *who produced* each push — the pipeline that ran the checks,
proven by its OIDC token — and records that verified identity verbatim beside every
verdict, so the trust judgment stays re-derivable. In order, it verifies:

1. **Signature** against the issuer's published JWKS (fetched once and cached;
   refreshed automatically when a token names a key id the cache does not yet hold, so
   key rotation heals with no redeploy).
2. **`iss`** is the GitHub Actions issuer.
3. **`aud`** is the hub's configured `audience` — this is what stops a token minted
   for another service from being replayed here.
4. **`exp`** is in the future.
5. **`repository`** is one of the configured connected `repositories`.

A valid push appends one event per check result and returns the ledger positions:

```json
{ "status": "accepted", "accepted": 1, "positions": [{ "position": 42, "new": true }] }
```

A **redelivery** — the same run reporting the same check again — dedups to the
original success and adds no row (`"new": false`), so a retried CI push never
double-counts. Evidence is capped at ingest: an oversized note is truncated with a
visible marker, never silently dropped.

Every failure is **loud and, where it is a judged-and-refused push, counted**:

| Failure | Status | Counted |
|---|---|---|
| Forged/invalid signature | `401` | yes |
| Expired token | `401` | yes |
| Wrong audience / issuer | `401` | yes |
| Unknown signing key | `401` | yes |
| Unconnected repository (authentic but unauthorized) | `403` | yes |
| Malformed `--json` envelope (names the field) | `400` | yes |
| Claim/check the registry has not synced | `400` | yes |
| Missing `Authorization` header (no identity to judge) | `401` | no |
| Identity provider unreachable (cannot verify *now*) | `503` | no |

A rejected push **writes no event** and returns a JSON body naming the reason
(`{"error": "..."}`). The counted rejections show up at `/status`, so a hub turning
telemetry away is visible rather than silently aging claims into stale. An
*unavailable* identity provider is a `503`, not a rejection: the hub could not judge
the token, so it never calls a possibly-valid push forged — the producer retries.
