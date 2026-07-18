# `claim` documentation

The versioned docs for `claim`. Start with the overview, then the topic docs for
the parts you touch.

- [`index.html`](index.html) — the overview: what `claim` is, the core concepts
  (claim, statement, check, the four verdicts, the CLI/hub boundary, the `hub:`
  hints, negate, supports), the honesty rules, the lifecycle and architecture
  diagrams, and a complete CLI reference. Open it in a browser; the diagrams it
  embeds live in [`assets/`](assets/) (copied from the repo's `diagrams/`).
- [`ci.md`](ci.md) — CI and the hub: the exit-code contract, how the hub's GitHub
  Action wraps and POSTs `claim check --json`, the renderer and its JSON shape.
- [`agent-checks.md`](agent-checks.md) — running `kind: agent` checks: the
  `CLAIM_AGENT_CMD` runner contract, the response schema, and the verdict mapping
  that stops a misbehaving runner from faking a pass.
- [`dogfooding.md`](dogfooding.md) — how this repository verifies its own
  load-bearing decisions with `claim`, and how to run those checks.
- [`design/`](design/) — the product and design canon: `PRODUCT.md` (what v1 is),
  `PROPOSAL.md` (why it exists), and `SPEC.md` (original notes). These describe the
  *product*; the docs above and `--help` describe *using the tool as built*. Parked
  decisions and deferred work are tracked as GitHub issues under the `deferred` label.

The authoritative flag reference is always `claim <verb> --help`; the CLI table
in `index.html` mirrors it as of the current release.
