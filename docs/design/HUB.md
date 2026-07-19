# The hub — high-level architecture

Status: draft framework, July 2026. Builds on `CLI-HUB-BOUNDARY.md`, which pins
the CLI/hub split; this document does not re-litigate that boundary, it designs
the hub's side of it. It is deliberately a framework, not a spec: enough
concepts to extend without rework, and no endpoint or schema detail an
implementation item would not immediately supersede. Scope: issue #8 (the hub),
with the seams for #5 (adversarial spot-audit), #6 (a home for the expensive
derivation), #7 (windowed claims), #9 (skip ranking), #10 (cross-repo routing),
and #32 (the hub MCP). Where a choice is consequential, the alternative is
named so it can be vetoed at the concept level.

## 1. Role and boundary (recap)

Three layers: the claim file in git is the truth; the CLI is a stateless
runtime verifier; the hub is the per-environment ledger and scheduler. The hub
ingests the CLI's reported `--json` — pushed by a pipeline's CI glue, with the
pipeline's attested identity — and owns everything stateful and over-time: the
verdict stream, standing, due-ness, the nag, routing, dashboards, and the
agent-facing knowledge interface. Its reason to exist is invariant #6: the nag
over time that the stateless CLI cannot issue.

Five things the hub must never do, fixed by the boundary, the invariants, and
the product's data-ownership stance:

- **Never the source of truth for claims.** The repos are; the hub mirrors and
  reindexes, and is wrong only by being briefly behind.
- **Never writes a verdict to git**, and never trusts a verdict that claims to
  have come from git (invariant #4).
- **Never instructs the CLI.** There is no hub→CLI protocol. The hub computes
  what is due; a pipeline lane (or, later, a hub-dispatched runner) runs the
  CLI and reports back through the one ingest path.
- **Never a gate on core verification.** `claim check` works standalone in any
  CI whether or not a hub exists (issue #8).
- **Never owns the customer's data.** The claims are the customer's, in their
  git; so is the verdict ledger the hub derives over. The hub operates a
  customer's data on their behalf, over storage they control — never as the
  product's own asset. This is an invariant, not a policy: the v1 shape (a
  self-hostable per-environment instance, section 4) makes customer ownership
  the default, and any hosted offering must preserve it.

Per-environment means per-history: a QA hub and a production hub track the
same claims on different cadences with different verdict streams. "The hub"
below is one such instance.

## 2. The conceptual model: two inputs, one derivation

Everything the hub knows reduces to one equation, and the architecture is the
equation made into components:

    answer = derive(registry, ledger, clock, config)

**The claim registry** is the hub's mirror of git: every claim file in every
connected store, indexed at the tip of its default branch, with the commit sha
it was read at. Provenance — author, PR approvals — is resolved from git and
the forge on demand (invariant #3), never from fields in a file. The registry
is derived data in the strictest sense: delete it and a re-scan rebuilds it.

**The event ledger** is the hub's one piece of primary state: an append-only
log of attested observations. In v1 every event is a verdict; the grammar is
deliberately wider — later kinds (spot-audit results, acknowledgements,
delivered nags) append to the same log. Events are never updated and never
deleted; retention is an operational policy, not a semantic one. Each event
carries, at minimum:

```json
{
  "kind": "verdict",
  "claim": "payments/libfoo-pin",
  "check": { "index": 1, "digest": "…" },
  "verdict": "held",
  "evidence": "…",
  "commit": "8f2c…",
  "store": "github.com/acme/payments",
  "producer": { "iss": "…", "repository": "…", "workflow": "…", "run": "…" },
  "reported_at": "2026-07-18T06:00:00Z"
}
```

The check identity — index plus a digest of the check's definition — is in the
envelope from day one, because a shallow check's pass must never clear a deep
check's drift (the multi-check finding in `CLI-HUB-BOUNDARY.md`, issue #18).
The producer block is the verified pipeline identity, recorded verbatim so the
trust judgment can be re-derived later, not just made once at the door.
Redelivery is deduplicated on (store, producer run, claim, check identity), so a
retried push cannot double-count an observation. Each component is load-bearing:
`store` because a run id is unique per repository, not globally (§4's identity is
(repository, run)) and the check digest is content-based and stable across repos;
`claim` because the digest is a property of the check's definition alone, so two
claims with identical checks must not collide; and a non-empty run is required —
a run-less verdict is unattributable and rejected, not bucketed.

**The derived read model** is everything else the hub appears to hold:
standing (verified / stale / drifted / suspect / retired), freshness, due-ness,
skip age, dashboards, the review queue. All of it is a pure function of
(registry, ledger, clock, config), computed at read time and memoized only as
cache. This is invariant #3 made load-bearing: there is no status column
anything must remember to update, so there is no path to a stored status
quietly disagreeing with the evidence. A wrong cache is discarded and
recomputed from the log; wrong truth would be forever. The join across a
claim's checks is conservative — bad news dominates: any drifted latest
verdict makes the claim drifted, any overdue or broken or unverifiable check
counts against freshness, and a green can never be manufactured by the join.

This is the event-sourcing / CQRS shape — append-only log as source of record,
projections as rebuildable read models — chosen not as fashion but because the
invariants force it: the alternative, a mutable per-claim status table, is
exactly the stored status invariant #3 forbids. What is *not* adopted is the
streaming infrastructure that usually rides along: v1's ledger is a database
table with append-only discipline, not a broker. The ledger interface is the
seam; a broker is a scale upgrade behind it, adopted when event volume says
so, never a semantic change.

## 3. Components

```
   git repos (.claims/)                CI lanes (per-PR + scheduled)
        │                                   │
        │ webhook / pull                    │ claim check --json, pushed by
        ▼                                   ▼ the hub's CI glue + OIDC identity
  ┌───────────────┐                  ┌───────────────┐
  │ registry sync │                  │  ingest gate  │ verify identity, validate,
  └───────┬───────┘                  └───────┬───────┘ append verbatim — or reject loudly
          ▼                                  ▼
  ┌───────────────┐                  ┌───────────────┐
  │ claim registry│                  │ event ledger  │
  │  (git mirror) │                  │ (append-only) │
  └───────┬───────┘                  └───────┬───────┘
          └──────────────┬───────────────────┘
                         ▼
               ┌──────────────────┐
               │     deriver      │ standing, freshness, due-ness,
               │ (pure, memoized) │ skip age — computed, never stored as truth
               └────────┬─────────┘
              ┌─────────┴─────────┐
              ▼                   ▼
       ┌────────────┐      ┌─────────────┐
       │ scheduler  │      │ router / nag│ → PR comment, standing issue,
       │ (due list) │      │             │   owner resolved at fire time
       └────────────┘      └─────────────┘
                         ▼
       ┌──────────────────────────────────────────┐
       │ one read model, four renderings:         │
       │ JSON API · hub MCP · llms.txt/md · web UI│
       └──────────────────────────────────────────┘
```

**Ingest gate.** The single write path for telemetry. It authenticates the
producer (section 4), validates the envelope against the schema, and appends
verbatim. It never coerces: a malformed or unattested push is rejected with a
reason, and the rejection is itself surfaced (a hub that silently drops
telemetry would age claims into staleness with nobody told why — a nag is
owed for that too). There is no other way in; no backfill endpoint, no manual
verdict entry. If a feature seems to need one, it is a new event kind with its
own producer, not a side door.

**Registry sync.** Pulls (or receives forge webhooks from) every connected
store and reindexes `.claims/` — full claim files and embedded claim blocks —
recording the commit each claim was read at. A claim deleted from git is a
retirement; the registry drops it from the live set and the claim's page
renders its history from git and the ledger. Sync also maintains the
`supports` index across stores, which is what cross-repo routing (#10) will
key on.

**Deriver.** The pure function of section 2, plus its memoization. Projections
(per-claim standing, the due set, the queue, dashboard aggregates) are caches
invalidated by exactly three things: a new event, a registry change, and the
clock crossing a claim's threshold. Because staleness is arithmetic over
`hub.max-age` and the latest passing verdict, nothing has to run for a claim
to *become* stale — the deriver just reports it at the next read, the way a
certificate expires. The deriver is also where issue #6's memoization framing
lands: a claim is a cached expensive derivation, and the hub is where the
evidence behind that derivation accumulates — agent-check evidence notes
arrive inside verdict events, and the claim's page joins the git-committed
definition with everything the ledger holds about it. Whether the establishing
investigation deserves a first-class field is deferred until a real corpus
shows the prose body failing (#6); the seam — evidence attached to events — is
there either way.

**Scheduler.** Reads the registry's `hub:` hints (`recheck`, `max-age`),
applies per-hub config overrides, and derives the due set. In v1 that is its
entire job: the due set is a published view, and the scheduled CI lane in each
repo remains a dumb cron that runs `claim check --json` and reports. The hub
never reaches into a repo to trigger anything — no hub→CLI protocol — so
scheduling degrades safely: if the hub is down, the cron lane still runs and
evidence still accumulates the moment ingest returns. Dispatching work to
managed runners is the later, additive lane (section 6).

**Router / nag.** Consumes derived *transitions* — a claim entering drifted, a
claim crossing into stale, a skip's `until` lapsing — resolves the owner at
fire time from CODEOWNERS or the team registry (never from a stored owner
field), groups by cause using the envelope's commit (one refactor breaking
twelve claims is one item), and delivers. v1 delivery is the forge surface
that already exists: the per-PR comment and the one standing "due & drifted"
issue the CI glue maintains. Escalation ladders, chat channels, and flap
damping are router policies added behind the same seam. The router's contract
with invariant #6: every terminal state of every path is a human (or an agent
acting for one) being asked to look. A routing dead-letter — no owner
resolves — is itself a first-class queue item, not a dropped notification.

**Surfaces.** One read model, rendered four ways: the JSON API, the hub MCP,
the markdown/llms.txt twin, and the web UI. Section 5 — the point of the hub
for agents — takes these in full.

## 4. Trust and attestation

The trust root for a verdict is *who produced it* — the SLSA framing:
provenance is generated by the platform that ran the work, and its value is
the identity it binds, not the assertion it carries. Applied here:

- **v1: pipeline identity via OIDC.** The hub's CI glue authenticates each
  push with the pipeline's OIDC id-token (on GitHub Actions: issuer,
  repository, workflow, ref, run id). The hub verifies the token, checks the
  repository is a connected store, and records the verified identity claims
  into the event's `producer` block verbatim. No long-lived shared secret
  exists to leak or to forge, and every verdict is permanently attributable to
  the workflow run that produced it.
- **Alternative, rejected: static ingest tokens.** Unrotated, unattributable
  beyond "someone with the token," and indistinguishable between a pipeline
  and a laptop. For a product whose whole substance is trust, the cheapest
  option is the one thing it cannot be.
- **Alternative, deferred: full signed attestation.** Sigstore keyless signing
  with a transparency log is the same trust root (the OIDC identity) with
  stronger non-repudiation. It is an additive upgrade — the envelope already
  records the identity a signature would bind — adopted when a tenant needs
  third-party-verifiable evidence, not before.

There is no unattested lane. A developer's local `claim check` is a local
report, read on the terminal and discarded; it never becomes hub telemetry.
The alternative — a second-class "untrusted" tier — doubles the explanation of
every surface ("verified, but by whom?") for no gain the attested lanes do not
already provide.

**Data ownership is the invariant; multi-tenancy is a later layer over it.**
The customer owns their data — the claim registry and the verdict ledger are
theirs, not the product's asset. The v1 shape makes this the default rather
than a promise: a hub is a per-environment instance the customer runs over
storage it controls, self-hostable, its ledger and registry exportable and
deletable wholesale — so there is no central store the product owns, and no
migration to escape later. A hosted, multi-tenant offering is a later layer,
not a new model: it adds tenant isolation (a tenant's ledger and registry are
theirs alone, never co-mingled) and per-source read access mirroring repo
permissions (the org-wide index must never become the one place where anyone
can read everything sensitive in aggregate, PRODUCT.md §6). It must preserve
the ownership invariant — the product operates a customer's data for them, it
does not own it. We are not building the multi-tenant control plane now; we
are keeping the registry/ledger boundary clean so it can be added without ever
re-owning the data.

Two honesty properties round this out. Every displayed standing carries its
*as-of* — the ledger position and registry commit it derives from — so the hub
can never show a green older than its evidence. And a source gone quiet is not
a hub failure to detect specially: the max-age arithmetic ages its claims into
stale, and the nag fires. The dead pipeline degrades into a human being asked
to look — a nag, never a lie.

## 5. Agent-native surfaces

Agents are first-class users of the hub, not consumers of a bolted-on API.
The 2026 practice this draws on: expose the product's actions and state as
structured, deterministic, tool-callable interfaces (MCP as the standard
binding); publish `llms.txt` and markdown twins so non-tool-using agents read
cheaply; make writes idempotent and outcomes explicit, because agents retry;
and give agents real principals, not scraped sessions. The design principle
that organizes all of it:

**One substrate.** The web UI is a rendering of the same read model the JSON
API serves and the MCP binds. Nothing is UI-only — no datum, no action. This
is what keeps humans and agents from arguing about what the hub said: they
read one derivation, through different lenses. A dashboard is a chart over a
query an agent can run; a review-queue row is a JSON object with a template
on top.

**Reads.** The primary agent verb is inheritance: *what does the org believe
about what I am touching, and how good is that belief right now?*

- The JSON API serves status-aware queries — claims by path, repo, standing,
  or supports target; the drifted, due, and suspect sets; and a claim's full
  **dossier**: the statement and check (by reference to git, at a commit), the
  derived standing with its as-of, the verdict history, evidence, and derived
  provenance (author, approvals).
- Every answer carries its derivation. A standing arrives with the events it
  derives from and the producer identities behind them — dated evidence to
  weigh, never instructions to obey. A claims surface agents obey blindly is
  an injection channel with a trust stamp (PRODUCT.md §6); the interface makes
  the safe reading the natural one.
- Reads are deterministic: the same ledger position, registry commit, and
  clock produce the same answer, and the response says which it used. An
  agent can cache, diff, and resume without guessing what changed underneath.
- **The hub MCP (#32)** is the primary agent binding: a small set of
  outcome-first tools over the API — on the order of `context` (the claims
  relevant to a path set, with standing), `dossier`, `drifts`, `due`,
  `search`. Small and bounded is deliberate; each tool answers a question an
  agent session actually asks, and each returns the same JSON the API serves.
  Choosing MCP over a bespoke agent protocol is cheap to revisit precisely
  because the tools are thin bindings — the substrate is the API.
- **`llms.txt` and markdown twins** cover every agent that reads rather than
  calls: the hub publishes an `llms.txt` index of its surfaces, every page has
  a markdown twin at a predictable URL, and a machine-readable status
  endpoint reports hub health and ledger position. The v1 UI being
  server-rendered over the API makes the twin nearly free.

**Acts.** Narrow, idempotent, explicit. Writes to *truth* are never hub
writes: an agent that wants to amend or retire a drifted claim goes to the
owning repo and opens a PR, exactly as PRODUCT.md prescribes. What the hub
accepts are acts about *attention* — acknowledge a drift (it is being
handled), request a re-check or, later, a spot-audit, subscribe to a scope.
Every act carries an idempotency key (agents retry; a retried acknowledgement
must not double-fire), lands as an event on the ledger (attributable,
derivable, auditable like everything else), and reports an explicit terminal
state. In v1 the act surface is minimal to absent: the forge is the action
surface — the standing issue and PR comments are already agent-operable — and
hub-native acts arrive with the review queue's growth.

**Subscriptions.** v1 is a cursor feed, not webhooks: the ledger and the
derived-transition stream are pollable with a position cursor, so an
intermittent agent session catches up deterministically from where it left
off. Webhook and streaming push are later additions for always-on consumers;
they fan out the same transition stream, so nothing is redesigned.

**Auth.** Agents are principals. Remote MCP authenticates with OAuth 2.1 (the
protocol's 2026 authorization model); API tokens are scoped — read broadly,
act narrowly — and every act is attributed in the ledger to the principal that
performed it. Read scopes respect per-source ACLs (section 4).

## 6. Extension seams

The framework is four seams. Every deferred feature lands in one or more of
them, with no rework of sections 2–5:

1. **A new event kind** appends to the ledger.
2. **A new deriver rule** reads the same inputs and adds to the read model.
3. **A new route** consumes derived transitions.
4. **A new rendering** serves the same read model.

Mapped to the deferred work:

- **Adversarial spot-audit (#5).** A new producer (the audit lane re-runs a
  sampled `held` agent check with a refuting second agent) and event kind
  (`audit`, referencing the audited verdict event), plus a deriver rule (a
  refuted `held` marks the claim contested and counts against freshness) and a
  route (contested claims enter the queue). Possible from day one because
  events carry per-check identity and can reference each other.
- **Windowed / SLO claims (#7).** Purely a deriver rule: "held if N of the
  last M runs held" is a read over history the ledger already keeps, fed by a
  window hint under `hub:`. No new storage, no CLI change — the CLI keeps
  reporting single-run verdicts.
- **Cross-repo drift routing (#10).** A route keyed on the registry's
  cross-store `supports` index: the router resolves the owner of the
  *decision*, wherever it lives, not the claim's repo. The registry index is
  built for this shape from the start.
- **Skip ranking (#9).** A projection: skips by age and lapsed `until`,
  ranked into the queue. Cheap enough that v1 includes it — the `--json` the
  hub ingests already reports every skip.
- **Suspect propagation.** A deriver rule over the registry's supports graph:
  a drifted claim marks its dependents suspect. Bad news travels; good news
  does not (PRODUCT.md §4).
- **Managed runners.** The scheduler grows a dispatch lane on the
  control-plane / data-plane model: the hub schedules, runners — self-hosted
  for anything touching internal systems — execute the CLI against a checkout
  and report through the same attested ingest. The seam holds because ingest
  never cared who *ran* a check, only who attests to the result.
- **Escalation, damping, grouping.** Router policies over the same transition
  stream, fed by the envelope's cause metadata.
- **New interfaces.** Anything — a Slack digest, an IDE panel, a second MCP —
  renders the read model. None of them can disagree with the others, because
  none of them holds state.

## 7. v1 and later

v1 is the smallest hub that closes the loop the stateless CLI cannot: evidence
in, staleness derived, a human nagged, an agent able to inherit.

| v1 (bare essentials) | Later (extension seams) |
|---|---|
| Single-tenant, self-hostable over customer-owned storage | Hosted multi-tenant control plane (tenant isolation, per-source read ACLs) |
| Ingest gate with OIDC pipeline identity | Sigstore signed attestations |
| Event ledger as an append-only table | Broker-backed stream at scale |
| Registry sync with supports index | — |
| Deriver: standing, freshness, due set, skip age (#9) | Windowed verdicts (#7), suspect propagation, spot-audit standing (#5) |
| Scheduler as a published due list; cron CI lanes run the CLI | Dispatch to managed / self-hosted runners |
| Router: forge nag (standing issue, PR comment), owner at fire time, dead-letter queue | Escalation ladders, damping, chat delivery, cross-repo routing (#10) |
| Read surfaces: JSON API, read-only hub MCP (#32), llms.txt + markdown twins, minimal UI (queue + claim dossier) | Hub-native acts (ack, audit request), subscriptions beyond cursor polling, dashboards beyond the queue |

## 8. Decisions taken here, for veto

1. **Event-sourced ledger with derived read models** (§2) — over a mutable
   status store. Forced by invariant #3; the only real choice was whether to
   admit it structurally.
2. **Plain append-only table, not a broker** (§2) — the pattern without the
   infrastructure; the ledger interface is the seam.
3. **Ledger generalized to event kinds beyond verdicts** (§2, §6) — over a
   verdicts-only store plus separate operational tables. One log keeps every
   hub statement derivable and auditable the same way.
4. **OIDC pipeline identity at ingest, recorded verbatim; no unattested
   lane** (§4) — over static tokens (rejected) and full signing (deferred,
   additive).
5. **One substrate: UI renders the agent-readable API** (§5) — over a
   separate agent gateway. Humans and agents share one source of truth.
6. **MCP as the primary agent binding, llms.txt/markdown for readers,
   cursor-feed subscriptions** (§5) — each a thin layer over the API, so each
   is cheap to revisit.
7. **v1 scheduler publishes due-ness and dispatches nothing** (§3) — over
   hub-triggered execution. Preserves "no hub→CLI protocol" and degrades
   safely when the hub is down.
8. **The customer owns their data; the hub does not** (§1, §4) — v1 is a
   self-hostable per-environment instance over customer-controlled storage, so
   ownership is the default, not a promise. Hosted multi-tenancy is a later
   layer that must preserve the invariant; deferred now, with the
   registry/ledger boundary kept clean so it needs no re-owning of data.

## Appendix: patterns drawn on (surveyed July 2026)

- Append-only event log with rebuildable projections (event sourcing / CQRS):
  the [Azure event-sourcing pattern](https://learn.microsoft.com/en-us/azure/architecture/patterns/event-sourcing)
  and [read-model design guidance](https://docs.eventsourcingdb.io/best-practices/designing-read-models/).
- Ingest → stream → derive → route as the shape of modern telemetry platforms:
  [Sentry's ingestion pipeline](https://develop.sentry.dev/application-architecture/overview/).
- Hosted control plane, customer-owned execution:
  [Buildkite's hybrid architecture](https://buildkite.com/docs/pipelines/architecture).
- Trust as producer identity — platform-generated provenance bound to OIDC
  workflow identity: [SLSA provenance](https://slsa.dev) and the
  [in-toto attestation format](https://in-toto.io), with
  [Sigstore keyless signing](https://www.sigstore.dev) as the upgrade path.
- Agent experience (AX) — every action API-accessible, machine-native auth,
  short agent feedback loops: [agentexperience.ax](https://agentexperience.ax/concepts/principles-of-ax/).
- MCP server design — few, bounded, outcome-first, deterministic tools;
  OAuth 2.1 authorization for remote servers:
  [modelcontextprotocol.io](https://modelcontextprotocol.io/docs/tutorials/security/authorization).
- Agent-readable documentation: [llms.txt](https://llmstxt.org) and
  llms-full.txt, with markdown twins for token-cheap ingestion.
- Agent-native API design — structured JSON, idempotency keys on writes,
  explicit terminal states for async work:
  [freeCodeCamp on APIs for agents](https://www.freecodecamp.org/news/how-to-design-apis-for-ai-agents/),
  [Apideck's agentic-era principles](https://www.apideck.com/blog/api-design-principles-agentic-era).
