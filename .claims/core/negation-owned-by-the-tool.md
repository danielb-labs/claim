---
id: core/negation-owned-by-the-tool
checks:
  - kind: agent
    instruction: >-
      Read crates/claim-core/src/check.rs and crates/claim-core/src/verdict.rs and
      verify golden invariant #2 (the tool owns negation). A claim's `negate` must
      invert only Held and Drifted, computed inside the tool — never by shelling out
      to `sh -c "! ..."` and never by wrapping the check command in a shell negation.
      Confirm no code path lets `negate` turn a Broken (a missing binary, a timeout,
      a non-0/1 exit, a spawn failure) into a false pass: Broken must stay Broken
      regardless of `negate`. Return held if the invariant holds; drifted if any code
      path violates it.
    skip:
      reason: >-
        Needs a model runner (CLAIM_AGENT_CMD); billing-free CI has none, so this
        check is verified locally and in any lane that wires a runner. See
        examples/claude-runner.sh.
      unless: 'test -n "$CLAIM_AGENT_CMD"'
supports:
  - CLAUDE.md#The tool owns negation
hub:
  max-age: 180d
---
Golden invariant #2 holds: negation is computed inside the tool, over Held/Drifted
only, never by a shell's interpretation of `!` — so a missing binary or a deleted path
stays Broken instead of inverting into a false pass.

This is the invariant a `cmd` grep cannot check. The only literal `sh -c "!` in the
tree is the doc comment warning against it, so a textual scan catches the
documentation, not a regression; proving the invariant needs a semantic reading of
`check.rs`, which is exactly what `kind: agent` is for. It carries a `skip` so a
billing-free CI reports it as skipped rather than blocking the build, while a
runner-equipped environment (local, or a lane that sets `CLAIM_AGENT_CMD`) verifies it
for real.
