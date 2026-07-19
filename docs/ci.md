# CI & the hub

New here? Start with the [overview](index.html) for the core concepts (claims,
checks, verdicts, the CLI/hub boundary); this page is the CI detail behind its "CI
& the hub" section.

The CLI is a **stateless runtime verifier**: `claim check --json` runs every claim's
checks and reports what held, drifted, or broke *right now*. It stores nothing — no
verdict log, no schedule, no due-dates. The verdict stream, the schedule, and the
standing nag live in a per-environment **hub** that ingests the CLI's reported output.

The CLI stays **hub-agnostic**: it verifies and emits `--json`, and never connects to
anything. Wrapping that output — authenticating and POSTing it to a hub, or rendering
it into a PR comment or issue body — lives in the hub's own CI glue (a GitHub Action
the hub ships), not in the core binary. So the same binary points at a QA hub or a
prod hub with zero change, and never grows a hub URL, token, or protocol.

This page explains the exit-code contract the CI glue keys off, the renderer that
turns `claim check --json` into the markdown a human sees, and how those pieces fit a
GitHub workflow. The example workflow and a drop-in consumer copy are in
`examples/consumer/`.

## The exit-code contract

Everything downstream keys off `claim check`'s exit code. It is a total, monotone
mapping — the highest applicable code wins, so a mixed store reports its worst finding:

| Exit | Meaning |
|------|---------|
| `0`  | Every check held and every `supports` target resolved. |
| `1`  | At least one drifted or unverifiable verdict, or an unresolved support — review needed. |
| `2`  | At least one broken check, an unloadable/duplicate-id claim file, or a tool error. |

The rule that matters most: **a broken check is exit 2, never a pass** (golden
invariant #1). A check that could not run tells us nothing, so it is the loudest
condition, not the quietest. Drift never fails the build — see "advisory, never a
block" below — but the renderer uses these codes to group findings and to decide
clean-vs-dirty.

## Running the CLI in CI

**Trigger:** `pull_request` for a per-change run, `schedule` (cron) plus
`workflow_dispatch` for the recurring run.
**Command:** `claim check --json`.
**Permissions:** `contents: read`, plus `pull-requests: write` (to comment) or
`issues: write` (to maintain the standing issue). No write to the repo's default
branch is needed — nothing is committed.

Two properties are load-bearing:

- **`claim check` writes nothing and needs no git identity.** The CLI never persists a
  verdict — a verdict is telemetry, not committed source (golden invariant #4). A fork
  PR's CI has no write token and needs none; it still runs every check and reports.
  There is no `--report-only`, because the CLI is only ever report-only: it has no
  other mode.
- **It never fails the build.** The check step is run so the finding is *routed*, not
  used as a gate. Drift is information delivered at the moment of maximal context — the
  person changing the world learns what their change broke and who owns the decision —
  not a wall in front of the merge. Escalation beyond the notification is deferred (see
  "The v1 escalation boundary").

There is no separate on-change lane and clock lane ending at a committed log. Whether a
subset runs on a PR and the rest on a clock is orchestration the CI step and the hub's
scheduler decide; the CLI just verifies whatever it is pointed at. The cadence *hint*
for a claim lives under its `hub:` subfield (`recheck`), which the CLI validates but
never acts on — the hub's schedule is the hub's.

## Selecting a subset

`claim check` runs every claim by default. Two selectors narrow the run — the
replacement for the removed `when:` field, which used to try to partition claims into
"on change" and "on a clock" from inside the claim file. Cadence is orchestration, not a
property a claim asserts about itself, so it moves out to the CI step:

### The `--path` PR-subset pattern (replacing `when: on-change`)

On a PR, run only the claims that are *about the files this PR touched*, so a change gets
a fast, focused verification instead of the whole store:

```yaml
- name: Verify the claims about the changed files
  run: |
    # The files this PR changed, against the base branch.
    git fetch origin "$GITHUB_BASE_REF" --depth=1
    changed=$(git diff --name-only "origin/$GITHUB_BASE_REF"...HEAD)
    # Run claim check once per changed top-level path prefix, aggregating the exit
    # codes so no iteration's finding is lost. A `run:` block is not `set -e` by
    # default, so a naive loop's step code would be only the LAST iteration's — a
    # drift (1) or broken check (2) in an earlier prefix would be silently swallowed,
    # the exact stale-green this tool exists to prevent. `rc` keeps the worst code and
    # the step exits on it. A --path that matches no claim is not an error (exit 0,
    # "no claims match"), so an unrelated change simply verifies nothing.
    rc=0
    for dir in $(echo "$changed" | xargs -n1 dirname | sort -u); do
      claim check --json --path "$dir" || rc=$?
    done
    exit $rc
```

`--path` matches a claim whose file *or* whose `supports` decision ref lies under the
prefix — the same match `claim list --path` uses — so "the claims about `src/auth/`"
means the same thing whether you are listing or checking them. A prefix that matches no
claim exits `0` and reports "no claims match," never a false "all held," so a PR that
touches paths no claim covers is cleanly empty, not a failure.

One caveat on the prefixes: `dirname` of a top-level changed file (e.g. `README.md`) is
`.`, and `claim check --path .` is the empty prefix, which matches the *whole store* — so
a change touching a root-level file re-runs every claim. That is a safe over-run (it
verifies more, never less), and the exit aggregation above keeps it honest — a broader
run's worst code still propagates; drop the `.` entry if you would rather a root change
stay a narrow subset.

The scheduled lane drops `--path` and runs `claim check --json` over the whole store, so
nothing a PR skipped goes unverified over time.

### The two-speed pattern via `skip.unless` (a check that runs only nightly)

Some checks are expensive — an `agent` check that spends a model's budget, or a check
that needs a runner a PR box does not have. Rather than a coarse `--kind` filter, a check
carries its own condition for when it should run, and is *honestly skipped* the rest of
the time (a skipped check is reported, never a false green):

```yaml
# In the claim file:
checks:
  - kind: agent
    instruction: is the CJK corruption still unfixed in libfoo 5.x?
    skip:
      reason: agent checks run on the nightly lane, where a runner is configured
      unless: 'test -n "$CLAIM_AGENT_CMD"'
```

`unless` runs the check when its command exits `0` and skips it when the command exits
`1` — so the condition reads as "true when you want it to run." On a PR, with no
`CLAIM_AGENT_CMD` set, `test -n "$CLAIM_AGENT_CMD"` exits `1` and the check is cleanly
skipped: the PR run reports it as `skipped` (with the reason), and its summary says so
(`held; N check(s) skipped`) — never "all held," so the deferral is visible, not hidden.
The nightly lane sets `CLAIM_AGENT_CMD` to a real runner, `unless` then exits `0`, and
the same check runs. This keeps cadence per-check and honest, where `--kind` would have
partitioned the store from the outside.

`--kind` selection was dropped in favor of this: a `skip.unless` condition is already
honest (a skipped check can never masquerade as a pass), and it lets one claim carry both
a cheap always-run check and an expensive nightly-only check side by side.

## From `--json` to the hub

The one CLI→hub path is in the pipeline domain: on push-to-production or on a PR, the
hub's GitHub Action runs `claim check --json` and pushes the authenticated, attested
evidence to the hub — stamped with the commit, the environment, and the CI identity.
Trust for a verdict comes from *provenance of production* (the authenticated pipeline
that produced it), the way a green CI check is trusted without being committed to the
repo. That authentication and attestation live in the hub's action, out of CLI scope.

That Action ships with the hub as **`hub-ingest`**
(`.github/actions/hub-ingest`): it needs `permissions: id-token: write` to mint the
runner's OIDC identity, and it **fails the CI step loudly on any non-2xx from the hub** —
a rejected or broken push never passes as green. See the hub doc's
[Pushing verdicts from CI](hub.md#pushing-verdicts-from-ci-the-ingest-action) for the
adoption workflow and the `id-token: write` / audience configuration.

For a human-facing surface without a full hub, the same `--json` feeds the renderer
below, which posts a PR comment or maintains one standing issue.

## Idempotency — one comment, one issue

The rendered surfaces are strictly idempotent, so a hundred pushes or a hundred nights
leave one comment and one issue, not a hundred.

- Every rendered body opens with a hidden HTML marker: `<!-- claim-bot:on-change -->`
  for the PR comment, `<!-- claim-bot:clock -->` for the standing issue. The two markers
  are distinct so a repo rendering both never edits one surface from the other.
- The comment step lists the PR's comments and edits the one whose body contains the
  comment marker, creating a new comment only when none exists.
- The issue step lists open issues carrying the `claim` label and edits the one whose
  body contains the issue marker, creating one only when there is a queue and none is
  open, and **closing** it when the store is clean.

A `concurrency` block on the workflow additionally serializes runs against the same PR
or store, so two runs can never race to double-post.

## The renderer — where the logic is, and how it is tested

The transformation from `claim check --json` to a markdown comment or issue body — the
grouping, the supports and statement rendering, the CODEOWNERS-owner lookup, and the
clean-vs-dirty decision — is not in the workflow YAML. It lives in `ci/render.mjs`
(Node, no dependencies) and is unit-tested in `ci/render.test.mjs` against real
`claim check` JSON fixtures in `ci/fixtures/`: `clean.json`, `one-drift.json` (a drift
with a support), `mixed.json` (a broken check, a drift, and an unresolved support in
one store, grouped by severity), and `load-error.json` (an unloadable claim file in
`errors[]`, which keeps the store dirty even though every check that ran held). The
workflow only runs `claim`, calls the renderer, and hands its output to the GitHub API
— so the part a reviewer might get wrong is the part covered by tests.

The tests run in the repo's own gate (`scripts/check.sh`), so a change to the CLI's
JSON shape or the renderer breaks the build rather than silently mis-rendering in
production.

### The `claim check --json` shape

The renderer consumes exactly this shape:

```json
{
  "status": "ok",
  "exit": 1,
  "checked": 1,
  "ran": 1,
  "skipped": 1,
  "claims": [
    {
      "id": "payments/libfoo-pin",
      "file": ".claims/payments/libfoo-pin.md",
      "checks": [
        { "index": 1, "verdict": "drifted", "end": { "kind": "exited", "code": 1 },
          "detail": "exit 1", "evidence": null, "note": null }
      ],
      "skipped": [
        { "index": 0, "reason": "agent check runs on the nightly lane", "until": null }
      ],
      "supports": [
        { "target": "requirements.txt#libfoo", "resolved": true, "reason": null }
      ],
      "exit": 1
    }
  ],
  "errors": []
}
```

The envelope's `checked` counts the claims evaluated; `ran` counts the checks that
produced a verdict and `skipped` counts the checks a declared skip suppressed, both
summed across every claim. The two run-level counts let a consumer see "this run
verified nothing" — `ran == 0` — without re-deriving it from the per-claim results, and
they keep the honesty explicit: `ran == 0` is never "all held," because a skip is not a
pass (golden invariant #6). A selection that matched no claim (an empty `--path`) reports
`checked`, `ran`, and `skipped` all `0`.

Each claim carries its `id`, `file`, the per-check `checks[]`, any `skipped` checks, the
`supports[]` with their resolution, and the claim's own worst `exit`. Each entry in
`checks[]` carries its declared `index` (its zero-based position in the claim's declared
check list), a `verdict`, its process `end`, a `detail`, `evidence`, and a `note`; each
`skipped` entry carries the same declared `index` plus its `reason` and optional `until`.
The `index` matters because **`checks[]` is compacted**: a check whose skip was in force
is omitted from `checks[]` (it appears in `skipped[]` instead), so a check's *position in
the array is not its declared index* once a skip precedes a run check. In the example
above the surviving check sits at array offset 0 but reports declared `index` 1 — the
skipped check took declared index 0. A consumer that ties a verdict back to a specific
declared check (the hub, keying a verdict on the check's content identity) must read the
`index` field, never the array offset. Top-level `errors[]` holds unloadable or
duplicate-id claim files. There is no `selection`, `report_only`, `now`, or top-level
`notes`, and no per-check `when` — selection is a command-line concern (positional ids
and `--path`), not something the report echoes back, and the CLI no longer persists,
timestamps a run against a stored log, or carries a trigger.

### CODEOWNERS and statements

The CODEOWNERS lookup implements GitHub's rule that the **last matching pattern wins**,
over the subset a claims store needs: a catch-all `*`, directory patterns (a bare
`payments/` matches at any depth, a leading-slash `/payments/` anchors to the repo
root — matching GitHub, so a store under `.claims/` routes correctly), and basename
globs (`*.md`). An unowned claim is rendered as a visible routing dead-letter, never
silently dropped.

The statement — the plain-language fact — is not carried in `claim check --json`, so the
renderer reads it from the claim file the checkout already has (the markdown body after
the frontmatter). This is a display convenience only: a file it cannot read (or an
embedded claim inside another file) simply omits the statement line rather than
guessing. The authoritative parser stays in `claim-core`.

## The v1 escalation boundary

The escalation ladder in v1 has exactly two rungs, and neither blocks a merge:

1. **The PR comment** — advisory, at the moment of maximal context.
2. **The standing issue** — the durable nag that outlives any single PR.

There is deliberately **no gate and no block** in v1 (docs/design/PRODUCT.md section 5). A drifted
fact routes to the people who own the decision; it does not stop the person who
happened to trigger it. The upper rungs — a nag that escalates with time-unhandled,
then the owning team's own merge gate, then a hard block — are policy the hub applies
per claim class, and they are built only when pilot metrics show drift actually being
ignored. Until then, making a wrong answer *loud* (a broken check is exit 2; an unowned
claim is a dead-letter; a clean store closes the issue) is the whole job.

## Adopting the workflow

Copy the workflow from `examples/consumer/`, and point `claim-repo` / `claim-ref` at
the claim tool repository and a pinned ref. Add a `.github/CODEOWNERS` (see the example)
so findings route to owners. That is the whole integration; the hub-side authentication
and ledger, if you run a hub, are configured in the hub's own action.
