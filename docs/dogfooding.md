# This repo dogfoods `claim`

`claim` is the tool built in this repository. It also uses itself: the claims in
`.claims/` bind a handful of this repo's own load-bearing decisions to executable
checks, so those decisions cannot silently rot. If someone re-pins a CI action to
a mutable tag, drops `-D warnings` from the gate, adds the archived `serde_yaml`,
or reshapes the workspace, the matching claim drifts and the check fails loudly.

This doc lives outside `.claims/` on purpose: `claim check` parses every `*.md`
under the store as a claim file (only `.claims/log/` is skipped), so a README
placed *inside* the store makes `check --all` exit 2 with a frontmatter error.
Store docs therefore live here in `docs/`, not in `.claims/`.

## Running the checks here

Build the CLI and run every claim's check against the current tree:

```sh
source "$HOME/.cargo/env"
cargo build -p claim
./target/debug/claim check --all
```

Exit 0 means every claim held and every `supports` reference resolved; exit 1
means something drifted, went unverifiable, or a support anchor went missing
(review needed); exit 2 means a check broke or a claim file could not load. See
the exit-code contract with `claim check --help`.

Other useful reads (none of them write to the store):

```sh
./target/debug/claim list            # every claim with its computed status
./target/debug/claim stats           # counts, drifts caught, staleness
./target/debug/claim log <id>        # one claim's full history and evidence
./target/debug/claim drift           # only the claims that have drifted
```

`claim check --all` in a trusted run (a real git identity) appends a verdict to
`.claims/log/` and expects that verdict to be committed. To run the checks
without writing anything — a fork PR's CI, or a quick local sanity pass — add
`--report-only`: the exit code is still set from the verdicts, but nothing is
persisted.

## What is claimed

Run `claim list` for the current set. As of this writing the store records seven
claims about: the archived `serde_yaml` staying out of the dependency graph
(`serde_norway` is the chosen fork), `jiff` as the time library, the gate denying
clippy warnings, CI running the same `scripts/check.sh` as local, the workspace
being exactly four crates, the exit-code→verdict mapping living in `verdict.rs`,
and the CI action being pinned to a full commit SHA rather than a mutable tag.

## How the store is laid out

- `.claims/<id>.md` — one file per claim: YAML frontmatter (id, checks,
  `max-age`, `supports`) plus the plain-language statement as the body.
- `.claims/log/<id>/<timestamp>-<hash>.json` — the append-only verdict history.
  Each entry records the verdict, the git commit it was taken against, and the
  git-derived actor. Status and provenance are *derived* from this log at read
  time, never stored in the claim file (see CLAUDE.md, golden invariant #3).
