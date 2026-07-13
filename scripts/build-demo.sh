#!/usr/bin/env bash
#
# Generate docs/demo/index.html from the canonical serve viewer.
#
# darkmux.com/demo IS the live observability viewer in playback mode, loading a
# committed flow-schema dataset (docs/demo/demo-flow.jsonl) — behaving exactly
# like opening `/play/<date>` locally on that file. There is no demo fork: this
# copies the ONE viewer (crates/darkmux-serve/assets/viewer.html) and injects
# the static-playback metas the viewer's boot() honors.
#
# Run after editing the viewer. CI re-runs this and fails on drift
# (.github/workflows/ci.yml) so the demo can never re-fork from the viewer.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/crates/darkmux-serve/assets/viewer.html"
OUT="$ROOT/docs/demo/index.html"

# Inject the demo metas right after the first <head> (exactly once), the same
# spot the daemon's inject_mode_meta uses for the live/play routes. Pure-shell
# sed split so it works on macOS BSD tools as well as GNU.
{
  sed '/<head>/q' "$SRC"
  cat <<'EOF'
<!-- GENERATED from crates/darkmux-serve/assets/viewer.html by scripts/build-demo.sh — edit the viewer, not this file. -->
<meta name="darkmux-mode" content="play">
<meta name="darkmux-flow-src" content="./demo-flow.jsonl">
<meta name="darkmux-missions-src" content="./demo-missions.json">
<meta name="darkmux-phases-src" content="./demo-phases.json">
EOF
  sed '1,/<head>/d' "$SRC"
} > "$OUT"

echo "generated $OUT from $SRC ($(wc -l < "$OUT" | tr -d ' ') lines)"
