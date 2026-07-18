#!/usr/bin/env bash
# Reference CLAIM_AGENT_CMD runner: verify a `kind: agent` check with `claude -p`.
#
# `claim check` builds the verification prompt (the claim's `instruction` plus the
# fixed directive to emit a `{verdict, evidence, citations}` JSON object) and writes
# it to this command's stdin; the command must print that JSON on stdout. `claude -p`
# (headless print mode) does exactly that: it reads the prompt on stdin, runs in the
# repo so it can read the files the instruction names, and prints the model's answer.
# claim-core's parser tolerates the JSON being wrapped in prose, so no post-processing
# is needed here.
#
# Read-only by construction: the Read/Grep/Glob tools are allow-listed and the
# tree-mutating tools are explicitly denied, so the read-only guarantee holds even if
# a future `claude` default changes — a verification run can inspect the tree but never
# modify it. A non-zero exit, a timeout, or malformed output all map to `Broken` inside
# `claim` — never a fabricated pass. The runner ships no credentials; `claude` uses
# whatever the operator is logged in with, and the operator owns the cost.
#
# Wire it up locally (never in a billing-free CI, where the claim's skip suppresses it):
#   export CLAIM_AGENT_CMD="$PWD/examples/claude-runner.sh"
#   claim check
set -euo pipefail
exec claude -p --allowedTools "Read Grep Glob" --disallowedTools "Edit Write Bash NotebookEdit"
