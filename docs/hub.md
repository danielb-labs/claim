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
> `POST /ingest`), the **read API** (claims queries, the drifted/due/suspect sets,
> the per-claim dossier, and the cursor feed — every response carrying its *as-of*, all
> over the deriver), the **web UI** (server-rendered pages over the same read model, each
> with a machine-readable **markdown twin**), **`/llms.txt`** (the agent-facing index of every
> surface), the **hub MCP** (the agent binding — five read-only tools over the same read
> model, mounted at `/mcp`), the **router / nag** (it notices when a fact drifts, goes stale by
> the clock, or a skip lapses, routes each to its CODEOWNERS owner exactly once, and serves the
> rendered nag content at `GET /api/nags` for the CI glue to deliver), and **read authentication**
> (a bearer-token layer over every read surface — an IdP or hub-minted scoped tokens, secure by
> default). A freshly booted hub reports a truthful *empty* position — head 0, version 0 — until a
> sync populates the registry and a CI lane starts pushing verdicts through the ingest gate.

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

The `[router]` section tunes the **router / nag** tick (see below): `period_secs` is how
often it re-derives and fires a nag for a newly-noticed transition. Its default (300, five
minutes) bounds how quickly a claim aging into stale by the clock alone is nagged; a shorter
period nags sooner at a negligible re-derivation cost:

```toml
[router]
period_secs = 300   # how often the router tick runs (default 300 = 5 minutes)
```

The `[read_auth]` section configures **read authentication** for the API, UI, and MCP
surfaces — see [Read authentication](#read-authentication) below. It is **secure by
default**: with no read-auth decision the hub refuses to boot rather than serve open reads.

Other sections a later item consumes are already accepted so an operator's file
keeps working as the hub grows: `[[stores]]` (connected git stores, syncing) and
`[hub_overrides]` (per-hub `hub:` cadence overrides).

A few environment variables override the file's fields per instance, so a shared
config can be pointed at different addresses or database paths without editing it:

- `CLAIM_HUB_LISTEN` — overrides `listen` (e.g. `0.0.0.0:8080`).
- `CLAIM_HUB_DATABASE` — overrides `database`.
- `CLAIM_HUB_OPEN_READS` — the explicit open-reads opt-in (`true`/`false`), so an
  empty-volume container can serve open reads on a trusted private network with no config
  file. Unset leaves the file's value (default: authed).

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

## Read authentication

The hub's read surfaces — the JSON API (`/api/*`), the web UI and its markdown twins
(`/ui/*`, `/llms.txt`), and the hub MCP (`/mcp`) — sit behind one **read-auth layer**, so
every read route is gated the same way. Only `/status` (health) and the discovery document
(below) are served unauthenticated, deliberately.

**Secure by default.** A hub with no read-auth decision does **not** boot: with authed
reads in force and no way to authenticate anyone, it fails loudly at startup and names the
fix, rather than silently serving open reads. There are three ways to make the decision —
configure an IdP, mint a scoped token, or explicitly open reads:

### Option A — an OAuth 2.1 identity provider

Point the hub at your IdP; it validates a read's `Authorization: Bearer <jwt>` against the
issuer's JWKS (RS256 pinned, `iss`/`aud`/`exp` required, the signature checked — the same
machinery the ingest gate uses, with the JWKS refresh rate-limited the same way). A token's
`scope` claim must include `read`:

```toml
[read_auth.issuer]
issuer = "https://idp.acme.example"                      # a read token's `iss` must equal this
audience = "https://hub.acme.example"                     # a read token's `aud` must equal this (the hub's identifier)
jwks_url = "https://idp.acme.example/.well-known/jwks.json"
```

An empty `issuer`, `audience`, or `jwks_url` is refused at boot (empty pinning would accept
any token).

### Option B — hub-minted scoped tokens (no IdP needed)

For a self-hoster with no IdP, the hub mints its own scoped bearer tokens. Run:

```sh
claim-hub mint-token --name ci-dashboard --scope read
```

It prints the **raw token once** (give it to the client) and a config snippet holding only
its **hash** (paste it into the config). The hub stores only the hash — a leaked config (or
backup) yields no usable token:

```toml
[[read_auth.tokens]]
name = "ci-dashboard"
scopes = ["read"]
hash = "sha256:…"          # the hash of the token; the raw token is never stored
```

The client then reads with `Authorization: Bearer <raw-token>`. To revoke a token, delete
its entry. `--scope` may repeat (`read`, and `act` for a future write surface); it defaults
to `read`.

### Option C — open reads (trusted private network only)

To serve reads with no credential — appropriate only behind a trusted private network —
opt in **explicitly**:

```toml
[read_auth]
open_reads = true
```

or set `CLAIM_HUB_OPEN_READS=true` (handy for an empty-volume container with no config
file). This is the **only** way to open reads; it is never the default.

### What a client sees

| Situation | Status |
|---|---|
| No credential (and reads not opened) | `401` with a `WWW-Authenticate: Bearer resource_metadata="…"` pointer |
| Bad signature / expired / wrong `aud` / wrong `iss` / unknown key | `401` |
| Unrecognized hub token | `401` |
| Authenticated, but the token lacks the `read` scope | `403` |
| Identity provider unreachable (cannot verify *now*) | `503` (never a silent allow) |
| Valid IdP token or hub token with `read` | `200` |

The hub reads a JWT's `scope` claim as the OAuth 2.1 space-delimited **string**. An IdP
that emits `scope` as a JSON *array* (nonstandard) fails closed — the token is rejected
`401`, never admitted with no scopes — matching how the ingest gate reads its own producer
claim.

A `401` points at the **RFC 9728 protected-resource metadata** document, served
unauthenticated so a client can discover how to authenticate:

```sh
curl -s http://127.0.0.1:8080/.well-known/oauth-protected-resource
```

```json
{
  "resource": "https://hub.acme.example",
  "authorization_servers": ["https://idp.acme.example"],
  "bearer_methods_supported": ["header"],
  "scopes_supported": ["read"]
}
```

`authorization_servers` is omitted when the hub runs on the scoped-token floor with no IdP
(a hub token is provisioned out of band, by `mint-token`). The scope model is **read
broadly, act narrowly**: v1 has no act endpoints, so `read` is the only scope any route
requires, but a future act surface will require `act` — a `read` token can never reach it.

## Pushing verdicts from CI (the ingest Action)

The hub ships one GitHub Action, **`hub-ingest`**, that closes the write half of the
loop: it runs `claim check --json` in your repo's CI, mints the runner's GitHub Actions
OIDC identity, and POSTs the report to the hub's `/ingest`. This is the *one* attested
path — the CLI stays hub-agnostic (it never learns a hub URL or token), and the
authenticate-and-push glue lives in the hub's Action, not the core binary (see
[the CI/hub boundary](ci.md)).

Add it to a workflow that runs on the events you want verdicts for — a push to your
default branch, a merge, or a schedule:

```yaml
name: push verdicts to the claim hub

on:
  push:
    branches: [main]
  schedule:
    - cron: "0 7 * * *"   # a daily re-verify, so a fact gone stale is re-reported

jobs:
  ingest:
    runs-on: ubuntu-latest
    # REQUIRED: `id-token: write` lets the runner mint the OIDC token the hub trusts.
    # `contents: read` checks out the .claims/ store. Without id-token: write the
    # Action fails loudly rather than pushing an unauthenticated report.
    permissions:
      id-token: write
      contents: read
    steps:
      - uses: actions/checkout@v4

      # Build/install the claim binary (or download a pinned release when one exists).
      - uses: your-org/claim/.github/actions/install-claim@v1
        with:
          source-path: crates/claim

      - name: Push verdicts to the hub
        uses: your-org/claim/.github/actions/hub-ingest@v1
        with:
          hub-url: https://hub.acme.example
          audience: https://hub.acme.example   # MUST equal the hub's [oidc].audience
```

### What the Action needs

| Input | Meaning |
|---|---|
| `hub-url` | The hub's base URL. `/ingest` is appended. |
| `audience` | The identifier the OIDC token is minted for. **It must equal the hub's configured `[oidc].audience`**, or the hub rejects the token (`401`, wrong audience). |
| `claims-dir` | The directory holding the `.claims/` store to check. Default `.`. |
| `claim-bin` | The `claim` binary to run. Default `claim` (on `PATH`). |
| `max-time` | Seconds bounding each HTTP request (the token mint and the ingest POST). Default `60`. A hub that stalls after accepting the connection times out into a loud failure rather than hanging the lane. |

Two things are load-bearing:

- **`permissions: id-token: write` on the job.** This is what lets the runner request
  a short-lived OIDC token proving *which workflow, on which repo, at which commit*
  produced the push (GitHub Actions OIDC). The hub verifies that token's signature,
  issuer, audience, expiry, and that its `repository` is a connected store — trust comes
  from the pipeline's identity, never a shared secret. Omit the permission and the Action
  fails loudly at the mint step; it never falls back to pushing anonymously.
- **The repository must be a connected store on the hub.** The hub ingests only for the
  repos it mirrors (its `[oidc].repositories`); a token from any other repo is rejected
  `403`. Configure the store on the hub first (the `[[stores]]` and `[oidc]` sections),
  then point the Action at it.

### It fails loudly, never a stale green

The Action **fails the CI step on any non-2xx from the hub**, printing the hub's
rejection reason (the `{"error": "..."}` body) into the log — a rejected or broken push
never passes as green (invariants #1 and #6). A `403` for an unconnected repo, a `401`
for a wrong audience or a bad signature, a `400` for a claim the hub has not synced, or
a `503` when the identity provider is unreachable each fail the step with the reason
named, so a hub silently dropping telemetry can never masquerade as a healthy lane.

The same holds when the hub does not answer at all. A refused connection fails the step
immediately; a hub that accepts the connection then stalls (a slow-loris or half-dead
hub) is bounded by `max-time` (default 60 seconds) and fails with `the hub did not
respond within Ns (timed out)` — never an indefinite hang to the runner's wall-clock. A
`2xx` is also not trusted on its face: the Action requires the hub's accepted-envelope
JSON (`{"accepted": N}`) before declaring success, so a proxy interstitial or CDN page
returning `200` with a non-JSON body fails loudly rather than reading as a phantom
acceptance.

A **drift is not a failure** of the push: `claim check` exiting `1` (drifted) or `2`
(broken) is exactly the telemetry the hub exists to receive, so the Action pushes the
report regardless of the check's verdict and succeeds when the *ingest* is accepted. What
fails the step is the ingest itself being refused — not a drift the report faithfully
carries. A **redelivery** (the same run re-run) dedups on the hub to the original success,
so a retried lane never double-counts.

### The core is testable off a runner

The Action's logic — check, obtain the token, POST, interpret the response, fail loud on
a non-2xx — lives in `ci/hub-ingest.sh`, and the OIDC-token *acquisition* is
parameterized so the flow runs without a real runner: the Action's YAML mints the token
(the one runner-specific step) and hands it to the script through `HUB_INGEST_TOKEN`; a
test injects a token the local hub accepts through the same seam. The gate exercises the
whole flow against a locally-served hub with a mocked identity provider
(`crates/claim-hub/tests/ingest_action.rs`) — proving a valid push succeeds, a drifted or
broken verdict is still pushed as telemetry, an ingest rejection fails the step with the
hub's reason, no non-2xx is ever swallowed, and a hub that refuses the connection or never
responds fails the step loudly (the latter via `--max-time`) rather than hanging — with no
network to GitHub.

## The read API (`GET /api/…`)

The read API is the hub's agent-and-human read surface: it derives standing, freshness,
and due-ness over the live store and serves them as JSON, **every response carrying its
*as-of*** — the exact inputs the answer was computed from. This is the read half of the
loop: the CLI reports a verdict, the ingest gate lands it on the ledger, and these
endpoints derive what those verdicts (plus the clock) *mean* right now.

Every route is a **read**: it computes at read time and stores nothing (invariant #3), so
a standing can never disagree with the evidence, and a read never appends an event. Reads
are **deterministic** — the same (`ledger_head`, `registry_version`, `clock`) always
yields byte-identical bytes — so an agent can cache, diff, and resume. Every `/api` route
requires a `read`-scoped bearer token unless the hub explicitly opens reads (`open_reads`);
see [Read authentication](#read-authentication). The `curl` examples below assume the
`open_reads` demo (or an equivalent trusted-network setup); against an authed hub, add
`-H "Authorization: Bearer <token>"` and a missing or unscoped token is a `401`/`403`.

| Endpoint | Returns |
|---|---|
| `GET /api/claims/{id}` | One claim's derived standing, with its as-of. |
| `GET /api/claims?path=&store=&standing=&supports=` | The live set, filtered; each claim with its standing. |
| `GET /api/drifted` | Every claim whose latest standing is `drifted`. |
| `GET /api/due` | The review queue: every drifted, stale, or due-for-recheck claim. |
| `GET /api/suspect` | Every `suspect` claim (populated once the propagation rule lands). |
| `GET /api/skips` | Every skipped check, ranked into the review queue by age and lapsed `until`. |
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

### The ranked skip queue (`GET /api/skips`)

A **skip** is a check deliberately not run — an acknowledged, bounded debt, never a pass. A
skipped check records no verdict, so it never makes a claim `verified` (a claim whose only
check is skipped is `stale`, never a green — invariant #4/#6); the skip is surfaced for a
human to look at, not folded into the standing. `GET /api/skips` ranks every skipped check
across the live set into the review queue, so the loudest debts rise to the top:

```sh
curl -s http://127.0.0.1:8080/api/skips
```

```json
{
  "skips": [
    { "store": "github.com/acme/payments", "claim": "payments/parked",
      "check_digest": "a1ce…4754", "reason": "no model runner in CI",
      "until": "2026-01-01T00:00:00Z", "lapsed": true },
    { "store": "github.com/acme/payments", "claim": "payments/parked",
      "check_digest": "3de4…2755", "reason": "dashboard behind a flag",
      "until": "2027-06-01T00:00:00Z", "lapsed": false },
    { "store": "github.com/acme/billing", "claim": "billing/muted",
      "check_digest": "1424…e197", "reason": "unbounded mute", "lapsed": false }
  ],
  "as_of": { "ledger_head": 5, "registry_version": 2, "clock": "2026-07-20T00:00:00Z" }
}
```

The ranking rule (`claim_hub_core::rank_skips`, a pure function of the read model) is a
total order, so the set is deterministic and every read surface renders the identical
sequence:

1. **A lapsed skip outranks a not-yet-lapsed one.** A skip whose `until` is at or before the
   read clock has *lapsed* — the deferred check is due again (the router routes it as a
   `lapsed-skip` transition) — so it leads, no matter how soon a not-yet-lapsed skip expires.
   Each skip carries its `lapsed` flag as of the read clock.
2. **Among lapsed skips, the one that lapsed *longer ago* leads** (ascending `until`) — the
   oldest debt is loudest.
3. **Among not-yet-lapsed skips, the one *nearer its expiry* leads**, and an **indefinite
   skip** (no `until`) sorts **last** — an unbounded mute is surfaced plainly (it omits the
   `until` field) so it cannot hide, but it is the least time-pressing since it will never
   lapse.

A ranked skip is **queue data, not a verdict**: the shape carries no `standing` and no
`verdict`, and reading `/api/skips` changes no claim's standing. The set carries one shared
`as_of`; an empty result is an empty `skips` array with a truthful as-of, never a fabricated
entry. The same ranked set is served by the MCP `skips` tool and rendered in the review-queue
page (`/ui/queue`) and its twin — one pure ranking, four surfaces, which cannot disagree.

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

The `statement` and `checks` resolve from git at `commit` — the sha the claim was read at.
The `standing`, `history`, and `as_of` all come from the one derived model, so the trust
judgment is stamped with exactly the inputs it derived from; the descriptive `statement`,
`checks`, `commit`, and `supports` are the registry's current rendering of the claim,
normally at that same `registry_version` and at most one sync ahead — a claim can read
current-or-newer than its `as_of`, never more verified than its `standing`. The
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

## The router and the nag (`GET /api/nags`)

The read surfaces answer "what is the state right now." The **router** is the part that
*notices when a fact stops holding and routes it to whoever owns the decision* — the whole
reason the hub exists (invariant #6, the nag over time the stateless CLI cannot issue). It
watches three **transitions**:

- **`drifted`** — a claim's latest verdict says the fact is false now;
- **`stale`** — a claim aged past its freshness window with **no new verdict**, purely by the
  clock crossing its `max-age` (a certificate expiring); and
- **`lapsed-skip`** — a check's skip declared an `until` that has now passed, so the deferred
  check is due again.

A background tick re-derives the read model on a cadence (`[router].period_secs`, default 300)
and fires each **new** transition exactly once. This is what catches a fact aging into stale:
there is no new verdict to trigger on, so only the tick notices.

**Fire-once is derived from the ledger, not stored.** When the router fires, it appends a
`nag` event to the same append-only ledger the verdicts live on. "Already nagged" is then
*derived* by diffing the current transitions against those `nag` events — there is no mutable
"notified" flag anywhere (invariant #3), so a hub restart re-derives the identical set from
the ledger and never re-nags. A `nag` event is the hub's own scheduling telemetry: it carries
no verdict and no single check, so it can never be read as a verdict (invariant #4).

**Owners resolve from CODEOWNERS at fire time**, read from the synced git mirror — never a
stored owner field (invariant #3), and never a forge call. The match is against the claim's
**real synced path** (a standalone file's own path, or an embedded claim's host file), not a
path guessed from the id — a claim's id does not fix where its file lives, so guessing would
route to a directory owner the claim never belongs to. Because the path is real and the
matcher is GitHub's last-matching-pattern-wins rule — the same path and rule the CI glue
uses — a hub-side owner and a glue-side owner never disagree. **One commit breaking N claims
is one grouped nag**, not N (the drift groups on the breaking commit). **A transition with no
resolvable owner is a dead-letter** — first-class in the queue, visible, never silently
dropped (invariant #6). Owners are re-resolved at read time, so a claim that dead-lettered
for lack of an owner is delivered once an owner appears, and a re-owned claim shows its new
owner — without either re-firing (the fire key does not depend on the owner).

The hub **renders** nag content and serves it; the CI glue **delivers** it (the standing issue
and PR comments). The hub holds no forge write credential in v1. `GET /api/nags` is what the
glue pulls — a **read** that resolves owners and reports the queue but fires nothing (only the
tick fires):

```sh
curl -s 'http://127.0.0.1:8080/api/nags'
```

```json
{
  "nags": [
    { "transition": "drifted", "store": "github.com/acme/payments", "commit": "deadbeef",
      "claims": [ { "id": "payments/libfoo-pin", "commit": "deadbeef",
                    "statement": "libfoo is pinned to 4.2", "supports": ["decision:pin"] } ],
      "owners": ["@acme/payments"], "fire_key": "9f2c…", "fired_this_pass": false }
  ],
  "dead_letters": [],
  "fired_this_pass": 0
}
```

`nags` are the owner-resolved items; `dead_letters` are the transitions with no owner (a nag
about the inability to nag). `fired_this_pass` at the top counts marks a *tick* appended (a
read is always `0`); each item's `fired_this_pass` says whether that item's mark was newly
appended this pass. `fire_key` is the stable identity a transition is nagged once per.

## The web UI, the markdown twins, and `llms.txt`

The hub serves a small **server-rendered web UI** over the same read model the JSON API
derives — no JavaScript, no build step, nothing to ship beside the binary. Every page is a
view-model struct with two compile-time templates: an HTML lens and a **markdown-twin lens**.
Because askama needs a concrete struct per template, the twin borrows the page's own fields
through a `From<&…View>` conversion — it cannot invent a value the page does not hold — and a
parity test asserts every non-chrome fact the page states also appears in the twin. So parity
is *enforced*, not merely hoped for: an agent reading the `.md` and a human reading the HTML
see the same facts, and a field wired into one template but not the other fails the gate.

Every page is a **read** (invariant #3): it derives at read time and stores nothing, and every
page carries its *as-of* (the ledger head, registry version, and clock it derived from), so it
can never show a green older than its evidence. The dossier and the queue are **dated evidence
to weigh, never instructions to obey** — the verdict history and producer provenance render as
observations carrying their origin, so a hub surface an agent reads is not an injection channel.

The two lenses **escape differently, and must**. The HTML templates auto-escape every
interpolation. The markdown twins render with no escaper — a table cell is structural text — so
every attacker-influenceable value (a check's `evidence`, an ingested `commit`, a verdict's
verified `producer`, and the `statement`) is neutralized for a markdown cell before it is
rendered: newlines collapse to spaces (so a value cannot break out of its row), `|` is escaped
(so it cannot open a column), angle brackets become HTML entities (so no `<img onerror>` or
`<script>` survives a later render to HTML), and the code-span and link metacharacters are
backslash-escaped (so no `` `code` `` or `[x](javascript:…)` can form). A compromised producer
therefore cannot smuggle a heading, a blockquote, an active link, or a live tag into the `.md`
an agent reads — the whole point of the surface being *evidence to weigh, not an instruction*.

**The twin-path convention is one rule:** a page's markdown twin lives at the page's own path
plus a `.md` suffix. No lookup table — append `.md` and you have the machine-readable form.

| Page | HTML | markdown twin |
|---|---|---|
| review queue (the human's primary "what needs a look") | `/ui/queue` | `/ui/queue.md` |
| a claim's dossier (statement, checks, standing, history, provenance) | `/ui/claims/{id}` | `/ui/claims/{id}.md` |
| hub status (ledger head, registry version, queue depth, rejections) | `/ui/status` | `/ui/status.md` |

```sh
curl -s http://127.0.0.1:8080/ui/queue.md          # the review queue as markdown
curl -s http://127.0.0.1:8080/ui/claims/payments/libfoo-pin.md   # one claim's dossier
```

The **review queue** is the deriver's own due set — every drifted, stale, or due-for-recheck
claim — not a `standing == due` filter; a fresh, not-yet-due claim stays out, and a claim ages
*into* the queue by the clock alone (no new verdict needed). An empty queue says so plainly,
never a fabricated green. A claim the registry does not hold is an honest `404`.

Below the claim rows, the queue page renders the **ranked skip section**: every skipped check
across the live set, ordered by the same rule as `GET /api/skips` — lapsed first, then by
`until` (oldest lapse and nearest expiry first, indefinite last) — each marked lapsed or
within-window. It is the *same ranked set* the API and the MCP `skips` tool serve (one pure
ranking, rendered by both the HTML page and its twin), so the four surfaces cannot disagree.
A skip is a debt surfaced for a look, never a pass: an empty skip section says "every check is
running" plainly.

**`/llms.txt`** is the agent's index of the whole hub: it names every surface — the JSON API
endpoints, the UI pages, the markdown twins, and the ingest write path — with the twin-path
rule, so an agent discovers where to look in one fetch rather than crawling.

```sh
curl -s http://127.0.0.1:8080/llms.txt
```

## The hub MCP (`/mcp`)

The hub MCP is the **agent binding**: a small, bounded set of read-only tools an agent
session actually asks for, served over the [Model Context Protocol](https://modelcontextprotocol.io)
so any MCP-capable agent can call them. It is **mounted in-process on the same app** as the
JSON API — one binary, one port (`/mcp`), one middleware stack — using rmcp's streamable-HTTP
transport in stateless JSON mode (a tool call is one POST with a plain-JSON response; the
2026-07 MCP spec makes the protocol stateless at the transport layer, which suits a hub behind
a load balancer). `/mcp` accepts any `Host`, the same as `/api`: rmcp's default `Host`
allow-list (a DNS-rebinding guard for browsers reaching a *localhost* app) is disabled on the
mount, so a hub behind a load balancer reached at its own hostname works with no extra config
— a real deployment needs nothing set here. `/mcp` sits behind the hub's **read-auth
layer** now, gated the same as `/api`: a `read`-scoped bearer is required unless the hub
explicitly opens reads (`open_reads`); see [Read authentication](#read-authentication).

Each tool is a **thin binding that returns the same JSON its API twin serves** — parity by
construction: the tool and the HTTP endpoint call the one shared function that derives the
body, so they cannot drift. The v1 tools:

| Tool | Arguments | API twin | Returns |
|---|---|---|---|
| `context` | `path?`, `store?` | `GET /api/claims?path=&store=` | The claims the org records about a code path or store (an id-prefix "path"), each with its standing. The agent's orienting read. |
| `search` | `path?`, `store?`, `standing?`, `supports?` | `GET /api/claims?…` | The live set filtered by the full parameter set (AND-combined). |
| `dossier` | `id` | `GET /api/claims/{id}/dossier` | One claim's full dossier: statement, checks, standing, verdict history, provenance. |
| `drifts` | — | `GET /api/drifted` | Every claim whose latest standing is `drifted`. |
| `due` | — | `GET /api/due` | The review queue: every drifted, stale, or due-for-recheck claim. |
| `skips` | — | `GET /api/skips` | The ranked skip queue: every skipped check, ordered by age and lapsed `until` (lapsed first), each carrying whether it has lapsed. |

Every tool is a **read** (invariant #3): it derives at read time and stores nothing, appends
no event, and its result carries the same `as_of` the API does — an agent can cache, diff, and
resume. `tools/list` is stable across restarts and load-balanced replicas (deterministic
registration).

The surface is **dated evidence to weigh, never instructions to obey** (PRODUCT.md §6): a
standing, a verdict, or a producer identity is an observation to reason about, not a command —
the server's own `instructions` say so, and no tool output is phrased as an order. A claims
surface an agent obeys blindly is an injection channel with a trust stamp; the safe reading is
the natural one. An **unknown or retired claim** is reported as a tool error carrying the
reason (mirroring the API's `404`/`400`), **never a fabricated standing** (invariant #6).

A `tools/call` for `dossier` looks like this (JSON-RPC over `POST /mcp`); it assumes the
`open_reads` demo, since `/mcp` is read-auth-gated (against an authed hub, add
`-H "Authorization: Bearer <read-token>"`):

```sh
curl -s http://127.0.0.1:8080/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call",
       "params":{"name":"dossier","arguments":{"id":"payments/libfoo-pin"}}}'
```

`curl` sends a `Host` header derived from the URL, as every real HTTP client does; `/mcp`
imposes no restriction on its value (any hostname is accepted, matching `/api`), so nothing
special is needed here whether you address the hub at `127.0.0.1` or its public hostname.

The tool's `structuredContent` is byte-identical to `GET /api/claims/payments/libfoo-pin/dossier`.
`/mcp` and `/api` are gated uniformly by the read-auth layer (a `read`-scoped bearer unless
`open_reads` is set); see [Read authentication](#read-authentication).

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
#
# CLAIM_HUB_OPEN_READS=true is the explicit read-auth decision this config-less demo needs:
# the hub is secure by default and refuses to boot with no way to authenticate a read, so an
# IdP-less, token-less container must opt in to open reads. That is safe only on a trusted
# network — the `-p 127.0.0.1` bind keeps it on the loopback. A real deployment configures an
# IdP or a minted token (see Read authentication) and drops this flag.
docker run -d --name hub \
  -p 127.0.0.1:8080:8080 \
  -v hub-data:/data \
  -e CLAIM_HUB_OPEN_READS=true \
  claim-hub:local

curl -s http://127.0.0.1:8080/status   # => {"ledger_head":0,"registry_version":0,"rejection_count":0}
```

The image sets `CLAIM_HUB_LISTEN=0.0.0.0:8080` and `CLAIM_HUB_DATABASE=/data/hub.db` so it
binds a reachable address and lands its database on the mounted volume with no config file at
all. It bakes in **no** read-auth decision, so the operator must make one: `CLAIM_HUB_OPEN_READS`
above, or a `hub.toml` with an `[read_auth.issuer]` or `[[read_auth.tokens]]` entry. To
configure connected stores and the ingest gate's `[oidc]` trust, drop a `hub.toml` into the
mounted volume (see [Configuration](#configuration) above) — env overrides still win, so the
address and database path stay correct regardless.

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
