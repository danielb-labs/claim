---
id: gate/dogfood-claims-run-in-gate
checks:
  - kind: cmd
    run: "grep -qE \"^\\./target/debug/claim check --all --report-only$\" scripts/check.sh"
    when: on-change
max-age: 180d
supports:
  - scripts/check.sh
  - "docs/dogfooding.md#dogfoods"
  - gate/ci-runs-same-gate
---
The gate (scripts/check.sh) runs this repo's own claims — `claim check --all --report-only` over `.claims/` — so a change that breaks a dogfooded decision fails the build, not just a hand-run `claim check`. This is what makes dogfooding actually gate development, and it rests on the CI workflow running that same gate (gate/ci-runs-same-gate): without both, a drifted claim would never block a merge. `--report-only` reports and sets the exit code without persisting a verdict, so the gate never dirties the tree.
