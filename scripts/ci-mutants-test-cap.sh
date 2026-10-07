#!/usr/bin/env bash
# Print `NAME=VALUE` for a cargo target runner that runs every test binary
# under a 4 GiB data-segment limit (`prlimit --data`), for the host triple of
# the toolchain in use. The mutation jobs `export` it before `cargo mutants`.
#
# Why: a mutant can turn a bounded loop into an unbounded one that allocates
# on every pass. Reproduced on Linux: `+=` -> `*=` in
# `StreamRedactor::feed` (crates/darkmux-serve/src/redaction_stream.rs)
# never advances its index and asked for 7.5 GB within seconds. Without a
# cap such a test eats the runner's 16 GB and the runner itself is shut down
# (quality run 37575267664, shard 14/20: "The runner has received a shutdown
# signal", no artifact uploaded). With the cap, `malloc` fails, the test
# aborts, nextest reports it failed, and the mutant reads as caught.
#
# RLIMIT_DATA, not RLIMIT_AS: glibc reserves large PROT_NONE address ranges
# for its arenas, which count against an address-space limit but not a data
# limit. The whole workspace suite (7,859 tests) and runtime/ (870) pass
# under this cap.
set -euo pipefail
triple=$(rustc -vV | sed -n 's/^host: //p')
echo "CARGO_TARGET_$(echo "$triple" | tr 'a-z-' 'A-Z_')_RUNNER=prlimit --data=$((4 << 30)) --"
