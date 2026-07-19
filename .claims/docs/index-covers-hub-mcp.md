---
id: docs/index-covers-hub-mcp
checks:
  - kind: cmd
    run: "./scripts/docs-cover-hub-mcp.sh"
supports:
  - docs/hub.md
  - crates/claim-hub/src/mcp.rs
  - "CLAUDE.md#Docs ship with the behavior they describe"
hub:
  max-age: 180d
---
The hub docs (docs/hub.md) document every MCP tool the hub ships: scripts/docs-cover-hub-mcp.sh reads the tool names from the `#[tool(name = "…")]` attributes in crates/claim-hub/src/mcp.rs and drifts if it names a tool the docs do not. This is the mechanical backstop extending the same-branch docs rule to the hub MCP surface (as docs/index-covers-cli does for CLI verbs); it proves coverage, not accuracy, which stays a human obligation.
