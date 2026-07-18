# Parked decisions and deferred work

Design changes and capabilities we've agreed are worth capturing but have
deliberately deferred, so the reasoning isn't lost. This is a record of
decisions, not a bug list. The broad post-v1 roadmap lives in PRODUCT.md §7
("Deliberately absent from v1") with build-signals; this file captures what came
out of working sessions and the few capabilities worth calling out by name.

## Near-term follow-ups

Small changes to make soon, after the current build items land.

### Rethink witnessed-red: demote from mandatory to optional — DONE (item 09)

**Status:** implemented on branch `item-09-witnessed-red-demote`. Golden invariant
#5 rewritten ("a passing check verifies the fact"), PRODUCT.md sections 3/5/6/7
reconciled, `claim add` simplified.

The change, as shipped: `claim add`'s default path runs the check once, requires
`Held`, and writes the claim plus its establishing verdict — never touching the
working tree, never requiring a clean tree. `Drifted`/`Broken` are still refused
(no recording an already-false or unrunnable fact). `--witness-cmd` remains as an
*optional* confidence signal: the perturb→observe-red dance runs in a throwaway
`git worktree` detached at HEAD, so it cannot touch or lose the user's uncommitted
work, and needs no clean-tree guard (it is refused on an unborn HEAD, which has no
commit to check out). The observed red is recorded as evidence on the establishing
entry. Removed: the mandatory perturb/restore on the default path, the dirty-tree
refusal and its `dirty-tree` error kind, the `not-restored` kind, `--unwitnessed`
and `--restore-cmd`, and the `git checkout -- .` restore that was the item-04
data-loss surface. `list --unverified` now means simply "no passing verdict on
record" (the `unwitnessed:` marker is gone).

### Fold the documentation site into the repo

A single self-contained HTML docs site was built during a session at
`~/claim-docs/` (concepts, CLI reference, the diagrams, an FAQ). Move it into the
repo as a versioned `docs/` (or `site/`) item so it ships with the tool and stays
in sync as verbs land; wire the diagrams from `diagrams/`. Right now it lives
outside version control and will drift.

### Reconsider an MCP create/propose verb

We cut `propose` from the MCP server (item 07 is `query` + `report` only), on the
logic that an in-repo agent just writes the file or runs `claim add`. The
agent-creation ergonomics discussion reopened it: now that witnessed-red is demoted
(above, item 09), `add` is simple enough — a single passing check, no tree
perturbation — that a thin MCP `create` wrapper may be worth it, so agents can
record a claim without shelling out. Decide as its own follow-up.

## Post-v1 capabilities

Deferred by design. PRODUCT.md §7 is the full list with build-signals; the two
most consequential are named here because they change what the product *is*.

### Agent checks (`kind: agent`)

The headline deferred capability — arguably the point of the whole product. A
check that is a natural-language instruction an agent executes ("read libfoo's
changelog since 5.0; is the CJK corruption fixed?"), returning a verdict plus
cited evidence. The format already parses `kind: agent`; v1 returns `Unverifiable`
for it (never a fake pass). Execution design is sketched: the clock lane runs an
agent CLI (e.g. `claude -p "<instruction>" --output-format json`) in the
customer's own CI, with their key and a budget, and a sample of `held` verdicts is
re-checked by a second agent instructed to refute the first. This is what makes
previously-uncheckable facts (world-facts, "a fix shipped") checkable — and it is
the honest home for the facts witnessed-red can't stage. Build-signal: when
proxy-only or checkless claims are a real share of a live corpus.

**Verdict protocol (how an agent check communicates its findings).** A `cmd`
check yields its verdict through the exit code (0 held, 1 drifted, anything else
broken) and its evidence through stdout. An agent check needs a richer channel,
because the verdict is a judgment and the *evidence* is the point. So an agent
check returns structured output — `{ verdict: held | drifted | unverifiable,
evidence, citations }` — and the runner maps `verdict` onto the `Verdict` enum and
appends the evidence and citations to the log. The agent process's own exit is
kept as the honesty backstop: a crash, a timeout, an API error, or malformed /
missing verdict output all map to `Broken`, never a fake `Held` — the same
"couldn't run ⇒ not a pass" contract as `cmd`. `Unverifiable` is the honest state
for "ran, but the evidence was conflicting or insufficient to conclude" — which is
exactly why the verdict model has four states, not two. So yes, an agent check can
land on held-vs-drifted by what it finds, just like a `cmd`'s exit 0 vs 1 — but it
carries its reasoning with it rather than collapsing to a bit, and it has an honest
"I couldn't tell" it can return instead of guessing.

### Capture the expensive derivation (the memoization framing)

A claim is best understood as a **verified cache of an expensive derivation**. The
statement is the cached result; the expensive work an agent already did — the
investigation, the evidence, the reasoning — is what a future agent inherits
instead of re-deriving; the check is the cache-invalidation function. That
asymmetry (deriving is expensive, re-checking is cheap) is what makes the whole
system pay off, and it reframes the product from "continuous verification" to
"verified memoization of expensive derivations" — the ledger is the accumulated
cognitive work of an org's agents and people, kept from rotting.

The model already has the slots — the Markdown body can hold the full derivation,
and every verdict carries an evidence note — but two things are under-designed:

1. **The original derivation has no first-class home.** Today it goes in the prose
   body or the establishing verdict's evidence. For a tool whose value is
   *inheriting expensive work*, "how this was established / what you're inheriting"
   probably deserves to be distinct from the one-line statement that shows in
   lists. Decide whether the Markdown body genuinely suffices or a dedicated
   evidence/derivation field is warranted. Sketch a real agent-derived claim on
   disk before deciding.
2. **Two roles for a prompt, worth separating.** The *derivation prompt* (what
   discovered the fact — captured once as evidence) versus the *verification
   prompt* (a usually-cheaper recurring re-check — the `kind: agent` check). Prefer
   a check that watches a cheap proxy of the premise over one that re-runs the whole
   derivation; when no proxy exists, the recurring check simply is a rarer
   re-investigation on the clock. This is the "tiered checks" idea.

### Windowed / SLO-style claims

Some recorded facts are recurring predictions, not static facts — "the vendor file
lands by 06:00 UTC", "p99 latency stays under 200ms". One late day isn't drift.
The verdict model can't express "held if N of the last M runs passed"; either add
windowed verdicts or exclude the class explicitly. Flagged in the original design
critique (an SLO is not a fact).

### The rest — see PRODUCT.md §7 and §8

Suspect status and doubt propagation along the graph; `class` and `tags`;
escalation beyond a PR comment plus a standing issue (nag → owning-team gate →
block); flap damping; read-set tracing for on-change triggers and `--path`;
forge-approval provenance (PR approvers via the forge API, over git commit
authorship); the hub (index, search, review queue, dashboards, scheduler,
runners); cross-repo drift routing; canonicalization of equivalent world-claims;
company-wide connectors (email/chat/docs). Open questions still unresolved:
automatic contradiction detection, `claim move` with id tombstones, and who
governs the graph's edge set.

## Dogfooding findings (item 10)

Running `claim` on its own repo (item 10) surfaced real product signal. The
concrete fixes are scheduled as item 13; the observations worth keeping:

- **Store scanner treats every `.claims/**/*.md` as a claim.** A plain
  `.claims/README.md` is parsed as a claim, fails "missing frontmatter", and
  forces `claim check` to exit 2 — a user will hit this the moment they document
  their store. A non-claim doc (no frontmatter, no embedded block) should be
  skipped; a frontmatter-fenced-but-malformed file must stay loud. (item 13)
- **`--max-age` reads as optional but is required** — non-interactive callers
  (every agent/CI use) hit the error on the first `add`. Mark it required or
  default it. (item 13)
- **`--supports` anchors are a literal substring scan, not GitHub slugs** — and
  an unresolvable anchor is accepted silently at `add`, only caught at `check`.
  Validate/warn at author time and document the rule. (item 13)
- **Every `check` writes verdicts**, so a "clean tree" is a moving target during
  exploration; `--report-only` avoids it but is easy to forget. Consider a
  "wrote N verdicts; commit them" hint, or report-only-by-default outside CI.
- **The agent-check gap is real and concrete.** Invariant #2 (the tool owns
  negation; no shell `!`) *cannot* be a `cmd` claim: the only literal `sh -c "!`
  in the tree is the doc comment warning against it, so any grep catches the
  documentation, not a regression. Proving it needs semantic reading of
  `check.rs` — exactly what `kind: agent` is for. Same for invariant #1 as a
  *behavior* (exit 137 → Broken). These map the gap agent checks (item 12) fill.
- **The granularity rule held up** — it actively stopped spam, rejecting hollow
  candidates ("CLAUDE.md lists 6 invariants", "edition = 2021"). The working
  line: a claim earns its place only when a real decision cites the fact AND the
  check would actually drift if the decision changed.
