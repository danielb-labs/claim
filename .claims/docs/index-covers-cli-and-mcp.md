---
id: docs/index-covers-cli-and-mcp
checks:
  - kind: cmd
    run: "./scripts/docs-cover-cli.sh"
supports:
  - docs/index.html
  - crates/claim/src/cli.rs
  - crates/claim-mcp/src/server.rs
  - "CLAUDE.md#same branch"
hub:
  max-age: 180d
---
The docs site (docs/index.html) documents every CLI verb and every MCP tool the tool ships: scripts/docs-cover-cli.sh reads the verb list from `claim --help` and the MCP tool list from the server source and drifts if either names something the site does not. This is the mechanical backstop for the same-branch docs rule; had it existed, it would have caught the item-14 `create` tool shipping undocumented.
