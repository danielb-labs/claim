#!/usr/bin/env bash
# Pull the hub's rendered nag view from `GET /api/nags` for the CI glue to deliver.
#
# This is the read half of the hub's delivery loop (docs/hub.md, "The router and the nag"):
# the hub RENDERS nag content (it derives standing, groups a breaking commit's claims,
# resolves owners from CODEOWNERS in its mirror) and serves it as JSON; the scheduled CI
# lane pulls that JSON and DELIVERS it to the forge (the standing issue, PR comments). The
# forge write credential lives in the CI lane, never in the hub. This script is the pull;
# ci/nag-deliver.mjs renders the pulled JSON to markdown, and the Action YAML posts it.
#
# Factored out of the Action YAML so the whole pull path is testable OUTSIDE a real runner
# (crates/claim-hub/tests/nag_delivery.rs runs it against a locally-served hub in the gate).
#
# Fail-loud discipline, mirroring ci/hub-ingest.sh (CLAUDE.md invariant #6 — a hub outage is
# a loud red lane, never a stale green or a blanked issue):
#
#   1. GET `<hub-url>/api/nags` with the read-scoped bearer token, bounded by
#      --connect-timeout/--max-time so a refused or half-dead hub fails fast, never hangs.
#   2. A non-2xx (401 unauthenticated, 403 wrong scope, 500 the hub cannot derive, etc.) is
#      NEVER swallowed: print the hub's `{"error": "..."}` reason and exit non-zero.
#   3. A 2xx must carry the nag-view envelope (a `nags` array), or it is an impostor (a proxy
#      interstitial, a CDN page) and fails loud rather than reading as an empty "nothing to
#      nag" — which would let the lane blank the standing issue over broken data.
#
# On success the raw response body is written to --out (or stdout), for the delivery step to
# render. On ANY failure the script exits non-zero and writes nothing to --out, so the
# delivery step can leave the previous standing issue intact rather than overwriting it with
# an empty or partial body.
#
# Usage:
#   ci/hub-nags.sh --hub-url <url> [--out <file>] [--max-time <seconds>]
#
# Required:
#   --hub-url    The hub's base URL (e.g. https://hub.acme.example). /api/nags is appended.
#
# Optional:
#   --out        Write the response body here on success. Default: stdout. On failure this
#                file is NOT written (and is truncated to empty first, so a stale prior body
#                is never mistaken for a fresh pull).
#   --max-time   Seconds bounding the HTTP request's total run before it is a loud failure.
#                Default: 60. A hub that accepts the connection then never answers must NOT
#                hang the lane (invariant #6); the connect phase is separately bounded.
#
# Environment (the read credential — never a command-line argument, so it stays off `ps`):
#   HUB_NAGS_TOKEN   REQUIRED. The hub-minted read-scoped token (`claim-hub mint-token
#                    --scope read`), sent as `Authorization: Bearer`. The hub's /api is behind
#                    read auth; without this the pull is a 401 and the lane fails loudly. This
#                    is the ONE credential the delivery lane holds; the forge write token is
#                    separate and lives in the Action's post step, never here.

set -euo pipefail

# --- fail loud -------------------------------------------------------------------

# Print an error to stderr and exit non-zero. Every failure path routes through here so the
# lane fails with a named reason, never a silent or ambiguous exit (invariant #6).
die() {
  echo "hub-nags: error: $*" >&2
  exit 1
}

# --- argument parsing ------------------------------------------------------------

hub_url=""
out_file=""
max_time=60
# The connect-phase bound (seconds). Separate from max_time so a refused/black-holed hub
# fails fast on connect rather than waiting out the full total-time budget.
readonly CONNECT_TIMEOUT=10

while [ "$#" -gt 0 ]; do
  case "$1" in
    --hub-url)
      [ "$#" -ge 2 ] || die "--hub-url needs a value"
      hub_url="$2"
      shift 2
      ;;
    --out)
      [ "$#" -ge 2 ] || die "--out needs a value"
      out_file="$2"
      shift 2
      ;;
    --max-time)
      [ "$#" -ge 2 ] || die "--max-time needs a value (seconds)"
      case "$2" in
        ''|*[!0-9]*) die "--max-time expects a positive integer number of seconds, got \`$2\`" ;;
      esac
      [ "$2" -gt 0 ] || die "--max-time must be greater than zero, got \`$2\`"
      max_time="$2"
      shift 2
      ;;
    *)
      die "unrecognized argument \`$1\` (usage: --hub-url <url> [--out <file>] [--max-time <seconds>])"
      ;;
  esac
done

[ -n "$hub_url" ] || die "--hub-url is required (the hub's base URL, e.g. https://hub.acme.example)"
[ -n "${HUB_NAGS_TOKEN:-}" ] || die \
  "HUB_NAGS_TOKEN is required: the hub's /api is behind read auth. Mint one with \
\`claim-hub mint-token --scope read\` and pass it as HUB_NAGS_TOKEN"

command -v curl >/dev/null 2>&1 || die "curl is required but not found on PATH"
command -v jq >/dev/null 2>&1 || die "jq is required but not found on PATH"

# Trim a single trailing slash so `<url>/api/nags` never doubles it.
hub_url="${hub_url%/}"
nags_endpoint="$hub_url/api/nags"

# Truncate --out up front so a FAILED pull never leaves a stale prior body that a later step
# might mistake for a fresh one. The body is written here only on a validated success.
if [ -n "$out_file" ]; then
  : > "$out_file" || die "could not write to --out \`$out_file\`"
fi

# --- pull the nag view -----------------------------------------------------------

response_file="$(mktemp)"
trap 'rm -f "$response_file"' EXIT

echo "hub-nags: GET $nags_endpoint" >&2
# Capture the body and the HTTP status separately: `--fail-with-body` is deliberately NOT
# used, because we want to READ the hub's rejection reason on a non-2xx, not discard it.
# Bounded by --connect-timeout/--max-time so a hub that accepts the connection then never
# answers (slow-loris / half-dead) fails the lane loudly rather than hanging it to the
# runner's wall-clock (invariant #6). curl exits 28 on a timeout, named distinctly below;
# any other non-zero is a connection failure (refused, DNS, reset). The token is passed via
# an `-H` built from the env var, never interpolated onto the command line.
curl_status=0
http_code="$(curl --silent --show-error \
  --connect-timeout "$CONNECT_TIMEOUT" \
  --max-time "$max_time" \
  -o "$response_file" \
  -w '%{http_code}' \
  -H "Authorization: Bearer $HUB_NAGS_TOKEN" \
  -H "Accept: application/json" \
  "$nags_endpoint")" || curl_status=$?
if [ "$curl_status" -eq 28 ]; then
  die "the hub did not respond within ${max_time}s (GET $nags_endpoint timed out); leaving the previous standing issue intact"
fi
[ "$curl_status" -eq 0 ] \
  || die "GET $nags_endpoint failed to complete (the hub was unreachable or the request errored: curl exit $curl_status); leaving the previous standing issue intact"

response_body="$(cat "$response_file")"

# --- interpret the response: 2xx with the nag envelope is success ----------------

case "$http_code" in
  2??)
    # A 2xx alone is not enough: a proxy interstitial or CDN page can return 200 with a
    # non-JSON body, which would otherwise read as an empty "nothing to nag" and blank the
    # standing issue. The real hub always answers with the nag-view envelope, whose `nags` is
    # an array; require that before declaring success. `dead_letters` and `fired_this_pass`
    # are validated at render time; `nags` present-and-array is the load-bearing gate here.
    is_view="$(printf '%s' "$response_body" | jq -r 'if (.nags|type) == "array" then "yes" else empty end' 2>/dev/null || true)"
    [ "$is_view" = "yes" ] \
      || die "the hub returned $http_code but not the nag-view JSON (a \`nags\` array); body: $response_body; leaving the previous standing issue intact"
    if [ -n "$out_file" ]; then
      printf '%s' "$response_body" > "$out_file"
    else
      printf '%s' "$response_body"
    fi
    echo "hub-nags: OK ($http_code) — pulled the hub's nag view." >&2
    exit 0
    ;;
  *)
    # A non-2xx is NEVER swallowed (invariant #6): print the hub's reason and fail the lane so
    # the previous standing issue is left intact rather than blanked over a failed pull. The
    # hub answers a rejection with {"error": "..."} naming what to fix (a 401 for a missing or
    # bad read token, a 403 for a wrong scope, a 500 when it cannot derive the queue).
    reason="$(printf '%s' "$response_body" | jq -r '.error // empty' 2>/dev/null || true)"
    if [ -z "$reason" ]; then
      # No parseable reason (an empty body, or a non-JSON error page): surface the raw body so
      # the failure is still diagnosable, never a bare code.
      reason="$response_body"
    fi
    die "the hub rejected the nag pull (HTTP $http_code): $reason; leaving the previous standing issue intact"
    ;;
esac
