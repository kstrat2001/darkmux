#!/usr/bin/env bash
# Check every package in a cargo workspace against the `rust-version` it
# DECLARES (#1793). Usage:
#
#     scripts/ci-msrv-check.sh Cargo.toml
#     scripts/ci-msrv-check.sh runtime/Cargo.toml
#
# The toolchain versions come from the manifests, read through
# `cargo metadata`, never from a literal in a workflow. So raising or lowering
# a `rust-version` changes what this checks in the same edit, and a claim
# cannot drift away from its check. Packages are grouped by declared version
# and each group gets one `cargo +<version> check`.
#
# `check`, not `test`: the claim is "compiles on this toolchain". Default
# targets only (lib + bin), because dev-dependencies are not part of what a
# consumer on the floor toolchain builds.
#
# Two traps this avoids, both of which produce a green run that tested
# nothing:
#   * rust-toolchain.toml pins the DEV toolchain. Installing the MSRV and
#     making it the default does nothing, because the file outranks the
#     default. `cargo +<version>` outranks the file, and the version assert
#     below proves it did.
#   * A package with no `rust-version` has no claim to check. It fails here
#     instead of being skipped silently.

set -euo pipefail

manifest="${1:?usage: $0 <path/to/Cargo.toml>}"

# One "<rust-version> <package>" line per workspace member. `--no-deps` keeps
# this to the manifest's own packages; `--locked` refuses a stale lockfile.
pairs="$(cargo metadata --manifest-path "$manifest" --no-deps --locked --format-version 1 \
  | jq -r '.packages[] | "\(.rust_version // "MISSING") \(.name)"')"

if [ -z "$pairs" ]; then
  echo "error: cargo metadata listed no packages for $manifest" >&2
  exit 1
fi

missing="$(printf '%s\n' "$pairs" | awk '$1 == "MISSING" { print $2 }')"
if [ -n "$missing" ]; then
  echo "error: these packages declare no rust-version, so there is nothing to check:" >&2
  printf '  %s\n' $missing >&2
  exit 1
fi

for version in $(printf '%s\n' "$pairs" | awk '{ print $1 }' | sort -u); do
  pkgs="$(printf '%s\n' "$pairs" | awk -v v="$version" '$1 == v { print $2 }')"
  echo "== rust-version $version: $(printf "%s " $pkgs)"

  rustup toolchain install "$version" --profile minimal --no-self-update

  # Prove `+<version>` actually selected the MSRV toolchain rather than the
  # pinned dev one. `rustc 1.86.0 (...)` must start with `rustc 1.86`.
  actual="$(rustc "+$version" --version)"
  case "$actual" in
    "rustc $version "*|"rustc $version."*) echo "   using: $actual" ;;
    *)
      echo "error: asked for $version, got '$actual'; the check would be vacuous" >&2
      exit 1
      ;;
  esac

  pkg_args=()
  for p in $pkgs; do pkg_args+=(-p "$p"); done
  cargo "+$version" check --manifest-path "$manifest" --locked "${pkg_args[@]}"
done
