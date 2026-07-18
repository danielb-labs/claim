# Parked decisions

Design changes we've agreed are worth making but have deliberately deferred, so
the reasoning isn't lost. Each entry is a decision waiting to be scheduled, not a
bug. Newest first.

## Rethink witnessed-red: demote from mandatory to optional

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

**When to do it.** As a small follow-up that simplifies item 04, after the current
build items land. Reconcile CLAUDE.md invariant #5 and PRODUCT.md in the same
change.
