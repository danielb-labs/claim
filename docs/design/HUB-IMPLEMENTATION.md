# The hub — implementation plan (stack and build order)

Status: draft plan, July 2026. This is the *how* to `HUB.md`'s *what*: the
concrete stack, and a dependency-ordered work breakdown a fleet of
implementation agents builds against. It does not re-litigate the architecture —
the spine is `answer = derive(registry, ledger, clock, config)`, the components
are HUB.md §3, the boundary is `CLI-HUB-BOUNDARY.md`. Where a stack choice is
consequential, it is flagged for veto (§3) before any agent starts. Technology
claims were verified against current (July 2026) releases and practice; sources
in the appendix.

CLAUDE.md binds hub code exactly as it binds CLI code: correctness first, the
golden invariants (here especially #3 derive-don't-store, #4
verdicts-are-telemetry, #6 nag-never-a-lie, and HUB.md's
customer-owns-their-data), one-line justification per dependency, branch → gate
→ two adversarial reviews → merge, docs shipping with the behavior they
describe, and tests where time is a parameter and the network never appears.

## 1. The stack

### 1.1 Language: Rust

The hub is Rust, for the CLI's three reasons and one new one that settles it.

The CLI's reasons carry over whole. Correctness-first: the deriver is invariant
#3 made executable — standing, freshness, and due-ness as exhaustive `match`
over enums, where a forgotten case is a compile error, not a silent green.
Single static binary: the customer-owns-their-data invariant (HUB.md §1, §4)
makes self-hosting the default shape, and a hub that deploys as one binary plus
one database file is a hub a customer actually runs; a runtime-heavy stack
taxes self-hosting until only the hosted offering is practical, which erodes
the invariant by attrition. Performance is trivially sufficient either way.

The new reason: **the registry must parse `.claims/` with the same parser the
CLI uses.** `claim-core` owns the claim grammar — frontmatter, embedded blocks,
`hub:` hints, `Days`, skip syntax. A hub in any other language reimplements
that grammar, and two parsers of one format disagree eventually; a registry
that reads a claim differently than the verifier that checks it is exactly the
silent rot this product exists to kill. Reusing `claim-core` makes the
divergence impossible rather than policed.

The alternative worth naming: **Go** (single binary, mature server ecosystem,
faster onboarding for web work) or **TypeScript** (fastest UI iteration). Both
fail the parser-reuse test, both give up the enum-and-`Result` enforcement the
invariants lean on, and both add a second toolchain to a one-gate repo.
Rejected. Veto point 1 in §3.

### 1.2 Workspace shape: three new crates, same workspace

```
crates/claim-hub-core    the hub domain: event envelope, report parsing,
                         standing model, derive() — pure, no IO, no async
crates/claim-hub-store   storage: Ledger and Registry traits, SQLite impl,
                         migrations, registry sync (git mirror + claim-core parse)
crates/claim-hub         the binary: axum app, ingest gate, surfaces,
                         router loop, config
```

Same repo, same Cargo workspace, not a separate repo. Three forces, all
pointing the same way: registry sync depends on `claim-core`'s parser (the
point of 1.1), and an in-workspace path dependency means a grammar change and
the registry that reads it land in one branch under one review; the ingest
contract (the CLI's `--json` is the wire) gets a real cross-crate test — run
the in-tree `claim` binary, parse its output with the hub's ingest types —
that a separate repo can only approximate with fixtures; and `scripts/check.sh`
gates the hub with zero new machinery. The mirrored discipline of the layering
is deliberate: `claim-hub-core` is to the hub what `claim-core` is to the CLI —
where correctness lives, where tests are densest, and where no process,
network, or terminal concern may leak.

Alternative: a separate `claim-hub` repo consuming `claim-core` as a published
crate. Cleaner release independence, but it buys that with a publishing cadence
and a version skew between the CLI's grammar and the hub's — coordination tax
now, drift risk forever. Revisit only if the hub's release cadence must diverge
from the CLI's. Veto point 2 in §3.

The dependency direction is one-way: hub crates depend on `claim-core` (parser,
`Verdict`, `Timestamp`) and reuse `claim-store`'s store-loading over a mirror
checkout. The CLI never depends on a hub crate. The CLI stays hub-agnostic per
`CLI-HUB-BOUNDARY.md`.

### 1.3 HTTP and runtime: axum on tokio

**axum** (0.8.x) is the API framework. It is the 2026 pragmatic default for new
Rust services: maintained by the tokio team, routing and extraction with native
async traits, and middleware as composable `tower` layers — which matters here
because auth (OIDC at ingest, bearer scopes on reads), tracing, and timeouts
each become one testable layer instead of framework-specific plumbing. The
deciding fit: the MCP surface. `rmcp`'s streamable-HTTP server is a tower
service that mounts into an axum router, so the JSON API and the hub MCP are
one process, one port, one middleware stack — the "one substrate" principle
(HUB.md §5) realized in the process topology.

**tokio** is the runtime — axum's and rmcp's shared substrate, and the one
async runtime this workspace has already justified (item 07's MCP work).

Alternative: **actix-web**, which still wins raw-throughput benchmarks by
10–15%. The hub's write volume is verdict ingest from CI lanes — tens of events
per push, not ad-serving — so that margin buys nothing, and actix's separate
lineage forfeits the tower/rmcp composition. Rejected.

### 1.4 Storage: SQLite behind traits, via sqlx; Postgres is a later impl, not a rewrite

**v1 stores everything in one SQLite file, WAL mode.** This is the
data-ownership invariant made physical: the ledger and registry are one file on
storage the customer controls — export is `cp`, wholesale delete is `rm`, and
there is no database server for the product to operate on the customer's
behalf. It is also the single-binary story: nothing to install beside the hub.
Production SQLite in 2026 is a settled practice, not a compromise: WAL mode
serves concurrent readers against a single writer at volumes orders of
magnitude past a hub's (a hub ingests per CI run and serves reads; NVMe-era
SQLite handles ~10⁴–10⁵ writes/sec and far more reads), and **Litestream**
streams the WAL to customer-controlled S3-compatible storage for near-real-time
backup — an operational sidecar, not a code dependency.

**The access layer is sqlx** (0.8.x): async (fits axum), compile-time-checked
SQL against the actual schema — correctness-first for queries; a typo'd column
is a build failure, not a wrong answer at read time — with migrations embedded
in the binary via `sqlx::migrate!` (self-host: the binary creates and upgrades
its own database), and the same crate speaks Postgres.

**The seam is a pair of traits in `claim-hub-store`:** `Ledger` (append,
scan-from-cursor, head position — deliberately no update and no delete, so
append-only discipline is unrepresentable to break from Rust) and `Registry`
(replace-store-snapshot, claim and supports queries). The deriver consumes
plain data from these traits and never sees SQL. HUB.md's v1-self-hostable /
later-multi-tenant split maps onto this seam directly: the hosted offering
implements the same two traits over Postgres — sqlx again, second driver — and
nothing above the trait changes. Defense in depth below the trait: SQLite
triggers raise on any `UPDATE` or `DELETE` against the events table, so even a
future bug reaching around the trait cannot rewrite history silently.

Schema shape, v1:

- `events` — append-only: monotonic `seq` (the ledger cursor), `kind`,
  `claim_id`, `check_index`, `check_digest`, `verdict`, `evidence`, `commit`,
  `store`, `producer` (the verified identity block, JSON, verbatim),
  `reported_at`. Unique index on (producer run, claim, check identity) — the
  dedup rule of HUB.md §2; a redelivered push hits the index and returns the
  original success.
- `stores`, `claims_at_tip`, `supports_edges` — the registry: each claim at
  the default-branch tip with the commit it was read at, plus the cross-store
  supports index (#10's substrate). The registry is derived data: a version
  counter marks each sync, and wipe-plus-resync is a supported, tested
  operation.

Alternatives: **rusqlite** — the most mature SQLite binding, but synchronous
(every call wrapped in `spawn_blocking` under axum) and it buys no Postgres
path, so "later" becomes a rewrite; **Diesel** — compile-time safety via a
query DSL, but the DSL abstraction earns nothing over an append-only log and a
handful of indexed reads, where plain checked SQL is the clearer artifact;
**Postgres-first** — operational weight on every self-hoster to serve a scale
v1 does not have, weakening the ownership default. All rejected for v1. Veto
point 3 in §3.

### 1.5 The deriver: pure functions, std-only memoization

The deriver lives in `claim-hub-core` as pure functions: registry snapshot,
ledger events, a `jiff` timestamp, and config in; the read model — per-claim
standing with its as-of, the due set, the queue, skip ages — out. No async, no
IO, and the clock is always a parameter (CLAUDE.md's determinism rule; the
tests set time explicitly). The bad-news-dominates join is an exhaustive
`match` over per-check latest states, and the property tests assert its shape:
no combination of events manufactures a green, a shallow check's pass never
clears a deep check's drift, `broken` counts against freshness exactly like
never-checked.

Memoization is a cache, never a store (invariant #3): one in-process slot
behind `RwLock`, keyed by (ledger head seq, registry version, config hash) —
exactly two of HUB.md's three invalidation causes. The third, the clock
crossing a threshold, needs no timer: each derivation records the earliest
future instant at which any of its answers changes (the soonest max-age expiry
or due threshold), and a read at or past that horizon recomputes. Nothing runs
for a claim to become stale; the next read reports it, the way a certificate
expires. No cache dependency: at v1 volume a full derivation is milliseconds
over thousands of events, and std suffices. (`moka` is the named alternative
if a profile ever disagrees; adopt it then, behind the same function.)

### 1.6 Registry sync: system git, claim-core's parser

Sync maintains a bare mirror per connected store — clone and fetch by shelling
out to the system `git` binary, the same choice `claim-store` already makes for
provenance (`Command::new("git")`), so the workspace has one way of talking to
git and no libgit2 C linkage or `gix` API surface to audit. Reading a store at
its tip reuses `claim-store`'s loading over a checkout, which parses with
`claim_core::parse_claim_file` and `extract_embedded_claims` — one grammar for
every front door. A claim absent at the new tip is a retirement: dropped from
the live set, history still renderable from git and the ledger. Malformed claim
files at a tip are surfaced as sync findings (a nag is owed), never silently
skipped.

v1 triggers: an interval poll plus an authenticated manual-resync endpoint.
Forge webhooks are a later trigger optimization behind the same sync entry
point, not a semantic change. The `git` binary is the one runtime dependency
beside the hub binary itself; the container image ships it, and the bare-metal
doc says so.

### 1.7 Ingest gate: jsonwebtoken + a cached JWKS

The single telemetry write path (HUB.md §3) is one axum route. Its auth
middleware verifies the GitHub Actions OIDC id-token: signature against the
issuer's JWKS (`https://token.actions.githubusercontent.com/.well-known/jwks`,
fetched with `reqwest`, cached, refreshed on unknown `kid`), then `iss`, `aud`
(the hub's own configured identifier), `exp`, and that the token's `repository`
is a connected store. The verified claims — issuer, repository, workflow, ref,
run id, sha — are recorded verbatim into the event's `producer` block, per
HUB.md §4: the trust judgment stays re-derivable, not made once at the door.

The verification stack is **jsonwebtoken** (the standard, actively maintained
Rust JWT library) plus a JWKS cache we write ourselves — a page of code. The
thin wrapper crates that exist for this exact job (`github-oidc`, `git-oidc`)
were considered and rejected: they are low-adoption single-purpose shims over
the same two layers, and the trust root of the entire ledger is the one place
where the approved-deps rule bites hardest — two widely-audited dependencies we
can read end-to-end beat a convenience crate we cannot. Veto point 5 in §3.

The envelope is validated against serde types in `claim-hub-core` that parse
the CLI's `--json` report **as a wire format** — deliberately not shared Rust
types with the CLI. The hub ingests from many repos running many CLI versions;
it must parse what is on the wire and reject with a reason naming the field,
and a shared type would only prove the in-tree CLI matches itself. The
workspace contract test (item hub-01) runs the built `claim` binary and parses
its real output, which keeps the two ends honest without coupling them.
Rejections are loud twice: a 4xx with the reason to the pusher, and a counted,
queryable rejection record on the hub — a hub silently dropping telemetry would
age claims into staleness with nobody told why (invariant #6).

### 1.8 Scheduler: a projection, not a process

Per HUB.md §3, the v1 scheduler dispatches nothing. The due set is a deriver
projection published through every read surface, and the scheduled CI lane in
each repo stays a dumb cron. The only recurring task in the hub is a tokio
interval tick that wakes the router to notice clock-crossing transitions —
no cron dependency, no job queue.

### 1.9 Router / nag: transitions from the ledger, owners at fire time

The router consumes derived transitions — a claim entering drifted, crossing
into stale, a skip's `until` lapsing. Transition detection must survive
restarts without double-firing, so delivery marks are themselves events: the
router appends a `nag` event (producer: the hub's own principal, via the
`Ledger` trait — not through the HTTP ingest gate, which remains
telemetry-only) recording what fired for which derived state. This is HUB.md
§2's wider event grammar doing its job: whether a nag already fired is
*derived* from the ledger like everything else, auditable the same way, no
mutable "notified" flag anywhere.

Owner resolution happens at fire time from CODEOWNERS in the registry mirror —
already local, no forge call, never a stored owner field (invariant #3).
Grouping keys on the envelope's commit: one refactor breaking twelve claims is
one item. A transition with no resolvable owner is a dead-letter queue item,
first-class in the read model — a nag about the inability to nag.

Delivery, v1: the forge surface the CI glue already maintains (HUB.md §3). The
hub *renders* nag content and serves it (JSON and markdown, like everything
else); the scheduled lane's glue — the `ci/render.mjs` lineage — pulls the
due-and-drifted view and upserts the standing issue and PR comments. The hub
holds **no forge write credential in v1**, which keeps the smallest credential
surface on the component that stores the ledger. Direct hub→forge delivery
(and chat, and escalation ladders) is a later route behind the same transition
stream. Forge *reads* the mirror cannot answer — PR approvals for dossier
provenance — use `reqwest` against the GitHub REST API with a read-scoped
token, typed by hand for the two or three endpoints v1 needs; `octocrab` is
the named alternative when the forge surface grows past that.

### 1.10 Surfaces: one read model, four renderings

**JSON API.** axum handlers over the deriver. Every response carries its as-of
— ledger seq, registry version, and the clock instant used — so the hub can
never show a green older than its evidence, and an agent can cache, diff, and
resume. Reads are deterministic: same (cursor, registry version, clock), same
bytes. Subscriptions are the cursor feed of HUB.md §5: the ledger and the
derived-transition stream, pollable from a position.

**Hub MCP (#32).** `rmcp` — the official MCP Rust SDK, already the workspace's
known quantity from the CLI's since-removed local server — serving the
streamable-HTTP transport, mounted in-process on the axum router. The
2026-07-28 MCP spec makes the protocol stateless at the transport layer, which
suits a hub that may later sit behind a plain load balancer; rmcp tracks the
spec so the hub does not. v1 tools are read-only and few, per HUB.md §5:
`context`, `dossier`, `drifts`, `due`, `search` — each a thin binding
returning the same JSON the API serves, schemas derived via `schemars` as the
MCP work already does.

**Markdown twins and `llms.txt`.** Every page is one view-model struct
rendered by two **askama** templates — `page.html` and `page.md` — so the twin
is structurally incapable of drifting from the page: same struct, two lenses,
both snapshot-tested. The twin lives at the page's own path with an `.md`
suffix; `/llms.txt` indexes the surfaces; `/status` is the machine-readable
health-and-position endpoint (ledger head, registry version, last sync,
rejection count).

**Web UI.** Server-rendered askama over the same view models — v1 is the queue
and the claim dossier, nothing more. askama compiles templates into the binary
(type-checked against the view model at build time — a renamed field is a
compile error, not a blank cell; and nothing to ship beside the binary). No
SPA, no JavaScript build chain: a second toolchain is attack surface, a second
gate, and a standing invitation for the UI to grow state of its own against
the one-substrate rule. Plain CSS via `include_str!`. Alternatives:
**minijinja** (runtime templates, hot reload — better template-editing DX,
but templates become runtime data that can fail at request time, and the
binary grows a loader); **maud** (HTML-from-Rust macros, compile-time too, but
its HTML-shaped API has no natural text/markdown mode, which forfeits the
nearly-free twin). Veto point 4 in §3.

### 1.11 Auth for reads and acts: OAuth 2.1 resource server, scoped tokens

Ingest trust is §1.7. For the read surfaces and the (v1-minimal) act surface,
the hub is an **OAuth 2.1 resource server**, per the MCP authorization spec:
it validates Bearer JWTs against a configured issuer's JWKS — the customer's
IdP acts as the authorization server — and publishes RFC 9728
protected-resource metadata so MCP clients discover where to get tokens. This
reuses the exact jsonwebtoken-plus-JWKS machinery of ingest with a different
trust anchor. For self-hosters without an IdP, the fallback is hub-minted
scoped API tokens (`read` broadly, `act` narrowly), stored hashed. Static
tokens are rejected for *ingest* because a forged verdict poisons the ledger;
a static *read* credential risks disclosure, not forgery — a different class,
acceptable as a floor. Every act lands in the ledger attributed to its
principal. Whether a v1 hub defaults to open reads inside a private network or
authed-everything is a config policy flagged for human decision (§4.5).

### 1.12 Config and observability

One TOML file plus environment overrides, deserialized with serde via the
`toml` crate: connected stores, OIDC trust (allowed repositories, audience),
per-hub overrides of `hub:` hints, read-auth policy, listen address, database
path. Config is an input to `derive()` — its hash keys the memo — so a config
change invalidates derived answers like any other input change. Diagnostics
use `tracing` + `tracing-subscriber`, the tokio-ecosystem standard, giving
span context through ingest → append → derive without hand-rolled logging.

### 1.13 Packaging and deployment

One `claim-hub` binary. SQLite compiles in via sqlx's bundled
`libsqlite3-sys`, so the binary is self-contained (musl target where feasible;
the one honest caveat to "static" is the `git` binary registry sync shells to).
Shipping shapes: the bare binary, and a small container image (binary + git +
CA certs) with a compose example mounting one volume for the database file.
The self-host doc states the ownership mechanics as operations: back up by
Litestream or file copy, export by copying the file, leave by taking it —
tested by a scripted backup-and-restore exercise, not asserted.

### 1.14 New dependencies (the approved list)

Each enters the workspace with this one-line justification in its crate's
`Cargo.toml`; anything not listed here needs its own case in review.

| Crate | Justification |
|---|---|
| `axum` | The API framework: tokio-team maintained, tower-composable middleware, native async traits; rmcp mounts into it. |
| `tokio` | The async runtime axum and rmcp both serve on; already justified for the workspace in item 07. |
| `tower`, `tower-http` | Middleware as testable layers: auth, tracing, timeouts, static assets. |
| `sqlx` | Async SQL with compile-time-checked queries and embedded migrations; one crate covers SQLite now and Postgres at the trait swap. |
| `reqwest` (rustls) | The one HTTP client: JWKS fetch and the few forge read endpoints; rustls avoids linking system OpenSSL. |
| `jsonwebtoken` | JWT verification for OIDC ingest and OAuth 2.1 bearer reads; the standard, actively maintained Rust implementation. |
| `askama` | Compile-time, type-checked templates embedded in the binary; text templates make the markdown twins one struct with two renderings. |
| `rmcp` | The official MCP Rust SDK; owns protocol framing and the streamable-HTTP transport so the MCP surface stays a thin binding. |
| `schemars` | Derives MCP tool input schemas from request types, as the CLI's MCP work already did. |
| `sha2` | The canonical check-digest (hub-01): SHA-256 over a check's canonical definition, so a check's identity is collision-resistant and stable across CLI versions. Audited, pure-Rust RustCrypto; chosen over a non-cryptographic hash (unstable across versions/platforms, no collision resistance). |
| `toml` | Deserializes the one config file; serde-native, tiny. |
| `tracing`, `tracing-subscriber` | Structured spans through ingest → append → derive; the tokio-ecosystem standard. |

Carried over, not new: `serde`/`serde_json`, `thiserror`/`anyhow`, `jiff`
(every hub timestamp is `claim_core::Timestamp`), `insta`, `tempfile`,
`assert_cmd`. Test HTTP goes through `tower::ServiceExt::oneshot` against the
axum app in-process — no test-server dependency, no network in tests.

## 2. What the invariants pin in the implementation

A reviewer's checklist, stated once so every item's review can point at it:

- **Derive, don't store (#3).** The only mutable "state" outside the ledger
  and the registry mirror is the memo cache, and it is discardable by
  construction. No status column exists to update. Nag-fired state is derived
  from `nag` events, not a flag.
- **Verdicts are telemetry (#4).** The hub never writes to git, and nothing in
  the hub trusts a verdict that claims git origin. The ingest gate is the only
  telemetry entrance; internal event kinds (`nag`) are appended by the hub's
  own attributed principal through the trait, never through the gate.
- **Nag, never a lie (#6).** Every rejection is counted and queryable; a quiet
  source ages into stale by arithmetic; a routing dead-letter is a queue item;
  a malformed claim at a synced tip is a finding, not a skip.
- **Customer owns their data (HUB.md §1/§4).** One file the customer controls;
  export and delete are file operations; no phone-home, no product-owned
  central store; the Postgres path preserves the same ownership behind the
  same traits.

## 3. The consequential choices, for veto

1. **Rust, in this workspace** — over Go or TypeScript in a separate repo.
   Bought: parser reuse from `claim-core` (one claim grammar, enforced by the
   compiler), the invariants as types, one toolchain, one gate. Cost: web UI
   iteration is slower than a JS stack's.
2. **In-workspace crates, not a separate hub repo** — lockstep grammar and
   wire-contract tests over release independence. Cheap to reverse later
   (crates extract cleanly); expensive to start with.
3. **SQLite-first behind `Ledger`/`Registry` traits, via sqlx; Postgres is a
   later second impl** — over Postgres-first. Bought: self-hosting as the
   ownership default, one-file data custody, zero database operations. Cost:
   the multi-tenant tier waits for a second trait impl (deliberately deferred
   by HUB.md §7 anyway).
4. **Server-rendered askama UI, markdown twin from the same view model** —
   over an SPA or runtime templates. Bought: one substrate provably shared by
   HTML and markdown, compile-checked templates, no JS toolchain. Cost:
   template edits recompile; rich interactivity would need revisiting (v1's UI
   is a queue and a dossier; it does not need it).
5. **Token verification built directly on `jsonwebtoken` + a hand-written JWKS
   cache** — over wrapper crates (`github-oidc`, `git-oidc`) or an auth proxy.
   The ledger's trust root stays two auditable dependencies and a page of our
   own code. Cost: we own the JWKS refresh logic and its tests.

(The architecture-level decisions — event-sourced ledger, no broker, OIDC
ingest with no unattested lane, MCP as the agent binding — were taken for veto
in HUB.md §8 and are not reopened here.)

## 4. Build order

### 4.1 How to read this

Each item is one branch (`hub-NN-short-name`), sized for a single agent to
build, gate, and take through the two-reviewer pass — the CLI's item-NN
discipline, new prefix. Every item owns its docs (CLAUDE.md: docs ship with
the behavior): the hub topic doc `docs/hub.md` starts at hub-03 and grows with
each surface, and `docs/index.html` mentions the hub when its first user-facing
surface lands. An agent handed an item should be able to read its row and
section here, plus HUB.md's matching component, and start.

### 4.2 Dependency graph and waves

```
wave 0        hub-01 envelope & contract ──────────────┐
                 │                                     │
wave 1        hub-02 storage ───────────────┐       hub-06 deriver (pure —
                 │                          │       parallel to all of wave 1–3)
wave 2        hub-03 app shell    hub-05 registry sync │
                 │                          │          │
wave 3        hub-04 ingest gate            │          │
                 └──────────┬───────────────┴──────────┘
wave 4        hub-07 walking skeleton (M0: verdict in → ledger → derive → read)
                 │
wave 5        hub-08 JSON API      hub-12a ingest action     hub-15 packaging
                 │
wave 6        hub-09 MCP   hub-10 UI+twins   hub-11 router   hub-13 read auth
                                                │
wave 7        hub-12b nag delivery       hub-14 skip ranking
```

Critical path: **hub-01 → hub-02 → hub-03 → hub-04 → hub-07 → hub-08 →
hub-11 → hub-12b** — the spine from "an attested verdict exists" to "a human
was nagged." The deriver (hub-06) is deliberately off the critical path: it is
pure functions over hub-01's types, so it parallelizes with all of storage,
sync, and ingest instead of waiting for them. The wide fan-out is wave 6: four
independent items over the skeleton and the API.

| id | delivers | depends on | parallel with | maps to |
|---|---|---|---|---|
| hub-01 | envelope types, CLI-report wire parsing, contract test | — | — | HUB.md §2 ledger grammar, #18 check identity |
| hub-02 | `Ledger`/`Registry` traits, SQLite impl, migrations, dedup | hub-01 | hub-06 | §2 event ledger |
| hub-03 | `claim-hub` binary: axum app, config, `/status`, tracing | hub-02 | hub-05, hub-06 | §3 (shell for all components) |
| hub-04 | ingest gate: OIDC verify, validate, append, loud reject | hub-02, hub-03 | hub-05, hub-06 | §3 ingest gate, §4 trust |
| hub-05 | registry sync: git mirror, claim-core parse, supports index | hub-02 | hub-03, hub-04, hub-06 | §3 registry sync |
| hub-06 | deriver: standing, freshness, due, joins, memo | hub-01 | waves 1–3 | §3 deriver, #6 seam |
| hub-07 | walking skeleton M0: end-to-end slice wired and tested | hub-03..06 | — | §2 whole spine |
| hub-08 | JSON API: queries, dossier, sets, as-of, cursor feed | hub-07 | hub-12a, hub-15 | §5 reads |
| hub-09 | hub MCP: context/dossier/drifts/due/search | hub-08 | hub-10, hub-11, hub-13 | §5, #32 |
| hub-10 | UI + markdown twins + llms.txt: queue, dossier, status | hub-08 | hub-09, hub-11, hub-13 | §5 surfaces |
| hub-11 | router/nag: transitions, owners, grouping, dead-letter | hub-07, hub-08 | hub-09, hub-10, hub-13 | §3 router |
| hub-12a | CI glue: ingest action (check --json → OIDC POST) | hub-04, hub-07 | hub-08.. | §1 one ingest path |
| hub-12b | CI glue: nag delivery (standing issue, PR comment) | hub-11, hub-12a | hub-14 | §3 router delivery |
| hub-13 | read auth: OAuth 2.1 RS, scoped tokens, RFC 9728 metadata | hub-08, hub-09 | hub-10, hub-11 | §5 auth |
| hub-14 | skip-ranking projection in queue and surfaces | hub-06, hub-10 | hub-12b | §6, #9 |
| hub-15 | packaging: container, compose, self-host + backup docs | hub-07 | hub-08.. | §4 ownership |

### 4.3 The items

**hub-01 — envelope and wire contract.** Creates `claim-hub-core` with the
event envelope of HUB.md §2 (kind, claim, check index + digest, verdict,
evidence, commit, store, producer verbatim, reported_at) and serde parsing of
the CLI's `--json` check report as a wire format. Done when: envelope
round-trips serde losslessly; unknown envelope fields are rejected with the
field named; a workspace contract test runs the built `claim` binary
(`assert_cmd`) against a temp store and parses its real `--json` into the hub's
report types — the test that keeps the two ends of the wire honest from day
one. Needs before start: envelope field sign-off (§4.5).

**hub-02 — storage.** Creates `claim-hub-store`: the `Ledger` and `Registry`
traits, the SQLite implementation via sqlx, embedded migrations, the events
dedup index, the append-only triggers. Done when: append/scan/head round-trip;
appending the same (producer run, check identity) twice yields one row and an
idempotent success; a raw `UPDATE`/`DELETE` against events fails (trigger
test); registry wipe-plus-resnapshot rebuilds identically; migrations run from
an empty file at first boot.

**hub-03 — app shell.** Creates the `claim-hub` binary: axum app assembled
from tower layers, TOML + env config, `tracing` wiring, `/status` reporting
ledger head, registry version, last sync, rejection count. Done when: the
binary boots from a minimal config against an empty directory and serves
`/status` truthfully; config parse errors name the file and field; `docs/hub.md`
exists with the self-host quickstart.

**hub-04 — ingest gate.** The one telemetry write path: OIDC verification
middleware (JWKS cache with kid-triggered refresh; `iss`/`aud`/`exp`/signature;
repository-is-connected check), envelope validation, verbatim append, loud
rejection with a counted record. Done when: a valid token + valid envelope
appends verbatim and returns the ledger position; forged signature, expired
token, wrong audience, and unconnected repository each reject 4xx with the
reason, appending nothing; a malformed envelope rejects naming the field;
redelivery returns the original success; JWKS fetch is mocked in tests (no
network); the rejection counter is visible at `/status`.

**hub-05 — registry sync.** Mirror clone/fetch via system git, tip read
through `claim-store` loading with `claim-core` parsing, `claims_at_tip` and
`supports_edges` maintenance, deletion-as-retirement, interval poll plus
authenticated manual resync. Done when: a temp git fixture syncs and its
claims (including embedded blocks) index at the tip sha; deleting a claim
drops it from the live set on the next sync; a malformed claim file becomes a
recorded sync finding, not a silent skip; wipe-plus-resync reproduces the
registry byte-for-byte; no network in tests (local fixture remotes).

**hub-06 — deriver.** Pure derivation in `claim-hub-core`: per-claim standing
(verified / stale / drifted / suspect / retired), freshness against `hub:`
hints and config overrides, the due set, the conservative multi-check join,
skip age, the memo keyed by (head seq, registry version, config hash) with the
clock-crossing horizon. Done when: property tests show no input combination
manufactures a green; a shallow check's pass never clears a deep check's
drift; `broken` counts as never-checked; a claim crosses into stale by clock
alone with no new event; memo invalidates on exactly the three causes and a
discarded cache recomputes identically; every function takes time as a
parameter. Needs before start: the `hub:` key set (§4.5).

**hub-07 — walking skeleton (milestone M0).** Wires 03–06 into the smallest
honest end-to-end slice: one integration test seeds a git fixture, syncs,
POSTs one attested verdict (mocked JWKS), and reads
`/api/claims/{id}` — standing derived from the real ledger, carrying its
as-of. Done when: that test passes in the gate; a second test shows the same
claim aging into stale by advancing the injected clock; the compose example
boots the binary against an empty volume. This is the integrated spine every
wave-5+ agent builds against — breadth waits until it exists.

**hub-08 — JSON API.** The read surface over the deriver: claims by path,
repo, standing, supports target; the drifted/due/suspect sets; the dossier
(statement and check by git reference at a commit, standing with as-of,
verdict history, evidence, derived provenance); the cursor feed over ledger
and transitions. Done when: every endpoint has an integration test asserting
shape and as-of; same (cursor, registry version, clock) yields identical bytes
(determinism test); `insta` snapshots pin response shapes; pagination by
ledger seq, not offset.

**hub-09 — hub MCP (#32).** rmcp streamable-HTTP service mounted on the axum
router; read-only tools `context`, `dossier`, `drifts`, `due`, `search`, each
returning the API's JSON. Done when: each tool has a test asserting parity
with its API endpoint's body; tool schemas are snapshot-pinned; `tools/list`
is stable across restarts; the docs list every tool (the docs-coverage
backstop pattern extends to hub tools).

**hub-10 — UI, markdown twins, llms.txt.** View-model structs per page; two
askama templates each; the queue and the claim dossier; `/llms.txt`;
`.md` twins at predictable paths. Done when: HTML and markdown for each page
render from one struct (twin-parity by construction) and both are
`insta`-snapshotted; `llms.txt` indexes every surface; no JavaScript build
step exists; `docs/index.html` documents the hub surfaces.

**hub-11 — router/nag.** Transition detection derived by diffing against
`nag` events; owner resolution from mirror CODEOWNERS at fire time; grouping
by envelope commit; the dead-letter queue; rendered nag content served for
delivery. Done when: a drift transition fires exactly once across a restart
(no mutable fired-flag — proven by killing and reviving the process in a
test); a clock-crossing stale fires with no new verdict; no-owner routes to
dead-letter and the queue shows it; one commit breaking N claims yields one
grouped item; a lapsed skip `until` fires. Needs before start: delivery-split
confirmation (§4.5).

**hub-12a — CI glue: ingest action.** The GitHub Action the hub ships:
`claim check --json`, exchange the runner's OIDC token, POST to the hub, fail
the lane loudly if ingest fails. Done when: the action's flow is exercised
against a locally-run hub in a test harness; an ingest rejection fails the
step with the hub's reason in the log; the action never swallows a non-2xx.

**hub-12b — CI glue: nag delivery.** The scheduled lane pulls the hub's
rendered due-and-drifted view and upserts the standing issue and PR comments,
evolving the `ci/render.mjs` lineage. Done when: upsert is idempotent (two
runs, one issue); content matches the hub's rendered nag body exactly (the hub
renders, the glue delivers); a hub outage leaves the previous issue intact and
the lane loud, not green.

**hub-13 — read auth.** Bearer-JWT validation against a configured issuer's
JWKS for API and MCP; hub-minted scoped tokens (hashed at rest) as the
IdP-less floor; RFC 9728 protected-resource metadata; scope enforcement (read
broadly, act narrowly — v1 has no act endpoints, the scope model ships ahead
of them). Done when: unauthenticated access to a protected surface returns 401
with the metadata pointer; scope violations 403; tokens verify with mocked
JWKS only; the open-read-vs-authed default follows the config policy decided
in §4.5.

**hub-14 — skip ranking (#9).** The deriver rule and queue rendering: skips
by age and lapsed `until`, ranked into the review queue, visible through API,
MCP, twins, and UI. Done when: ranking is a pure deriver function with
property tests; a lapsed `until` outranks an aging skip per the rule; all four
surfaces show the same ranked set.

**hub-15 — packaging and self-host.** Musl-where-feasible build with bundled
SQLite, the container image (binary + git + CA certs), the compose example,
the self-host doc with Litestream and file-copy backup, and a scripted
backup-restore exercise. Done when: the image builds in CI; a cold start from
an empty volume reaches a truthful `/status`; the backup-restore script runs
in CI against a seeded hub and the restored hub derives identical answers.

### 4.4 v1 and deferred

The table mirrors HUB.md §7; items above are the left column. Deferred, in
their extension seams, none blocking v1: spot-audit (#5: new event kind +
deriver rule + route), windowed claims (#7: deriver rule), cross-repo routing
(#10: route over the supports index hub-05 already builds), suspect
propagation (deriver rule), hosted multi-tenancy and per-source read ACLs
(second `Ledger`/`Registry` impl over Postgres, tenant scoping above the
traits), Sigstore attestation (additive at the ingest gate), managed runners
(scheduler dispatch lane), webhook/streaming subscriptions (fan out the
transition stream), hub-native acts (ack, audit-request: new event kinds
through an authenticated act endpoint), broker-backed ledger (behind the
`Ledger` trait), escalation/damping/chat delivery (router policies). The first
"later" item to actually schedule is the Postgres trait impl, because it
proves the seam while the seam is young.

### 4.5 Decisions (resolved 2026-07-18)

Signed off by the owner before the fleet started; agents build to these, not to
the open questions this section once held.

1. **Stack vetoes (§3):** all five accepted — Rust in-workspace, SQLite behind
   `Ledger`/`Registry` traits, askama SSR with markdown twins, `jsonwebtoken` +
   own JWKS cache, in-workspace crates (not a separate repo).
2. **Envelope fields (hub-01):** HUB.md §2's set is the frozen v1 wire. The
   **check-digest is a hash of the check's canonical definition** (kind plus the
   run/instruction, negate, and skip fields in a normalized form), so a check's
   identity is stable across reordering and cosmetic edits. **`evidence` is
   capped at ingest** (a few KB); over-cap evidence is truncated with a recorded
   marker, never dropped silently (invariant #6).
3. **`hub:` keys (hub-06):** v1 is `recheck` + `max-age` only; no additions. Any
   later key is a `claim-core` parser change first, on its own branch.
4. **Nag delivery (hub-11/12b):** v1 keeps forge write credentials in the CI
   glue — the hub renders nag content and serves it, the glue delivers (§1.9).
   The hub holds no forge write token in v1.
5. **Read-auth default (hub-13):** authed-everything with the scoped-token floor
   is the default; open reads are an explicit opt-in config for a trusted private
   network. Secure by default.
6. **Naming:** crates `claim-hub-core` / `claim-hub-store` / `claim-hub`, binary
   `claim-hub`.

## Appendix: sources (surveyed July 2026)

- Framework: [axum 0.8 announcement](https://tokio.rs/blog/2025-01-01-announcing-axum-0-8-0)
  (tokio team; native async traits), current release
  [0.8.9](https://docs.rs/crate/axum/latest); 2026 comparisons place axum as
  the maintainability default with actix-web ahead only on raw throughput
  ([1](https://medium.com/@abhinav.dobhal/actix-web-vs-e1e019714542),
  [2](https://aarambhdevhub.medium.com/rust-web-frameworks-in-2026-axum-vs-actix-web-vs-rocket-vs-warp-vs-salvo-which-one-should-you-2db3792c79a2)).
- Storage: [sqlx](https://github.com/launchbadge/sqlx) 0.8.x (compile-time
  checked SQL, SQLite + Postgres, offline mode; the
  [FAQ](https://github.com/launchbadge/sqlx/blob/main/FAQ.md) notes queries
  bind to one database — hence per-impl checking behind the traits);
  production-SQLite practice and WAL characteristics
  ([SQLite renaissance, 2026](https://dev.to/pockit_tools/the-sqlite-renaissance-why-the-worlds-most-deployed-database-is-taking-over-production-in-2026-3jcc));
  [Litestream](https://litestream.io/) WAL streaming to S3-compatible storage
  ([Fly.io on server-side SQLite](https://fly.io/blog/all-in-on-sqlite-litestream/)).
- Templating: [Are we web yet — templating](https://www.arewewebyet.org/topics/templating/);
  [askama-rs template benchmark](https://github.com/askama-rs/template-benchmark)
  (compiled askama ~10⁶ renders/s; interpreted minijinja trades speed for
  hot reload); [the MASH stack](https://emschwartz.me/building-a-fast-website-with-the-mash-stack-in-rust/)
  as the axum + askama + sqlx reference shape.
- MCP: [modelcontextprotocol/rust-sdk](https://github.com/modelcontextprotocol/rust-sdk)
  (`rmcp`, [2.2.0](https://docs.rs/crate/rmcp/latest));
  [MCP 2026-07-28 release candidate](https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/)
  (protocol stateless at the transport layer; authorization SEPs aligned to
  OAuth 2.1/OIDC); [OAuth 2.1 for remote MCP servers](https://mcp.directory/blog/oauth-21-for-remote-mcp-servers-streamable-http-explained-2026);
  [MCP authorization docs](https://modelcontextprotocol.io/docs/tutorials/security/authorization).
- Ingest identity: [GitHub Actions OIDC](https://docs.github.com/en/actions/concepts/security/openid-connect)
  (issuer, JWKS endpoint, workflow claims);
  [jsonwebtoken](https://github.com/Keats/jsonwebtoken) (current
  [10.x](https://docs.rs/crate/jsonwebtoken/latest));
  wrapper crates surveyed and passed over:
  [github-oidc](https://lib.rs/crates/github-oidc),
  [git-oidc](https://docs.rs/git-oidc/latest/git_oidc/); flow walkthrough:
  [authenticating GitHub Actions requests with OIDC](https://gal.hagever.com/posts/authenticating-github-actions-requests-with-github-openid-connect).
