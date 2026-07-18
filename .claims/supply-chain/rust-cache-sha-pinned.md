---
id: supply-chain/rust-cache-sha-pinned
checks:
  - kind: cmd
    run: "grep -qE \"Swatinem/rust-cache@[0-9a-f]{40}\" .github/workflows/ci.yml"
supports:
  - .github/workflows/ci.yml
hub:
  max-age: 180d
---
The third-party rust-cache GitHub Action in CI is pinned to a full 40-hex commit SHA, not a mutable tag, so the supply chain cannot shift under us — the exact rot this tool exists to prevent.
