---
id: arch/workspace-three-crates
checks:
  - kind: cmd
    run: "grep -qE \"^members = \\[\\\"crates/claim-core\\\", \\\"crates/claim-store\\\", \\\"crates/claim\\\"\\]$\" Cargo.toml"
supports:
  - "Cargo.toml#members"
  - "CLAUDE.md#claim-core"
hub:
  max-age: 180d
---
The workspace is exactly these three crates in this layering: claim-core (domain), claim-store (shared store + git provenance), claim (CLI, the sole front door).
