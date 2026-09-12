//! Integration coverage for #501: `scripts/lab-init.sh` must be
//! idempotent — re-running it after the built-ins are already
//! registered reports them as "skipped" and exits 0, rather than
//! treating the "already registered" rejection as a failure (which
//! made a clean second run exit non-zero).
//!
//! POSIX-only — the script is bash and uses `flock`-backed registry
//! writes underneath.

#![cfg(unix)]

use std::path::Path;
use std::process::Command;

// ===== DARKMUX-SPAWN-HELPERS: BEGIN (#2710) ==========================
//
// The only place this file resolves the darkmux binary.
//
// The child here is `bash`, not darkmux — but `scripts/lab-init.sh`
// calls `darkmux` off the PATH this function prepends, so the grandchild
// is a darkmux process and inherits everything bash inherited. Before
// #2710 this pinned `HOME` and `PATH`, removed `DARKMUX_HOME`, and
// neutralized nothing else. Measured at that head with the dir set
// exported to sentinels and a fake `$HOME`: EXIT=0, zero files leaked —
// a fact about which destinations `lab fixture register` happens to
// touch today, not about the isolation, which was the same unneutralized
// shape as the two targets in this sweep that DID write into a sentinel
// (one of them into a hash chain). Converted for the shape.
//
// `DARKMUX_HOME` stays REMOVED, deliberately: the script's registry
// writes are asserted to land in USER scope under `$HOME`, from a cwd
// with no `.darkmux` ancestor. `neutralize_state_vars` removes it along
// with the rest.
use darkmux_types::test_isolation::neutralize_state_vars;

/// Run `scripts/lab-init.sh` with `darkmux` on PATH and an isolated
/// HOME, from a cwd with no project `.darkmux` (so registry writes land
/// in user scope under HOME).
///
/// Neutralize FIRST, pin SECOND — `Command` applies `.env` and
/// `.env_remove` in call order, so the `HOME` / `PATH` pins have to come
/// after.
fn run_lab_init(home: &Path) -> std::process::Output {
    let bin = Path::new(env!("CARGO_BIN_EXE_darkmux"));
    let bin_dir = bin.parent().expect("binary has a parent dir");
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/lab-init.sh");
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = Command::new("bash");
    neutralize_state_vars(&mut cmd);
    cmd.arg(script)
        .current_dir(home)
        .env("HOME", home)
        .env("PATH", path)
        .output()
        .expect("running scripts/lab-init.sh")
}

// ===== DARKMUX-SPAWN-HELPERS: END (#2710) ============================

#[test]
fn lab_init_is_idempotent_on_rerun() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path();

    // First run: registers the built-in(s) fresh.
    let first = run_lab_init(home);
    let first_out = String::from_utf8_lossy(&first.stdout);
    let first_err = String::from_utf8_lossy(&first.stderr);
    assert!(
        first.status.success(),
        "first lab-init run failed: stdout={first_out} stderr={first_err}"
    );
    assert!(
        first_out.contains("registered"),
        "first run should report registrations: {first_out}"
    );

    // Second run: the built-in is already registered. Pre-#501 this
    // exited non-zero (the rejection counted as a failure). Post-fix it
    // is reported as "skipped" and exits 0.
    let second = run_lab_init(home);
    let second_out = String::from_utf8_lossy(&second.stdout);
    let second_err = String::from_utf8_lossy(&second.stderr);
    assert!(
        second.status.success(),
        "second (idempotent) lab-init run must exit 0: stdout={second_out} stderr={second_err}"
    );
    assert!(
        second_out.contains("skipped: ") || second_out.contains(" skipped,"),
        "second run should report skips, not failures: {second_out}"
    );
    assert!(
        second_out.contains("0 failed"),
        "second run should record no failures: {second_out}"
    );
}
