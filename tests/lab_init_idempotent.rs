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

/// Run `scripts/lab-init.sh` with `darkmux` on PATH and an isolated
/// HOME, from a cwd with no project `.darkmux` (so registry writes land
/// in user scope under HOME).
fn run_lab_init(script: &str, bin_dir: &Path, home: &Path) -> std::process::Output {
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new("bash")
        .arg(script)
        .current_dir(home)
        .env("HOME", home)
        .env("PATH", path)
        .env_remove("DARKMUX_HOME")
        .output()
        .expect("running scripts/lab-init.sh")
}

#[test]
fn lab_init_is_idempotent_on_rerun() {
    let bin = Path::new(env!("CARGO_BIN_EXE_darkmux"));
    let bin_dir = bin.parent().expect("binary has a parent dir");
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/lab-init.sh");

    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path();

    // First run: registers the built-in(s) fresh.
    let first = run_lab_init(script, bin_dir, home);
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
    let second = run_lab_init(script, bin_dir, home);
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
