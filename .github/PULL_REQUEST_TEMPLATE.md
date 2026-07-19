<!--
CLAUDE.md is the full contract for how we build here; this template is the short
version. Keep the PR scoped to one item — raise unrelated refactors separately. Keep
every `## ` section below — a required CI check (pr-template) fails a PR whose body drops
one — and write "None" (or "None affected", as each section indicates) where a section
does not apply.
-->

## What & why

<!--
What changed, and the motivation a reviewer needs to judge it. If this changes anything
a user or agent can observe — a CLI verb, a flag, an exit code, an output shape, or the
`.claims/` file format — say so explicitly; that triggers the docs obligation in the
checklist and means the change is potentially breaking for existing
users and agents.
-->

Closes #

## How it was verified

<!--
The commands you ran and what they proved. `./scripts/check.sh` passing is the baseline,
not the whole story — name the new tests and the behavior each one pins, especially the
negative paths (the cases that must fail loudly).
-->

## Golden invariants

<!--
The honesty invariants in CLAUDE.md are load-bearing: a broken check never reports a
pass; the tool owns negation; status and provenance are derived, not stored; a write to
the truth is a commit; a passing check verifies the fact; the failure mode is a nag,
never a lie. If this PR touches verdict mapping, negation, status/provenance derivation,
check execution, or the write path, name the invariant(s) it affects and, for each, the
test that would fail if it broke. If it touches none, write "None affected."
-->

## Checklist

- [ ] `./scripts/check.sh` passes locally — formatting, clippy with warnings denied, all tests, docs, the CI renderer tests, and this repo's own dogfood claims.
- [ ] The diff is scoped to one item; no drive-by refactors of unrelated code.
- [ ] Tests cover the change, including the negative paths — a check that can't run or was never written ages the claim toward review, never toward a pass.
- [ ] Docs ship with the behavior: if a verb, flag, exit code, output shape, or `.claims/` file-format field changed, `docs/index.html`, the affected topic docs under `docs/`, and `--help` are updated in this same branch.
- [ ] Any new dependency carries a one-line justification in the crate's `Cargo.toml` and is called out below — every dependency is attack surface and maintenance.
- [ ] Commit subjects are imperative and under ~70 characters; co-authored commits carry the trailer.

## New dependencies

<!-- None, or list each with its one-line justification. -->
