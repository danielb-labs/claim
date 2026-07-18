---
id: gate/ci-runs-same-gate
checks:
  - kind: cmd
    run: "grep -q \"scripts/check.sh\" .github/workflows/ci.yml"
supports:
  - .github/workflows/ci.yml
  - "CLAUDE.md#gate"
hub:
  max-age: 180d
---
The GitHub Actions CI workflow (.github/workflows/ci.yml) runs scripts/check.sh as its gate, so local and CI can never disagree about what green means.
