---
id: deps/jiff-time-arithmetic
checks:
  - kind: cmd
    run: "grep -qE \"^jiff = \" Cargo.toml"
supports:
  - "Cargo.toml#jiff"
  - CLAUDE.md
hub:
  max-age: 180d
---
Instant and duration arithmetic (verdict timestamps, skip `until` expiry) uses jiff (correctness-first, checked overflow, lossless RFC 3339), chosen over time/chrono; declared in the workspace Cargo.toml.
