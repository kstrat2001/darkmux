# Testing and releasing

Agent reference, read when the work touches it. Moved verbatim from the root `CLAUDE.md` (#3133), which keeps a one-line summary and a pointer here.

## Testing — run the area, not the world (operator, 2026-08-13)

**The full workspace suite is CI's job, not yours.** CI runs it on every PR, for
free, on a public repo. Running it locally before every commit buys almost
nothing — the area you actually touched tests in **seconds**, and the merge gate
is CI's conclusion, not a local green.

**Two CI tiers (#2896).** An ordinary PR gets the LIGHT gate: build, the full
nextest suite plus doc tests, clippy, the runtime crate, runtime/ + bundler
mutation, fleet e2e, the viewer XSS gate, audit and docs drift. The isolation
leak check and coverage run on every push to main; the root PR-diff mutation
shards do not run on main at all, and the workspace crates are mutated by the
nightly sweep instead. **Label a PR `full-ci`** to run the leak check, the root
mutation shards and coverage before merge: do it for release candidates and for
anything risky (test isolation, state paths, new test infrastructure). Without
the label, a leak is caught on main after merge, and a test that cannot fail by
the next nightly sweep.

Everything below wraps **`cargo nextest`**, which is CONTRIBUTING.md's documented
loop. Install it: `cargo install cargo-nextest --locked`.

| alias | covers | measured |
|---|---|---|
| `cargo t-fast` | pure-logic crates, no I/O | **281 tests / 1.1s** |
| `cargo t-flow` | flow records, sinks, audit chain, schema, config access | **252 / 1.3s** |
| `cargo t-cli` | the whole root binary crate — every CLI verb module + all 11 integration targets | **632** |
| `cargo t-review` | lab harness (bundle scanning, review envelope, crawl) + crew scheduler/step kinds — `-p darkmux-lab -p darkmux-crew`; the bespoke review funnel this alias's name comes from was deleted in #2310 P4d, the crates weren't | **1324 / 5.3s** |
| `cargo t-serve` | the HTTP daemon + bundled viewer | |
| `cargo t-doctor` | preflight checks and their remedies | |
| `cargo t-fleet` | roster + cross-machine routing | |
| `cargo t-gestalt` | residency arbiter, hardware/heuristics providers | |
| `cargo t-runtime` | the agent runtime — **not a workspace member, so `t-all` misses it** | ~418 |
| `cargo t-all` | the same scope CI gates on (CI runs it as `cargo nextest run --workspace --profile ci`) | ~75s |

Narrower still is better when you know the name: `cargo nextest run -p
darkmux-flow integrity_exit_code` runs one function's tests in under a second.
A filter is almost always the right first move after an edit.

**Why nextest rather than `cargo test`** — and it is NOT mainly speed. On a
single area the two are equivalent (measured 4.6s vs 4.5s); the gap only opens
on `--workspace` (~75s vs ~10min), which you rarely run. The real reason is
`.config/nextest.toml`'s per-test `terminate-after`: a test that **hangs** fails
loudly instead of wedging the run. That has happened twice here, turning a 6s
suite into 10+ minutes of silence. A hang that reports nothing is the worst kind
of green. (nextest does not run doctests. The repo has one, a `compile_fail`
check that `WorkspaceLock` stays `!Send`; CI runs it in its own `cargo test
--workspace --doc` step.)

**Reach for `t-all` only when there is a reason you can state**: a change that
crosses crate boundaries in a way no single area covers, or a release tag. "To
be safe" is not a reason — it is the reflex this section exists to interrupt.

### Over-testing is a real cost, not a virtue

Three habits to avoid, all of which feel diligent:

- **Running the world when one area covers it.** If you edited `darkmux-flow`,
  `t-flow` tells you everything a `--workspace` run would about that change,
  minutes sooner.
- **Re-running a green suite to feel sure.** A second identical run adds no
  information. If you doubt a result, the fix is a test that can FAIL for the
  reason you doubt (red-prove it), not another pass of the same one.
- **Running the full suite before every commit on a branch.** Push and let CI
  do it. The merge gate is CI's conclusion, not a local green.

### Background lanes — keep working while tests run

Two cargo invocations share `target/`, so a background test run fights a
foreground build for it. `scripts/test-lane.sh` gives a run its own
`CARGO_TARGET_DIR` so they genuinely run in parallel:

```bash
scripts/test-lane.sh review t-review     # own lane, no contention
scripts/test-lane.sh cli test --test cli integrity
```

Kick the lane off **first**, then do the next piece of work while it runs —
the same priority-queue rule that applies to backgrounded crew dispatches. A
lane is a full target directory (~13 GB warm), so keep two or three, not one
per area; `rm -rf target/lanes/<name>` any time.

### What this does NOT buy

Faster tests are not more trustworthy tests. A suite with a false-green gate
(#1716) or a vacuous assertion (#1664) returns its wrong answer sooner in a
lane. Speed is an ergonomics fix; trust is a separate, open problem.

**And "CI is the gate" has one real hole**: `plugins/darkmux-bundler-rust` is
workspace-excluded, so `t-all` and the workspace suite never run its tests. They
run in exactly one place, the PR-diff mutation job, and only on a pull request
whose diff changes a mutable-looking line in the plugin's `*.rs` files (the
`bundler_changed_lines` count in `quality.yml`). A `runtime/`-only PR, or a
change to the plugin's `Cargo.toml` alone, runs none of them. A change elsewhere that breaks it
reaches `main` unseen. Deferring to CI is right everywhere else; there, it is
deferring to a check that mostly skips.

**Reading test output costs tokens (#3134).** nextest prints a line only for a
test that failed, retried or ran slow, then the summary; each failure's captured
output comes once, at the end. A green area run is about six lines, where it
used to be one per test. Add `--status-level pass` when you want every line.
For a red CI run, `scripts/ci-failures.py <run-id>` prints only the failing
tests' blocks, with their panic messages and the summary, instead of the job
log: one measured run went from 12,183 log lines to 4.

## Releasing — the gate, and where the steps live

**Cutting a release? Invoke the `darkmux-point-release` skill and follow it.
Do NOT improvise the sequence.** The steps, their ordering, and the traps that
ordering exists to avoid live there — not here. This section is only the gate:
the thing that decides whether a release should be cut at all.

> **Release gate (operator mandate, 2026-06-29): NO release is cut until local
> darkmux runs against REAL AI dispatches showing the release's FEATURES work
> — not merely that the dispatch path runs.** `cargo test`, CI, and a trivial
> path-smoke are necessary and NOT sufficient: they exercise the pieces, never
> the live invocation, and never the behavior.

Two failures are why, and both shipped:

- **#1135** — `dispatch --profile` silently loaded the model at LMStudio's 4096
  default instead of the profile's `n_ctx`. **A trivial smoke message FITS
  4096**, so `result: "stop"` looked perfectly healthy while the feature was
  broken and would have shipped garbage reviews. Only a dispatch that exercised
  the feature *and read `lms ps`* caught it.
- **#975** — v1.3.x–1.4.0 shipped a completely broken internal runtime (`docker
  docker run`, exit 125). Every unit test asserted the docker argv vector;
  nothing ever constructed and ran the real `Command`, so it sailed through four
  releases of green CI.

The generalization worth carrying: **a green test proves the pieces; only a
live run proves the thing.** That applies past releases — see "No blind runs"
and the lab/verify doctrine.
