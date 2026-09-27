# darkmux-runtime — container image for the in-house agent runtime.
#
# Multi-stage build:
#   1. Build the Rust binary against musl (alpine-native).
#   2. Run it in a small Alpine image with the Unix toolkit agents need.
#
# The runtime hosts a per-dispatch agent loop: receives a model + system
# prompt + user message, runs the tool-call loop until stop, records its
# trajectory, exits. The container is the kernel-enforced
# workspace boundary — workspace mounts at /workspace, and the binary's
# path validators reject anything that resolves outside it.

# ─── builder ──────────────────────────────────────────────────────────────
# rust:alpine pulls the latest stable Rust on Alpine. Pinning to a
# specific 1.NN-alpine tag caused MSRV-bump issues with transitive deps
# (e.g. idna_adapter 1.2.2 requires Rust ≥1.83 as of 2026-05). Letting
# the image float on the latest-stable tag keeps the build robust;
# pin once exact dep versions stabilize.
FROM rust:alpine AS builder

# musl-dev: C runtime for musl-linked Rust
# pkgconfig + openssl-dev: present so future deps that need OpenSSL build
# without a second image revision (ureq + rustls don't need them, but
# safer to have them for Phase 3+).
RUN apk add --no-cache musl-dev pkgconfig openssl-dev

WORKDIR /build

# The build context is the REPO ROOT, not runtime/: the runtime depends by
# path on crates/darkmux-trajectory, the trajectory event types it shares
# with the host. `runtime/Dockerfile.dockerignore` restricts the context
# to exactly the files copied here. Build it with:
#   docker build --build-arg DARKMUX_VERSION=<version> -f runtime/Dockerfile -t darkmux-runtime:latest .
# Cargo.lock ensures reproducibility — the docker build uses the same dep
# versions as the local build.
COPY crates/darkmux-trajectory ./crates/darkmux-trajectory
COPY runtime/Cargo.toml runtime/Cargo.lock ./runtime/
COPY runtime/src ./runtime/src
WORKDIR /build/runtime

# Build the release binary.
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
#   python3    — stdlib unittest runs out of the box; lets coding-task
#                workloads with `setupContent: { "*.py": ... }` run their
#                verify commands inside the container without an external
#                pip install (#338).
#   py3-pip    — for workloads that need third-party Python packages
#                (pytest, requests, etc.); operators install per-dispatch
#                or per-sandbox as needed.
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
        npm \
        python3 \
        py3-pip

# Non-root user. UID 1000 is the convention; matches what a typical
# host-side macOS user maps to inside Docker for Mac when sharing volumes.
RUN adduser -D -u 1000 -h /home/darkmux -s /bin/bash darkmux

# Copy the runtime binary from the builder stage.
COPY --from=builder /build/runtime/target/release/darkmux-runtime /usr/local/bin/darkmux-runtime

# Dispatch mounts the operator's per-dispatch workspace at /workspace.
# Pre-creating the dir means the sanity check (`--check`) has something
# to probe against when no volume is mounted, surfacing "you forgot to
# mount" failures loudly rather than silently no-oping.
RUN mkdir -p /workspace && chown -R darkmux:darkmux /workspace

# (#1461, #2923) Stamp WHICH darkmux version this image was built for.
# Dispatch reads this label (metadata only) and runs a local
# `darkmux-runtime:<tag>` only when it equals the host's version; otherwise it
# runs the version-pinned GHCR image, and refuses an explicitly named
# mismatched one. Unset (a plain `docker build` with no --build-arg) leaves the
# label empty, which counts as a mismatch: an unlabeled local image is never
# run. Declared AFTER the last RUN on purpose: a build-arg is part of the
# cache key for every RUN that follows it, so declaring it early would make a
# rebuild at a new version re-run `apk add` and the rest. Here, a new version
# only changes the label layer. Nothing below uses the arg. The build
# command is at the top of the builder stage.
ARG DARKMUX_VERSION=""
LABEL org.opencontainers.image.version=$DARKMUX_VERSION

USER darkmux
WORKDIR /workspace

ENTRYPOINT ["/usr/local/bin/darkmux-runtime"]
CMD ["--check"]
