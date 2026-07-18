# Agent checks (`kind: agent`)

For the concepts (claims, checks, the four verdicts), see the
[overview](index.html); this page is the operator's contract for the
`kind: agent` check the overview introduces.

A `cmd` check answers a claim with an exit code: run a command, exit 0 means the
fact holds, exit 1 means it drifted. Many facts have no such command. "Has the
upstream CJK-corruption bug been fixed in libfoo 5.x?" is not a grep — it is a
reading of a changelog and an issue tracker, a judgement. An `agent` check binds
that judgement to a natural-language instruction an agent (a model behind a CLI)
carries out, returning a verdict *and the evidence for it*.

This page is the contract for operators who want to run agent checks: what the
runner is, what it receives, what it must emit, and — most importantly — how the
tool refuses to let a misbehaving runner fake a pass.

## The honesty rule, first

Everything below serves one invariant (golden invariant #1): **a check that could
not produce a clean, well-formed answer is `Broken`, never `Held`.** There is no
path from a crashed runner, a timeout, a non-zero exit, or garbled output to a
verdict that keeps a claim fresh. An agent check is held to exactly the same
broken-never-passes contract as a `cmd` check; it just carries its reasoning with
it instead of collapsing to an exit code.

## Opt-in: nothing runs by default

Agent checks are executed **only** when the `CLAIM_AGENT_CMD` environment variable
is set for `claim check`. With it unset — the default — every `agent` check is
reported `Unverifiable` (exit 1: review needed), and **no subprocess is spawned and
no model is contacted**. A plain `claim check` never makes an API call. The runner,
its credentials, and its budget are entirely the operator's to provide and pay for;
the tool ships no model client and reaches no network on its own.

```sh
# Default: agent checks are Unverifiable, no runner is spawned.
claim check

# Opt in: point CLAIM_AGENT_CMD at your runner.
CLAIM_AGENT_CMD='my-agent-runner --model some-model' claim check
```

### Cost scales with how many agent checks run

The runner is invoked once per agent check that runs — so `claim check` runs it for
**every** agent check in the store, multiplying the per-check model cost on a large
corpus. The CLI is stateless and runs everything it is pointed at; it does not select a
subset. Which claims run when — a cheap subset on a PR, the rest on a clock — is
orchestration a CI step and the hub's scheduler decide (the cadence *hint* lives under a
claim's `hub:` subfield). Reserve a full agent-check sweep for a deliberate run; the
per-check spend is the operator's.

## The `CLAIM_AGENT_CMD` contract

`CLAIM_AGENT_CMD` is a **shell command** (run as `sh -c`). For each `agent` check
the tool:

1. Builds a prompt: the claim's `instruction`, followed by a fixed directive that
   states the required response shape and the honesty framing.
2. Runs your command, feeding the prompt on **stdin** (never as a shell argument,
   so a long natural-language instruction is never subject to shell quoting or
   injection).
3. Reads your command's **stdout**, expecting the verdict JSON. The verdict is
   parsed from **stdout only**; stderr is captured for diagnostics but is never a
   verdict source, so a `{"verdict":...}` fragment in a stderr log can never be
   mistaken for the answer.
4. Bounds the run exactly like a `cmd` check: a working-directory of the store
   root, a wall-clock timeout, a process group killed on timeout so no grandchild
   is orphaned, and a cap on retained output.

Your command must therefore **read the prompt from stdin and print the verdict JSON
to stdout**.

## The response schema

The runner must print a single JSON object:

```json
{
  "verdict": "held",
  "evidence": "libfoo's 5.x changelog and issue #1234 show no CJK fix as of 5.3.",
  "citations": ["CHANGELOG.md", "https://example.test/libfoo/issues/1234"]
}
```

- **`verdict`** (required): exactly one of `"held"`, `"drifted"`, or
  `"unverifiable"`.
  - `"held"` — the fact stated by the claim is still true.
  - `"drifted"` — the fact is now false.
  - `"unverifiable"` — the evidence was insufficient or conflicting to decide. This
    is the honest "I couldn't tell"; it counts against freshness (exit 1) but is not
    a tooling failure. Prefer it to guessing.
- **`evidence`** (optional): a short prose justification. Reported alongside the
  verdict in `claim check --json` — the evidence is the point of an agent check, so a
  human (or the hub) reading the report sees the reasoning.
- **`citations`** (optional): an array of source strings (files, URLs, issue refs).
  Appended to the evidence in the report.

The object may be wrapped in surrounding prose — a model that narrates before
answering is fine. The tool scans **every** balanced `{…}` span on stdout for one
carrying a `verdict`; a stray object with no `verdict` is skipped. But it never
guesses when the answer is ambiguous:

- **Zero** verdict-bearing objects → `Broken`.
- **More than one distinct verdict** across the objects (a model that emits a
  tentative `held` then a corrected `drifted`) → `Broken`, not the first and not the
  last. A run that did not settle on one answer is not a pass. Emitting the same
  verdict more than once is fine — only *disagreeing* verdicts conflict.
- A single object with a **duplicate `verdict` key**
  (`{"verdict":"drifted","verdict":"held"}`, which a JSON parser would silently
  resolve to the last value) → `Broken`. Ambiguity is never resolved toward a value.

Emit exactly one object, your final answer — the directive the tool sends the runner
says so, precisely to avoid the conflict cases above.

## The exact verdict mapping

| Runner outcome | Verdict |
|---|---|
| No `CLAIM_AGENT_CMD` set | `Unverifiable` (nothing spawned) |
| Fails to spawn (missing program, empty command) | `Broken` |
| Killed by a signal | `Broken` |
| Times out | `Broken` |
| Exits non-zero | `Broken` (its output is discarded, even a `held`) |
| Exits 0, stdout has no parseable verdict object | `Broken` |
| Exits 0, `verdict` missing or not one of the three | `Broken` |
| Exits 0, two objects with distinct verdicts | `Broken` (conflicting) |
| Exits 0, one object with a duplicate `verdict` key | `Broken` (ambiguous) |
| Exits 0, a `verdict` only on stderr | `Broken` (stderr is not a source) |
| Exits 0, one distinct `verdict: "held"` on stdout | `Held` |
| Exits 0, one distinct `verdict: "drifted"` on stdout | `Drifted` |
| Exits 0, one distinct `verdict: "unverifiable"` on stdout | `Unverifiable` |

Note the load-bearing rows: a runner that exits non-zero while printing
`{"verdict":"held"}`, exits 0 while printing prose or `{"verdict":"maybe"}`, emits
two disagreeing verdicts, or hides a `held` on stderr, is `Broken`. A runner cannot
fake a pass by claiming one — it must exit 0 *and* emit exactly one well-formed,
valid, unambiguous verdict on stdout.

A blank `CLAIM_AGENT_CMD` (set but only whitespace) is a configuration mistake and
is rejected loudly (exit 2), rather than silently falling back to leaving agent
checks unverifiable.

## An example mock runner

The runner is any executable that follows the stdin→stdout contract. This script
(used by the test suite) reads and discards the prompt and prints a canned verdict —
useful for wiring up and testing the plumbing without a real model:

```sh
#!/bin/sh
# mock-agent.sh — reads the prompt on stdin, prints a canned verdict.
cat >/dev/null
cat <<'EOF'
{"verdict":"held","evidence":"canned answer from the mock runner","citations":[]}
EOF
```

```sh
chmod +x mock-agent.sh
CLAIM_AGENT_CMD="$PWD/mock-agent.sh" claim check
```

A real runner is a wrapper around a model CLI that reads the prompt from stdin,
sends it to the model, and prints the model's JSON answer — for example a wrapper
around `claude -p` or an equivalent. That wrapper, its API key, and its per-check
budget are the operator's responsibility.

## A reference runner, and this repo's own agent claim

This repository ships one: [`examples/claude-runner.sh`](../examples/claude-runner.sh)
is a two-line wrapper around `claude -p` that reads the prompt on stdin, lets the
model read the tree (read-only `Read`/`Grep`/`Glob` tools only, so a check can inspect
but never modify), and prints its verdict JSON. Point `CLAIM_AGENT_CMD` at it to run
agent checks against a real model:

```sh
export CLAIM_AGENT_CMD="$PWD/examples/claude-runner.sh"
claim check
```

The store's own agent claim, `core/negation-owned-by-the-tool`, uses it to verify
golden invariant #2 (the tool owns negation) — a fact a `cmd` grep cannot check,
because the only literal `sh -c "!` in the tree is the doc comment warning against it.
Proving it needs a semantic reading of `check.rs`, which is exactly what an agent
check is for.

Because a billing-free CI has no model runner, that claim carries a `skip` with
`unless: test -n "$CLAIM_AGENT_CMD"`: it is **skipped** (reported, never a pass)
wherever no runner is wired, and **verified** wherever one is — locally, or in a CI
run that sets `CLAIM_AGENT_CMD`. That is the intended shape for an agent claim in a repo
that will not call a model on every CI run: honest about not verifying, without
failing the build.

## What is not built yet

Execution — the mechanism this page documents — is done. The **adversarial
spot-audit** described in the product design (re-running a sample of `held` verdicts
through a second agent instructed to refute the first, to catch confabulated greens
that no human would otherwise read) is deferred. Until it exists, treat a `held`
from an agent check as you would any single reviewer's judgement: trustworthy, but
not yet double-checked. A `drifted` verdict gets human eyes naturally; an unread
`held` is where a wrong answer would hide, which is exactly what the spot-audit is
for.
