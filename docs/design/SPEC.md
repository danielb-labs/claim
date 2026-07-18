# claims — continuous verification for the agentic era

> **Historical record — this is the original v1 design (v0.3).** It is kept as
> written, to preserve the initial thinking; it is **not** the shipped
> architecture. The product moved to a **v2 CLI/hub boundary**: the claim in git
> is the truth, the CLI is a stateless verifier that reports verdicts and stores
> nothing, and a per-environment hub owns the verdict stream, the schedule, and
> staleness. Where this document commits verdicts to a log, models a CLI-computed
> status, or gives claims a `when`/trigger and a top-level `max-age`, read the
> current model in `docs/design/CLI-HUB-BOUNDARY.md` and `docs/design/PRODUCT.md`
> instead. Nothing below is edited to match; treat it as the record of where the
> design started.

**Draft spec v0.3 — written to be reviewed.** Working name for the tool: `claim`
(candidates: `warrant`, `grounds`, `tenet`). Everything here is up for argument;
§10 lists the questions the authors most want challenged.

---

## 0. Thesis

CI verifies that code still does what its authors intended. It verifies nothing
about the layer the code *rests on*: the recorded knowledge — observations,
premises, justifications — that people and agents cite when they make decisions.
That layer lives in comments, PR descriptions, exception lists, and tickets,
where it rots silently, because prose cannot scream.

This mattered less when one team held a codebase in its heads: the person who
invalidated a fact usually knew who depended on it. It breaks completely in the
agentic era. Agents have total amnesia between sessions; a workforce of them
sharded across a system has *no* head that holds both a fact and the decisions
resting on it. Every session either re-derives its premises from scratch
(expensive) or trusts prose (wrong, at a measurable and surprisingly high rate:
in the working sessions that motivated this spec, roughly a third of
investigator-written diagnoses were false by the time the next agent read them).

The proposal: extend CI's contract from *"the code still behaves as specified"*
to *"the recorded knowledge still holds."* Concretely — a small, git-native tool
that binds recorded statements to the means of re-verifying them, keeps an
attributed history of their truth status, and routes **drift** (a fact that
stopped being true) to the **owners of the decisions that depend on it** rather
than failing the build of whoever happened to change the world.

The system being "integrated" is no longer just code. It is knowledge, produced
and verified by a mixture of human and machine intelligence. That is the
redefinition of CI this document pursues.

---

## 1. The problem, concretely — and why tests don't already solve it

Every mature codebase accumulates **decision records with perishable
justifications**:

- `# eslint-disable-next-line — safe: this value is server-generated`
- `@Skip("flaky on CI — tracked in INFRA-2041")`
- `libfoo==4.2  # pinned: 5.x corrupts PDF export for CJK fonts`
- security-scanner exceptions: `CVE-2024-31337: not applicable — user input
  never reaches XmlParser`
- ops assumptions: `# vendor file lands by 06:00 UTC; job scheduled 06:30`

Each cites a fact that was true, verified by someone, on some date — and that
someone *else* can make false without knowing the record exists.

**The failure story** (every engineer has lived a version of it): a scanner
exception is filed for CVE-2024-31337 — "not applicable; user input never
reaches `XmlParser`." True, verified by grep, approved. Eight months later a
feature PR routes a new upload handler through a helper that calls `XmlParser`.
Nothing fails. The exception file matches on CVE id, unconditionally. The
vulnerability is now live, behind a document that says it isn't.

**Now run the counterfactual where a test asserted the fact.** Suppose someone
had written `assert count_references("XmlParser", "src/handlers/") == 0`
(architecture-test frameworks — ArchUnit, import-linter — exist for exactly
this, which proves the appetite). The feature PR goes red. The author looks at
the failure and sees a test forbidding them from calling a function, with no
stated reason. Their local reasoning is sound: *"there's no visible reason this
call is wrong; the assertion is stale."* They update the count — or delete the
test — a reviewer shrugs, merge. **The drift was detected and the routing still
failed**, because the load-bearing content was never "references == 0"; it was
*"the CVE exception is justified by references == 0."* That dependency lived in
prose. The test carried the fact but not the *why*, so the person who tripped
it resolved it in place and the decision was never re-reviewed.

Generalizing — a failing test asks one question: *"is the change that triggered
me wrong?"* When the honest answer is "no, the world legitimately changed," the
assertion gets edited by the wrong person, for locally-correct reasons, and the
information the failure was supposed to carry is destroyed at the moment it
fires. The differences are structural:

| | a test | a claim |
|---|---|---|
| asserts | intended behavior (a spec) | an observed fact (a snapshot) |
| red means | someone erred → fix the code | the world changed → review the decisions |
| resolution | make it green | adjudicate: retire / amend / harden |
| written by | the author, at change time | an investigator, mid-diagnosis |
| truth relative to | the current tree | a recorded baseline (commit, timestamp) |
| failure blocks | the changer | nobody — it routes to the decision's owner |

**Nearest existing tools, and the gap.** Data-validation suites (Great
Expectations et al.): executable, but suite-owned, schema-oriented, pass/fail —
no capture-at-diagnosis, no decision links. ADRs: capture decisions and context
beautifully — nothing is executable, so they rot like all prose. Snapshot
tests: drift-tolerant, but anonymous and code-owned. Architecture tests: the
right assertions with the wrong lifecycle (see above). The gap is the
intersection: **executable like a test, attributed and adjudicated like a
decision record, and cheap enough to write mid-investigation that it actually
gets written.**

---

## 2. Vocabulary

- **Claim** — a recorded statement of fact: text, author, timestamp, and
  *basis* (the commit/context in which it was established).
- **Decision record** — the durable artifact the claim justifies (an exception
  entry, a pin, a skip, a config choice). Claims exist *only* attached to
  these (§3).
- **Verifier** — a means of re-checking the claim: `cmd` (a command; exit 0 =
  holds), `agent` (a natural-language verification instruction executed by an
  agent), or `human` (routed to a named owner as a review item). A claim may
  carry several (§6).
- **Trigger** — when verification is due: `on-change:<paths>`, `every:<duration>`,
  `on-event:<source>` (future), or `manual` (§5).
- **Verdict** — the result of a verification: `held`, `drifted`, `unverifiable`
  (could not determine — distinct from drifted), or `error` (verifier itself
  broken — a rot signal in its own right). Each verdict is appended to the
  claim's history with attribution and, for agent/human verifiers, an evidence
  note.
- **Drift** — a claim whose latest verdict is `drifted`. Drift is *not*
  failure. It opens an adjudication, owned by the decision's owner:
  - **retire** — the world changed legitimately; the decision was re-reviewed
    and no longer needs the premise (or was itself removed).
  - **amend** — update statement and verifier to the new truth; history
    preserved.
  - **promote** — the fact turned out to be an invariant the team wants
    *enforced*; emit a real test/CI gate (now carrying its reason), and mark
    the claim promoted. This is the deliberate, explicit bridge from
    observation to spec — the answer to "shouldn't these just be tests?"
    (some should — *after* they've earned it, with their why attached).

---

## 3. Rule one — granularity: claims ride existing writing

**Claims never create writing; they attach to writing that already exists.**
The trigger for authoring a claim is not "I learned a fact" — it is "I am
recording a decision in a durable artifact whose justification cites a fact
someone else could change." If nobody would write the sentence anyway, there is
no claim.

This bounds the corpus by the number of *decision records*, not by the amount
of knowledge — which is what makes the idea tenable at all. A mature repo has
tens-to-hundreds of exception entries, pins, and skips; not tens of thousands.
And those entries already carry prose reasons (many suppression formats
*require* one), so the marginal cost of a claim is pasting the command the
investigator just ran to convince themselves.

The negative example, to make the boundary vivid: "`parseHeader()` has a single
call site" — a fact someone might note mid-refactor — is **below the
threshold** unless it is the recorded justification of a durable decision. If
it lives only in a PR description, a claim adds ceremony to something nobody
will maintain. Comments and review remain the right tool below the line, and
the marginal claim there has *negative* value. A one-sentence authoring test:

> *If you are writing an exception, suppression, pin, skip, or config choice,
> and its justification mentions a fact someone else could change — bind the
> fact to a verifier. Otherwise, don't.*

---

## 4. Rule two — storage: co-located annotations, no registry

Claims live **inside the decision artifacts they justify** — a `claim:` key
next to the `reason:` key in an exceptions YAML, a structured trailer on a
suppression line, a block in the lockfile comment. There is no parallel
`claims/` registry to maintain, no sync problem, no orphaning: deleting the
exception deletes its claim.

The tool is therefore a **harvester + runner**, not a database: `claim check`
walks the tree for annotations (a small set of extractors per file kind, plus a
generic comment/trailer syntax), runs what's due, and reports. Git supplies
identity, history, attribution, review, merge semantics, and distribution.
There is no server and no daemon. Two agents adding claims concurrently is two
diffs, resolved like any other.

Example (a scanner-exceptions file):

```yaml
- cve: CVE-2024-31337
  reason: "not applicable: user input never reaches XmlParser"
  claim:
    verify:
      cmd: "! rg -q 'XmlParser' src/handlers/"
    trigger: "on-change:src/handlers/"
    established: { by: "sec-review-2025-11", sha: "abc1234" }
```

When the feature PR routes input through `XmlParser`, the on-change lane
reports: **DRIFTED — "user input never reaches XmlParser" — supports
CVE-2024-31337 exception → route to security-exceptions owner.** The PR is not
blocked. The exception's owner re-adjudicates, which is the correct actor
answering the correct question at the correct time.

---

## 5. Rule three — triggers: the invalidator decides the cadence

**A claim's check schedule is determined by where its invalidator lives**, not
by where CI happens to run:

- **Repo-triggered facts** ("no handler references X", "this config key is
  unused") can only be invalidated by a commit. Check them `on-change:<paths>`
  in CI — exact, and cheap by construction, since a given commit touches the
  watched paths of at most a few claims.
- **World-triggered facts** ("upstream issue #123 still open", "the vendor's
  rate limit is still 100 rps", "the vendor file still lands by 06:00") cannot
  be invalidated by any commit. Running them per-merge is not wasteful — it is
  *categorically wrong*, polling the world on an event stream uncorrelated with
  the fact's change process. They run on a clock (`every: 7d / 30d / 90d`) or,
  eventually, on events (release feeds, webhooks), and they batch: a weekly
  `claim check --due` is one invocation running a dozen verifiers, producing
  one drift report.
- **Cadence = change-rate × staleness-cost.** A pin can drift harmlessly for a
  month (you keep running what you were running) → monthly. A security
  exception has brutal staleness cost and a commit-shaped invalidator → merge
  time, on the exact PR that introduces the reach. Both derivable from two
  questions the author can answer in five seconds.

**Cadence is data; invocation is environment.** Each claim carries its trigger;
the tool's entire scheduling contract is that `claim check --due` is idempotent
and near-free to *ask* (compare last-verified sha/timestamp against triggers;
run only what's due). Where it gets invoked — a path-filtered CI step, a cron,
a scheduled Action, an agent's pre-work ritual — is the operator's business.
The tool ships no scheduler and no opinion about your pipeline, in the same way
git does not decide when you fetch.

---

## 6. The verifier spectrum — where this becomes native to the agentic era

Verification is pluggable, per claim, chosen by the engineer. Not black or
white:

- **`cmd`** — deterministic script, exit code 0 = held. Preferred *where a
  cheap proxy for the premise exists* — and note the discipline this imposes:
  the verifier checks the **premise**, not the **decision**. "Issue #123 still
  open" is one API call; "is the 5.x upgrade now safe" is open-ended
  re-derivation. Claims need only the former — when the premise dies, the
  expensive re-derivation happens once, on purpose, by the decision's owner.
- **`agent`** — a natural-language verification instruction; an agent executes
  it (changelog review, sandboxed repro, research) and returns a structured
  verdict — `held` / `drifted` / `unverifiable` — plus an evidence note that is
  appended to the claim's history. This is the honest home for premises with
  **no structured proxy** (the bug was found internally; there is no issue to
  poll): the claim text plus the instruction *is* the probe. Agent verifiers
  naturally live in the clock lane, where cost amortizes: expensive per check,
  cheap per month.
- **`human`** — routed to a named owner as a periodic review item. Used
  sparingly, for premises that are genuinely judgment (aesthetic and product
  assumptions have this shape). Pretending these are mechanically checkable
  would be dishonest; leaving them unrecorded is worse.
- **Tiered** — one claim, several verifiers at different cadences: a cheap
  proxy weekly (`cmd`: is the issue open), a deep check quarterly (`agent`:
  does the bug still reproduce). The proxy catches common drift fast; the deep
  check catches the case where the issue closed as wontfix but the bug quietly
  vanished anyway.
- **None (`manual`)** — legal. A dated, attributed statement with no verifier
  is still strictly better than a comment: it is enumerable
  (`claim list --unverified` is your acknowledged epistemically-soft debt),
  historied, and a future engineer or agent can *add* a verifier when one
  becomes possible.

The compounding artifact is the **history**: every verification appends
`{timestamp, sha, verdict, verifier-kind, verifier-identity, evidence}`. Over
time this ledger is the institutional memory an agent workforce otherwise
lacks. A human joining a team absorbs "what do we believe, and how confident
are we" through tenure. An agent, with total amnesia between sessions, gets it
from nothing — unless it is written down in a form that cannot silently rot.
That is the product in one line: **tenure, as a file format.**

---

## 7. Level 1 — the local tool

Philosophy: Unix lineage. Plain text, git as the database, any executable as a
verifier, no DSL, no server, no scheduler. Porcelain over a dumb store.

**Verbs**

```
claim add        # interactive/flags: attach a claim to a decision record
claim check      # [--due | --all | --lane cmd|agent|human | --supports <path> | --since <sha>]
                 #   → held / DRIFTED / unverifiable / error, per claim; --json for agents
claim drift      # drifted claims + the decision each supports = the review queue
claim retire     # world changed legitimately; decision re-reviewed   (adjudication)
claim amend      # update statement/verifier to the new truth         (adjudication)
claim promote    # emit a test/gate stub carrying the reason; mark promoted
claim list       # inventory; --unverified surfaces claims never genuinely verified
claim log <id>   # truth status over time (thin wrapper over history + git log)
```

**Semantics worth pinning:** `check` never mutates anything except appending
verdicts; exit codes are scriptable (0 all held; 1 drift present; 2 errors);
drift never blocks anything unless a claim is explicitly marked
`on_drift: fail` (which makes it a test-in-waiting — a deliberate, visible
escalation, not a default). `--json` output is a first-class interface: agents
are expected to be the heaviest readers.

**The three lanes, all riding existing checkpoints:** (1) CI, advisory,
on-change-filtered — repo-triggered claims only; (2) the clock lane — cron or
scheduled job invoking `check --due`, where agent/human verifiers live;
(3) pre-work — an agent about to touch an area runs
`claim check --supports <paths>` and inherits verified premises instead of
re-deriving them.

**What it refuses to be:** a test runner (no fixtures; verifiers are
processes), a build system (no caching/ordering; expensive verifiers carry
`cost:` tags and are excluded from broad sweeps), a knowledge graph (statements
are for humans; only verifier, trigger, supports are machine-meaningful), a
scheduler, or a hosted service.

**MVP cut:** a single script/binary; extractors for YAML-style annotations + a
generic comment-trailer syntax; `add/check/list/log`; sha-stamped verdict
history. Second pass: `drift/retire/amend/promote`, `on-change` via
`git diff <last-verified-sha>..HEAD`, `--json`, `--since`. Validation: adopt in
one mature repo's suppression/exception/pin corpus and replay recent history —
success is the tool flagging known premise-rot incidents *at the commits where
they actually drifted*.

---

## 8. Level 2 — the organization: many repos, many agents, one ledger

The local tool is complete in itself. The organizational layer is where the
thesis pays off — and where discipline about *capabilities vs. implementation*
matters most. This section specifies capabilities; stores and protocols are
deliberately deferred (§8.5).

### 8.1 The prime directive: repos remain the source of truth

Claims stay co-located in the repos that own their decisions — reviewed,
versioned, merged like code. The global layer is a **derived index and an event
bus, never an authority**. Writes happen only via commits/PRs in owning repos
(including machine-authored verification verdicts, which land as commits — so
every verdict is auditable and revertable). This inherits the entire git trust
model and dissolves the worst governance question ("who may write to the truth
database?") before it is asked. World-facts with no natural home repo get a
designated org-level *commons* repo — still just git.

The relationship to repos is that of a code-search index or package registry to
source: fast, queryable, rebuildable from scratch, and wrong only ever by
being briefly behind.

### 8.2 Capabilities the global layer should provide

1. **Discovery.** "What claims exist about service X / dataset Y / dependency
   Z / this API I'm about to call?" — queryable by path, entity, dependency,
   tag, owner, staleness, verdict. The agent-onboarding workflow, org-wide: a
   session starting anywhere begins from the org's verified premises instead
   of from zero.
2. **Cross-repo drift routing.** The core problem at org scale is that the
   invalidator and the decision owner are on *different teams*. Team A's
   timeout rests on a claim about Team B's latency; the invalidating change
   lands in B's repo. The index links the claim's watched facts to their home
   repos and routes drift notifications across the boundary — the thing no
   amount of per-repo tooling can do.
3. **Canonicalization and amortized verification.** Forty services pin
   assumptions on the same vendor's rate limit. The index recognizes
   equivalent world-claims, designates a canonical instance (in the commons
   repo), verifies once, and fans the verdict out to all subscribers. One
   agent-verification per month serves forty teams.
4. **Subscriptions.** Any team or agent can subscribe to drift on a selector
   ("anything supporting our service's exceptions"; "any claim about
   libfoo"). Delivery mechanism deferred; the capability is the selector.
5. **The org risk register, for free.** Because claims attach to decision
   records, the index can answer: "every security exception org-wide, sorted
   by premise staleness"; "every team whose decisions rest on unverified
   claims"; "the ten oldest drifted-and-unadjudicated claims." Nobody builds
   this view today because the data doesn't exist in checkable form.
6. **A verification workforce.** The clock lane at org scale: a budgeted agent
   pool pulls due claims across repos, executes verifiers within cost limits,
   and submits verdicts back as PRs/commits to the owning repos. Cost
   accounting per team; budgets are policy, not tool.
7. **Attribution and track record.** Every claim and verdict carries its
   author (human or agent) and evidence. Over time the ledger supports
   questions like "how often do this source's claims survive verification?" —
   useful signal for weighting trust in a mixed human/agent workforce. (Flagged
   as a capability, with a warning: this is the top of a slippery slope toward
   reputation systems; see §8.4.)

### 8.3 Interfaces (sketch — capability level only)

- **Write path:** git, exclusively (commits/PRs to owning repos). No API
  writes.
- **Read path:** a query API for humans and agents (the agent surface should
  be trivially consumable by LLM tooling — structured, self-describing,
  filterable); a subscription/notification surface; per-team drift-review
  queues as the primary human UI.
- **The index's own freshness is a claim** ("index lag < N minutes"),
  verified like any other. The system should be able to describe itself.

### 8.4 Failure modes to design against (for the reviewer's attention)

- **Ontology creep.** The temptation to canonicalize statement *semantics*
  (schemas, entity models, a claims language). Resist: statements are prose
  for humans; canonicalization (§8.2.3) should be assisted matching, not an
  ontology.
- **Claim spam.** At org scale the granularity rule (§3) is load-bearing: if
  claims decouple from decision records, the index becomes an unqueryable
  swamp and trust dies. The rule must be enforced culturally and by review,
  not by the tool — the tool can only make the ratio visible.
- **Authority drift.** Every pressure will push the index toward becoming the
  source of truth ("just write to the API, it's faster"). Refusing this is
  the design's spine; §8.1 is non-negotiable.
- **Verification cost explosion.** Agent verifiers without budgets become a
  denial-of-wallet attack on yourself. Budgets, cost tags, and lane
  separation are first-class from day one.
- **Reputation-system capture.** §8.2.7 used carelessly turns a knowledge
  ledger into a leaderboard, with all the gaming that implies. Track record
  should inform *verification cadence* (verify distrusted sources more
  often), not gate participation.

### 8.5 Explicitly deferred

Backing store for the index (a search index over harvested files is probably
sufficient for a long time); event-bus technology; consistency model (eventual,
since repos are truth); identity/authn (org SSO); the wire protocol for the
agent read surface. These are implementation choices that should be made as
late as possible, and the capability list above is written to survive any
reasonable choice.

---

## 9. When not to use this

A solo developer on a slow-moving codebase holds both the facts and the
decisions in one head; comments genuinely suffice, and this tool would be
ceremony. The system pays where knowledge is sharded across actors — many
agents, many teams, high change velocity — because that is where the
invalidator of a fact cannot know the fact was load-bearing. If claims are
being written that nobody would have written as prose reasons anyway, the
granularity rule is being violated and the corpus is turning into noise;
prefer deleting claims to accumulating them.

---

## 10. Open questions for review

1. **Naming and framing.** Is "claims" the right noun? Is "redefining CI" the
   right pitch, or does it invite tribal argument that the tool doesn't need?
2. **Hermeticity tagging.** Should world-facing verifiers be formally marked
   (`world:` vs `repo:`) so lanes can be enforced rather than conventional?
3. **Claims supporting claims.** Derivation chains (claim B rests on claim A)
   are powerful and dangerous — is the knowledge-graph temptation worth the
   expressive gain, or should `supports` point only at decision records,
   forever?
4. **`promote` semantics.** What exactly is emitted (a test stub? a CI gate
   config?), and does the claim remain in the ledger as history or leave it?
5. **Blocking drift.** Is `on_drift: fail` a necessary escape hatch or the
   crack that re-collapses claims into tests? Argue both ways.
6. **The extractor surface.** Co-location requires format extractors (YAML,
   TOML, comment trailers, lockfiles). Where is the line between "harvester
   with a few extractors" and "a parser zoo the tool drowns in"?
7. **Verdict trust.** When an agent verifier reports `held`, how is *that*
   audited? (Evidence notes are the current answer — are they enough?)
8. **Adoption wedge.** Which single decision-record corpus (security
   exceptions? dependency pins? flaky-test skips?) is the best first market —
   highest rot pain, clearest owner, easiest extractor?
9. **The org layer's minimum viable capability.** If only one of §8.2's seven
   capabilities can be built first, the authors believe it is cross-repo drift
   routing (§8.2.2). Challenge this.
10. **What would falsify the whole idea?** Proposed: adopt in one mature repo
    (§7 MVP validation); if the replayed history surfaces no real premise-rot
    incidents, or surfaces them without anyone caring, the tool should not
    exist.
