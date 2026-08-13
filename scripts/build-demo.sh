#!/usr/bin/env bash
#
# Generate docs/demo/index.html from whatever the daemon serves at `/`.
#
# darkmux.com/demo IS the observability viewer in playback mode, loading a
# committed flow-schema dataset (docs/demo/demo-flow.jsonl) — behaving exactly
# like opening `/play/<date>` locally on that file. There is no demo fork: this
# copies the ONE served viewer and injects the static-playback metas its boot
# path honors.
#
# The SOURCE IS DERIVED, not named here (#1801). This script used to hardcode
# `assets/viewer.html`, which meant two files independently encoded "which
# viewer is the real one" with nothing keeping them agreeing — so when the
# route flip (#1800) changed what `/` serves, the demo silently stayed on the
# legacy UI and no drift guard could notice, because the file still generated
# cleanly. Now `crates/darkmux-serve/src/lib.rs` is the single owner: this
# reads the constant `root_html` actually serves, then that constant's own
# `include_str!` path. Flip the route or rename the asset and the demo follows
# by construction, with nothing to remember.
#
# Deliberately pure shell rather than a `darkmux --emit-demo` verb (the other
# option #1801 weighed): CI's docs-drift job runs this script WITHOUT a Rust
# toolchain build, and making the demo guard depend on compiling the binary
# would trade a fast docs check for a multi-minute one.
#
# Run after editing the UI. CI re-runs this and fails on drift
# (.github/workflows/ci.yml) so the demo can never re-fork from the viewer.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LIB="$ROOT/crates/darkmux-serve/src/lib.rs"
OUT="$ROOT/docs/demo/index.html"

case "${1:-}" in
  ""|--print-source) ;;
  *)
    echo "build-demo: unknown argument '$1' (expected no arguments, or --print-source)" >&2
    exit 2
    ;;
esac

# Step 1: which embedded constant does `GET /` serve? Read it out of the
# handler body rather than trusting a name, so a future flip is picked up.
#
# EXACTLY ONE match required, deliberately. An earlier version took `head -1`,
# which meant a handler that grew a second branch —
#   if experimental() { return html_response(&h, inject_mode_meta(NEW, "live", None)); }
#   html_response(&h, inject_mode_meta(NEXT_HTML, "live", None))
# — silently derived from whichever call came FIRST in the file, generating the
# demo from the wrong asset and exiting 0. That is the same silent-wrong-source
# failure this script was rewritten to end, so ambiguity has to be loud rather
# than resolved by position.
SERVED_MATCHES=$(
  sed -n '/async fn root_html/,/^}/p' "$LIB" |
    sed -n 's/.*inject_mode_meta(\([A-Z_][A-Z0-9_]*\),.*/\1/p'
)
SERVED_COUNT=$(printf '%s\n' "$SERVED_MATCHES" | grep -c . || true)
if [ "$SERVED_COUNT" -eq 0 ]; then
  echo "build-demo: could not determine which constant root_html serves in $LIB" >&2
  echo "build-demo: (expected an inject_mode_meta(<CONST>, \"live\", ...) call)" >&2
  exit 1
fi
if [ "$SERVED_COUNT" -gt 1 ]; then
  echo "build-demo: root_html serves more than one constant — cannot tell which the demo should ship:" >&2
  printf 'build-demo:   %s\n' $SERVED_MATCHES >&2
  echo "build-demo: resolve the ambiguity in $LIB, or teach this script which branch is the default." >&2
  exit 1
fi
SERVED_CONST=$(printf '%s\n' "$SERVED_MATCHES" | grep . | head -1)

# Step 2: which file does that constant embed? Same exactly-one discipline.
ASSET_MATCHES=$(
  sed -n "s|^const ${SERVED_CONST}: &str = include_str!(\"\.\./assets/\([^\"]*\)\");|\1|p" "$LIB"
)
ASSET_COUNT=$(printf '%s\n' "$ASSET_MATCHES" | grep -c . || true)
if [ "$ASSET_COUNT" -eq 0 ]; then
  echo "build-demo: found served constant '$SERVED_CONST' but no include_str! for it in $LIB" >&2
  exit 1
fi
if [ "$ASSET_COUNT" -gt 1 ]; then
  echo "build-demo: '$SERVED_CONST' has more than one include_str! in $LIB — ambiguous source." >&2
  exit 1
fi
ASSET=$(printf '%s\n' "$ASSET_MATCHES" | grep . | head -1)

SRC="$ROOT/crates/darkmux-serve/assets/$ASSET"
if [ ! -f "$SRC" ]; then
  echo "build-demo: derived source $SRC does not exist" >&2
  exit 1
fi

# `--print-source` exists so the pre-commit hook can ask WHICH file the demo is
# built from instead of hardcoding an answer that goes stale (which is exactly
# what happened to this script itself). One owner of the derivation, two
# consumers: this generator, and `.githooks/pre-commit`.
if [ "${1:-}" = "--print-source" ]; then
  printf '%s\n' "${SRC#"$ROOT/"}"
  exit 0
fi

# Inject the demo metas right after the first <head> (exactly once), the same
# spot the daemon's inject_mode_meta uses for the live/play routes. Pure-shell
# sed split so it works on macOS BSD tools as well as GNU.
#
# No `darkmux-date` meta, deliberately: the demo dataset carries its own dates,
# and the viewer derives the playback day from the first record — so the demo
# never needs updating when the fixture is re-recorded.
{
  sed '/<head>/q' "$SRC"
  cat <<EOF
<!-- GENERATED from crates/darkmux-serve/assets/$ASSET by scripts/build-demo.sh — edit the UI source, not this file. -->
<meta name="darkmux-mode" content="play">
<meta name="darkmux-flow-src" content="./demo-flow.jsonl">
<meta name="darkmux-missions-src" content="./demo-missions.json">
<meta name="darkmux-phases-src" content="./demo-phases.json">
<meta name="darkmux-runs-src" content="./demo-runs.json">
EOF
  sed '1,/<head>/d' "$SRC"
} > "$OUT"

echo "generated $OUT from $SRC ($(wc -l < "$OUT" | tr -d ' ') lines)"
