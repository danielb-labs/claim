#!/usr/bin/env bash
# The quality gate. Every branch must pass this before it merges to main, and
# CI runs the same steps. Kept identical to CI so "works on my machine" and
# "passes CI" cannot diverge.
#
# Fails on the first problem. Warnings are errors: a warning nobody fixes is a
# warning everybody learns to ignore.
set -euo pipefail

# Make the toolchain available whether or not the shell sourced the profile.
if ! command -v cargo >/dev/null 2>&1; then
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi

echo "==> formatting"
cargo fmt --all --check

echo "==> clippy (warnings are errors)"
cargo clippy --workspace --all-targets --all-features -- -D warnings

echo "==> tests"
cargo test --workspace --all-features

echo "==> docs (broken intra-doc links are errors)"
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items

# The CI lanes' body-building logic (ci/render.mjs) is the one non-Rust piece of the
# product, and its unit tests are the only coverage GitHub Actions can't run locally.
# Gate on them too, so a change that breaks the comment/issue rendering fails here, not
# in production. Node ships on GitHub runners; if a dev shell lacks it, we skip loudly
# rather than pass silently — a skipped test that looks green is exactly what this tool
# exists to prevent.
echo "==> ci renderer tests"
if command -v node >/dev/null 2>&1; then
  node --test "ci/*.test.mjs"
else
  echo "WARNING: node not found; skipping ci/render.test.mjs (CI runs it)." >&2
fi

echo "OK"
