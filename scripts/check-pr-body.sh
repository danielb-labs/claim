#!/usr/bin/env bash
# Enforce that a pull request body carries every section the PR template defines.
# GitHub only *pre-fills* .github/PULL_REQUEST_TEMPLATE.md; it never enforces it, and a
# PR opened with an explicit body (`gh pr create --body ...`) silently bypasses it. This
# is the check behind the pr-template CI lane: it fails a PR whose body skips a template
# section, so the sections a reviewer relies on cannot be dropped.
#
# The required section list is DERIVED from the template's own `## ` headings, not
# hard-coded here, so adding or renaming a section in PULL_REQUEST_TEMPLATE.md updates the
# contract with no change to this script — the same way scripts/docs-cover-cli.sh reads
# the real verb list rather than a copy of it.
#
# Contract (0/1/2 so a consumer can map it honestly, golden invariant #1):
#   exit 0  every template section header appears in the body        -> pass
#   exit 1  one or more section headers are missing (drift)          -> fail, names each
#   exit 2  the body or the template cannot be read (broken)         -> fail, never a pass
#
# The body comes from a file path argument ($1) or, absent that, stdin. An empty or
# near-empty body has no section headers, so it fails with exit 1 (drift) — loudly, never
# a silent pass.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
template="$repo_root/.github/PULL_REQUEST_TEMPLATE.md"

if [ ! -f "$template" ]; then
  echo "check-pr-body: template $template not found; cannot derive required sections" >&2
  exit 2
fi

# Read the body from the file argument if given, else from stdin. A missing argument
# file is broken (exit 2): we were told to read a body and could not, which must never
# collapse into a pass.
if [ "$#" -ge 1 ]; then
  if [ ! -f "$1" ]; then
    echo "check-pr-body: body file '$1' not found" >&2
    exit 2
  fi
  body="$(cat "$1")"
else
  body="$(cat)"
fi

# The required sections are the template's `## ` headings, verbatim after the marker.
# `##` (a level-2 heading) is the section grammar the template uses; `#`/`###` are not
# sections. Read into an array so a heading containing spaces stays one entry.
required=()
while IFS= read -r line; do
  case "$line" in
    "## "*)
      required+=("${line#\#\# }")
      ;;
  esac
done <"$template"

if [ "${#required[@]}" -eq 0 ]; then
  echo "check-pr-body: no '## ' section headers found in $template" >&2
  exit 2
fi

# A section is present when its exact `## <name>` heading appears in the body. Matching
# the whole heading line (not just the name) means the body must reproduce the section
# structure, not merely mention the words somewhere in prose.
missing=0
for section in "${required[@]}"; do
  if ! printf '%s\n' "$body" | grep -qxF "## $section"; then
    echo "check-pr-body: PR body is missing the '## $section' section" >&2
    missing=1
  fi
done

if [ "$missing" -ne 0 ]; then
  echo "check-pr-body: the PR body must keep every template section; add the missing ones above (write 'None' where a section does not apply)." >&2
  exit 1
fi

echo "check-pr-body: all ${#required[@]} template sections are present in the PR body."
exit 0
