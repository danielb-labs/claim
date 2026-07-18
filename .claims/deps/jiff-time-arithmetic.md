---
id: deps/jiff-time-arithmetic
checks:
  - kind: cmd
    run: "grep -qE \"^jiff = \" Cargo.toml"
    when: on-change
max-age: 180d
supports:
  - "Cargo.toml#jiff"
  - CLAUDE.md
---
Instant and duration arithmetic for the verdict log and status computation uses jiff (correctness-first, checked overflow, lossless RFC 3339), chosen over time/chrono; declared in the workspace Cargo.toml.
