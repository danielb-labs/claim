#!/usr/bin/env bash
# The cold-start self-host check (hub-15): boot the real `claim-hub` binary against an
# EMPTY directory with NO config file — only CLAIM_HUB_* env overrides, exactly as the
# container image runs — and prove it stands up its own database and serves a TRUTHFUL
# /status: head 0, version 0, no rejections. This is the "point the hub at a fresh volume
# and it just boots" guarantee (HUB-IMPLEMENTATION.md §1.13) exercised against the real
# binary, no Docker required.
#
# Config-less on purpose: with no --config and no hub.toml, a missing default config is not
# an error — the binary starts from an empty config so the env overrides alone drive the
# boot. This is the exact path `docker run` against an empty volume hits; a regression that
# made a missing default config fatal would fail here (and in the container step) loudly.
#
# CLAIM_HUB_OPEN_READS=true is this demo's explicit read-auth decision: the hub is secure by
# default and refuses to boot without one, so a cold-start with no IdP and no minted token
# opts into open reads. That mirrors the compose example, which binds loopback; a real
# deployment configures an IdP or a minted token instead. (This script binds loopback too.)
#
# Truthful means the empty state is reported as empty, never as an error and never as a
# fabricated "healthy" (invariant #6): a hub that lies about its own position is the first
# thing a monitor trusts and the last thing it should.
#
# Contract, so `claim check` / the gate map it honestly (golden invariant #1):
#   exit 0  the hub booted from empty and reported head 0 / version 0   -> success
#   exit 1  it failed to boot, or reported a non-empty/absent position  -> failure
set -euo pipefail

if ! command -v cargo >/dev/null 2>&1; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
export SQLX_OFFLINE=true

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

PORT="${CLAIM_HUB_TEST_PORT:-8233}"
work="$(mktemp -d)"
srv=""
cleanup() {
  [ -n "$srv" ] && kill "$srv" 2>/dev/null || true
  [ -n "$srv" ] && wait "$srv" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

fail() { echo "hub-cold-start: $*" >&2; exit 1; }

echo "==> building claim-hub"
cargo build -q -p claim-hub
hub_bin="$repo_root/target/debug/claim-hub"
[ -x "$hub_bin" ] || fail "claim-hub binary not found at $hub_bin"

# An EMPTY data directory: no database file and NO hub.toml. The hub must create and
# migrate the database on boot and start from an empty config (env overrides only).
data="$work/data"
mkdir -p "$data"
db="$data/hub.db"
[ -e "$db" ] && fail "the data directory is not empty; $db already exists"
[ -e "$data/hub.toml" ] && fail "the data directory has a hub.toml; the cold-start must be config-less"

echo "==> booting config-less from the empty directory (env overrides only)"
# Run with the empty data dir as the working directory, so the default hub.toml lookup
# resolves *there* and provably finds nothing — the env overrides alone drive the boot.
# `exec` so the hub replaces the subshell and `$!` is the hub's own PID for cleanup.
( cd "$data" && exec env CLAIM_HUB_LISTEN="127.0.0.1:$PORT" CLAIM_HUB_DATABASE="$db" \
  CLAIM_HUB_OPEN_READS="true" \
  "$hub_bin" >"$work/hub.log" 2>&1 ) &
srv=$!

ready=""
for _ in $(seq 1 100); do
  if ! kill -0 "$srv" 2>/dev/null; then
    cat "$work/hub.log" >&2
    fail "the hub exited during boot from an empty directory"
  fi
  if curl -fsS -o /dev/null "http://127.0.0.1:$PORT/status" 2>/dev/null; then
    ready=1
    break
  fi
  sleep 0.1
done
[ -n "$ready" ] || { cat "$work/hub.log" >&2; fail "the hub did not become ready"; }

# The hub created its own database on first boot.
[ -f "$db" ] || fail "the hub did not create its database at $db"

status="$(curl -fsS "http://127.0.0.1:$PORT/status")" || fail "reading /status failed"
echo "    /status: $status"

head="$(printf '%s' "$status" | jq '.ledger_head')"
version="$(printf '%s' "$status" | jq '.registry_version')"
rejections="$(printf '%s' "$status" | jq '.rejection_count')"

[ "$head" = "0" ] || fail "empty hub reports ledger_head=$head, expected 0"
[ "$version" = "0" ] || fail "empty hub reports registry_version=$version, expected 0"
[ "$rejections" = "0" ] || fail "empty hub reports rejection_count=$rejections, expected 0"

echo "hub-cold-start: OK — booted from an empty directory, /status reports a truthful empty position"
