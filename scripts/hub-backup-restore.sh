#!/usr/bin/env bash
# The load-bearing self-host test (hub-15): back up a live hub with SQLite's online backup,
# restore it into a fresh hub, and prove the restored hub derives IDENTICAL answers. This
# is the data-ownership invariant (HUB.md §1/§4) exercised as an operation rather than
# asserted: the whole hub is one file the customer controls, so a consistent backup and a
# restore into a clean instance must reproduce the exact standing.
#
# The backup is `sqlite3 hub.db ".backup"`, NOT a file copy: a `cp` against a live WAL-mode
# hub is a hot copy that races a checkpoint and can silently drop the ledger tail (invariants
# #4/#6). `.backup` reads a transactionally-consistent snapshot and writes ONE self-contained
# file with no `-wal`/`-shm` sidecars, so a restore is a plain copy of that one file.
#
# It runs the REAL binaries, no Docker required:
#   1. seed a hub — sync a local git fixture and ingest one attested `held` verdict, through
#      the real sync and the real ingest gate (`--example seed_hub`), into a file-backed DB;
#   2. boot the real `claim-hub` server over that DB and read the claim's standing;
#   3. back up with `sqlite3 ".backup"` against the LIVE (still-running) hub — the safe
#      "leave by taking the file" operation the docs promise;
#   4. restore the single backup file into a fresh directory (deleting any stale sidecar so
#      SQLite cannot recover a wrong-but-consistent state) and boot a second real `claim-hub`
#      server over it — no re-seed, no re-sync;
#   5. assert the restored hub's derived standing is byte-identical to the original's on the
#      load-bearing fields (standing, verified_as_of, stale_at, and the ledger/registry
#      as-of). The read clock (wall-clock now) legitimately differs between the two reads and
#      is excluded — those fields are functions of the ledger event alone, so they must match.
#
# Contract, so `claim check` / the gate map it honestly (golden invariant #1):
#   exit 0  the restored hub derived an identical standing   -> success
#   exit 1  the standings differ, or a step failed loudly    -> failure (never a false pass)
#
# Determinism: the fixture is a local git repo (no network), the JWKS is injected in the
# seed (no GitHub), and the verdict's `reported_at` is a fixed instant — so verified_as_of
# and stale_at are fixed values, identical across the copy. The gate runs this; CI runs the
# same script.
#
# `sqlite3` is a HOST tool here (the gate runner and CI's ubuntu-latest both ship it); the
# runtime container image deliberately does NOT bundle it, so an in-container backup runs
# `sqlite3` on the host against the mounted volume (see docs/hub.md).
set -euo pipefail

if ! command -v cargo >/dev/null 2>&1; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
command -v sqlite3 >/dev/null 2>&1 || {
  echo "hub-backup-restore: sqlite3 is required for the online backup; install it and retry" >&2
  exit 1
}
export SQLX_OFFLINE=true

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

# A free-enough loopback port; if it is taken the boot fails loudly and the run is retried.
PORT_A="${CLAIM_HUB_TEST_PORT_A:-8231}"
PORT_B="${CLAIM_HUB_TEST_PORT_B:-8232}"

work="$(mktemp -d)"
srv_a=""
srv_b=""
cleanup() {
  [ -n "$srv_a" ] && kill "$srv_a" 2>/dev/null || true
  [ -n "$srv_b" ] && kill "$srv_b" 2>/dev/null || true
  [ -n "$srv_a" ] && wait "$srv_a" 2>/dev/null || true
  [ -n "$srv_b" ] && wait "$srv_b" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

fail() {
  echo "hub-backup-restore: $*" >&2
  exit 1
}

# Build the two binaries the test drives, up front, so a compile error fails before any
# server boots (and the boot loop does not race a slow first build).
echo "==> building claim-hub and the seed example"
cargo build -q -p claim-hub
cargo build -q -p claim-hub --example seed_hub

hub_bin="$repo_root/target/debug/claim-hub"
[ -x "$hub_bin" ] || fail "claim-hub binary not found at $hub_bin"

# A git fixture carrying one claim: the store the hub syncs. A local repo, so no network.
fixture="$work/fixture"
mkdir -p "$fixture/.claims"
cat > "$fixture/.claims/pin.md" <<'CLAIM'
---
id: payments/libfoo-pin
hub:
  max-age: 30d
checks:
  - kind: cmd
    run: "grep -q 'libfoo==4.2' requirements.txt"
---
The libfoo pin holds.
CLAIM
# Wall off ambient git config so a developer's global identity or default branch cannot
# change the fixture's shape (mirrors the integration tests' fixture discipline).
git_c() { git -C "$fixture" -c user.name=Test -c user.email=test@example.com \
  -c commit.gpgsign=false -c init.defaultBranch=main "$@"; }
git_c init -q
git_c add -A
git_c commit -q -m "add claim"

# 1. Seed the original hub: sync the fixture + ingest one attested verdict into db_a.
data_a="$work/a"
mkdir -p "$data_a"
db_a="$data_a/hub.db"
echo "==> seeding the original hub (sync fixture + ingest a verdict)"
claim_id="$(cargo run -q -p claim-hub --example seed_hub -- "$db_a" "$fixture")" \
  || fail "seeding failed"
[ -n "$claim_id" ] || fail "seed produced no claim id"
[ -f "$db_a" ] || fail "seed created no database at $db_a"

# Boot a real claim-hub server over the seeded DB and wait until it answers /status.
boot_hub() {
  local bin="$1" db="$2" port="$3" logf="$4"
  local cfg
  cfg="$(dirname "$db")/hub.toml"
  # `open_reads = true` is this local exercise's read-auth decision: the hub is secure by
  # default and will not boot without one, and this script reads /api/claims over loopback.
  # A real deployment configures an IdP or a minted token instead (docs/hub.md).
  printf 'listen = "127.0.0.1:%s"\ndatabase = "%s"\n[read_auth]\nopen_reads = true\n' \
    "$port" "$db" > "$cfg"
  "$bin" --config "$cfg" >"$logf" 2>&1 &
  local pid=$!
  for _ in $(seq 1 100); do
    if ! kill -0 "$pid" 2>/dev/null; then
      cat "$logf" >&2
      fail "hub on port $port exited during boot"
    fi
    if curl -fsS -o /dev/null "http://127.0.0.1:$port/status" 2>/dev/null; then
      echo "$pid"
      return 0
    fi
    sleep 0.1
  done
  cat "$logf" >&2
  fail "hub on port $port did not become ready"
}

# A stable projection of a claim's standing: everything derived from the ledger event and
# the registry, with the wall-clock read instant (`as_of.clock`) dropped — it legitimately
# differs between two reads seconds apart, while every other field is a pure function of the
# stored evidence and MUST survive the backup unchanged. jq sorts keys so the two renderings
# are byte-comparable regardless of field order.
standing_projection() {
  local port="$1" id="$2"
  local body
  body="$(curl -fsS "http://127.0.0.1:$port/api/claims/$id")" \
    || fail "reading /api/claims/$id from port $port failed"
  printf '%s' "$body" | jq -S '{
    id, store, standing, verified_as_of, stale_at, due_at, skips,
    as_of: { ledger_head: .as_of.ledger_head, registry_version: .as_of.registry_version }
  }'
}

echo "==> booting the original hub and reading the claim's standing"
srv_a="$(boot_hub "$hub_bin" "$db_a" "$PORT_A" "$work/a.log")"
status_a="$(curl -fsS "http://127.0.0.1:$PORT_A/status")" || fail "reading /status (a) failed"
answer_a="$(standing_projection "$PORT_A" "$claim_id")"
echo "    original /status:   $status_a"
echo "    original standing:  $(printf '%s' "$answer_a" | jq -c .)"

# The original hub must actually hold the seeded evidence, else "identical" is vacuous.
head_a="$(printf '%s' "$status_a" | jq '.ledger_head')"
[ "$head_a" = "1" ] || fail "original hub has ledger_head=$head_a, expected 1 (seed did not land)"
standing_val="$(printf '%s' "$answer_a" | jq -r '.standing')"
[ "$standing_val" = "verified" ] || fail "seeded claim is '$standing_val', expected 'verified'"

# 2. Back up the LIVE hub with SQLite's online backup: a transactionally-consistent snapshot
#    into ONE self-contained file, no sidecars. The original hub is still running (a live hub
#    is not stopped to be backed up), which is exactly why a `cp` would be unsafe and `.backup`
#    is not. The backup file must not pre-exist (SQLite writes it fresh).
echo "==> backing up the live hub with sqlite3 .backup (online, consistent)"
backup_dir="$work/backup"
mkdir -p "$backup_dir"
backup_file="$backup_dir/hub.db"
sqlite3 "$db_a" ".backup '$backup_file'" || fail "online backup (.backup) failed"
[ -f "$backup_file" ] || fail "the backup produced no file at $backup_file"
{ [ -f "$backup_file-wal" ] || [ -f "$backup_file-shm" ]; } \
  && fail "the online backup left a WAL/SHM sidecar; it must be one self-contained file"

# 3. Restore: a plain copy of the ONE backup file into a fresh directory. Delete any stale
#    -wal/-shm beside the destination first, so SQLite cannot recover a wrong-but-consistent
#    state from a leftover sidecar. No sidecars are restored — the backup carries none.
echo "==> restoring the single backup file into a fresh hub and reading the same claim"
data_b="$work/b"
mkdir -p "$data_b"
db_b="$data_b/hub.db"
rm -f "$db_b" "$db_b-wal" "$db_b-shm"
cp "$backup_file" "$db_b"

srv_b="$(boot_hub "$hub_bin" "$db_b" "$PORT_B" "$work/b.log")"
status_b="$(curl -fsS "http://127.0.0.1:$PORT_B/status")" || fail "reading /status (b) failed"
answer_b="$(standing_projection "$PORT_B" "$claim_id")"
echo "    restored /status:   $status_b"
echo "    restored standing:  $(printf '%s' "$answer_b" | jq -c .)"

# 4. The load-bearing assertion: the restored hub derives the identical standing.
if [ "$answer_a" != "$answer_b" ]; then
  echo "hub-backup-restore: the restored hub derived a DIFFERENT standing." >&2
  echo "--- original ---" >&2; printf '%s\n' "$answer_a" >&2
  echo "--- restored ---" >&2; printf '%s\n' "$answer_b" >&2
  exit 1
fi

# The restored hub's position must match too — same ledger head and registry version, since
# the file carried the whole ledger and registry.
head_b="$(printf '%s' "$status_b" | jq '.ledger_head')"
ver_a="$(printf '%s' "$status_a" | jq '.registry_version')"
ver_b="$(printf '%s' "$status_b" | jq '.registry_version')"
[ "$head_a" = "$head_b" ] || fail "ledger_head differs after restore: $head_a vs $head_b"
[ "$ver_a" = "$ver_b" ] || fail "registry_version differs after restore: $ver_a vs $ver_b"

echo "hub-backup-restore: OK — the restored hub derives an identical standing"
echo "  claim=$claim_id standing=$standing_val ledger_head=$head_a registry_version=$ver_a"
