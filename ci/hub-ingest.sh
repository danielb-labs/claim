#!/usr/bin/env bash
# Push attested verdicts from a CI lane to the hub — the one CLI→hub ingest path.
#
# This is the core flow of the hub's ingest Action (docs/hub.md, "Pushing verdicts
# from CI"), factored out of the GitHub Action YAML so it is testable OUTSIDE a real
# runner: the whole check → token → POST → interpret-response path lives here and runs
# against a locally-served hub in the gate (crates/claim-hub/tests/ingest_action.rs),
# with the OIDC token injected rather than fetched. The Action YAML
# (.github/actions/hub-ingest/action.yml) is a thin wrapper that supplies the
# runner-specific token and calls this script.
#
# The flow, each step loud on failure (CLAUDE.md invariants #1 and #6 — a broken push
# is never a silent green, and a rejected push fails the lane with the hub's reason):
#
#   1. Run `claim check --json` over the repo's `.claims/` store, capturing the report.
#      The CLI's own exit code (0 held / 1 drifted / 2 broken) does NOT gate this push:
#      a drifted or broken verdict is TELEMETRY the hub must still receive so it can
#      derive standing and nag — swallowing it here would hide exactly the rot the hub
#      exists to surface. So the report is pushed regardless of the check's verdict; what
#      this script fails on is the *ingest* failing (a non-2xx from the hub), never a
#      drift the report faithfully carries.
#   2. Obtain the OIDC id-token. Parameterized for testing (see acquire_token): a
#      caller may inject a token via HUB_INGEST_TOKEN, or the script requests one from
#      the GitHub Actions token endpoint (the runner-specific path). The token proves
#      WHO produced this push — the pipeline — which is the whole basis of the hub's
#      trust (HUB.md §4).
#   3. POST the report + token to the hub's /ingest.
#   4. Fail LOUDLY on any non-2xx, printing the hub's rejection reason. A rejected or
#      broken push never passes as green (invariants #1/#6): the step exits non-zero and
#      the hub's `{"error": "..."}` reason is printed for the operator.
#
# Usage:
#   ci/hub-ingest.sh --hub-url <url> --audience <aud> [--claims-dir <dir>] [--claim-bin <path>]
#
# Required:
#   --hub-url    The hub's base URL (e.g. https://hub.acme.example). /ingest is appended.
#   --audience   The audience to mint the OIDC token for — MUST equal the hub's
#                configured `[oidc].audience`, or the hub rejects the token (401).
#
# Optional:
#   --claims-dir The working directory whose `.claims/` store to check. Default: `.`.
#   --claim-bin  The `claim` binary to run. Default: `claim` (on PATH).
#   --max-time   Seconds to bound each HTTP request's total run before it is a loud
#                failure. Default: 60. A hub that accepts the connection then never
#                answers (a slow-loris or half-dead hub) must NOT hang the lane to the
#                runner's wall-clock — invariants #1/#6 demand a timeout fail loudly, not
#                a stale green. The connect phase is separately bounded (see
#                CONNECT_TIMEOUT below).
#
# Environment (the token-acquisition seam — see acquire_token):
#   HUB_INGEST_TOKEN                 If set, used verbatim as the OIDC token; the GitHub
#                                    token endpoint is NOT called. This is the injection
#                                    point tests and the Action's token step use.
#   ACTIONS_ID_TOKEN_REQUEST_URL     GitHub Actions OIDC token endpoint (set by the
#   ACTIONS_ID_TOKEN_REQUEST_TOKEN   runner when `permissions: id-token: write` is
#                                    granted). Used only when HUB_INGEST_TOKEN is unset.

set -euo pipefail

# --- fail loud -------------------------------------------------------------------

# Print an error to stderr and exit non-zero. Every failure path routes through here so
# the lane fails with a named reason, never a silent or ambiguous exit (invariant #6).
die() {
  echo "hub-ingest: error: $*" >&2
  exit 1
}

# --- argument parsing ------------------------------------------------------------

hub_url=""
audience=""
claims_dir="."
claim_bin="claim"
# The total-time bound (seconds) on each HTTP request. A hub that stalls after accepting
# the connection must fail the lane loudly rather than hang it (invariants #1/#6), so
# both the token mint and the ingest POST carry `--max-time`. Overridable via --max-time.
max_time=60
# The connect-phase bound (seconds). Separate from max_time so a refused/black-holed hub
# fails fast on connect rather than waiting out the full total-time budget. Not a flag:
# 10s is ample for any reachable hub, and a longer connect wait only delays the loud
# failure.
readonly CONNECT_TIMEOUT=10

while [ "$#" -gt 0 ]; do
  case "$1" in
    --hub-url)
      [ "$#" -ge 2 ] || die "--hub-url needs a value"
      hub_url="$2"
      shift 2
      ;;
    --audience)
      [ "$#" -ge 2 ] || die "--audience needs a value"
      audience="$2"
      shift 2
      ;;
    --claims-dir)
      [ "$#" -ge 2 ] || die "--claims-dir needs a value"
      claims_dir="$2"
      shift 2
      ;;
    --claim-bin)
      [ "$#" -ge 2 ] || die "--claim-bin needs a value"
      claim_bin="$2"
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
      die "unrecognized argument \`$1\` (usage: --hub-url <url> --audience <aud> [--claims-dir <dir>] [--claim-bin <path>] [--max-time <seconds>])"
      ;;
  esac
done

[ -n "$hub_url" ] || die "--hub-url is required (the hub's base URL, e.g. https://hub.acme.example)"
[ -n "$audience" ] || die "--audience is required (must equal the hub's [oidc].audience)"

command -v curl >/dev/null 2>&1 || die "curl is required but not found on PATH"
command -v jq >/dev/null 2>&1 || die "jq is required but not found on PATH"

# Trim a single trailing slash so `<url>/ingest` never doubles it.
hub_url="${hub_url%/}"
ingest_endpoint="$hub_url/ingest"

# --- token acquisition (the runner-specific, injectable seam) --------------------

# Echo the OIDC id-token to stdout, or die.
#
# This is the ONE runner-specific step, deliberately isolated so a test can bypass it:
#
#   - If HUB_INGEST_TOKEN is set, it is used verbatim. This is the injection seam — a
#     test mints a token the local hub accepts and exports it here, and the Action's
#     own token step (which calls actions/github-script to mint the real token) exports
#     it the same way. Neither the test nor the Action touches the GitHub endpoint from
#     inside this script.
#   - Otherwise the script requests a token from the GitHub Actions OIDC endpoint, which
#     needs `permissions: id-token: write` on the job. The endpoint returns {"value":
#     "<jwt>"}; the token is minted for `audience`, which MUST match the hub's configured
#     audience or the hub rejects it.
#
# The token is never echoed to logs (only used to build the Authorization header); a
# leaked id-token is a replayable credential until it expires.
acquire_token() {
  if [ -n "${HUB_INGEST_TOKEN:-}" ]; then
    printf '%s' "$HUB_INGEST_TOKEN"
    return 0
  fi

  [ -n "${ACTIONS_ID_TOKEN_REQUEST_URL:-}" ] || die \
    "no OIDC token available: set HUB_INGEST_TOKEN, or run under GitHub Actions with \
\`permissions: id-token: write\` so ACTIONS_ID_TOKEN_REQUEST_URL is set"
  [ -n "${ACTIONS_ID_TOKEN_REQUEST_TOKEN:-}" ] || die \
    "ACTIONS_ID_TOKEN_REQUEST_TOKEN is unset (the runner grants it with \
\`permissions: id-token: write\`); cannot request an OIDC token"

  # The GitHub Actions token endpoint mints a token for the given audience. Bounded by
  # --connect-timeout/--max-time so a hung endpoint fails loudly rather than stalling the
  # lane (invariants #1/#6); curl exits 28 on a timeout, which we name distinctly.
  local encoded_audience response token curl_status
  encoded_audience="$(jq -rn --arg a "$audience" '$a|@uri')"
  curl_status=0
  response="$(curl --silent --show-error --fail-with-body \
    --connect-timeout "$CONNECT_TIMEOUT" \
    --max-time "$max_time" \
    -H "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" \
    -H "Accept: application/json; api-version=2.0" \
    "${ACTIONS_ID_TOKEN_REQUEST_URL}&audience=${encoded_audience}")" || curl_status=$?
  if [ "$curl_status" -eq 28 ]; then
    die "the GitHub Actions token endpoint did not respond within ${max_time}s (timed out)"
  fi
  [ "$curl_status" -eq 0 ] \
    || die "requesting an OIDC token from the GitHub Actions endpoint failed (curl exit $curl_status)"
  token="$(printf '%s' "$response" | jq -r '.value // empty')"
  [ -n "$token" ] || die "the GitHub Actions token endpoint returned no \`value\`"
  printf '%s' "$token"
}

# --- run the check ---------------------------------------------------------------

report_file="$(mktemp)"
response_file="$(mktemp)"
trap 'rm -f "$report_file" "$response_file"' EXIT

# Guard the two ways `claim check` never even runs, so the failure names the real cause
# rather than a fabricated "exited N": a missing --claims-dir (the `cd` fails) or a
# --claim-bin that is not an executable. `command -v` resolves a PATH name or an explicit
# path; without this the diagnostic below would misreport a 127/cd failure as a check that
# "ran and exited".
[ -d "$claims_dir" ] \
  || die "could not run \`claim check\`: --claims-dir \`$claims_dir\` is not a directory"
command -v "$claim_bin" >/dev/null 2>&1 \
  || die "could not run \`claim check\`: --claim-bin \`$claim_bin\` is not an executable on PATH"

echo "hub-ingest: running \`$claim_bin check --json\` in \`$claims_dir\`" >&2
# The check's exit code is captured but does NOT gate the push: a drifted (1) or broken
# (2) verdict is telemetry the hub must receive (see the header). A tool error that
# produces no parseable report is caught below when the POST body is validated. `|| ...`
# under `set -e` keeps a drift/broken exit from aborting the script before the push.
check_status=0
( cd "$claims_dir" && "$claim_bin" check --json ) > "$report_file" || check_status=$?
echo "hub-ingest: \`claim check\` exited $check_status (0=held 1=drifted 2=broken); pushing the report regardless" >&2

# A report that is not valid JSON means `claim check` ran but its output is not a report
# (a crash mid-run, a panic) — there is nothing honest to push, so fail loud rather than
# POST garbage the hub would reject anyway. The guards above already excluded the "could
# not run at all" cases, so this message honestly describes a check that ran.
jq -e . "$report_file" >/dev/null 2>&1 \
  || die "\`$claim_bin check --json\` ran (exit $check_status) but produced no JSON report; nothing to push"

# --- acquire the token, then POST ------------------------------------------------

token="$(acquire_token)"

echo "hub-ingest: POSTing the report to $ingest_endpoint" >&2
# Capture the body and the HTTP status separately: `--fail-with-body` is deliberately
# NOT used, because we want to READ the hub's rejection reason on a non-2xx, not just
# discard it. `-w '%{http_code}'` prints the status; the body goes to a file with `-o`.
# Bounded by --connect-timeout/--max-time so a hub that accepts the connection then never
# answers (slow-loris / half-dead) fails the lane loudly rather than hanging it to the
# runner's wall-clock (invariants #1/#6). curl exits 28 on a timeout, named distinctly
# below; any other non-zero is a connection failure (refused, DNS, reset).
curl_status=0
http_code="$(curl --silent --show-error \
  --connect-timeout "$CONNECT_TIMEOUT" \
  --max-time "$max_time" \
  -o "$response_file" \
  -w '%{http_code}' \
  -X POST \
  -H "Authorization: Bearer $token" \
  -H "Content-Type: application/json" \
  --data-binary "@$report_file" \
  "$ingest_endpoint")" || curl_status=$?
if [ "$curl_status" -eq 28 ]; then
  die "the hub did not respond within ${max_time}s (POST to $ingest_endpoint timed out)"
fi
[ "$curl_status" -eq 0 ] \
  || die "POST to $ingest_endpoint failed to complete (the hub was unreachable or the request errored: curl exit $curl_status)"

response_body="$(cat "$response_file")"

# --- interpret the response: 2xx is success, anything else fails loud -------------

case "$http_code" in
  2??)
    # A 2xx alone is not enough: a proxy interstitial or a CDN page can return 200 with a
    # non-JSON body, which would otherwise read as success. The real hub always answers an
    # accepted push with `{"accepted": N}`, so require that envelope — `.accepted` present
    # and numeric — before declaring success; anything else is an impostor and fails loud
    # rather than a stale green (invariants #1/#6).
    accepted="$(printf '%s' "$response_body" | jq -r 'if (.accepted|type) == "number" then .accepted else empty end' 2>/dev/null || true)"
    [ -n "$accepted" ] \
      || die "the hub returned $http_code but not the accepted-envelope JSON (\`{\"accepted\": N}\`); body: $response_body"
    echo "hub-ingest: OK ($http_code) — the hub accepted the push ($accepted event(s))." >&2
    exit 0
    ;;
  *)
    # A non-2xx is NEVER swallowed (invariants #1/#6): print the hub's reason and fail
    # the lane. The hub answers a rejection with {"error": "..."} naming what to fix.
    reason="$(printf '%s' "$response_body" | jq -r '.error // empty' 2>/dev/null || true)"
    if [ -z "$reason" ]; then
      # No parseable reason (an empty body, or a non-JSON error page): surface the raw
      # body so the failure is still diagnosable, never a bare code.
      reason="$response_body"
    fi
    die "the hub rejected the push (HTTP $http_code): $reason"
    ;;
esac
