---
id: arch/workspace-crates
checks:
  - kind: cmd
    run: "grep -qE \"^members = \\[\\\"crates/claim-core\\\", \\\"crates/claim-store\\\", \\\"crates/claim\\\", \\\"crates/claim-hub-core\\\"\\]$\" Cargo.toml"
supports:
  - "Cargo.toml#members"
  - "CLAUDE.md#claim-core"
hub:
  max-age: 180d
---
The CLI is three crates in this layering — claim-core (domain), claim-store (shared store + git provenance), claim (CLI, the sole front door) — and the hub adds its own domain crate claim-hub-core (the hub's pure domain, the hub's answer to claim-core), depending one-way on claim-core and never depended on by the CLI.
