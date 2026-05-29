#!/usr/bin/env bash
# scripts/lab-init.sh — populate ~/.darkmux/lab-registry.json with the
# built-in synthetic lab fixtures that ship in this repo.
#
# Phase 5 of the lab-reproducibility cluster (#487, #492). This is a
# STANDALONE utility, NOT a darkmux CLI verb — operators can run it
# once, fork it, or skip it entirely. The discoverability path lives
# in `darkmux doctor`'s "no registry" warning.
#
# Idempotent: re-run after `git pull` to register any new built-ins;
# existing registry entries are preserved (use `--force` to refresh
# them if a built-in fixture's content has changed upstream).
#
# Usage:
#   scripts/lab-init.sh         # register all built-ins
#   scripts/lab-init.sh --force # re-register, accepting any drift
#   scripts/lab-init.sh --dry   # print what would be registered; no writes
#
# Exits non-zero on any registration failure. Successful registrations
# print a one-line summary.

set -euo pipefail

# Resolve script dir → repo root → built-ins dir.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILTINS_DIR="$REPO_ROOT/templates/builtin/lab-fixtures"

FORCE=false
DRY=false
for arg in "$@"; do
    case "$arg" in
        --force) FORCE=true ;;
        --dry|--dry-run) DRY=true ;;
        -h|--help)
            sed -n 's/^# //p' "$0" | sed -n '/^Usage:/,/^$/p'
            exit 0
            ;;
        *)
            echo "lab-init: unknown arg: $arg" >&2
            echo "Try: $0 --help" >&2
            exit 2
            ;;
    esac
done

# Check prerequisites — darkmux binary must be on PATH.
if ! command -v darkmux >/dev/null 2>&1; then
    cat >&2 <<EOF
lab-init: \`darkmux\` not found on PATH.

  Install first:
    cargo install --path "$REPO_ROOT"

  Or use a debug binary directly (\`cargo build\` first):
    PATH="$REPO_ROOT/target/debug:\$PATH" $0 $@
EOF
    exit 1
fi

if [ ! -d "$BUILTINS_DIR" ]; then
    echo "lab-init: built-ins dir not found: $BUILTINS_DIR" >&2
    exit 1
fi

# Find every subdir with a .fixture.json.
fixtures_found=()
for d in "$BUILTINS_DIR"/*/; do
    if [ -f "$d/.fixture.json" ]; then
        fixtures_found+=("$d")
    fi
done

if [ ${#fixtures_found[@]} -eq 0 ]; then
    echo "lab-init: no built-in fixtures found under $BUILTINS_DIR"
    exit 0
fi

echo "lab-init: found ${#fixtures_found[@]} built-in fixture(s) under $BUILTINS_DIR"

# Register each. --force flag passes through to `dm lab register`.
register_args=()
if [ "$FORCE" = true ]; then
    register_args+=("--force")
fi

ok=0
fail=0
for fixture in "${fixtures_found[@]}"; do
    # Strip trailing slash for clean output.
    fixture_clean="${fixture%/}"
    if [ "$DRY" = true ]; then
        echo "  [dry-run] darkmux lab register${register_args:+ ${register_args[*]}} $fixture_clean"
        ok=$((ok + 1))
        continue
    fi
    if darkmux lab register "${register_args[@]}" "$fixture_clean" 2>&1; then
        ok=$((ok + 1))
    else
        echo "lab-init: registration failed for $fixture_clean" >&2
        fail=$((fail + 1))
    fi
done

echo ""
echo "lab-init: $ok registered, $fail failed"

if [ $fail -gt 0 ]; then
    exit 1
fi

if [ "$DRY" = false ]; then
    echo ""
    echo "Next steps:"
    echo "  darkmux lab fixtures              # confirm registrations"
    echo "  darkmux lab doctor                # lint the registry"
    echo "  darkmux lab run demo-quickstart   # end-to-end sanity check"
fi
