#!/usr/bin/env bash
# Assert that the docs site names every CLI verb the tool actually ships. This is the
# check behind the `docs/index-covers-cli` claim: it drifts (exit 1) the moment a verb
# is added to the code without being mentioned in docs/index.html, which is exactly how
# item-14's tool once shipped undocumented. It holds (exit 0) when the site is complete.
#
# Contract, so `claim check` maps it honestly (golden invariant #1):
#   exit 0  every verb is documented                  -> Held
#   exit 1  at least one is undocumented (drift)       -> Drifted
#   exit 2  the sources it reads are missing/unusable  -> Broken (never a false pass)
#
# Runs from the repository root (the claim store root). The verb list comes from the
# *debug* binary's own `--help`, so it tracks the shipped CLI surface exactly. The
# gate keeps that binary current: `scripts/check.sh` has a "dogfood claims" step that
# builds `claim` (also a side effect of `cargo test`) and then runs
# `./target/debug/claim check`, which is what executes this check. So on the gate the
# binary is always fresh; a missing one is Broken (exit 2), never a false pass.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

doc="docs/index.html"
# The debug binary specifically: the one scripts/check.sh's dogfood step builds. A
# release binary is deliberately not consulted, because a stale release artifact from
# an earlier build would report a verb list that does not match the current source — a
# silent miscount is exactly the rot this claim exists to catch, so we never risk it.
bin="target/debug/claim"

if [ ! -x "$bin" ]; then
  echo "docs-cover-cli: $bin not found; build it first (cargo build -p claim)" >&2
  exit 2
fi
if [ ! -f "$doc" ]; then
  echo "docs-cover-cli: $doc not found" >&2
  exit 2
fi

# Verbs: the indented command names in the `Commands:` block of `claim --help`,
# excluding clap's built-in `help`. The block runs from the `Commands:` line to the
# first blank line; each entry is `  <verb>  <description>`.
verbs="$(
  "$bin" --help \
    | awk '/^Commands:/{grab=1; next} grab && NF==0{grab=0} grab{print $1}' \
    | grep -vx 'help'
)"
if [ -z "$verbs" ]; then
  echo "docs-cover-cli: could not extract any verb from '$bin --help'" >&2
  exit 2
fi

missing=0

# A verb is documented when the site mentions it as `claim <verb>` — the form the CLI
# reference and examples use — so a bare word that merely appears in prose does not
# count as coverage.
for verb in $verbs; do
  if ! grep -qF "claim $verb" "$doc"; then
    echo "docs-cover-cli: CLI verb '$verb' is not documented in $doc (expected 'claim $verb')" >&2
    missing=1
  fi
done

if [ "$missing" -ne 0 ]; then
  echo "docs-cover-cli: docs/index.html is missing coverage above; update it." >&2
  exit 1
fi

echo "docs-cover-cli: all $(echo "$verbs" | wc -w | tr -d ' ') verbs are documented in $doc"
exit 0
