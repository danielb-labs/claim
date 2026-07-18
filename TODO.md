# Parked decisions and deferred work

Design changes and capabilities we've agreed are worth capturing but have
deliberately deferred, so the reasoning isn't lost. This is a record of
decisions, not a bug list. The broad post-v1 roadmap lives in PRODUCT.md §7
("Deliberately absent from v1") with build-signals; this file captures what came
out of working sessions and the few capabilities worth calling out by name.

## Near-term follow-ups

Small changes to make soon, after the current build items land.

### Rethink witnessed-red: demote from mandatory to optional

**Status:** parked 2026-07-18, pending decision. Touches golden invariant #5 and
simplifies the already-merged item 04 (`claim add`).

**Today.** A check must be *seen failing once* before `claim add` trusts it
(invariant #5). In practice `add` runs a green→red→green dance: it runs the check
(expects `Held`), runs `--witness-cmd` to break the fact (expects `Drifted`),
restores the tree, and confirms `Held`. If the red can't be staged, `--unwitnessed`
records the claim but marks it unverified.

**Why it's wrong.** Four separate problems, all pointing the same way:

1. **Impossible for world-facts.** You can't stage the red for "libfoo 5.x still
   corrupts CJK PDF export" — that would mean fabricating a future fixed release.
   A whole class of facts ("a fix shipped", "the vendor changed the limit", "the
   upstream bug was closed") has a red that is a future event in the world, not a
   file you can edit.
2. **The "unverified" label is too harsh.** A claim whose agent check runs on a
   clock and passes *is* verified — the check really did evaluate reality.
   Witnessed-red conflates two different things: "is the fact currently confirmed?"
   (verification) and "can this check discriminate?" (a separate worry, and mostly
   only about dumb mechanical checks — a `grep` for the wrong string).
3. **Agent-hostile.** The perturb/restore dance needs a clean tree and a
   perturbation command; agents work in dirty trees.
4. **It caused item 04's data-loss Critical.** The `git checkout -- .` restore
   existed *only* to undo the perturbation.

**The change.** A passing check verifies the fact, full stop. `--witness-cmd`
becomes an optional convenience for cheap mechanical (`cmd`) checks whose red is
easily stageable. Agent checks and world-facts are verified by running against
reality — never asked to fabricate a red, never marked "unverified" for not doing
so. The real guard against a dumb check is review at creation (a human or agent
reading "your grep targets the wrong file") plus pairing a cheap proxy with an
agent check that evaluates the true fact. If we want to surface "this check hasn't
been demonstrated to discriminate," do it as neutral metadata on `claim log`, not
as a status penalty.

**Payoff.** One change dissolves all four problems: no fabrication, no harsh
label, an agent-friendly `add`, and the tree-perturbation (and its data-loss risk)
gone. `claim add` simplifies to: run the check once, require it passes, write the
claim.

**Implementation note.** If we keep any at-creation witnessing for stageable
checks, run the green→red→green dance in a throwaway `git worktree` at HEAD so it
never touches the user's or agent's working tree — which also removes the
clean-tree requirement. Mostly moot if witnessing becomes optional.

**When to do it.** As a small follow-up that simplifies item 04, after the current
build items land. Reconcile CLAUDE.md invariant #5 and PRODUCT.md in the same
change.

### Fold the documentation site into the repo

A single self-contained HTML docs site was built during a session at
`~/claim-docs/` (concepts, CLI reference, the diagrams, an FAQ). Move it into the
repo as a versioned `docs/` (or `site/`) item so it ships with the tool and stays
in sync as verbs land; wire the diagrams from `diagrams/`. Right now it lives
outside version control and will drift.

### Reconsider an MCP create/propose verb

We cut `propose` from the MCP server (item 07 is `query` + `report` only), on the
logic that an in-repo agent just writes the file or runs `claim add`. The
agent-creation ergonomics discussion reopened it: if witnessed-red is demoted
(above), `add` becomes simple enough that a thin MCP `create` wrapper may be worth
it, so agents can record a claim without shelling out. Decide alongside the
witnessed-red change.

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
