---
id: gate/clippy-denies-warnings
checks:
  - kind: cmd
    run: "grep -q -- \"-D warnings\" scripts/check.sh"
supports:
  - scripts/check.sh
hub:
  max-age: 180d
---
The quality gate (scripts/check.sh) runs clippy with -D warnings, so any clippy warning fails the build; a warning nobody fixes is a warning everybody ignores.
