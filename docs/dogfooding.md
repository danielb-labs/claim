# This repo dogfoods `claim`

`claim` is the tool built in this repository. It also uses itself: the claims in
`.claims/` bind a handful of this repo's own load-bearing decisions to executable
checks, so those decisions cannot silently rot. If someone re-pins a CI action to
a mutable tag, drops `-D warnings` from the gate, adds the archived `serde_yaml`,
or reshapes the workspace, the matching claim drifts and the check fails loudly.
For the concepts these checks rest on, see the [overview](index.html).

This doc lives here in `docs/`, but it could also live inside the store: the
scanner parses a `.md` under `.claims/` as a claim only when it opens with a
`---` frontmatter fence, so a plain `README.md` documenting the store is skipped
silently rather than parsed and failing. A file that *does* open with a fence but
is malformed stays a loud per-file error. Keeping this doc in `docs/` is a
placement choice, not a work-around for the scanner.

## Running the checks here

Build the CLI and run every claim's check against the current tree:

```sh
source "$HOME/.cargo/env"
cargo build -p claim
./target/debug/claim check
```

`claim check` is a stateless verifier: it runs every claim's checks and reports,
storing nothing. Exit 0 means every claim held and every `supports` reference
resolved; exit 1 means something drifted, went unverifiable, or a support anchor
went missing (review needed); exit 2 means a check broke or a claim file could not
load. See the exit-code contract with `claim check --help`.

Other useful reads (none of them write to the store):

```sh
./target/debug/claim list            # inventory: id, statement, file, supports count
./target/debug/claim drift           # run checks, show only the drifted claims
```

`claim check` never writes anything — there is no verdict log to commit and no
`--report-only` distinction, because reporting is all it does. A verdict is
telemetry a per-environment hub ingests from the `--json` output, not something the
CLI commits (see the [CLI/hub boundary](index.html)). A fork PR's CI can run it with
no write token.

## What is claimed

Run `claim list` for the current set. As of this writing the store records eight
claims about: the archived `serde_yaml` staying out of the dependency graph
(`serde_norway` is the chosen fork), `jiff` as the time library, the gate denying
clippy warnings, CI running the same `scripts/check.sh` as local, the workspace
being exactly four crates, the exit-code→verdict mapping living in `verdict.rs`,
the CI action being pinned to a full commit SHA rather than a mutable tag, and the
docs site (`docs/index.html`) documenting every CLI verb and MCP tool the tool
ships — that last one drifts if a verb or tool is added to the code without a
mention in the site, the mechanical backstop for the same-branch docs rule in
CLAUDE.md.

## How the store is laid out

- `.claims/<id>.md` — one file per claim: YAML frontmatter (id, checks,
  `supports`, and an optional `hub:` subfield of scheduling hints like `max-age`)
  plus the plain-language statement as the body.
- There is no verdict log in the store. A verdict is telemetry the CLI reports and a
  hub ingests, never committed to git — so the store holds claims and only claims.
  Provenance (author, reviewer) is *derived* from git; a claim's status is *derived*
  by the hub from the verdict stream, never stored in the claim file (see CLAUDE.md,
  golden invariants #3 and #4).
