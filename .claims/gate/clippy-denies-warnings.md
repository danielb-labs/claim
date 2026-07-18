---
id: gate/clippy-denies-warnings
checks:
  - kind: cmd
    run: "grep -q -- \"-D warnings\" scripts/check.sh"
    when: on-change
max-age: 180d
supports:
  - scripts/check.sh
---
The quality gate (scripts/check.sh) runs clippy with -D warnings, so any clippy warning fails the build; a warning nobody fixes is a warning everybody ignores.
