# `claim` documentation

The versioned docs for `claim`. Start with the overview, then the topic docs for
the parts you touch.

- [`index.html`](index.html) — the overview: what `claim` is, the core concepts
  (claim, statement, check, the four verdicts, status, max-age, triggers, negate,
  supports, the verdict log), the honesty rules, the lifecycle and architecture
  diagrams, and a complete CLI reference. Open it in a browser; the diagrams it
  embeds live in [`assets/`](assets/) (copied from the repo's `diagrams/`).
- [`ci.md`](ci.md) — the two CI lanes (on-change and clock), the exit-code
  contract, report-only vs. persisting, the standing issue, and the renderer.
- [`agent-checks.md`](agent-checks.md) — running `kind: agent` checks: the
  `CLAIM_AGENT_CMD` runner contract, the response schema, and the verdict mapping
  that stops a misbehaving runner from faking a pass.
- [`dogfooding.md`](dogfooding.md) — how this repository verifies its own
  load-bearing decisions with `claim`, and how to run those checks.
- [`design/`](design/) — the product and design canon: `PRODUCT.md` (what v1 is),
  `PROPOSAL.md` (why it exists), `SPEC.md` (original notes), and `TODO.md` (parked
  decisions and deferred work). These describe the *product*; the docs above and
  `--help` describe *using the tool as built*.

The authoritative flag reference is always `claim <verb> --help`; the CLI table
in `index.html` mirrors it as of the current release.
