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

echo "OK"
