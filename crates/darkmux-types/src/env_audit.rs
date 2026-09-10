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
//! `#[serial_test::serial]`.
//!
//! ## Chokepoints instrumented (as of #2632)
//!
//! - `config_access::env_str` — the single env-read idiom for every
//!   `DARKMUX_*` config setting `pick_string`/`pick_parsed`/`pick_dir`
//!   resolve (covers the large majority of settings in both
//!   `darkmux-types` and `darkmux-crew`, since crew reads config through
//!   these accessors rather than raw `std::env::var`).
//! - `paths::resolve` — `DARKMUX_HOME` (the bootstrap pointer, which can't
//!   live inside the config it locates) and `DARKMUX_NOTEBOOK_DIR`.
//! - `dispatch_liveness::liveness_root` — a direct `DARKMUX_HOME` read
//!   outside both chokepoints above.
//! - `residency_lease` — a second direct `DARKMUX_HOME` read site.
//!
//! This module deliberately does NOT try to intercept literal
//! `std::env::var("DARKMUX_...")` calls made directly inside test bodies
//! (the save/restore idiom every mutator test uses) — those are mutation
//! sites, not resolution sites, and are enumerated separately by scanning
//! for `set_var`/`remove_var` call sites, which — unlike "does this test
//! transitively depend on env state" — IS a fully mechanical, unambiguous
//! textual fact (see the PR body for how that half of the sweep was done).
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
    // One `write_all` call, not `writeln!` — `writeln!` on a raw `File` can
    // issue more than one underlying `write(2)` syscall (one per format
    // piece), and two threads' writes can then interleave mid-line. Building
    // the whole line first and writing it in a single syscall keeps each
    // append atomic under O_APPEND.
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
