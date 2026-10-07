#!/usr/bin/env bash
# Print a scratch directory for cargo-mutants' copied build trees, on the
# filesystem with the most free space: the runner's temp directory (root
# disk) or `/mnt` (the second disk GitHub's Linux runners mount there), when
# it can be written to. Each free figure goes to stderr for the job log.
#
# Why it matters: `cargo mutants -j 4` copies the tree into `$TMPDIR` once
# per worker and builds each copy, so four workspace test builds sit there at
# once. Measured on Linux, one such build is 2.0 GB without debug info.
set -euo pipefail
best=""
best_avail=0
for base in "${RUNNER_TEMP:-/tmp}" /mnt; do
  dir="$base/cargo-mutants-tmp"
  if [ "$base" = /mnt ]; then
    { [ -d /mnt ] && sudo -n mkdir -p "$dir" && sudo -n chown "$(id -u):$(id -g)" "$dir"; } 2>/dev/null || continue
  else
    mkdir -p "$dir" || continue
  fi
  avail=$(df -Pk "$dir" | awk 'NR==2 {print $4}')
  echo "$dir: $((avail / 1024 / 1024)) GiB free" >&2
  if [ "$avail" -gt "$best_avail" ]; then
    best=$dir
    best_avail=$avail
  fi
done
echo "${best:-${RUNNER_TEMP:-/tmp}}"
