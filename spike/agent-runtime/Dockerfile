# darkmux-agent — Phase 1 spike image.
#
# Multi-stage build:
#   1. Build the Rust binary against musl (alpine-native).
#   2. Run it in a small Alpine image with the Unix toolkit agents need.
#
# Goal of this image (Phase 1): prove the build + runtime path works.
# Subsequent phases add the LMStudio client + tool-call loop + tool set.

# ─── builder ──────────────────────────────────────────────────────────────
# rust:alpine pulls the latest stable Rust on Alpine. Pinning to a
# specific 1.NN-alpine tag caused MSRV-bump issues with transitive deps
# (e.g. idna_adapter 1.2.2 requires Rust ≥1.83 as of 2026-05). Letting
# the image float on the latest-stable tag keeps the spike build robust;
# the production design can pin once we know exact dep versions.
FROM rust:alpine AS builder

# musl-dev: C runtime for musl-linked Rust
# pkgconfig + openssl-dev: present so future deps that need OpenSSL build
# without a second image revision (ureq + rustls don't need them, but
# safer to have them for Phase 3+).
RUN apk add --no-cache musl-dev pkgconfig openssl-dev

WORKDIR /build

# Copy manifest + lockfile first so the cargo dep cache is reused when
# only src changes. Cargo.lock ensures reproducibility — the docker
# build uses the same dep versions as the local build.
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# Build the release binary. The Phase 1 binary had no runtime deps;
# Phase 2 adds ureq + serde so this step now does real work on first
# build (~30s on cache miss, <2s on cache hit).
RUN cargo build --release --locked || cargo build --release

# ─── runtime ──────────────────────────────────────────────────────────────
FROM alpine:3.20

# The Unix toolkit agents reach for. Stays small.
#
#   bash       — agents prefer bash to ash for shell tool calls
#   jq         — JSON munging in agent scripts
#   ripgrep    — fast search (called as `rg`)
#   findutils  — GNU find (alpine ships busybox find by default; agents
#                expect GNU-style flags like -path / -name combos)
#   grep, sed, gawk — coreutils that agents reach for; alpine's busybox
#                versions are missing flags
#   git        — source-tree operations
#   curl       — host-networking sanity checks + future HTTP probes from
#                inside the container
#   ca-certificates — for any TLS-bearing tool the agent invokes
RUN apk add --no-cache \
        bash \
        jq \
        ripgrep \
        findutils \
        grep \
        sed \
        gawk \
        git \
        curl \
        ca-certificates \
        nodejs \
        npm

# Non-root user. UID 1000 is the convention; matches what a typical
# host-side macOS user maps to inside Docker for Mac when sharing volumes.
RUN adduser -D -u 1000 -h /home/darkmux -s /bin/bash darkmux

# Copy the agent binary from the builder stage.
COPY --from=builder /build/target/release/darkmux-agent /usr/local/bin/darkmux-agent

# The dispatch protocol Phase 4 will wire up mounts the operator's
# per-dispatch workspace at /workspace. Phase 1 just creates the dir so
# the sanity check has something to probe against when no volume is mounted
# (catches "you forgot to mount" failure mode loudly).
RUN mkdir -p /workspace && chown -R darkmux:darkmux /workspace

USER darkmux
WORKDIR /workspace

ENTRYPOINT ["/usr/local/bin/darkmux-agent"]
CMD ["--check"]
