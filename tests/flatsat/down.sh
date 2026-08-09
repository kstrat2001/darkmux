#!/usr/bin/env bash
# Tears down the flatsat fleet: containers + the compose network. The
# bind-mounted state under .state/ is left on disk (gitignored) — pass
# --clean to also wipe it, so the next `up` starts from a truly empty seed
# rather than reusing a run's leftover state.
set -euo pipefail
cd "$(dirname "$0")"

docker compose -p darkmux-flatsat down

if [ "${1:-}" = "--clean" ]; then
  rm -rf .state
  echo "==> .state/ wiped"
fi
