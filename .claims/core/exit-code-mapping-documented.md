---
id: core/exit-code-mapping-documented
checks:
  - kind: cmd
    run: "grep -q \"exit 0\" crates/claim-core/src/verdict.rs && grep -q \"exit 1\" crates/claim-core/src/verdict.rs && grep -q \"any other exit\" crates/claim-core/src/verdict.rs"
supports:
  - crates/claim-core/src/verdict.rs
  - "CLAUDE.md#golden"
hub:
  max-age: 180d
---
The canonical exit-code to verdict mapping (exit 0 -> Held, exit 1 -> Drifted, any other exit/signal/spawn-failure -> Broken) is stated in crates/claim-core/src/verdict.rs, the single source of truth for invariant #1.
