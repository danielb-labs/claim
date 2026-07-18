# claim

**A tool that checks whether the facts you wrote down are still true.**

Every codebase is full of recorded reasons: a security exception that says a
vulnerability doesn't apply, a dependency pinned to an old version, a skipped test,
a line in an agent's context file. Each was true when someone wrote it. Nothing
checks it afterward, so it quietly rots — and the person who later makes it false
usually has no idea the note exists.

`claim` binds each of those facts to a way of re-checking it. When a fact stops
being true, the system notices and says so, instead of leaving a confident sentence
that is now wrong. The one-line version: **recorded knowledge either re-verifies
itself or comes back to a human — the failure mode is a nag, never a lie.**

## The core idea

A **claim** is three things written down together:

1. a **statement** — a fact, in plain language;
2. a **check** — a way to re-verify the fact (a shell command, or an agent
   investigation);
3. a **schedule** — when the check should run.

Claims are plain Markdown files with YAML frontmatter under a `.claims/` directory.
There is no database and no server: **git is the store.** A write to the truth is a
commit, so claims are versioned, reviewed, and attributed exactly like code. A
claim's status (`verified` / `drifted` / `stale` / `retired`) is *computed* from its
verdict log at read time, never stored — anything typed into a file can be forged or
go stale.

The honesty contract is strict, by design: a `cmd` check that exits `0` is `held`,
exit `1` is `drifted`, and **anything else — a missing binary, a timeout, a signal —
is `broken`, never a pass.** A check that could not run tells us nothing, so it ages
the claim toward review exactly like a check that never ran.

## Install

Build the CLI from source with Rust's package manager, from a checkout of this
repository:

```sh
cargo install --path crates/claim
claim --help
```

## Quick start

```sh
# in a git repo
claim init                         # create the .claims/ store

claim add \
  --id libfoo-pin \
  --statement "Pin libfoo at 4.2 — 5.x corrupts CJK PDF export." \
  --run "grep -q 'libfoo==4.2' requirements.txt" \
  --max-age 120d

git add .claims && git commit -m "record libfoo pin claim"
```

`add` runs the check once, requires it to hold (a passing check *is* the
verification), and writes the claim with its first recorded result. Later, `claim
check` re-runs the checks and records verdicts; `claim drift` lists what has gone
false; `claim list`, `claim log`, and `claim stats` read the store. Every command
takes `--json`.

Open the full documentation, bundled into the binary and version-locked to it:

```sh
claim docs            # prints the path to the bundled site (headless / scripting)
claim docs --open     # also opens it in your browser
```

## Layout

A Cargo workspace of four crates:

- `crates/claim-core` — the domain: parsing, verdict history, status, check
  execution.
- `crates/claim-store` — shared store discovery, loading, and git provenance.
- `crates/claim` — the `claim` CLI, a thin shell over core and store.
- `crates/claim-mcp` — the MCP server (`query`, `report`, `create`), how agents
  touch the store over the Model Context Protocol.

## Documentation

- [`docs/`](docs/) — the user docs: the [overview site](docs/index.html), the two
  [CI lanes](docs/ci.md), [agent checks](docs/agent-checks.md), and
  [dogfooding](docs/dogfooding.md).
- [`docs/design/`](docs/design/) — the product and design canon (`PRODUCT.md`,
  `PROPOSAL.md`, `SPEC.md`).
- [`CLAUDE.md`](CLAUDE.md) — how the code gets built: the golden invariants, the
  stack, and the branch → review → merge workflow. Binding for every contributor.

Parked decisions and deferred work live in the [issue tracker](../../issues) under
the `deferred` label.

## Development

```sh
./scripts/check.sh    # the full gate: fmt, clippy -D warnings, tests, docs,
                      # the CI renderer tests, and this repo's own claims
```

The gate is the same locally and in CI. No commits to `main`: every change is a
branch, reviewed, then merged. See [`CLAUDE.md`](CLAUDE.md) for the details.

## License

Apache-2.0.
