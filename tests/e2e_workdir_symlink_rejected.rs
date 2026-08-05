//! E2E TDD scenario for Wave-E.2 (#255): cross-machine + local
//! dispatches MUST reject operator-named symlink workdirs at the
//! queue boundary AND the runtime boundary.
//!
//! Pre-fix (Wave-E.1 main): the (since-removed, #1405) openclaw shell-out
//! path's `apply_workdir_override` followed symlinks silently — a
//! `--workdir /tmp/sym-to-etc` would point the openclaw workspace at the
//! symlink target. The internal-runtime path had the guard since #232 but
//! they were duplicate implementations.
//!
//! Post-fix (this PR): shared `crate::workdir::validate_workdir`
//! enforces the symlink check + canonicalize + is_dir uniformly
//! across BOTH runtime paths AND the runner's `WorkJob.workdir`
//! validation in `handle_claimed_job`.
//!
//! Test shape: spawn a single-node fleet (no Redis needed since we're
//! exercising the LOCAL runtime-boundary validation here), create a
//! tempdir + a symlink pointing to it, invoke
//! `darkmux dispatch coder <symlink> hi` (message positional, #1426),
//! assert non-zero exit + the symlink-reject error.

#[path = "e2e/mod.rs"]
mod e2e;

use e2e::harness::{FleetHarness, NodeSpec};

fn redis_available() -> bool {
    let ok = std::process::Command::new("redis-server")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    // (#1662) Where the suite is REQUIRED, a missing redis is a HARD
    // FAILURE, never a skip.
    //
    // The skip exists so a contributor without redis installed isn't
    // blocked locally. But CI is this project's merge gate (local runs are
    // targeted-module by doctrine), and for the entire life of this harness
    // no workflow installed redis-server — so every e2e test on every run
    // returned early and reported `ok`. A real boot pays a release build
    // plus two daemon spawns; `dual_node_harness_boots_cleanly` was
    // finishing in 8 milliseconds. The fleet layer was guarded by nothing,
    // loudly reporting that it was guarded.
    //
    // A dependency that silently converts "did not run" into "passed" is
    // the same defect class as a status inferred rather than recorded: the
    // absence of evidence rendered as evidence of absence.
    //
    // Keyed on DARKMUX_E2E_REQUIRED, deliberately NOT on `CI`. GitHub sets
    // `CI` on EVERY runner, and the macOS workspace job runs these same
    // binaries via `cargo test --workspace` without installing redis — so a
    // `CI` gate would have failed the job that is correctly not responsible
    // for this suite. The env var names the actual requirement ("this job
    // opted in to running the fleet e2e") instead of a proxy for it, and
    // only `fleet-e2e` sets it.
    if !ok && std::env::var("DARKMUX_E2E_REQUIRED").is_ok() {
        panic!(
            "redis-server is not on PATH, but DARKMUX_E2E_REQUIRED is set — the job that \
             opted in must never silently skip the fleet e2e suite (#1662). Install it \
             (`apt-get install -y redis-server`) or fix the runner image — do NOT relax \
             this back into a skip."
        );
    }
    ok
}

#[test]
fn dispatch_rejects_symlink_workdir() {
    if !redis_available() {
        eprintln!("skipping dispatch_rejects_symlink_workdir: redis-server not on PATH");
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

    // Run `darkmux dispatch coder --workdir <sym> hi` from node-a's env
    // (message is positional, #1426). We use --skip-preflight to bypass the
    // Docker preflight check (this test doesn't need a real container run —
    // the workdir validation happens before `docker run` is ever invoked, so
    // it fires regardless of Docker availability).
    let node = harness.node("node-a").unwrap();
    let output = node
        .cmd()
        .args([
            "dispatch",
            "coder",
            "--workdir",
            sym.to_str().unwrap(),
            "hi",
            "--skip-preflight",
        ])
        .output()
        .expect("running darkmux dispatch");

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
