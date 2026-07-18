# The CLI / hub boundary — where the truth lives, and where the telemetry lives

Status: proposed (v2 direction). This note pins a decision reached in design
discussion; it changes a golden invariant, so it is written down before any code
moves. It supersedes the parts of `PRODUCT.md` it contradicts once accepted.

## The problem

v1 commits the **verdict log** — every check's result over time — into git under
`.claims/log/`. That conflates two different things:

- the **claim** (a fact plus how to verify it) — *source*, authored and reviewed;
- the **verdict** (whether the fact held, at some moment) — *telemetry*, the output
  of running a check.

Committing telemetry to git is the mistake. It pollutes pull requests, accumulates
thousands of tiny files, causes merge conflicts, and bakes operational state
(when is a claim "due"? is it "stale"?) into a tool that should be stateless.
Nobody commits `test-results.xml`; a verdict is a test result.

## The model: three layers

1. **The claim file (git) — the truth.** A statement, its check(s), its `supports`,
   and a `hub:` subfield of scheduling hints. Source of truth for *what is believed*
   and *how to re-verify it*. Reviewed in PRs, attributed by `git blame`. Nothing
   operational lives here.

2. **The CLI (standalone, stateless) — a runtime verifier.** Reads claim files, runs
   their checks, and reports `held` / `drifted` / `broken` **right now** (human, or
   `--json`). It stores no verdicts, computes no staleness, tracks no due-dates,
   keeps no birth certificate. It never connects to anything. This is the whole of
   local development and the whole of an on-change CI lane.

3. **The hub (per-environment) — the ledger and the scheduler.** A separate system —
   at its simplest a scheduled job that runs the CLI and pipes `--json` into a
   database. It owns the verdict stream, *its own* schedule of what is due when,
   staleness, dashboards, drift routing, and the adversarial spot-audit. A QA hub and
   a production hub track the same claims on different cadences with different
   histories — impossible to encode in one committed file, trivial in a per-hub store.

## The claim schema

The claim keeps the fact, the check, and the `supports` graph edges. The scheduling
fields move under a `hub:` subfield — **co-located and reviewable, but consumed only
by the hub**:

```yaml
id: payments/libfoo-pin
statement: We pin libfoo at 4.2 because 5.x corrupts CJK PDFs.
checks:
  - kind: cmd
    run: "grep -q 'libfoo==4.2' requirements.txt"
    # `skip` may stay on the check (whether to run it *here*). `when` is gone —
    # see below.
supports:
  - requirements.txt#libfoo
hub:
  recheck: 30d         # cadence hint for the hub scheduler; a hub may override.
```

The CLI **validates** `hub:` syntactically (a malformed `recheck` is a loud parse
error, so a hub never ingests garbage) but does **not act on it**. The hub reads it
as a default and may override it in its own config.

**`when` is removed.** Whether a fact is re-checked on a code change or on a clock is
*orchestration* — a CI step or the hub's scheduler decides it — not a property the
claim asserts about itself. A PR lane runs a cheap subset on every change; a scheduled
lane runs the rest. This depends on the CLI gaining real **selection** (`claim check
<id>` / `--path` / `--kind`), which issue #19 restores; without selectors, "run a
subset" has no expression. The cadence *hint* lives under `hub:` (`recheck`).

## The verdict is telemetry, not source

`claim check` runs the check and **reports** the outcome; it never writes it back.
The `--json` output *is* the interface — a hub, a CI lane, or a person consumes it.
There is no `.claims/log/`, no committed verdict, no side channel.

## Trust: invariant #4, rewritten

v1's invariant #4 was "a write to the truth is a commit; the tool appends verdicts as
files that get committed; there is no side channel." That was the trust story: git
can't be forged. Under this model it becomes:

> **The truth — the claim — is a commit. A verdict is a reported observation, never
> committed. The trusted authority for verdicts is the pipeline that produced them
> and the hub that stores them, the way a green CI check is trusted without being
> committed to the repo.**

Trust for a verdict now comes from *provenance of production*, not from git: an
authenticated production pipeline attests "these claims held at this deploy," stamped
with the commit, the environment, and the CI identity — the SLSA / signed-attestation
model. The pipeline is the attester; the hub is the ledger.

The other invariants reframe, not break:

- **#3 (derived, not stored):** still true. Claim provenance is git's; verdict
  provenance and status are the hub's, derived from the stream it holds.
- **#5 (a passing check verifies the fact):** becomes a **birth gate**, not a stored
  certificate — `claim add` still refuses a claim whose check does not currently hold,
  but writes no establishing verdict. A false claim is caught by the next check, so
  the receipt is unnecessary.
- **#1, #2, #6:** unchanged in spirit. The broken-never-passes mapping and
  tool-owned negation are check-execution, which stays in the CLI. The nag "never a
  lie" still holds — it now issues from the hub, which knows due-dates and staleness;
  the CLI just reports current truth loudly.

## The two directions of CLI↔hub

- **hub → CLI: none.** The hub reads the claim files (it has the repo) and runs its
  own checks; it never has to instruct the CLI.
- **CLI → hub: exactly one path, in the pipeline domain.** On push-to-production or on
  a PR, a GitHub Action runs the checks and pushes the **authoritative, attested
  evidence** to the hub. Crucially, **this lives in the hub's own CI glue (a GitHub
  Action the hub ships), not in the core CLI.** The CLI stays hub-agnostic — it
  verifies and emits `--json`; the hub's action wraps `claim check --all --json` and
  POSTs it with authentication. So the binary never grows a hub URL, token, or
  protocol, and the same binary points at a QA hub or a prod hub with zero change.

## What changes in the code

- **CLI keeps (the trust core, untouched):** the claim model, check execution and the
  honesty mapping (exit → held/drifted/broken), `negate`, `skip`, `supports`, the
  parser, and the authoring verbs (`add` as a birth gate, `amend`, `retire`, `check`).
- **CLI sheds:** the `.claims/log/` verdict tree and all of `log.rs`'s status
  computation; the `Verified/Drifted/Stale/Retired` status model; the `when`/`Trigger`
  field, due-ness, and `scheduling.rs`; `--report-only` (the CLI never writes now, so
  it is the only mode); the `CLAIM_NOW` clock seam. `drift` becomes "run checks, show
  the drifted ones," not "read the log." `list` becomes a plain inventory, not a status
  view. `retire` removes the claim (git *is* the changelog: `git log .claims/`), rather
  than writing a retirement event.
- **CLI gains (a prerequisite):** selection on `check` — `claim check <id>` / `--path`
  / `--kind` — so a CI step can run a cheap subset on PRs and the rest on a clock, now
  that `when` no longer partitions them (issue #19).
- **Claim file demotes, not removes:** `max_age`/cadence move under `hub:` — validated
  by the CLI, consumed by the hub.
- **Establishing verdicts** committed by v1 (e.g. the eight dogfood claims and the
  agent claim) are deleted from git; they become hub-side telemetry, or nothing.

## How this composes with the Fable generalization review

- **Finding #1 (multi-check per-check identity):** the *storage* half dissolves — there
  is no committed log to redesign. The *semantics* half survives and matters: whatever
  the hub stores must carry check identity, and a shallow check's pass must never clear
  a deep check's drift. Design the hub's verdict schema with per-check identity from the
  start (issue #18).
- **#2 shared store-query primitive, #6 find-claim dedup:** still worth doing; some
  status surface changes as the status model leaves.
- **#5 typed LoadError / structured adjudication:** the LoadError half stands; the
  verdict/adjudication half is subsumed by the verdict stream leaving git.
- **#9 `--stale`, #26:** likely absorbed as the status surface changes.
- Everything else (renderer, witness, embedded claims, wiki_links) is independent.

## Resolved (design discussion)

1. **`when` is removed.** Trigger/cadence is CI/hub orchestration (above), gated on the
   CLI gaining selectors (#19). A cadence hint lives under `hub:` (`recheck`).
2. **`list` is a plain inventory** — id, statement, file, supports; no status. `drift`
   becomes "run checks, show the drifted ones"; `stats` folds away or becomes a hub view.
3. **`retire` stays but writes no log:** it removes the claim, and the changelog is git
   history (`git log .claims/`), which the hub can render. A `retired:` tombstone field
   kept in-tree is the alternative, worth it only if browsing retired facts in place
   beats the git log; decide when built.

## Still open

- The exact `hub:` schema keys (`recheck`, and anything else the scheduler needs).
- How the hub's GitHub Action authenticates and attests to the hub (out of CLI scope;
  the model is a signed attestation / OIDC to the hub).

## Implementation approach

This is a large, subtractive change that removes subsystems and rewrites a golden
invariant. It is staged, and CLAUDE.md and the invariant list are updated in the same
work. **The log-removal MR is built by a subagent** under orchestrator review, with the
two-reviewer adversarial pass on the diff. The trust-critical check-execution core is
explicitly *not* touched; the risk is in the deletions, so the reviewers' mandate is
"prove nothing load-bearing was removed with the log."
