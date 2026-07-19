#!/usr/bin/env bash
# Assert that the hub docs name every MCP tool the hub actually ships. This is the check
# behind the `docs/index-covers-hub-mcp` claim: it drifts (exit 1) the moment a `#[tool]` is
# added to the hub MCP without being documented in docs/hub.md — the same structural backstop
# the CLI's docs-cover-cli.sh gives the CLI verbs (CLAUDE.md, "Docs ship with the behavior
# they describe"). It holds (exit 0) when the hub docs mention every tool.
#
# Contract, so `claim check` maps it honestly (golden invariant #1):
#   exit 0  every MCP tool is documented                 -> Held
#   exit 1  at least one is undocumented (drift)          -> Drifted
#   exit 2  the sources it reads are missing/unusable     -> Broken (never a false pass)
#
# The tool list is the source of truth: it comes from the `#[tool(name = "...")]` attributes
# in the hub MCP module, so it tracks the shipped tool set exactly. Unlike the CLI backstop
# (which reads a built binary's `--help`), the hub's `tools/list` needs a running hub with a
# database; reading the registered names from the one file that declares them is the same
# coverage guarantee without booting a server — a tool cannot ship without a `#[tool(name)]`
# line here, and this reads every such line.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

doc="docs/hub.md"
src="crates/claim-hub/src/mcp.rs"

if [ ! -f "$src" ]; then
  echo "docs-cover-hub-mcp: $src not found; the hub MCP module is the tool-name source" >&2
  exit 2
fi
if [ ! -f "$doc" ]; then
  echo "docs-cover-hub-mcp: $doc not found" >&2
  exit 2
fi

# Tool names: the string in each `#[tool(name = "<name>", ...)]` attribute in the MCP module.
# Each registered tool declares its name there, so this is the exact shipped set.
tools="$(
  grep -oE 'name = "[a-z][a-z0-9_-]*"' "$src" \
    | sed -E 's/name = "([^"]+)"/\1/' \
    | sort -u
)"
if [ -z "$tools" ]; then
  echo "docs-cover-hub-mcp: could not extract any tool name from '$src'" >&2
  exit 2
fi

missing=0

# A tool is documented when the hub docs mention it as an inline-code token (`<tool>`) — the
# form the MCP section's tool table uses — so a bare word in prose does not count as coverage.
for tool in $tools; do
  if ! grep -qF "\`$tool\`" "$doc"; then
    echo "docs-cover-hub-mcp: MCP tool '$tool' is not documented in $doc (expected \`$tool\`)" >&2
    missing=1
  fi
done

if [ "$missing" -ne 0 ]; then
  echo "docs-cover-hub-mcp: docs/hub.md is missing coverage above; update it." >&2
  exit 1
fi

echo "docs-cover-hub-mcp: all $(echo "$tools" | wc -w | tr -d ' ') hub MCP tools are documented in $doc"
exit 0
