# CI integration: the two lanes

`claim` verifies recorded facts in two places, on two clocks. Both end at the same
verdict log; they differ in what triggers them and what they are allowed to do.

- **The on-change lane** runs on every pull request. It answers "does this diff break
  a fact someone wrote down?" and, when it does, comments on the PR — advisory, never
  blocking.
- **The clock lane** runs on a schedule. It answers "what has gone stale or drifted
  while nobody was looking?" and maintains one standing issue that is the product's
  entire v1 nag mechanism.

This page explains both, the exit-code contract they rely on, and the boundaries that
keep them honest. The reusable workflows live in `.github/workflows/claim-on-change.yml`
and `.github/workflows/claim-clock.yml`; a drop-in consumer copy is in
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
condition, not the quietest. Neither lane fails your build on any of these codes — see
"advisory, never a block" below — but the renderer uses them to group findings and to
decide clean-vs-dirty.

## The on-change lane — report-only and advisory

**Trigger:** `pull_request`.
**Command:** `claim check --all --report-only --json`.
**Permissions:** `contents: read`, `pull-requests: write`. No write to the repo.

Two properties are load-bearing:

- **`--report-only` writes nothing and needs no git identity.** A fork PR's CI has no
  write token; this mode still runs every check and still reports, it just persists no
  verdict (PRODUCT.md section 3: fork-PR runs report only; trusted runs persist). The
  comment step is additionally gated off for fork PRs, so a contribution from a fork
  gets its checks and an advisory job summary without the workflow ever handling a
  write token.
- **It never fails the build.** The check step ends with `|| true`; the finding is
  routed as a comment, not a gate. Drift is information delivered at the moment of
  maximal context — the person changing the world learns what their change broke and
  who owns the decision — not a wall in front of the merge. Escalation beyond the
  comment is deferred (see "The v1 escalation boundary").

The comment is **one** grouped comment, updated in place. Findings are grouped by
severity — broken checks first (loudest), then drifted claims, then unresolved
supports — and each names the claim's statement, the decisions it supports, and the
CODEOWNERS owner of its file.

## The clock lane — persist and nag

**Trigger:** `schedule` (cron), plus `workflow_dispatch` for a manual run.
**Command:** `claim check --due --json` (persisting) — or `--due --report-only` when
`persist: false`.
**Permissions:** `contents: write`, `issues: write`.

This lane runs on the trusted default branch with the repo's own token, so it *is* the
persisting context. It records each due claim's verdict and **commits the new log files
back** — because a write to the truth is a commit (golden invariant #4), there is no
side channel that records a verdict. Committing the verdicts also advances each claim's
freshness clock, so an `every Nd` claim is not perpetually due.

If you would rather not have CI commit to your default branch, set `persist: false`.
The lane then runs report-only: it still nags via the issue, but records nothing, so
`every Nd` claims stay due every run. This is a deliberate, documented trade — the nag
still works; only the clock reset is skipped.

The lane maintains **one** standing issue titled `claim: due & drifted`:

- **Created** when there is a queue (drift, overdue, broken, unresolved, or an
  unloadable file) and no such issue is open.
- **Updated** in place, every run, to the current queue.
- **Closed** when the store is clean. It never opens an issue to say "all clear".

This single issue is the entire v1 nag mechanism: the bell that turns computed
staleness into something a human hears.

## Idempotency — one comment, one issue

Both lanes are strictly idempotent, so a hundred pushes or a hundred nights leave one
surface, not a hundred.

- Every rendered body opens with a hidden HTML marker: `<!-- claim-bot:on-change -->`
  for the comment, `<!-- claim-bot:clock -->` for the issue. The two markers are
  distinct so a repo running both lanes never edits one lane's surface from the other.
- The comment step lists the PR's comments and edits the one whose body contains the
  comment marker, creating a new comment only when none exists.
- The issue step lists open issues carrying the `claim` label and edits the one whose
  body contains the issue marker, creating one only when there is a queue and none is
  open.

The `concurrency` block on each workflow additionally serializes runs against the same
PR (on-change) or store (clock), so two runs can never race to double-post.

## The renderer — where the logic is, and how it is tested

The transformation from `claim check --json` to a markdown comment or issue body — the
grouping, the supports and statement rendering, the CODEOWNERS-owner lookup, and the
clean-vs-dirty decision — is not in the workflow YAML. It lives in `ci/render.mjs`
(Node, no dependencies) and is unit-tested in `ci/render.test.mjs` against real
`claim check` JSON fixtures in `ci/fixtures/` (a clean store, one drift with supports,
a mixed store grouped by severity, a broken check, an unresolved support, and a load
error). The workflow only runs `claim`, calls the renderer, and hands its output to
the GitHub API — so the part a reviewer might get wrong is the part covered by tests.

The tests run in the repo's own gate (`scripts/check.sh`), so a change to the CLI's
JSON shape or the renderer breaks the build rather than silently mis-rendering in
production.

The CODEOWNERS lookup implements GitHub's rule that the **last matching pattern wins**,
over the subset a claims store needs: a catch-all `*`, directory prefixes
(`.claims/payments/`), anchored paths, and basename globs (`*.md`). An unowned claim is
rendered as a visible routing dead-letter, never silently dropped.

The statement — the plain-language fact — is not carried in `claim check --json`, so the
renderer reads it from the claim file the checkout already has (the markdown body after
the frontmatter). This is a display convenience only: a file it cannot read (or an
embedded claim inside another file) simply omits the statement line rather than
guessing. The authoritative parser stays in `claim-core`.

## The v1 escalation boundary

The escalation ladder in v1 has exactly two rungs, and neither blocks a merge:

1. **The on-change PR comment** — advisory, at the moment of maximal context.
2. **The clock-lane standing issue** — the durable nag that outlives any single PR.

There is deliberately **no gate and no block** in v1 (PRODUCT.md section 5). A drifted
fact routes to the people who own the decision; it does not stop the person who
happened to trigger it. The upper rungs — a nag that escalates with time-unhandled,
then the owning team's own merge gate, then a hard block — are policy per claim class,
and they are built only when pilot metrics show drift actually being ignored. Until
then, making a wrong answer *loud* (a broken check is exit 2; an unowned claim is a
dead-letter; a clean store closes the issue) is the whole job.

## Adopting the lanes

Copy `examples/consumer/.github/workflows/claims-on-change.yml` and
`claims-clock.yml` into your repo, and point `claim-repo` / `claim-ref` at the claim
tool repository and a pinned ref. Add a `.github/CODEOWNERS` (see the example) so
findings route to owners. That is the whole integration.
