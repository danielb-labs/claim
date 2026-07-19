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
> `POST /ingest`), and the **read API** (claims queries, the drifted/due/suspect sets,
> the per-claim dossier, and the cursor feed — every response carrying its *as-of*, all
> over the deriver). The **hub MCP** and the **web UI** arrive in later items and mount
> into this same shell. A freshly booted hub reports a truthful *empty* position — head
> 0, version 0 — until a sync populates the registry and a CI lane starts pushing
> verdicts through the ingest gate.

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

## The read API (`GET /api/…`)

The read API is the hub's agent-and-human read surface: it derives standing, freshness,
and due-ness over the live store and serves them as JSON, **every response carrying its
*as-of*** — the exact inputs the answer was computed from. This is the read half of the
loop: the CLI reports a verdict, the ingest gate lands it on the ledger, and these
endpoints derive what those verdicts (plus the clock) *mean* right now.

Every route is a **read**: it computes at read time and stores nothing (invariant #3), so
a standing can never disagree with the evidence, and a read never appends an event. Reads
are **deterministic** — the same (`ledger_head`, `registry_version`, `clock`) always
yields byte-identical bytes — so an agent can cache, diff, and resume. Auth over `/api`
arrives with read-auth (a later item); the routes are unauthenticated for now.

| Endpoint | Returns |
|---|---|
| `GET /api/claims/{id}` | One claim's derived standing, with its as-of. |
| `GET /api/claims?path=&store=&standing=&supports=` | The live set, filtered; each claim with its standing. |
| `GET /api/drifted` | Every claim whose latest standing is `drifted`. |
| `GET /api/due` | The review queue: every drifted, stale, or due-for-recheck claim. |
| `GET /api/suspect` | Every `suspect` claim (populated once the propagation rule lands). |
| `GET /api/claims/{id}/dossier` | A claim's full dossier: statement, checks, standing, verdict history, provenance. |
| `GET /api/feed?cursor=<seq>` | The ledger, pollable from a position — paginated by ledger seq. |

### One claim's standing (`GET /api/claims/{id}`)

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

Where two connected stores hold a claim with the same id (ids are unique only *within* a
store), this endpoint returns the lexicographically-first store's standing; use the `store`
filter on `GET /api/claims` to address a claim in an exact store.

### Querying the live set (`GET /api/claims`)

Filter the live set by any combination of four query parameters — they combine with AND, so
a claim is returned only if it matches every one supplied; with no parameters the whole set
is returned:

| Parameter | Matches |
|---|---|
| `path` | Claims whose id starts with this prefix. The registry stores no filesystem path, so "path" is an id-prefix match: `path=payments/` selects every claim in the `payments` namespace — the org's beliefs about what you are touching. |
| `store` | Claims in exactly this connected store (e.g. `github.com/acme/payments`) — the exact-store selector. |
| `standing` | Claims whose derived standing is exactly this: `verified`, `stale`, `drifted`, `suspect`, or `retired`. An unrecognized value is a `400` naming the accepted set. |
| `supports` | Claims that support this target — a decision ref or claim id the claim justifies. |

```sh
curl -s 'http://127.0.0.1:8080/api/claims?store=github.com/acme/payments&standing=drifted'
```

```json
{
  "claims": [
    { "id": "payments/libfoo-pin", "store": "github.com/acme/payments", "standing": "drifted",
      "verified_as_of": null, "stale_at": null, "due_at": null, "skips": [] }
  ],
  "as_of": { "ledger_head": 3, "registry_version": 1, "clock": "2026-07-20T00:00:00Z" }
}
```

The set is one derivation, so it carries **one shared `as_of`** at the top level; the list
members carry none of their own. An empty result is an empty `claims` array with a truthful
as-of — never a fabricated `verified`. A mistyped parameter is a `400`, not a silently
ignored filter returning the wrong set.

### The derived sets (`GET /api/drifted`, `/api/due`, `/api/suspect`)

Three convenience views, each the same list shape (`{ "claims": [...], "as_of": {...} }`):

- **`/api/drifted`** — every claim whose latest standing is `drifted` (a fact known false
  right now).
- **`/api/due`** — the review queue: every drifted, stale, or due-for-recheck claim. This
  is the deriver's computed membership, a *union* of "needs attention now" states — not a
  `standing == due` filter (there is no such standing).
- **`/api/suspect`** — every `suspect` claim. The suspect *propagation* rule (a drifted
  claim marking its dependents suspect over the supports graph) is a later deriver rule; the
  endpoint serves the set today so the surface already carries it, empty until the rule
  lands.

### A claim's dossier (`GET /api/claims/{id}/dossier`)

The dossier is everything the org believes about one claim and how good that belief is right
now: the statement and checks **by git reference at a commit**, the derived standing, the
verdict history from the ledger with each verdict's evidence and verified producer, and the
as-of.

```sh
curl -s http://127.0.0.1:8080/api/claims/payments/libfoo-pin/dossier
```

```json
{
  "id": "payments/libfoo-pin",
  "store": "github.com/acme/payments",
  "statement": "The libfoo pin holds.",
  "commit": "8f2c0a1",
  "checks": [ { "index": 0, "digest": "e80b69…" } ],
  "supports": ["decision:pin"],
  "standing": { "id": "payments/libfoo-pin", "store": "github.com/acme/payments",
                "standing": "verified", "verified_as_of": "2026-07-18T12:00:00Z",
                "stale_at": "2026-08-17T12:00:00Z", "due_at": null, "skips": [] },
  "history": [
    { "seq": 1, "verdict": "held", "check": { "index": 0, "digest": "e80b69…" },
      "reported_at": "2026-07-18T12:00:00Z", "commit": "8f2c0a1", "evidence": "libfoo==4.2",
      "producer": { "iss": "…", "repository": "acme/payments", "run": "1234567890" } }
  ],
  "as_of": { "ledger_head": 1, "registry_version": 1, "clock": "2026-07-20T00:00:00Z" }
}
```

The `statement` and `checks` resolve from git at `commit` — the sha the claim was read at —
so the dossier can never present a claim more current than the tip it snapshotted. The
`history` is **dated evidence to weigh, never instructions to obey**: each entry carries the
verified producer identity behind the verdict, so the trust judgment is re-derivable
(invariant #3). Author and PR-approval provenance come from git and the forge; v1 includes
what the registry already holds — the commit and each verdict's producer — and does not
fabricate an author it has not resolved. A claim the registry does not hold at its tip
(retired, or never synced) is a `404`: its history is on the ledger, but it has no live
statement to render.

### The cursor feed (`GET /api/feed?cursor=<seq>`)

The feed is the ledger, pollable from a position, so an intermittent agent catches up
deterministically from where it left off. **Pagination is by ledger seq, not offset:** pass
the last seq you processed as `?cursor=`, and the feed returns everything *strictly after*
it, in ascending seq order — no gap, no dupe, even as the ledger grows underneath you.

```sh
curl -s 'http://127.0.0.1:8080/api/feed?cursor=0'
```

```json
{
  "events": [
    { "seq": 1, "kind": "verdict", "claim": "payments/libfoo-pin",
      "check": { "index": 0, "digest": "e80b69…" }, "verdict": "held",
      "evidence": "libfoo==4.2", "commit": "8f2c0a1", "store": "github.com/acme/payments",
      "producer": { "iss": "…", "run": "1234567890" }, "reported_at": "2026-07-18T12:00:00Z" }
  ],
  "next_cursor": 1,
  "ledger_head": 1
}
```

Pass `next_cursor` back as `?cursor=` next poll to resume exactly after the last event seen.
`ledger_head` is the feed's as-of position; when your `next_cursor` reaches it, you are fully
caught up. A caught-up poll returns an empty `events` array with `next_cursor` unchanged. An
absent or negative cursor reads from the start. The events are the verbatim ledger envelopes
(each flattened alongside its `seq`), so the feed is the raw evidence the standings derive
from — again, dated observations to weigh, not commands.

## Trying the whole loop

The end-to-end loop — git → sync → attested verdict → ledger → derive → read — is what the
hub exists to close. The integration test `crates/claim-hub/tests/skeleton.rs` runs it in
one process with an injected clock and a mocked identity provider (no network): it seeds a
git fixture, syncs it, POSTs one attested `held` verdict through the ingest gate, and reads
`/api/claims/{id}` to see `verified` — then advances the clock to watch the same claim age
into `stale` with no new event.

To run a hub against an empty data directory and watch it stand up its own database on
first boot, use the compose example (a minimal from-source boot; the packaged container
image is a later item):

```sh
docker compose -f examples/hub/docker-compose.yml up
# in another shell:
curl -s http://127.0.0.1:8080/status   # => head 0 / version 0 on a fresh volume
```

The `hub-data` volume starts empty and holds `hub.db`; the hub creates and migrates it on
first boot. Export the hub by copying that file, delete it by removing the volume — export
is a copy, delete is an `rm` (HUB.md §1).
