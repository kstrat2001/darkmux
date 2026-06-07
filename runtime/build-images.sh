#!/usr/bin/env bash
#
# Build the darkmux-runtime container images (#703).
#
#   darkmux-runtime:latest       — slim base (Unix toolkit + python + node).
#                                  The default dispatch image; coders edit +
#                                  verify Python/JS in-sandbox.
#   darkmux-runtime-rust:latest  — base + the Rust toolchain (cargo/rustc), so
#                                  a dispatched coder can `cargo check`/`test`
#                                  its edits in-sandbox — the inner verify loop.
#                                  Bigger image; opt-in per dispatch via
#                                  `darkmux crew dispatch --image darkmux-runtime-rust:latest`.
#
# Run from anywhere — paths resolve relative to this script. The build context
# is `runtime/` (the darkmux-runtime crate: its own Cargo.toml + src).
set -euo pipefail

RT="$(cd "$(dirname "$0")" && pwd)"

echo "==> building darkmux-runtime:latest (slim base)"
docker build -t darkmux-runtime:latest "$RT"

echo "==> building darkmux-runtime-rust:latest (base + Rust toolchain)"
docker build --build-arg RUNTIME_BASE=rust:alpine -t darkmux-runtime-rust:latest "$RT"

echo "==> done. images:"
docker images --format 'table {{.Repository}}:{{.Tag}}\t{{.Size}}' \
  | grep -E '^darkmux-runtime(-rust)?:latest' || true
