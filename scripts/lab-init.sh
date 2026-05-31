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
# Idempotent: an already-registered built-in is reported as "skipped"
# (NOT a failure), so a clean re-run exits 0. Exits non-zero only on a
# real registration failure. Successful registrations print a one-line
# summary.

set -euo pipefail

# Resolve script dir → repo root → built-ins dir.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILTINS_DIR="$REPO_ROOT/templates/builtin/lab-fixtures"

FORCE=false
DRY=false
print_help() {
    # Hand-written help block — earlier draft parsed the script's own
    # `# ` comments via sed, which bled body comments into the output.
    cat <<'EOH'
scripts/lab-init.sh — register the built-in lab fixtures shipped with
this darkmux repo into ~/.darkmux/lab-registry.json.

Usage:
  scripts/lab-init.sh             # register all built-ins (idempotent)
  scripts/lab-init.sh --force     # re-register, accepting any drift
  scripts/lab-init.sh --dry       # print what would be registered; no writes
  scripts/lab-init.sh -h, --help  # this help
EOH
}

for arg in "$@"; do
    case "$arg" in
        --force) FORCE=true ;;
        --dry|--dry-run) DRY=true ;;
        -h|--help) print_help; exit 0 ;;
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
    PATH="$REPO_ROOT/target/debug:\$PATH" $0
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

# Register each. With --force we replace existing entries; otherwise
# --if-absent makes `dm lab register` idempotent (#544): it exits 0
# whether it registered or skipped an already-present fixture, so a
# re-run after `git pull` is clean WITHOUT parsing error text. (force
# and if-absent are mutually exclusive; force wins.)
register_args=()
if [ "$FORCE" = true ]; then
    register_args+=("--force")
else
    register_args+=("--if-absent")
fi

# Render the optional register-args portion for dry-run output. Using
# `${#array[@]}` rather than `${array:+...}` (which tests the scalar
# form = first element only — fragile if more flags are added).
register_args_str=""
if [ ${#register_args[@]} -gt 0 ]; then
    register_args_str=" ${register_args[*]}"
fi

ok=0
skipped=0
fail=0
for fixture in "${fixtures_found[@]}"; do
    # Strip trailing slash for clean output.
    fixture_clean="${fixture%/}"
    if [ "$DRY" = true ]; then
        echo "  [dry-run] darkmux lab register$register_args_str $fixture_clean"
        ok=$((ok + 1))
        continue
    fi
    # With --if-absent / --force, register exits 0 on success (registered
    # OR skipped), so a NON-ZERO exit is now an unambiguous real failure —
    # no error-text grep gates the skip path, so the #501 non-idempotent
    # regression can't be silently reintroduced by rewording a Rust error.
    # `if cmd; then` keeps `set -e` from aborting on a genuine failure.
    #
    # `${register_args[@]+"${register_args[@]}"}` is the bash-3.2-safe
    # empty-array expansion (macOS bash 3.2 trips `set -u` on a bare
    # `"${arr[@]}"` over an empty array; the `+`-guard expands to nothing).
    if out="$(darkmux lab register ${register_args[@]+"${register_args[@]}"} "$fixture_clean" 2>&1)"; then
        # COSMETIC ONLY: a skip prints "skipped (--if-absent)"; matching it
        # just splits the summary counts. A miss mis-labels (counts as
        # registered) but never fails the run, so this grep is no longer
        # load-bearing.
        if printf '%s' "$out" | grep -qi "skipped"; then
            echo "  [skip] already registered: $fixture_clean (use --force to refresh)"
            skipped=$((skipped + 1))
        else
            printf '%s\n' "$out"
            ok=$((ok + 1))
        fi
    else
        printf '%s\n' "$out" >&2
        echo "lab-init: registration failed for $fixture_clean" >&2
        fail=$((fail + 1))
    fi
done

echo ""
echo "lab-init: $ok registered, $skipped skipped, $fail failed"

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
