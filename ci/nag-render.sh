#!/usr/bin/env bash
# Render a pulled `/api/nags` view to the delivery body and enforce the renderer's exit
# contract, for the CI glue's delivery step. This is the render half of the delivery loop
# (docs/ci.md, "The hub-sourced lane"): ci/hub-nags.sh PULLS the view, this script RENDERS it
# via ci/nag-deliver.mjs and gates the result, and the Action's github-script step upserts it.
#
# Factored out of the Action YAML — exactly as ci/hub-nags.sh was — so the exit-code handling
# that decides open-vs-close AND fails loud on a renderer fault is testable OUTSIDE a real
# runner (crates/claim-hub/tests/nag_delivery.rs runs it in the gate). Keeping this logic in
# the YAML's `run:` block would leave the invariant-#6 fault path (a crashed renderer, an empty
# body) untestable and prone to drift.
#
# The renderer's exit code is a THREE-VALUE contract:
#   0  the hub reports nothing to nag — close the surface (clean).
#   1  the hub has a queue — open/update the surface (dirty).
#   2  the hub response could not be parsed.
#
# Only 0 and 1 are real findings. ANY other rc — the documented parse failure (2), or an
# undocumented fault (a node crash 139/134, an OOM 137, a spawn failure 127) — is a LOUD
# failure here, never a fall-through that writes a body the upsert would post. A crashed
# renderer's stdout redirect can leave the body file empty; posting that empty body would blank
# a good existing issue on the update path or spam a markerless new issue on the create path —
# the exact stale-green/blank this lane exists to prevent (CLAUDE.md invariant #6). As defense
# in depth, a body that came back empty even under rc 0/1 (a truncated or partial write) is
# also a loud failure: every real body opens with a marker line, so a zero-byte body is a fault.
#
# On success this appends `clean=<0|1>` and `body_file=<path>` to --output-file (the Action
# passes $GITHUB_OUTPUT), the two values the upsert step reads. On any fault it writes NOTHING
# to --output-file and exits non-zero, so the upsert step's `body_file != ''` gate is never
# satisfied and the previous surface is left intact.
#
# Usage:
#   ci/nag-render.sh --renderer <path> --mode issue|comment --nags <file> \
#     --body-file <file> --output-file <file>
#
# Required:
#   --renderer     Path to ci/nag-deliver.mjs (run with `node`).
#   --mode         `issue` or `comment` — which surface's body to render.
#   --nags         The pulled `/api/nags` JSON (ci/hub-nags.sh's --out).
#   --body-file    Where to write the rendered body. Passed to the upsert step on success.
#   --output-file  A GITHUB_OUTPUT-style file to append `clean=`/`body_file=` to on success.

set -euo pipefail

# Print an error to stderr and exit non-zero. Every fault routes through here so the lane
# fails with a named reason, never a silent or ambiguous exit (invariant #6).
die() {
  echo "nag-render: error: $*" >&2
  exit 1
}

renderer=""
mode=""
nags_file=""
body_file=""
output_file=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --renderer)    [ "$#" -ge 2 ] || die "--renderer needs a value";    renderer="$2";    shift 2 ;;
    --mode)        [ "$#" -ge 2 ] || die "--mode needs a value";        mode="$2";        shift 2 ;;
    --nags)        [ "$#" -ge 2 ] || die "--nags needs a value";        nags_file="$2";   shift 2 ;;
    --body-file)   [ "$#" -ge 2 ] || die "--body-file needs a value";   body_file="$2";   shift 2 ;;
    --output-file) [ "$#" -ge 2 ] || die "--output-file needs a value"; output_file="$2"; shift 2 ;;
    *) die "unrecognized argument \`$1\`" ;;
  esac
done

[ -n "$renderer" ]    || die "--renderer is required (the path to ci/nag-deliver.mjs)"
[ -n "$nags_file" ]   || die "--nags is required (the pulled /api/nags JSON)"
[ -n "$body_file" ]   || die "--body-file is required (where to write the rendered body)"
[ -n "$output_file" ] || die "--output-file is required (the GITHUB_OUTPUT-style file)"
case "$mode" in
  issue|comment) ;;
  *) die "--mode must be 'issue' or 'comment', got \`$mode\`" ;;
esac

command -v node >/dev/null 2>&1 || die "node is required but not found on PATH"

# Render the pulled view to the delivery body, in the same one place regardless of the forge
# surface. `set +e` around the node call so a non-zero rc is captured and adjudicated below
# rather than aborting under `set -e` before the reason can be named.
set +e
node "$renderer" --mode "$mode" --nags "$nags_file" > "$body_file"
rc=$?
set -e

if [ "$rc" -ne 0 ] && [ "$rc" -ne 1 ]; then
  die "the renderer exited $rc (not a clean/dirty finding); failing loud rather than posting a possibly-empty body, leaving the previous surface intact"
fi
if [ ! -s "$body_file" ]; then
  die "the renderer produced an empty body (rc=$rc); failing loud rather than posting it, leaving the previous surface intact"
fi

{
  echo "clean=$rc"
  echo "body_file=$body_file"
} >> "$output_file"
