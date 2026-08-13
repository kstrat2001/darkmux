#!/bin/sh
# Run a cargo test command in its own build lane, so it does not contend with
# whatever you are doing in the foreground.
#
#   scripts/test-lane.sh review t-review
#   scripts/test-lane.sh flow   t-flow
#   scripts/test-lane.sh cli    test --test cli integrity
#
# WHY: two cargo invocations share `target/`, and a background test run will
# fight a foreground build for it. A lane gets `CARGO_TARGET_DIR=target/lanes/
# <name>`, so the two never touch. That is what makes "kick off the tests, keep
# working" actually parallel instead of merely asynchronous.
#
# COST, stated plainly: a lane is a FULL target directory — the main one is
# ~13 GB. Two or three lanes is fine on a roomy disk; one per area is not.
# sccache makes the COMPILES cheap to redo; it does nothing for the disk.
# Lanes are disposable — `rm -rf target/lanes` any time.
set -eu

if [ $# -lt 2 ]; then
  echo "usage: $0 <lane-name> <cargo-args...>" >&2
  echo "  e.g. $0 review t-review" >&2
  exit 2
fi

LANE="$1"; shift

# A lane name containing `/` or `..` escapes target/ — verified: `../../x`
# lands OUTSIDE the repo, where .gitignore does not cover it, so the lane
# becomes committable. Reject rather than sanitize: there is no legitimate
# nested lane name.
case "$LANE" in
  */*|*..*|"")
    echo "[test-lane] refusing lane name '$LANE' — no '/' or '..' (it would escape target/)" >&2
    exit 2 ;;
esac

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"   # so cargo resolves this repo's aliases even when invoked by absolute path from elsewhere
DIR="$ROOT/target/lanes/$LANE"

mkdir -p "$DIR"

# Warn once when lanes start adding up — the disk cost is easy to forget
# because nothing surfaces it until the disk is full.
COUNT=$(find "$ROOT/target/lanes" -maxdepth 1 -mindepth 1 -type d 2>/dev/null | wc -l | tr -d ' ')
if [ "$COUNT" -gt 3 ]; then
  echo "[test-lane] note: $COUNT lanes exist under target/lanes (~13 GB each when warm)." >&2
  echo "[test-lane]       drop the ones you are not using: rm -rf target/lanes/<name>" >&2
fi

echo "[test-lane] lane=$LANE target=$DIR"
echo "[test-lane] cargo $*"

# `exec` so the caller's job control (and any background wait) tracks cargo
# itself rather than this wrapper — a killed lane actually stops the build.
CARGO_TARGET_DIR="$DIR" exec cargo "$@"
