---
id: arch/workspace-crates
checks:
  - kind: cmd
    run: "grep -qE \"^members = \\[\\\"crates/claim-core\\\", \\\"crates/claim-store\\\", \\\"crates/claim\\\", \\\"crates/claim-hub-core\\\", \\\"crates/claim-hub-store\\\", \\\"crates/claim-hub\\\"\\]$\" Cargo.toml"
supports:
  - "Cargo.toml#members"
  - "CLAUDE.md#claim-core"
hub:
  max-age: 180d
---
The CLI is three crates in this layering — claim-core (domain), claim-store (shared store + git provenance), claim (CLI, the sole front door) — and the hub adds its own three: claim-hub-core (the hub's pure domain, the hub's answer to claim-core), claim-hub-store (the hub's Ledger/Registry storage seam over SQLite), and claim-hub (the hub binary — the axum app shell that hosts /status and every later surface), all depending one-way on claim-core and never depended on by the CLI.
