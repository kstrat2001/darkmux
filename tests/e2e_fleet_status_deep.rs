//! TDD coverage for #275 PR-B — `darkmux fleet status --deep` fans
//! out to each reachable peer's `GET /machine/specs` endpoint
//! (shipped in PR-A) and aggregates into one enriched table.
//!
//! Pre-fix: `fleet status` only reports reachability columns (id,
//! tier, address, probe_ms). `--deep` flag doesn't exist; passing it
//! is rejected by clap.
//!
//! Post-fix: `--deep` adds an HTTP fetch per reachable machine, and
//! the rendered output gains a ram-free + models-loaded summary per
//! row. Unreachable peers degrade gracefully — row prints with "specs
//! unavailable" rather than failing the whole command.

#![cfg(unix)]

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

/// Populate node-a's roster with both spawned machines via the existing
/// `fleet add` CLI verb. Returns nothing — exits the test on add failure.
fn populate_roster_via_cli(viewer: &e2e::harness::FleetNode, peers: &[&e2e::harness::FleetNode]) {
    for peer in peers {
        let out = viewer
            .cmd()
            .args([
                "fleet", "add", &peer.machine_id,
                "--tier", &peer.tier,
                "--address", &format!("127.0.0.1:{}", peer.daemon_port),
            ])
            .output()
            .expect("running `darkmux fleet add`");
        assert!(
            out.status.success(),
            "fleet add of {} failed: stdout={}\nstderr={}",
            peer.machine_id,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

#[test]
fn fleet_status_deep_aggregates_specs_from_reachable_peers() {
    if !redis_available() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }

    let harness = FleetHarness::boot(vec![
        NodeSpec::inference("node-a"),
        NodeSpec::hub("node-b"),
    ])
    .expect("FleetHarness::boot");

    let node_a = harness.node("node-a").expect("node-a");
    let node_b = harness.node("node-b").expect("node-b");

    // From node-a's perspective, register both machines in the roster.
    populate_roster_via_cli(node_a, &[node_a, node_b]);

    let out = node_a
        .cmd()
        .args(["fleet", "status", "--deep"])
        .output()
        .expect("running `darkmux fleet status --deep`");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "fleet status --deep should succeed; stdout={stdout}\nstderr={stderr}"
    );

    // Both machine_ids should appear in the rendered table.
    assert!(
        stdout.contains("node-a"),
        "expected node-a in --deep output: {stdout}"
    );
    assert!(
        stdout.contains("node-b"),
        "expected node-b in --deep output: {stdout}"
    );

    // The --deep flag must surface AT LEAST one spec dimension we didn't
    // previously have. Two reliable signals: a "ram-free" / "ram_free"
    // column header or the darkmux version (which the specs endpoint
    // returns). Either is sufficient — the test just needs to verify
    // that --deep is actually fetching the specs payload.
    let has_ram_signal =
        stdout.contains("ram-free") || stdout.contains("ram_free") || stdout.contains("RAM");
    let has_version_signal = stdout.contains(env!("CARGO_PKG_VERSION"));
    assert!(
        has_ram_signal || has_version_signal,
        "--deep must surface a spec dimension (ram or version); got:\n{stdout}"
    );
}

#[test]
fn fleet_status_deep_degrades_gracefully_for_unreachable_peers() {
    if !redis_available() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }

    let harness = FleetHarness::boot(vec![NodeSpec::inference("node-a")])
        .expect("FleetHarness::boot");
    let node_a = harness.node("node-a").expect("node-a");

    // Register a real peer + a synthetic unreachable peer (port 1 is
    // not going to have a listener).
    populate_roster_via_cli(node_a, &[node_a]);
    let add_unreachable = node_a
        .cmd()
        .args([
            "fleet", "add", "ghost-machine",
            "--tier", "inference",
            "--address", "127.0.0.1:1",
        ])
        .output()
        .expect("adding ghost-machine to roster");
    assert!(
        add_unreachable.status.success(),
        "ghost-machine add failed: {}",
        String::from_utf8_lossy(&add_unreachable.stderr)
    );

    let out = node_a
        .cmd()
        .args(["fleet", "status", "--deep"])
        .output()
        .expect("running `darkmux fleet status --deep`");
    let stdout = String::from_utf8_lossy(&out.stdout);

    // The whole command MUST succeed even when one peer is unreachable.
    assert!(
        out.status.success(),
        "fleet status --deep should not fail on an unreachable peer; stdout={stdout}\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    // node-a's specs should be present (it's local + reachable).
    assert!(stdout.contains("node-a"), "node-a row missing: {stdout}");
    // ghost-machine should appear, with some indication it's unreachable
    // (the existing `fleet status` already prints unreachability; --deep
    // must not regress that).
    assert!(
        stdout.contains("ghost-machine"),
        "ghost-machine row missing: {stdout}"
    );
}

#[test]
fn fleet_status_default_unchanged_without_deep_flag() {
    if !redis_available() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }

    let harness = FleetHarness::boot(vec![NodeSpec::inference("node-a")])
        .expect("FleetHarness::boot");
    let node_a = harness.node("node-a").expect("node-a");
    populate_roster_via_cli(node_a, &[node_a]);

    let out = node_a
        .cmd()
        .args(["fleet", "status"])
        .output()
        .expect("running `darkmux fleet status`");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());

    // Without --deep, the output MUST NOT include the spec dimensions
    // (no HTTP fetch happened; we just have reachability columns).
    let leaked_version = stdout.contains(env!("CARGO_PKG_VERSION"));
    assert!(
        !leaked_version,
        "non-deep fleet status leaked spec data (version visible): {stdout}"
    );
}
