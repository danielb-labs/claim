#!/usr/bin/env bash
# Assert that the docs site names every CLI verb and every MCP tool the tool
# actually ships. This is the check behind the `docs/index-covers-cli-and-mcp`
# claim: it drifts (exit 1) the moment a verb or an MCP tool is added to the code
# without being mentioned in docs/index.html, which is exactly how the item-14
# `create` tool shipped undocumented. It holds (exit 0) when the site is complete.
#
# Contract, so `claim check` maps it honestly (golden invariant #1):
#   exit 0  every verb and tool is documented        -> Held
#   exit 1  at least one is undocumented (drift)      -> Drifted
#   exit 2  the sources it reads are missing/unusable -> Broken (never a false pass)
#
# Runs from the repository root (the claim store root). The verb list comes from the
# *debug* binary's own `--help`, so it tracks the shipped CLI surface exactly. The
# gate keeps that binary current: `scripts/check.sh` has a "dogfood claims" step that
# builds `claim` (also a side effect of `cargo test`) and then runs
# `./target/debug/claim check`, which is what executes this check. So on the gate the
# binary is always fresh; a missing one is Broken (exit 2), never a false pass. The MCP
# tool list comes from the server source (each `#[tool]` handler), the single source of
# truth for what the server registers.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

doc="docs/index.html"
mcp_src="crates/claim-mcp/src/server.rs"
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
if [ ! -f "$mcp_src" ]; then
  echo "docs-cover-cli: $mcp_src not found" >&2
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

# MCP tools: every `#[tool]`-annotated handler in the server. Each registered tool is
# the `async fn <name>(` immediately under a `#[tool(` attribute, so anchoring on the
# attribute excludes ordinary helper fns. The rmcp macro names the tool after the fn.
tools="$(
  awk '
    /#\[tool\(/ { intool=1 }
    intool && /async fn [a-z_]+\(/ {
      line=$0
      sub(/.*async fn /, "", line)
      sub(/\(.*/, "", line)
      print line
      intool=0
    }
  ' "$mcp_src"
)"
if [ -z "$tools" ]; then
  echo "docs-cover-cli: could not extract any MCP tool from $mcp_src" >&2
  exit 2
fi

# The MCP tools are checked against only the MCP section of the site, and each must
# appear there as a *defined term* — the `<strong>tool</strong>` bullet heading the
# reference uses — not merely as a word in prose. A tool name like `create` is an
# ordinary English word ("created when there is a queue", "the counterpart of claim
# add creates ..."), so a whole-page or even whole-section word match would let
# incidental prose fake coverage — the precise vacuous-pass that let item-14's
# `create` tool ship undocumented. Requiring the `<strong>` entry ties coverage to an
# actual documentation entry for the tool, so deleting a tool's bullet drifts even if
# the word survives elsewhere. The section runs from its opening tag to its close.
mcp_section="$(
  awk '
    /<section id="mcp">/ { grab=1 }
    grab { print }
    grab && /<\/section>/ { exit }
  ' "$doc"
)"
if [ -z "$mcp_section" ]; then
  echo "docs-cover-cli: could not find the <section id=\"mcp\"> block in $doc" >&2
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

# A tool is documented when the MCP section carries its bullet entry
# `<strong>tool</strong>` (see the scoping note above) — the reference's defining
# mention, not a coincidental word in prose.
for tool in $tools; do
  if ! printf '%s\n' "$mcp_section" | grep -qF "<strong>$tool</strong>"; then
    echo "docs-cover-cli: MCP tool '$tool' has no <strong>$tool</strong> entry in the MCP section of $doc" >&2
    missing=1
  fi
done

if [ "$missing" -ne 0 ]; then
  echo "docs-cover-cli: docs/index.html is missing coverage above; update it." >&2
  exit 1
fi

echo "docs-cover-cli: all $(echo "$verbs" | wc -w | tr -d ' ') verbs and $(echo "$tools" | wc -w | tr -d ' ') MCP tools are documented in $doc"
exit 0
