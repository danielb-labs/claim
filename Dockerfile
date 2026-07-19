# The packaged `claim-hub` image (hub-15): one static binary, bundled SQLite, plus the
# two runtime dependencies the hub genuinely needs — `git` (registry sync shells out to
# it) and CA certificates (the JWKS fetch over HTTPS validates GitHub's cert against the
# OS trust store). Multi-stage so the runtime image ships the binary and those two things
# and nothing of the ~2 GB build toolchain.
#
# Why musl-static: HUB-IMPLEMENTATION.md §1.13 wants "one binary … drops into any runner
# with no runtime." A musl target links libc statically, so the binary carries no glibc
# version dependency and runs on a `scratch`-thin base. SQLite is compiled in via sqlx's
# bundled `libsqlite3-sys` regardless of target, so there is no system libsqlite to link.
#
# The one honest build-time cost, recorded in crates/claim-hub/Cargo.toml: reqwest's
# `rustls` feature pulls `aws-lc-rs` for the TLS handshake, and `aws-lc-sys` builds C and
# assembly — so the BUILD stage must have a C toolchain and `cmake`. The runtime image
# needs none of that; the compiled binary has no such dependency. (JWT verification stays
# pure-Rust via jsonwebtoken's `rust_crypto`; only the TLS handshake uses aws-lc-rs.)

# ---- build stage -------------------------------------------------------------------
# Alpine's Rust image is musl-native, so the default target is already
# x86_64-unknown-linux-musl — no cross-linker to configure. Pin the Rust minor so an
# image rebuild is reproducible; bump it deliberately, the way the CI toolchain pin is.
FROM rust:1.90-alpine AS build

# The build toolchain aws-lc-sys needs (C compiler, assembler, make, cmake) plus musl-dev
# for the static libc headers, and perl, which aws-lc-sys's build invokes. Without these
# the reqwest/rustls dependency fails to compile — the note in claim-hub's Cargo.toml made
# concrete as a build dependency.
RUN apk add --no-cache musl-dev gcc make cmake perl

WORKDIR /src

# Build the hub against the committed sqlx offline cache, never a live database — exactly
# as scripts/check.sh does. The image build has no DATABASE_URL, so this is what lets
# sqlx's compile-time-checked queries build here.
ENV SQLX_OFFLINE=true

# Copy the whole workspace. A .dockerignore keeps `target/`, the git dir, and local
# scratch out of the build context so the copy is small and cache-friendly.
COPY . .

# Build only the hub binary, release-optimized. `--locked` builds against the committed
# Cargo.lock so the image's dependency versions match the repo's exactly — a floating
# lockfile in a trust tool's own image is the drift this product exists to prevent.
RUN cargo build --locked --release -p claim-hub --target x86_64-unknown-linux-musl

# ---- runtime stage -----------------------------------------------------------------
# Alpine, not scratch: the hub needs `git` (registry sync) and CA certificates (the JWKS
# HTTPS fetch validates against the OS trust store — reqwest/rustls uses the platform
# verifier, not a crate-bundled root set, per the caveat in claim-hub/Cargo.toml). Alpine
# is musl-native, so the musl-static binary runs directly, and it stays tiny.
FROM alpine:3.20 AS runtime

# git: the one runtime dependency besides the binary (registry sync shells to `git`).
# ca-certificates: the trust roots the JWKS TLS handshake validates GitHub's cert against.
RUN apk add --no-cache git ca-certificates

# A non-root user owns the data directory, so the one file the customer owns is never
# written as root. The compose example mounts the data volume here.
RUN addgroup -S hub && adduser -S -G hub -h /data hub

COPY --from=build /src/target/x86_64-unknown-linux-musl/release/claim-hub /usr/local/bin/claim-hub

# The data directory is the mount point for the customer-owned volume: the hub creates and
# migrates its SQLite database here on first boot, and export/backup is a copy of this dir.
WORKDIR /data
USER hub

# The hub binds loopback by default; the container sets 0.0.0.0 via CLAIM_HUB_LISTEN so the
# mapped port is reachable, and points the database at the mounted data volume. These are
# env overrides (not baked into a config file) so a compose file or `docker run -e` can
# vary them per instance without editing an image-internal file.
ENV CLAIM_HUB_LISTEN="0.0.0.0:8080" \
    CLAIM_HUB_DATABASE="/data/hub.db"
EXPOSE 8080

# The hub reads hub.toml in the working directory (/data) if present. With no --config flag
# and no hub.toml, a *missing* default config is not an error: the binary starts from an
# empty config so these CLAIM_HUB_* env overrides alone drive the boot — which is what lets
# an EMPTY data volume with no config file boot on the right address and database path. (A
# malformed hub.toml, or a --config path that is missing, is still a loud failure.) A
# self-hoster who wants richer config (stores, [oidc], [read_auth]) drops a hub.toml into the
# mounted volume; env overrides still win.
#
# The image deliberately does NOT set CLAIM_HUB_OPEN_READS: read auth is secure by default,
# so an unconfigured hub refuses to boot rather than serve open reads. The operator makes the
# read-auth decision explicitly — a [read_auth.issuer] or [[read_auth.tokens]] in a mounted
# hub.toml, or `CLAIM_HUB_OPEN_READS=true` for a trusted private network. Baking open reads
# into the image would be exactly the "open by accident" regression secure-by-default prevents.
ENTRYPOINT ["claim-hub"]
