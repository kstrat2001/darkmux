# #463 Workspace split — execution plan (resume anchor)

Status: **PR1 + PR2 (partial) landed; crew/lab/serve blocked on dep cycles.**
See "Execution status" at the bottom for what's done and the cycle-breaking
design needed to resume.

Beat 54 milestone confirmed tested → timing caveat in #463 cleared.

Decisions locked with the operator:
- **Delivery:** ~3 grouped PRs (not 8 tiny ones, not one big-bang).
- **Structure:** deviate from the issue's 8-crate table — add a foundation
  `darkmux-types` crate (extract `types.rs` *whole*, do **not** split it) and
  lift `lab::paths` into that foundation so `crew` does not depend on `lab`.
- **Technique:** re-export shims to minimize call-site churn (see below).

## Verified dependency DAG (no cycles)

```
darkmux-types  (foundation: types.rs whole + lab/paths.rs lifted in)
   ▲   ▲   ▲   ▲
   │   │   │   └── darkmux-lab ── also depends on darkmux-profiles
   │   │   └────── darkmux-crew ── also depends on darkmux-flow
   │   └────────── darkmux-profiles
   └────────────── (everything)

darkmux-flow   (true leaf — zero darkmux deps)
   ▲
   └── darkmux-crew, darkmux-serve, binary (flow_cli, phase_cli)

binary (darkmux) ── everything above + the ~18 unassigned modules
runtime/         ── separate crate, EXCLUDED from workspace (own Cargo.lock; "stays as-is")
```

Verified facts (from source, 2026-05-29):
- `types.rs`: no `crate::` refs, 0 `pub(crate)`. External use: `serde` (derive),
  `serde_json` (33×), and via the lifted `paths.rs`: `anyhow` (1×), `dirs` (4×).
- `flow.rs`: no `crate::` refs (true leaf), **11 `pub(crate)` → must flip to `pub`**:
  `flows_dir`, `day_utc_now`, `ts_utc_now`, `audit_dir`, `RawRedisUrl` (+`new`,
  `expose_for_probe`), `REDIS_CONNECT_TIMEOUT`, `isolate_test_env_once`,
  `record_at`, `redact_url_creds`. External use: `anyhow`, `clap` (ValueEnum, 1×),
  `serde` (derive), `serde_json` (31×), `blake3` (2×), `libc` (2×), `redis` (14×),
  `dirs` (2×).
- `lab/paths.rs`: no `crate::` refs, 0 `pub(crate)`. Consumers of `crate::lab::paths`
  (10 files): `crew/index.rs`, `crew/loader.rs`, `lab/{doctor,fixture_cli,inspect,list,registry,run}.rs`,
  `main.rs`, `notebook.rs` — all keep working via the shim below.
- `lab/mod.rs` declares: characterize, compare, cow_clone, doctor, fixture,
  fixture_cli, inspect, instrument, list, **paths**, registry, run, sandbox_hash, tune.
- Root `Cargo.toml` is a plain `[package]` (no workspace yet). `runtime/` has its
  own `Cargo.lock`; root does not reference it.

## Re-export shim technique (keeps PR1 diff small)

- `src/main.rs`: `mod types;` → `pub use darkmux_types as types;`
- `src/main.rs`: `pub mod flow;` → `pub use darkmux_flow as flow;`
- `src/lab/mod.rs`: `pub mod paths;` → `pub use darkmux_types::paths;`

All existing `crate::types::*`, `crate::flow::*`, `crate::lab::paths::*` paths
continue to resolve unchanged — only crate-root wiring + the `pub(crate)→pub`
flips in `flow.rs` actually change.

## PR1 — Foundation (`darkmux-types` + `darkmux-flow`)

1. `mkdir -p crates/darkmux-types/src crates/darkmux-flow/src`
2. `git mv src/types.rs crates/darkmux-types/src/lib.rs`
3. `git mv src/lab/paths.rs crates/darkmux-types/src/paths.rs`; add `pub mod paths;` to lib.rs.
4. `crates/darkmux-types/Cargo.toml`: name `darkmux-types`, edition 2021, deps:
   `serde` (derive), `serde_json`, `anyhow`, `dirs`. (No `clap` — `types.rs` has 0 clap refs.)
5. `git mv src/flow.rs crates/darkmux-flow/src/lib.rs`; flip the 11 `pub(crate)`→`pub`.
6. `crates/darkmux-flow/Cargo.toml`: deps `anyhow`, `clap` (derive), `serde` (derive),
   `serde_json`, `blake3`, `libc`, `redis` (default-features=false, features=["streams"]), `dirs`.
7. Root `Cargo.toml`: add
   `[workspace]\nmembers = [".", "crates/darkmux-types", "crates/darkmux-flow"]\nexclude = ["runtime"]`
   and path deps `darkmux-types = { path = "crates/darkmux-types" }`,
   `darkmux-flow = { path = "crates/darkmux-flow" }`. Consider `[workspace.package]`/
   `[workspace.dependencies]` later; not required for PR1.
8. Apply the three re-export shims above.
9. Build/test gates (MUST be green before commit):
   `cargo build --workspace`, `cargo test --workspace`,
   `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --release`
   (proxy for `cargo install --path .`). Capture output to a file and read it back
   if the live channel looks unreliable.
10. Watch-outs: `flow`/`types` `#[cfg(test)]` modules move with their files and may
    reference helpers now behind the crate boundary; `isolate_test_env_once` is now
    `pub` and re-exported, so `flow::isolate_test_env_once()` callers still resolve.

## PR2 — `darkmux-profiles` + `darkmux-lab` + `darkmux-crew`

- `darkmux-profiles`: `profiles.rs`, `swap.rs`, `lms.rs`, `runtime.rs`. Dep: `darkmux-types`.
- `darkmux-lab`: `lab/` (minus `paths`, now in types), `workloads/`, `providers/`.
  Deps: `darkmux-types`, `darkmux-profiles`.
- `darkmux-crew`: `crew/`. Deps: `darkmux-types`, `darkmux-flow`. **No `lab` dep**
  (paths moved to foundation). Verify `crew/loader.rs` + `crew/index.rs` now import
  `darkmux_types::paths` not `crate::lab::paths`.
- Shims again: `mod profiles;`→`pub use darkmux_profiles as profiles;`, etc.
- Flip cross-boundary `pub(crate)`→`pub` as the compiler flags them.
- Empirical acceptance check: after crew is its own crate, time a touch+rebuild of
  `crates/darkmux-crew/src/dispatch_internal.rs` — target < 30s (vs 8+ min pre-split).

## PR3 — `darkmux-serve` + binary thinning (+ doctor decision)

- `darkmux-serve`: `serve.rs`. Dep: `darkmux-flow` (+ `darkmux-types` for `LoadedModel`).
- Thin the binary: `main.rs` becomes the clap orchestrator wiring the crates; the
  ~18 unassigned modules (`agent_roles`, `eureka`, `external`, `fleet`, `hardware`,
  `heuristics`, `init`, `recommendations`, `skills`, `flow_cli`, `phase_cli`,
  `mission_propose`, `notebook`, `role_cli`, `migrate`, `optimize`, `workdir`) stay
  in the binary, importing from the new crates.
- **Open sub-decision — `darkmux-doctor`:** the issue lists it as its own crate, but
  `doctor.rs` depends on `agent_roles`, `eureka`, `hardware`, `heuristics`, `lms`,
  `profiles`, `types` — and `eureka`/`hardware`/`heuristics`/`agent_roles` are
  binary-resident. Extracting `darkmux-doctor` therefore requires *also* extracting
  those, or leaving doctor in the binary. **Recommendation:** keep `doctor` in the
  binary for now (defer its crate) unless we also pull `hardware`+`heuristics` out —
  surface this to the operator at PR3 time.

## Acceptance criteria (#463)

- [ ] Workspace `Cargo.toml` declares the crates; each has its own `Cargo.toml`.
- [ ] `cargo build --workspace` + `cargo test --workspace` pass clean.
- [ ] `cargo install --path .` still produces the `darkmux` binary.
- [ ] CI updated to `cargo test --workspace` if needed.
- [ ] Touching `crew/dispatch_internal.rs` rebuilds in < 30s (verify in PR2).

---

## Execution status (2026-05-29)

### Landed (green: build/clippy `--workspace`, new-crate tests pass)

- **PR1** — `darkmux-types` (types.rs whole + lab/paths.rs lifted in) and
  `darkmux-flow` (flow.rs; 11 `pub(crate)`→`pub`). Re-export shims in place.
  One wrinkle vs the plan's watch-out #10: `isolate_test_env_once` was
  `#[cfg(test)]`-gated, so once flow became its own crate it was invisible to
  the binary's test build (`flow_cli` tests). Fixed by gating it on
  `any(test, feature = "test-support")` and enabling that feature from the
  binary's dev-dependency on `darkmux-flow`.
- **PR2 (partial)** — `darkmux-profiles` (profiles/swap/lms/runtime). Clean
  leaf above types; `crate::types`→`darkmux_types`, internal
  `crate::{lms,swap,runtime}` unchanged. No `pub(crate)` flips needed.

Known pre-existing failure (NOT introduced here, also fails on `main`):
`phase_cli::tests::flow_record_failure_does_not_crash_review` — sets a dir
read-only to force a write failure, but CI/dev runs as root and root bypasses
the permission, so a record gets written. Orthogonal to #463; track separately.

### Blocked — the plan's DAG missed two cycles

`crew`/`lab`/`serve` cannot be split along the plan's lines. Rust crates may
not contain dependency cycles, and the real call graph has two:

- **crew ↔ fleet**: `crew/dispatch.rs` uses `fleet::{load_roster,
  candidates_for_tier, CompletionResult}`; `fleet.rs` uses
  `crew::dispatch::{Runtime, dispatch, DispatchOpts, CompactionDispatchArgs}`
  (10+ sites). Tightly coupled both ways.
- **crew ↔ serve**: `crew/dispatch.rs` calls
  `serve::nudge_if_daemon_unreachable` (1 site); `serve.rs` calls
  `crew::loader::{load_missions, load_phases, missions_dir, phases_dir}`.

(`crew → lab::paths` is trivially fixable — paths is now in `darkmux-types`;
rewrite the 3 sites in `crew/{loader,index}.rs` to `darkmux_types::paths`.
`lab → crew` is one-directional and fine once crew is a crate.)

### Proposed cycle-breaking design (for the resume PR)

1. **Break crew↔fleet** in two moves:
   - Extract the shared *dispatch vocabulary* (`Runtime`, `DispatchOpts`,
     `CompactionDispatchArgs`, `CompletionResult`, `DispatchResult`) into a
     low crate both can depend on (either `darkmux-types` or a new
     `darkmux-dispatch-types`). Removes the *type* coupling.
   - Invert the *behavior* edge: today crew reaches up into
     `fleet::{load_roster, candidates_for_tier}`. Either move roster loading
     down to a lower crate, or have the caller pass the resolved roster/
     candidates *into* crew dispatch as parameters. Goal: leave only
     `fleet → crew` (fleet calls `crew::dispatch::dispatch`).
2. **Break crew↔serve**: `nudge_if_daemon_unreachable` is a tiny
   daemon-reachability helper — lift it into a low crate (or inject it as a
   callback) so the only remaining edge is `serve → crew` (loader fns).
3. After the cycles are gone: `darkmux-crew` (deps types, flow, dispatch-
   types), `darkmux-lab` (deps types, profiles, crew), `darkmux-serve` (deps
   flow, crew, types). `fleet` stays binary-resident (per PR3) or becomes its
   own crate depending on crew. Doctor stays in the binary (the PR3 open
   sub-decision still stands).

This is design work, not the mechanical refactor the plan scoped — hence
deferred to a dedicated follow-up rather than bolted onto this branch.
