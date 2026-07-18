---
id: gate/ci-runs-same-gate
checks:
  - kind: cmd
    run: "grep -q \"scripts/check.sh\" .github/workflows/ci.yml"
    when: on-change
max-age: 180d
supports:
  - .github/workflows/ci.yml
  - "CLAUDE.md#gate"
---
The GitHub Actions CI workflow (.github/workflows/ci.yml) runs scripts/check.sh as its gate, so local and CI can never disagree about what green means.
