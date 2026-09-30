//! `darkmux machine list` reads the fleet view: one row per machine, this
//! machine's own row always among them, and each peer's card from its fleet
//! listener over the verified peer path. (Before 5.0 this was `machine list
//! --deep`, a sequential pull of each peer's `/machine/specs`; the flag is
//! retired.)
//!
//! These daemons run no fleet listener and no network identity tool, so no
//! peer can be verified: this file proves the parts that need no overlay. This
//! machine's own row carries its card, and a machine that cannot be asked
//! degrades to its own row with a typed reason and never fails the whole
//! command. The card crossing between two daemons, over a verified listener, is
//! `tests/fleet_profile_address_two_daemons.rs`.

#![cfg(unix)]

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

/// Populate node-a's roster with both spawned machines via the existing
/// `machine add` CLI verb. Returns nothing — exits the test on add failure.
fn populate_roster_via_cli(viewer: &e2e::harness::FleetNode, peers: &[&e2e::harness::FleetNode]) {
    for peer in peers {
        let out = viewer
            .cmd()
            .args([
                "machine", "add", &peer.machine_id,
                "--address", &format!("127.0.0.1:{}", peer.daemon_port),
                // Same-host test fleet: loopback does reach the peer (#2924).
                "--allow-loopback",
            ])
            .output()
            .expect("running `darkmux machine add`");
        assert!(
            out.status.success(),
            "machine add of {} failed: stdout={}\nstderr={}",
            peer.machine_id,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

#[test]
fn machine_list_shows_this_machines_card_and_says_why_a_peer_is_not_asked() {
    if !redis_available() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }

    // (#2727) Shares this binary's one redis-server (see harness.rs's
    // `boot_sharing_redis` doc for why this file is a safe candidate:
    // pure roster + card fetch, no dispatch
    // fan-out).
    let harness = FleetHarness::boot_sharing_redis(
        vec![NodeSpec::new("node-a"), NodeSpec::new("node-b")],
        "machine_list_shows_this_machines_card_and_says_why_a_peer_is_not_asked",
    )
    .expect("FleetHarness::boot_sharing_redis");

    let node_a = harness.node("node-a").expect("node-a");
    let node_b = harness.node("node-b").expect("node-b");

    // From node-a's perspective, register both machines in the roster.
    populate_roster_via_cli(node_a, &[node_a, node_b]);

    let out = node_a
        .cmd()
        .args(["machine", "list"])
        .output()
        .expect("running `darkmux machine list`");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "machine list should succeed; stdout={stdout}\nstderr={stderr}"
    );

    // Both machine_ids appear in the rendered table, with the card's columns.
    for id in ["node-a", "node-b"] {
        assert!(stdout.contains(id), "expected {id} in the output: {stdout}");
    }
    assert!(stdout.contains("AI-HEADROOM"), "the card's columns are the default: {stdout}");
    // This machine's row carries its own card: node-a is the CLI's own entry.
    let view = list_json(node_a);
    let a = row(&view, "node-a");
    assert_eq!(a["is_this_machine"], true, "the entry that is this machine is flagged: {a}");
    assert_eq!(a["card"]["state"], "available", "node-a must state its card: {a}");
    assert_eq!(a["card"]["source"], "local", "{a}");
    assert_eq!(a["card"]["card"]["specs"]["darkmux_version"], env!("CARGO_PKG_VERSION"));
    // node-b cannot be verified (no overlay here): it was not asked, and its
    // row says why in a typed reason, never a blank or a guess.
    let b = row(&view, "node-b");
    assert_eq!(b["card"]["state"], "unreachable", "{b}");
    let reason = b["card"]["reason"].as_str().unwrap();
    assert!(["identity_unavailable", "not_on_overlay"].contains(&reason), "a typed not-verified reason, got {reason}: {b}");
    assert_eq!(b["accepts"]["state"], "unknown", "nothing answered, so no grant is known: {b}");
}

/// `viewer`'s `machine list --json`.
fn list_json(viewer: &e2e::harness::FleetNode) -> serde_json::Value {
    let out = viewer
        .cmd()
        .args(["machine", "list", "--json"])
        .output()
        .expect("running `darkmux machine list --json`");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).expect("machine list --json")
}

/// The row for roster entry `id`.
fn row<'a>(view: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    view["machines"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["entry"]["id"] == id))
        .unwrap_or_else(|| panic!("{id} row missing: {view}"))
}

#[test]
fn machine_list_degrades_gracefully_for_a_machine_it_cannot_ask() {
    if !redis_available() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }

    // (#2727) See the aggregates-from-reachable-peers test above for why
    // this file shares its redis.
    let harness = FleetHarness::boot_sharing_redis(
        vec![NodeSpec::new("node-a")],
        "machine_list_degrades_gracefully_for_a_machine_it_cannot_ask",
    )
    .expect("FleetHarness::boot_sharing_redis");
    let node_a = harness.node("node-a").expect("node-a");

    // Register a real peer + a synthetic unreachable peer (port 1 is
    // not going to have a listener).
    populate_roster_via_cli(node_a, &[node_a]);
    let add_unreachable = node_a
        .cmd()
        .args([
            "machine", "add", "ghost-machine",
            "--address", "127.0.0.1:1",
            "--allow-loopback",
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
        .args(["machine", "list"])
        .output()
        .expect("running `darkmux machine list`");
    let stdout = String::from_utf8_lossy(&out.stdout);

    // The whole command MUST succeed even when one machine cannot be asked.
    assert!(
        out.status.success(),
        "machine list should not fail on a machine it cannot ask; stdout={stdout}\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    // node-a's card is present (it is this machine).
    assert!(stdout.contains("node-a"), "node-a row missing: {stdout}");
    let view = list_json(node_a);
    assert_eq!(row(&view, "node-a")["card"]["state"], "available");
    // ghost-machine appears with its reason: no node stands behind its
    // address, so it is not asked and nothing is sent to it.
    assert!(stdout.contains("ghost-machine"), "ghost-machine row missing: {stdout}");
    assert!(stdout.contains("unreachable"), "ghost-machine says why: {stdout}");
    assert_eq!(row(&view, "ghost-machine")["card"]["state"], "unreachable");
}
