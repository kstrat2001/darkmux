//! (#2632) Instrumentation-only `DARKMUX_*` env-read audit sink.
//!
//! Compiled ONLY under `#[cfg(any(test, feature = "test-support"))]` — the
//! same gate `test-support` itself uses — so this never ships in a release
//! binary and costs a release build nothing.
//!
//! The problem this exists to solve: `serial_test::serial` only serializes
//! tests that carry the annotation. A test that reads `DARKMUX_*`-derived
//! process state (directly, or transitively through a production function)
//! without the annotation can still observe another test's env mutation
//! mid-flight — a guarded writer racing an unguarded reader is still a race
//! (#2632). Finding every such reader by reading source and guessing is
//! exactly the failure mode the issue named: two instances were found "by
//! accident" on two different days before anyone asked whether it was a
//! class. Grepping for `env::var("DARKMUX_` also structurally under-counts:
//! it cannot see a test that reaches the env through a call chain (e.g. a
//! test calling `load_roles()`, which calls `roles_dir()`, which resolves
//! through `paths::resolve()`).
//!
//! Instrumenting the resolution chokepoints instead makes the enumeration
//! exhaustive and mechanical: every test that ever actually reads a
//! `DARKMUX_*`-derived value, however indirectly, shows up in the log by
//! construction. The precedent is #2262's shared-sink audit (62 emitters
//! found by logging the calling thread inside the sink and diffing against
//! the annotated set, rather than reading test files).
//!
//! ## Usage
//!
//! Set `DARKMUX_ENV_AUDIT_LOG=<path>` before running `cargo test`. Every
//! read of a `DARKMUX_*` key through an instrumented chokepoint appends one
//! `<test-thread-name>\t<key>` line to that file. `cargo test`'s default
//! harness names each test's thread after its fully-qualified test path
//! (`mod::tests::test_name`), so the log is a direct (test, key) read
//! table — no symbol lookup or stack walk required.
//!
//! `scripts/env-audit-report.py` turns the raw log into a report: every
//! reading test cross-referenced against whether it carries
//! `#[serial_test::serial]`. A read whose thread has no name (a spawned
//! worker thread `cargo test`'s harness never named — see below) is NOT
//! silently dropped: the script buckets it as unattributable and fails
//! loud on it, same as a genuinely unguarded named reader (#2632 fix
//! pass — an earlier version of the script `continue`d past these,
//! which is how seven `darkmux-crew::scheduler` tests raced
//! `bounded_command`'s two `DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS`-
//! mutating tests invisibly: the read happened three `thread::scope`
//! hops below the test's own thread). `darkmux-crew::concurrent_dispatch::
//! spawn_scoped_named` now propagates the current thread's name at every
//! `thread::scope`/`scope.spawn` boundary a dispatch job crosses, so a
//! read on one of those worker threads still attributes back to the test
//! that ultimately caused it instead of showing up unnamed.
//!
//! ## Chokepoints instrumented (as of #2632)
//!
//! - `config_access::env_str` — the single env-read idiom for every
//!   `DARKMUX_*` config setting `pick_string`/`pick_parsed`/`pick_dir`
//!   resolve (covers the large majority of settings in both
//!   `darkmux-types` and `darkmux-crew`, since crew reads config through
//!   these accessors rather than raw `std::env::var`).
//! - `paths::resolve` — `DARKMUX_HOME` (the bootstrap pointer, which can't
//!   live inside the config it locates), read directly in `resolve`
//!   itself. `paths::paths_from_root` (called by `resolve`, and shared by
//!   every scope it resolves) additionally reads `DARKMUX_NOTEBOOK_DIR`.
//! - `dispatch_liveness::liveness_dir` — a direct `DARKMUX_HOME` read
//!   outside both chokepoints above.
//! - `residency_lease::residency_dir` — a second direct `DARKMUX_HOME`
//!   read site, mirroring `liveness_dir`'s resolution exactly.
//! - `darkmux-profiles::profiles::default_locations` (`DARKMUX_HOME`) and
//!   `profiles::load_registry` (`DARKMUX_PROFILES`) — a fifth chokepoint,
//!   in a DIFFERENT crate (#2632 CONSIDER 3). `darkmux-crew` calls
//!   `load_registry` at five production sites and its tests mutate both
//!   keys, so this one matters for crew's own sweep even though
//!   `darkmux-profiles` isn't itself among the crates
//!   `scripts/env-audit-report.py` sweeps for `#[serial]` annotations.
//!   Wired via a forwarded `test-support` feature (darkmux-profiles'
//!   `test-support` → `darkmux-types/test-support`), the same shape as
//!   every other dev-dependency-only feature gate in this workspace —
//!   see darkmux-profiles' and darkmux-crew's `Cargo.toml`.
//!
//! This module deliberately does NOT try to intercept literal
//! `std::env::var("DARKMUX_...")` calls made directly inside test bodies
//! (the save/restore idiom every mutator test uses) — those are mutation
//! sites, not resolution sites, and are enumerated separately by scanning
//! for `set_var`/`remove_var` call sites, which — unlike "does this test
//! transitively depend on env state" — IS a fully mechanical, unambiguous
//! textual fact (see the PR body for how that half of the sweep was done).
//!
//! ## Known gaps (honest, not exhaustive)
//!
//! **#2643 update.** The three production `thread::spawn` calls in
//! `dispatch_internal.rs` (the tailer, the inactivity watchdog, the
//! thermal sampler) now go through `concurrent_dispatch::
//! spawn_detached_named` — the same current-thread-name-propagation
//! trick `spawn_scoped_named` uses for the dispatch-execution path,
//! adapted for a DETACHED (non-`thread::scope`) spawn. This measurably
//! re-attributed a large share of `dispatch_internal::tests::*`'s
//! previously-`<unnamed>` reads to their real spawning test. It did NOT
//! reach zero: a residual of exactly 4 lines (`DARKMUX_FLOWS_DIR`: 2,
//! `DARKMUX_MACHINE_ID`: 2 — the SAME keys and count #2632 originally
//! found) still logs as `<unnamed>`, landing in the log between two named
//! `dispatch_internal_tests.rs` tests under `--test-threads=1`. Neither
//! test whose name brackets it in the log calls `spawn_guarded_watchdog`/
//! `_tailer`/`_sampler` from an unnamed thread (confirmed: zero
//! `"darkmux-worker"` fallback-name lines appear anywhere in a full crew
//! sweep, so every call into `spawn_detached_named` in this run had a
//! real caller-thread name to propagate) — so this residual comes from
//! a FOURTH, not-yet-located detached thread, not from the three fixed
//! here. Left open rather than guessed at further; the next person
//! chasing it should start from the exact log position (immediately
//! before `spawn_guarded_sampler_wiring_survives_a_real_thread_spawn` in
//! a `--test-threads=1` run) rather than re-deriving that from scratch.
//!
//! The several test-local `thread::spawn` calls in `absence_backstop.rs`,
//! `remote_budget.rs`, `workspace_spec/materialize.rs`, and
//! `step_kinds/builtins.rs`'s mock Redis server are still unnamed — not
//! implicated in the residual above (per the original #2632 investigation
//! and this pass's own check), but still a live gap. So is a cluster
//! found NEW in this pass: a `--test-threads=1` crew sweep shows ~50
//! unnamed `DARKMUX_HOME` reads (plus ~10 `DARKMUX_MODEL_LOAD_TIMEOUT_
//! SECONDS`) in tight repeating groups, almost certainly a
//! `darkmux-gestalt` residency/host-probe background thread rather than
//! anything in `dispatch_internal.rs` — unidentified beyond that;
//! tracked as follow-up, not chased further here.
//!
//! **Crate coverage, as of #2643:** `darkmux-flow` and `darkmux-lab` are
//! now swept (see `scripts/env-audit-report.py`'s `CRATE_DIRS`). Neither
//! needed its own chokepoint gauntlet the way `darkmux-crew` did — lab
//! reaches every `DARKMUX_*` value it needs through this module's
//! existing `config_access`/`paths` chokepoints; flow's three resolvers
//! that bypass `config_access` by construction (secrets: `redis_url`,
//! `serve_token`, `hook_signing_secret`) were wired directly into
//! `audit_env_read` in `crates/darkmux-flow/src/lib.rs`. Sweeping flow
//! found and fixed two real, reproduced hazard classes (a shared
//! hook-rule-signing env key racing ~30 unrelated tests, fixed by giving
//! the one mutating test a rule index nothing else occupies; a shared
//! outbox-size-cap env key racing ~25 unrelated tests, fixed by
//! injecting the cap directly instead of mutating global state) — see
//! `hooks.rs`'s `delivery_carries_a_signature_the_receiver_can_recompute_
//! when_signed` and `HookSink::new_for_test_with_max_outbox_mb` for the
//! detail. A further ~29 empirically-real (mutated-key-vs-named-reader)
//! findings across `darkmux-home`-flavored races in `darkmux-lab`'s
//! `crawl`/`providers` tests and `DARKMUX_MACHINE_ID` races in
//! `darkmux-flow`'s `session_presence`/`hooks` tests remain UNFIXED —
//! several spot-checked ones turn out to be benign (the mutated key is
//! read but never observably changes the reading test's own assertion,
//! e.g. `lab::inspect::tests::resolve_run_dir_id_falls_back_when_missing`
//! ends up on the same `PathBuf::from(path)` fallback regardless of which
//! root `DARKMUX_HOME` resolves to for that call) and reflexively
//! `#[serial]`-annotating all ~29 without that same case-by-case check
//! would add real suite wall-clock for a mix of real and non-hazards.
//! Left as named follow-up (see the #2643 PR body for the exact list)
//! rather than done by rote. `darkmux-serve`, `darkmux-doctor`,
//! `darkmux-fleet`, top-level `src/`, and the `runtime/` crate
//! (structurally unreachable — a separate Docker-image binary, not
//! linked into this workspace) remain unswept.
#[cfg(any(test, feature = "test-support"))]
pub fn audit_env_read(key: &str) {
    if !key.starts_with("DARKMUX_") {
        return;
    }
    // Raw `std::env::var`, not `config_access::env_str` — reading the audit
    // sink's own destination must never re-enter this function.
    let Ok(log_path) = std::env::var("DARKMUX_ENV_AUDIT_LOG") else {
        return;
    };
    if log_path.trim().is_empty() {
        return;
    }
    let thread = std::thread::current()
        .name()
        .unwrap_or("<unnamed>")
        .to_string();
    // One `write_all` call on the whole pre-built line, not `writeln!` —
    // `writeln!` on a raw `File` issues one `write(2)` per format piece, and
    // two threads' writes can then interleave mid-line. `write_all` still
    // loops internally on a short write, so this is not a hard OS-level
    // atomicity guarantee (a genuinely partial `write(2)` under O_APPEND
    // could still interleave in principle) — it is "one syscall in the
    // overwhelmingly common case" for a short single-line append on a local
    // filesystem, which is what this sink actually writes.
    let line = format!("{thread}\t{key}\n");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}
