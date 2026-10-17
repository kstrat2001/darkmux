#!/usr/bin/env bash
# Linux check, run on a real Linux machine (the Omarchy laptop) before a
# release. CI runs on macOS only, so a `cfg(target_os = "linux")` leaf, the
# darkmux-platform Linux strategy or a dead-code warning only Linux sees is
# compiled nowhere else. Run from the repo root at the commit being released.
set -euo pipefail

if [ "$(uname -s)" != "Linux" ]; then
  echo "linux-check: run this on Linux (this is $(uname -s))" >&2
  exit 2
fi

step() { printf '\n== %s\n' "$1"; }

step "clippy: whole workspace, all targets, no warnings"
cargo clippy --workspace --all-targets -- -D warnings

step "tests: the crates whose answers differ by OS"
if command -v cargo-nextest >/dev/null; then
  cargo nextest run -p darkmux-platform -p darkmux-hardware -p darkmux-types
else
  cargo test -p darkmux-platform -p darkmux-hardware -p darkmux-types
fi

step "release build"
cargo build --release --bin darkmux
./target/release/darkmux --version

# CI (the self-hosted Omarchy job) skips doctor: it reads the runner user's
# ~/.darkmux, not a configured install.
if [ "${LINUX_CHECK_SKIP_DOCTOR:-}" != "1" ]; then
  step "doctor (read-only; review the Linux rows by eye)"
  ./target/release/darkmux doctor || echo "doctor exited $? (read the rows above)"
fi

printf '\nlinux-check: PASS (clippy, OS tests, release build). Doctor output is for review.\n'
