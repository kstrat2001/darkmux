//! Smoke test for the Wave-E.1 dual-node e2e harness (#255).
//!
//! Boots a 2-node fleet + mock LMStudio + Redis; confirms the daemons
//! come up healthy. Doesn't TDD any specific bug yet — that's Wave-E.2+.
//! This test exists so the harness itself has regression coverage
//! before later PRs build TDD scenarios on top of it.
//!
//! Skip when `redis-server` isn't on PATH (CI runners without redis
//! shouldn't fail — they get a clean skip message via `#[ignore]`
//! semantics).

#[path = "e2e/mod.rs"]
mod e2e;

use e2e::harness::{FleetHarness, NodeSpec};

/// Skip the test (with a clear message) if redis-server isn't installed.
/// Returns true if the test should proceed.
fn redis_available() -> bool {
    std::process::Command::new("redis-server")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn dual_node_harness_boots_cleanly() {
    if !redis_available() {
        eprintln!(
            "skipping dual_node_harness_boots_cleanly: redis-server not on PATH \
             (install via `brew install redis` or distro package). \
             This isn't a regression — the harness requires redis."
        );
        return;
    }

    let harness = FleetHarness::boot(vec![
        NodeSpec::inference("node-a"),
        NodeSpec::hub("node-b"),
    ])
    .expect("FleetHarness::boot");

    assert_eq!(harness.nodes.len(), 2, "expected two nodes");
    assert!(
        !harness.redis_url().is_empty(),
        "redis_url should be set"
    );
    assert!(
        harness.mock_lmstudio.addr().port() != 0,
        "mock_lmstudio should have a real port"
    );

    let node_a = harness.node("node-a").expect("node-a present");
    let node_b = harness.node("node-b").expect("node-b present");

    // Each daemon should have been spawned on a free port (not 0).
    assert!(node_a.daemon_port != 0);
    assert!(node_b.daemon_port != 0);
    assert_ne!(node_a.daemon_port, node_b.daemon_port);

    // Per-node tier env propagated.
    assert_eq!(node_a.tier, "inference");
    assert_eq!(node_b.tier, "hub");

    // Sanity: `darkmux doctor` from inside node-a's env should report
    // machine-tier=inference (verifies the daemon's env vars wired
    // through). Doctor may exit non-zero in the isolated test env
    // (missing real LMStudio models, etc.) — that's expected. The
    // assertion is on the printed tier value, not the exit code.
    let mut cmd = node_a.cmd();
    let output = cmd
        .arg("doctor")
        .output()
        .expect("running darkmux doctor on node-a");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("machine-tier") && stdout.contains("inference"),
        "node-a doctor should report machine-tier=inference; got:\n{stdout}"
    );
    let output_b = node_b
        .cmd()
        .arg("doctor")
        .output()
        .expect("running darkmux doctor on node-b");
    let stdout_b = String::from_utf8_lossy(&output_b.stdout);
    assert!(
        stdout_b.contains("machine-tier") && stdout_b.contains("hub"),
        "node-b doctor should report machine-tier=hub; got:\n{stdout_b}"
    );
}
