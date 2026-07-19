# CLAUDE.md — how we build `claim`

This file is binding for every agent and person who writes code here. Read it
before touching anything. The product design lives under `docs/design/`:
`PRODUCT.md` (what v1 is), `PROPOSAL.md` (why it exists, and why existing tools
don't solve it), and `SPEC.md` (original notes); parked decisions and deferred
work are tracked as GitHub issues under the `deferred` label. This file is only
about *how the code gets built*.

## What we're building

`claim` binds plain-language facts to executable checks, so an engineering org's
recorded knowledge — and its coding agents' context files — cannot silently rot.
A claim is a statement plus a way to re-verify it, committed to git; the CLI is a
stateless runtime verifier that runs the checks and reports whether each fact holds
right now. When a fact stops being true, the system notices and routes it to whoever
owns the decision that rested on it. The verdict stream, the schedule, and the
staleness ledger live in a per-environment **hub** that ingests the CLI's reported
output — not in git and not in the CLI (see `docs/design/CLI-HUB-BOUNDARY.md`). The
whole product is trust, so the code that produces verdicts has to be trustworthy
itself.

## The golden invariants

These are not preferences. Code that violates one does not merge, no matter how
nice it otherwise is. If a change seems to require breaking one, stop and raise
it — the design is wrong, or the invariant needs to change first, in its own
discussion.

1. **A broken check never reports a pass.** Exit 0 → `Held`; exit 1 → `Drifted`;
   any other exit, a failure to spawn, or a signal → `Broken`. `Broken` counts
   against freshness exactly like "never checked." There is no path from "the
   check couldn't run" to "the fact is fine." (`crates/claim-core/src/verdict.rs`
   states the full mapping.)
2. **The tool owns negation.** A claim's `negate` inverts `Held`/`Drifted` only,
   inside the tool. Never shell out to `sh -c "! ..."`: a missing binary or a
   deleted path would invert into a false pass. Never let a check's success
   depend on a shell's interpretation of `!`.
3. **Provenance is derived, never stored.** Who authored or reviewed a claim comes
   from git and the forge (commit author, PR approvals), not from fields in the
   file. Anything a claim file asserts about itself can be forged; anything git
   records cannot. A claim's *status* — freshness, staleness, due-ness — is not the
   CLI's to hold at all: it is derived by the hub from the verdict stream it stores
   (see `docs/design/CLI-HUB-BOUNDARY.md`), the same read-time derivation, one layer
   out.
4. **The truth is the claim, and a verdict is telemetry.** The truth — the claim —
   is a commit: a statement, its check, its `supports`, authored and reviewed in git.
   A verdict is a reported observation, **never committed**. The CLI runs checks and
   reports `held`/`drifted`/`broken` now (human or `--json`); it stores nothing. The
   trusted authority for verdicts is the pipeline that produced them and the hub that
   stores them, the way a green CI check is trusted without being committed to the
   repo. There is no `.claims/log/`, no side channel, no database, no API that writes
   verdicts. If a feature seems to need the tool to persist a verdict, it belongs in
   the hub, not the CLI.
5. **A passing check verifies the fact — as a birth gate, not a stored receipt.**
   `claim add` refuses a claim whose check does not report `Held` against reality;
   `Drifted` (already false) and `Broken` (can't run) are refused. But it writes **no
   establishing verdict** — a false claim is caught by the next check, so the receipt
   is unnecessary and committing it would put telemetry in git. A check is never
   penalized or marked "unverified" for a red that can't be staged — world-facts and
   agent checks have no red to fabricate, and a pass against reality is the whole of
   verification.
6. **The failure mode is a nag, never a lie.** Every path — a broken check, an
   unverifiable answer, a check that was never written, a deleted decision — must
   degrade toward a human being asked to look, never toward a stale green light. The
   CLI reports current truth loudly; the *nag* over time (a fact gone unchecked,
   overdue for its cadence) issues from the hub, which knows the schedule and the
   history the stateless CLI does not.

When in doubt, prefer the choice that makes a wrong answer *loud*.

## Language and stack, and why

**Rust.** Chosen for correctness first (the criterion that matters most for a
verification tool), then performance (checks run in CI, per merge and on a
clock), then a single static binary that drops into any repo or runner with no
runtime. The type system is doing real work here: the honesty invariants above
are encoded as `enum`s with exhaustive `match`, and `Result` forces the
"check couldn't run" path to be handled rather than forgotten. Agents write Rust
well *because* the compiler is a strict, immediate oracle — a wrong change fails
to build instead of shipping.

Layout is a Cargo workspace:

- `crates/claim-core` — the domain: parsing, the verdict enum, and check
  execution. No terminal, network, or process concerns leak in except where a
  check genuinely runs a subprocess. This is where correctness lives and where
  test coverage is densest. There is no verdict-log or status code here: a verdict
  is a check result the CLI reports, never a stored history.
- `crates/claim-store` — the shared infrastructure over core: store discovery,
  loading a store's claims, git provenance (commit author, HEAD sha), and the
  authoring gate. The CLI depends on it to read one store and attribute authored
  claims consistently; it is kept a separate crate so this store-and-git logic
  stays free of CLI concerns and a future front door could reuse it unchanged.
- `crates/claim` — the `claim` CLI, a thin shell over core and store. It is the
  sole front door in v1.

**Approved dependencies.** `serde`/`serde_json` (models and `--json` output),
`thiserror` (library errors), `anyhow` (binary errors), `clap` (CLI, derive
API), `assert_cmd` + `predicates` + `tempfile` (CLI integration tests), `insta`
(snapshot tests for output). For YAML frontmatter, **do not use `serde_yaml`** —
it is archived. Use a maintained fork (`serde_yaml_ng` or `serde_norway`); the
first item to need it picks one and records the choice in this file. **Chosen:
`serde_norway`** (item 01), for its more recent release cadence over
`serde_yaml_ng` — a live signal that dependency and security fixes keep landing,
which is what a trust tool needs from its parser; rationale in
`crates/claim-core/Cargo.toml`. For instant and duration arithmetic (a verdict's
report timestamp, a skip's `until` expiry, and the `hub:` day-count hints the parser
validates), **`jiff`** (item 02): correctness-first, with unambiguous UTC instants,
lossless RFC 3339 round-trips, and checked duration arithmetic that surfaces overflow
instead of wrapping — chosen over `time`/`chrono`;
rationale in `crates/claim-core/Cargo.toml`. For check execution (item 03),
**`wait-timeout`** to bound a check's run so a hung command times out to `Broken`
instead of hanging the tool, and **`libc`** for the one syscall std does not expose,
`killpg` — killing a timed-out check's whole process group so `sleep 100 | foo`
leaves no orphaned grandchild (std's `process_group(0)` creates the group but gives
no way to signal it); both unix-only and rationale in `crates/claim-core/Cargo.toml`.
(The store and git-provenance logic the CLI needs lives in the shared
**`claim-store`** workspace crate, extracted in item 07 so store discovery, claim
loading, and the authoring gate live in one place, layered over core and kept free
of CLI concerns.)
Adding any other dependency requires a one-line justification in the crate's
`Cargo.toml` and a note in the review — every dependency is attack surface and
maintenance.

**Toolchain.** `cargo` may not be on a fresh shell's `PATH`; run
`source "$HOME/.cargo/env"` first (`scripts/check.sh` does this for you).

## How we work: branch → review → merge

No commits to `main`. Ever. Every build item is one branch, reviewed, then
merged.

1. **Branch.** `git switch -c item-NN-short-name` off the latest `main`.
2. **Build.** Implement the item. Write the tests and docs with the code, not
   after. Keep the diff scoped to the item — no drive-by refactors of unrelated
   code (raise those separately).
3. **Gate.** `./scripts/check.sh` must pass — formatting, clippy with warnings
   denied, all tests, and docs. A branch that doesn't pass the gate is not ready
   for review.
4. **Review.** Two independent adversarial reviewers read the diff, with
   different mandates (one hunts correctness and broken invariants, one hunts
   design, test adequacy, security, and slop). Their findings come back
   classified by severity. **Docs are part of the diff, not a follow-up:** a
   reviewer rejects a branch that changes user-facing behavior without updating
   the docs it affects (see "Docs ship with the behavior they describe").
5. **Adjudicate and fix.** The orchestrator decides which findings are real.
   Every accepted finding is fixed on the same branch, and the gate runs again.
6. **Merge.** `git switch main && git merge --no-ff item-NN-short-name` with a
   message that says what shipped and why. The next item branches from there.

The gate is the same locally and in CI so the two can never disagree.

Every PR is opened with `.github/PULL_REQUEST_TEMPLATE.md`, which surfaces these
obligations as an author checklist — the gate, a diff scoped to one item, tests on
the negative paths, docs shipping with the behavior they describe, and a one-line
justification for any new dependency. GitHub only *pre-fills* that template; a PR
opened with an explicit body (`gh pr create --body …`) bypasses it silently. So a
required `pull_request` check (`pr-template`, workflow `.github/workflows/pr-template.yml`,
script `scripts/check-pr-body.sh`) enforces it: it derives the required `## ` sections
from the template itself and fails a PR whose body drops one, naming each missing
section. Keep every section and write "None" where one does not apply; a body that skips
a section fails the check and cannot merge.

## Testing

Coverage is not a percentage target; it's a set of obligations:

- **Every golden invariant has a test that would fail if the invariant broke** —
  especially the negative paths: a check that exits 137 is `Broken` not `Held`; a
  `negate` claim with a missing binary is `Broken` not a pass; `claim add` refuses a
  `Drifted`/`Broken` establish and writes nothing; `claim check` reports without
  persisting any verdict.
- **Every public function in `claim-core`** has unit tests for its ordinary case
  and its edge cases (empty, malformed, boundary values).
- **Every CLI command** has an integration test (`assert_cmd`) asserting exit
  code, human output, and `--json` shape, run against a real temp store.
- **Output has snapshot tests** (`insta`) so format changes are deliberate and
  visible in review.
- Tests are deterministic. No wall-clock `now()` reaching into real time inside
  logic under test — time is a parameter. No network. No ordering dependence
  between tests.

A bug found in review means a missing test; add the test that would have caught
it, then the fix.

## Documentation and style

Write for the next engineer, who is as smart as you and has none of your context.

- **Doc-comment every public item** with what it guarantees and the contract a
  caller must keep — not a restatement of the signature. Explain *why* and
  *what's load-bearing*, not *what the next line does*.
- **No redundant comments.** `// increment i` earns nothing. A comment states a
  constraint the code can't: a non-obvious invariant, a reason for an unusual
  choice, a link to the design.
- **No AI-slop.** No "Step 1 / Step 2" narration, no emoji, no restating the code
  in prose above it, no hedging filler ("essentially", "basically"), no comments
  addressed to the reviewer ("as requested", "this correctly handles"). Complete
  sentences, ending in periods. If a comment could be deleted with no loss of
  understanding, delete it.
- **Error messages are for the person who hit them**: name the file, the field,
  the fix. "invalid claim" is useless; "checks.cmd: expected a string" is not.
- **Names carry weight.** A reader should infer a function's job from its name and
  types before reading its body.

Match the style of the code already here. When you change behavior, update the
docs and design files that describe it in the same commit.

### Docs ship with the behavior they describe

A branch that changes user-facing behavior — a verb, a flag, an exit code, an
output shape — MUST update, add, or remove the docs it affects **in the same
branch**, as part of the definition of done: the `docs/index.html` site, the topic
docs under `docs/`, and the `--help` text. This is checked in review (see step 4 of
"How we work"): an unaccompanied behavior change is not mergeable, however correct
the code is.

Docs are **never** a separate batch item. Batching documentation into its own later
item is exactly what once let the MCP `create` tool ship while the site still said
the server "exposes two tools" — the drift was structural, not an oversight, because
the item that added the tool had no obligation to touch the docs. (That local MCP
server has since been removed entirely, but the lesson stands.) Removing that
structure removes the drift: the item that changes the behavior owns its docs.

The mechanical backstop is the self-checking docs claim in this repo's own store
(`docs/index-covers-cli`): its check (`scripts/docs-cover-cli.sh`) reads the shipped
verb list from `claim --help` and **drifts** when it names a verb `docs/index.html`
does not, so `claim check`/CI catches an undocumented verb even if a reviewer misses
it. It is a backstop, not a substitute for writing the docs with the change — it
proves *coverage* (every verb is mentioned), not *accuracy* (that what is written is
true), which stays a human obligation.

## Commits

Small, focused, buildable at every step. Subject line in the imperative, under
~70 chars, saying what changed and implying why. Body when the why isn't obvious.
End co-authored commits with:

```
Co-Authored-By: Claude <noreply@anthropic.com>
```
