# flatsat — the darkmux binary, built for the container's OWN platform.
#
# DEVIATION FROM THE PACKET BRIEF (documented, not silent): the brief asks
# to "build once on the host (`cargo build --release`) and bind-mount
# target/release/darkmux". On this dev machine the host is macOS/arm64
# (Mach-O binaries); `docker info` reports the Docker Desktop VM as
# linux/arm64. A Mach-O binary cannot execute under a Linux kernel
# regardless of bind-mount — this isn't a flatsat quirk, it's true for
# EVERY machine in the fleet doctrine's "a few Macs on a tailnet" shape
# (README/OPERATIONS.md), so no host in this project ever produces a
# container-runnable binary directly.
#
# What preserves the brief's actual INTENT (build once, not per-service) is
# this multi-stage build compiled to ONE image tag that both `hub` and
# `peer` reference (see up.sh — `docker build` runs exactly once; compose
# does not rebuild per service). Docker's own layer cache makes repeat
# `up` runs on an unchanged tree fast, same effect a bind-mounted
# pre-built binary would have had.
#
# Mirrors runtime/Dockerfile's builder shape (rust:alpine + musl) — same
# project precedent, different binary.
FROM rust:alpine AS builder
RUN apk add --no-cache musl-dev pkgconfig openssl-dev build-base
WORKDIR /build

# Only what the workspace build (and its include_str!/include_bytes!
# embeds — roles, mission-configs, skills, workloads, the viewer, the
# vendored React Flow bundle) actually needs. `runtime/` is excluded from
# the workspace (Cargo.toml's own `exclude`) and irrelevant here — the
# flatsat runs `darkmux serve`, never a dispatch, so no darkmux-runtime
# image is required.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY crates ./crates
COPY templates ./templates
COPY skills ./skills
COPY profiles.example.json config.example.json ./

RUN cargo build --release --locked --bin darkmux || cargo build --release --bin darkmux

# ─── runtime ──────────────────────────────────────────────────────────────
FROM alpine:3.20
RUN apk add --no-cache bash coreutils curl
COPY --from=builder /build/target/release/darkmux /usr/local/bin/darkmux
ENTRYPOINT ["darkmux"]
