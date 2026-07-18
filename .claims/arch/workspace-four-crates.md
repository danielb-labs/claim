---
id: arch/workspace-four-crates
checks:
  - kind: cmd
    run: "grep -qE \"^members = \\[\\\"crates/claim-core\\\", \\\"crates/claim-store\\\", \\\"crates/claim\\\", \\\"crates/claim-mcp\\\"\\]$\" Cargo.toml"
supports:
  - "Cargo.toml#members"
  - "CLAUDE.md#claim-core"
hub:
  max-age: 180d
---
The workspace is exactly these four crates in this layering: claim-core (domain), claim-store (shared store + git provenance), claim (CLI), claim-mcp (MCP server).
