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
//! Not every `DARKMUX_*`-reading spawn boundary in `darkmux-crew`
//! propagates the thread name the way `concurrent_dispatch::
//! spawn_scoped_named` does for the dispatch-execution path — three
//! production `thread::spawn` calls in `dispatch_internal.rs` (the
//! tailer, the inactivity watchdog, the thermal sampler) and several
//! test-local `thread::spawn` calls (`absence_backstop.rs`,
//! `remote_budget.rs`, `workspace_spec/materialize.rs`,
//! `step_kinds/builtins.rs`'s mock Redis server) do not. A read on one of
//! those threads still logs (as `<unnamed>`) rather than vanishing, and
//! the report script now fails loud on it rather than silently passing —
//! but closing every one of those gaps with real attribution is left as
//! follow-up, not done here. This module also covers only
//! `darkmux-types` + `darkmux-crew` (plus the one `darkmux-profiles`
//! chokepoint above); `darkmux-flow`, `darkmux-lab`, `darkmux-serve`, and
//! the `runtime/` crate (structurally unreachable — it's a separate
//! Docker-image binary, not linked into this workspace) are unswept.
//! Tracked as #2643, with the concrete counts (34 unguarded when
//! `SRC_DIRS` is pointed at `darkmux-flow`; the 4 residual unattributable
//! `DARKMUX_FLOWS_DIR`/`DARKMUX_MACHINE_ID` reads from the
//! `dispatch_internal.rs` gap above) recorded there.
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
