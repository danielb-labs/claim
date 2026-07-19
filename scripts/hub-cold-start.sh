#!/usr/bin/env bash
# The cold-start self-host check (hub-15): boot the real `claim-hub` binary against an
# EMPTY directory and prove it stands up its own database and serves a TRUTHFUL /status —
# head 0, version 0, no rejections. This is the "point the hub at a fresh volume and it just
# boots" guarantee (HUB-IMPLEMENTATION.md §1.13) exercised against the real binary, no
# Docker required: the same first-boot path the container image runs on an empty volume.
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

# An EMPTY data directory: no database file yet. The hub must create and migrate it on boot.
data="$work/data"
mkdir -p "$data"
db="$data/hub.db"
[ -e "$db" ] && fail "the data directory is not empty; $db already exists"

cfg="$data/hub.toml"
printf 'listen = "127.0.0.1:%s"\ndatabase = "%s"\n' "$PORT" "$db" > "$cfg"

echo "==> booting from the empty directory"
"$hub_bin" --config "$cfg" >"$work/hub.log" 2>&1 &
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
