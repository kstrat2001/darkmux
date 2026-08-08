#!/usr/bin/env bash
# up -> scenarios -> down, always tearing down (even on failure) and
# propagating the scenarios' real exit code — this is what `bun run check`
# calls.
set -uo pipefail
cd "$(dirname "$0")"

./up.sh
up_status=$?
if [ $up_status -ne 0 ]; then
  echo "==> up.sh failed (exit $up_status) — tearing down and aborting" >&2
  ./down.sh || true
  exit $up_status
fi

bunx playwright test
scenarios_status=$?

./down.sh

exit $scenarios_status
