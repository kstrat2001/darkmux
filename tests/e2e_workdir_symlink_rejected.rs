//! E2E TDD scenario for Wave-E.2 (#255): cross-machine + local
//! dispatches MUST reject operator-named symlink workdirs at the
//! queue boundary AND the runtime boundary.
//!
//! Pre-fix (Wave-E.1 main): the openclaw path's `apply_workdir_override`
//! followed symlinks silently — a `--workdir /tmp/sym-to-etc` would
//! point the openclaw workspace at the symlink target. The
//! internal-runtime path had the guard since #232 but they were
//! duplicate implementations.
//!
//! Post-fix (this PR): shared `crate::workdir::validate_workdir`
//! enforces the symlink check + canonicalize + is_dir uniformly
//! across BOTH runtime paths AND the runner's `WorkJob.workdir`
//! validation in `handle_claimed_job`.
//!
//! Test shape: spawn a single-node fleet (no Redis needed since we're
//! exercising the LOCAL openclaw-path validation here), create a
//! tempdir + a symlink pointing to it, invoke
//! `darkmux crew dispatch coder --workdir <symlink> --message hi`,
//! assert non-zero exit + the symlink-reject error.

#[path = "e2e/mod.rs"]
mod e2e;

use e2e::harness::{FleetHarness, NodeSpec};

fn redis_available() -> bool {
    std::process::Command::new("redis-server")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn crew_dispatch_rejects_symlink_workdir() {
    if !redis_available() {
        eprintln!("skipping crew_dispatch_rejects_symlink_workdir: redis-server not on PATH");
        return;
    }

    let harness = FleetHarness::boot(vec![NodeSpec::new("node-a")])
        .expect("FleetHarness::boot");

    // Build a fixture: a real directory + a symlink pointing at it.
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("real");
    std::fs::create_dir(&target).unwrap();
    let sym = tmp.path().join("evil-symlink");
    std::os::unix::fs::symlink(&target, &sym).unwrap();

    // Run `darkmux crew dispatch coder --workdir <sym>` from node-a's
    // env. We use --skip-preflight to bypass any other-state checks
    // that the test env doesn't have set up (the openclaw config is a
    // stub). The workdir validation runs in `apply_workdir_override`,
    // which is BEFORE the openclaw subprocess spawns — so we hit our
    // validation regardless of whether openclaw is installed.
    let node = harness.node("node-a").unwrap();
    let output = node
        .cmd()
        .args([
            "crew",
            "dispatch",
            "coder",
            "--workdir",
            sym.to_str().unwrap(),
            "--message",
            "hi",
            "--skip-preflight",
        ])
        .output()
        .expect("running darkmux crew dispatch");

    assert!(
        !output.status.success(),
        "dispatch with symlink --workdir should fail; got success. stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("symlink") || combined.contains("refusing"),
        "expected symlink-reject error in output; got:\n{combined}"
    );
}
