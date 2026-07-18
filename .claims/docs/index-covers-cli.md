---
id: docs/index-covers-cli
checks:
  - kind: cmd
    run: "./scripts/docs-cover-cli.sh"
supports:
  - docs/index.html
  - crates/claim/src/cli.rs
  - "CLAUDE.md#same branch"
hub:
  max-age: 180d
---
The docs site (docs/index.html) documents every CLI verb the tool ships: scripts/docs-cover-cli.sh reads the verb list from `claim --help` and drifts if it names a verb the site does not. This is the mechanical backstop for the same-branch docs rule; it proves coverage, not accuracy, which stays a human obligation.
