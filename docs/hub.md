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
> stores), the **ingest gate** (the single OIDC-authenticated verdict write path,
> `POST /ingest`), and the **first read endpoint** (`GET /api/claims/{id}`, the walking
> skeleton that derives a claim's standing over the live store). The *full* JSON API,
> the **hub MCP**, and the **web UI** arrive in later items and mount into this same
> shell. A freshly booted hub reports a truthful *empty* position — head 0, version 0 —
> until a sync populates the registry and a CI lane starts pushing verdicts through the
> ingest gate.

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
working directory. An **invalid** config — malformed TOML, a bad field, an unreadable
file — fails loudly before anything binds, naming the file and pointing at the offending
line, never a silent default. There is one deliberate exception: with **no `--config`
flag**, a *missing* `hub.toml` is not an error — the binary starts from an empty config so
the `CLAIM_HUB_*` env overrides alone can drive a boot. This is what lets a container boot
against an empty mounted volume with no config file at all (see [Run the
container](#run-the-container)). A missing file at an **explicit** `--config` path is still
a loud error: naming a config and mistyping it must never silently fall back to defaults.

A typo'd `listen` address reports:

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

The `[deriver]` section is the operator's layer over each claim's *own* `hub.max-age`
(HUB.md §2). By default the read API ages a claim on the window it declares in its own
`hub:` block, which registry sync persists; `[deriver]` only overrides or backfills that.
Both values are `<N>d` day counts, the same spelling a claim file uses; a malformed value
fails the boot loudly, naming the field:

```toml
[deriver]
default_max_age = "30d"    # window ONLY for a claim that declares no hub.max-age of its own
max_age_override = "7d"     # (optional) forces this window on EVERY claim, winning over its own
```

With no `[deriver]` section, each claim ages on its own `hub.max-age`; a claim that
declares neither its own window nor a config default stays `verified` on a passing check —
absent a window, the hub does not invent one. `max_age_override` is the operator's final
word on cadence for this environment; `default_max_age` is the fallback for claims that
declare none of their own.

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
   refreshed when a token names a key id the cache does not yet hold, so key rotation
   heals with no redeploy). That refresh is **rate-limited** — the key id is
   attacker-controlled and read before the signature is checked, so an un-throttled
   fetch-per-unknown-key would let a flood of forged tokens drive the hub's outbound
   request rate; a refresh fires at most once per short window regardless.
2. **`iss`** is present *and* the GitHub Actions issuer. A token that omits `iss`, `aud`,
   or `exp` is rejected outright — the issuer/audience pinning is never hollow.
3. **`aud`** is the hub's configured `audience` — this is what stops a token minted
   for another service from being replayed here. An empty configured `audience` is
   refused at boot, so the gate never stands up with vacuous audience pinning.
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

## Reading a claim's standing (`GET /api/claims/{id}`)

The hub's first read endpoint derives one claim's **standing** over the live store and
returns it with its *as-of* — the exact inputs the answer was computed from. This is the
read half of the loop: the CLI reports a verdict, the ingest gate lands it on the ledger,
and this endpoint derives what that verdict (plus the clock) *means* right now.

```sh
curl -s http://127.0.0.1:8080/api/claims/payments/libfoo-pin
```

```json
{
  "id": "payments/libfoo-pin",
  "store": "github.com/acme/payments",
  "standing": "verified",
  "verified_as_of": "2026-07-18T12:00:00Z",
  "stale_at": "2026-08-17T12:00:00Z",
  "due_at": null,
  "skips": [],
  "as_of": { "ledger_head": 1, "registry_version": 1, "clock": "2026-07-20T00:00:00Z" }
}
```

The `standing` is the conservative join over the claim's checks — bad news dominates, so
no combination of verdicts manufactures a green: `verified` only when every check's latest
verdict holds *and* the claim is within its freshness window; `stale` when a check is
overdue, broken, or never verified; `drifted` when any check's latest is `drifted`. A
claim the registry does not know is a `404`.

Freshness honors **the claim's own `hub.max-age`** first: registry sync persists each
claim's `hub:` hints, so a claim declaring `max-age: 30d` ages into `stale` 30 days after
its last passing verdict — on its own declared cadence, not a hub-wide default. The
`[deriver]` config is the operator's layer over that: `max_age_override` forces a window on
every claim (winning over its own), and `default_max_age` supplies one only for claims that
declare none. A claim with neither its own `max-age` nor a config window never ages by the
clock (a passing check keeps it `verified`) — absent a window, the hub does not invent one.

The **as-of** makes every answer honest and reproducible: the same (`ledger_head`,
`registry_version`, `clock`) always derives the same standing, and the standing can never
be older than the evidence it names. Nothing is stored — the standing is computed at read
time from the ledger and the clock every time (invariant #3), so a claim **ages into stale
by the clock alone**: a claim that was `verified` reads `stale` once the read clock passes
`stale_at`, with no new verdict and no write.

> **Note (v1):** the `/api/claims/{id}` endpoint is the walking skeleton — one read that
> proves the loop. The full read surface (claims by path/repo/standing, the drifted and due
> sets, the dossier, the cursor feed) arrives with the JSON API. Where two connected stores
> hold a claim with the same id, this endpoint returns the lexicographically-first store's
> standing; the JSON API adds a `store` selector to address a claim exactly.

## Trying the whole loop

The end-to-end loop — git → sync → attested verdict → ledger → derive → read — is what the
hub exists to close. The integration test `crates/claim-hub/tests/skeleton.rs` runs it in
one process with an injected clock and a mocked identity provider (no network): it seeds a
git fixture, syncs it, POSTs one attested `held` verdict through the ingest gate, and reads
`/api/claims/{id}` to see `verified` — then advances the clock to watch the same claim age
into `stale` with no new event.

To run a hub against an empty data directory and watch it stand up its own database on
first boot, use the compose example, which now boots the **packaged container image**:

```sh
docker compose -f examples/hub/docker-compose.yml up --build
# in another shell:
curl -s http://127.0.0.1:8080/status   # => head 0 / version 0 on a fresh volume
```

The `hub-data` volume starts empty and holds `hub.db`; the hub creates and migrates it on
first boot. Export the hub by copying that file, delete it by removing the volume — export
is a copy, delete is an `rm` (HUB.md §1). The next section covers this ownership story in
full.

## Self-host: run it, own it, back it up, leave

The hub is **one binary plus one SQLite file the customer owns** (HUB.md §1, §4). There is
no product-run database, no phone-home, and no central store. Everything the hub knows lives
in that file: the append-only verdict ledger and the git-mirror registry it derives standing
from. That makes the ownership operations plain file operations — stated here as the things
you actually run.

### Run the container

The packaged image is a small musl-static binary plus the two runtime dependencies the hub
genuinely needs: `git` (registry sync shells out to it) and CA certificates (the JWKS fetch
over HTTPS validates GitHub's certificate against the OS trust store). Build it from the
repository's `Dockerfile`, or pull a prebuilt tag you have pushed to your own registry:

```sh
# Build the image from source.
docker build -t claim-hub:local .

# Run it against a customer-owned volume. The volume starts EMPTY; the hub creates and
# migrates hub.db in it on first boot. Nothing else is installed.
docker run -d --name hub \
  -p 127.0.0.1:8080:8080 \
  -v hub-data:/data \
  claim-hub:local

curl -s http://127.0.0.1:8080/status   # => {"ledger_head":0,"registry_version":0,"rejection_count":0}
```

The image sets `CLAIM_HUB_LISTEN=0.0.0.0:8080` and `CLAIM_HUB_DATABASE=/data/hub.db` so it
binds a reachable address and lands its database on the mounted volume with no config file at
all. To configure connected stores and the ingest gate's `[oidc]` trust, drop a `hub.toml`
into the mounted volume (see [Configuration](#configuration) above) — env overrides still
win, so the address and database path stay correct regardless.

`examples/hub/docker-compose.yml` is the same run as a compose file, mounting one named
volume for the one owned file.

> **The single caveat to "static."** The build stage needs a C toolchain and `cmake`: the
> HTTPS client's TLS backend (`aws-lc-rs`, via reqwest/rustls) compiles C and assembly. The
> `Dockerfile`'s build stage installs them; the runtime image needs none of it. The one
> runtime dependency beside the binary is the `git` the image ships.

### Own, export, and delete the data — it is one file

The whole hub is `hub.db` (with `-wal`/`-shm` sidecars alongside it while the hub is
running). A *stopped* hub is one complete file; a *running* hub must be snapshotted, not
naively copied (see [Back up](#back-up)). So:

- **Export** a *stopped* hub with a copy: `cp /path/to/hub.db /elsewhere/`. Export a
  *running* hub with the online backup below (`cp hub.db` on a live hub can drop the WAL and
  lose the ledger tail). That file *is* your hub's history.
- **Delete** is an `rm`: remove the file, or `docker compose down -v` to destroy the volume.
  There is no server-side remnant, no soft-delete, no product-held copy.
- **Leave** is taking the file: stand up `claim-hub` anywhere against a backup of `hub.db` and
  it derives the identical answers — proven by the backup-restore exercise below. There is no
  migration to escape, because there was never a store you did not control.

### Back up

A hub in WAL mode holds recent, committed writes in the `hub.db-wal` sidecar until a
checkpoint folds them into `hub.db`. So a plain `cp hub.db` against a **running** hub is a
hot copy that races the checkpoint: it can produce a file that passes `PRAGMA
integrity_check` yet has silently dropped the newest ledger events. Never `cp` a live hub's
`hub.db` alone. Two supported ways, both to storage **you** control:

1. **Litestream (continuous, near-real-time).** [Litestream](https://litestream.io) streams
   the SQLite WAL to S3-compatible object storage as it is written, so a crash loses seconds,
   not a day. It runs as an operational sidecar next to the hub — no code dependency, no hub
   change — pointed at the same `hub.db`. Restore is `litestream restore` into a fresh data
   directory, then boot the hub over it. Use this when the ledger is load-bearing enough that
   point-in-time recovery matters.

2. **Online snapshot (periodic, one self-contained file).** SQLite's online backup takes a
   transactionally-consistent snapshot of a *live* hub into one new file with **no** sidecars,
   so nothing races the WAL:

   ```sh
   # A running hub, backed up safely into one file:
   sqlite3 /path/to/hub.db ".backup '/backups/hub-$(date +%F).db'"
   # equivalently: sqlite3 /path/to/hub.db "VACUUM INTO '/backups/hub.db'"
   ```

   Restore is a plain copy of that one file into a fresh data directory — copy no `-wal`/`-shm`
   (there are none), and delete any stale sidecar beside the destination first so SQLite cannot
   recover a wrong-but-consistent state — then boot the hub over it. A *stopped* hub needs no
   snapshot: it is one complete file you can `cp` directly. Schedule the snapshot however you
   back up any file (a nightly `.backup` to durable storage is enough for many hubs).

   > **`sqlite3` runs on the host, not in the container.** The runtime image ships only the
   > hub binary, `git`, and CA certificates — deliberately no `sqlite3`. Run the `.backup`
   > from the host against the mounted volume's `hub.db` (e.g. a path under the `hub-data`
   > volume), or install `sqlite3` on the host; do not expect it inside the container.

Either way the backup is your file on your storage; the product never holds a copy.

### The backup-restore exercise is tested, not asserted

The ownership promise — *a copy of the file, restored into a fresh hub, derives the identical
answer* — is checked, not just claimed:

- `scripts/hub-backup-restore.sh` runs the **real server binary**: it seeds a hub (syncs a
  git fixture, ingests one attested verdict through the real ingest gate), reads a claim's
  standing over `/api/claims/{id}`, takes an online `sqlite3 ".backup"` of the live hub,
  restores that single file into a second hub, and asserts the restored hub's standing is
  byte-identical (`standing`, `verified_as_of`, `stale_at`, and the ledger/registry as-of) —
  only the wall-clock read instant legitimately differs.
- `scripts/hub-cold-start.sh` boots the real binary against an empty directory and asserts a
  truthful empty `/status` (head 0 / version 0 / no rejections) — the "point it at a fresh
  volume and it just works" guarantee.
- `crates/claim-hub/tests/backup_restore.rs` is the same identical-answers property as a
  deterministic, in-process gate test (no ports, no external tools), so every branch and CI
  run proves it, and `crates/claim-hub-store/tests/backup.rs` pins the data-loss directly:
  the online backup captures uncheckpointed writes a bare `cp hub.db` drops, and survives a
  checkpoint racing a concurrent writer.

The container image builds and cold-starts, and the two scripts run, in CI
(`.github/workflows/hub-image.yml`); the deterministic test runs in the gate.
