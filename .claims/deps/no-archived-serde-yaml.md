---
id: deps/no-archived-serde-yaml
checks:
  - kind: cmd
    run: "grep -qE \"^name = \\\"serde_yaml\\\"$\" Cargo.lock"
    negate: true
    when: on-change
max-age: 180d
supports:
  - "Cargo.toml#serde_norway"
  - "CLAUDE.md#serde_norway"
---
claim-core parses YAML with the maintained serde_norway fork, never the archived serde_yaml crate (not present anywhere in the resolved dependency graph).
