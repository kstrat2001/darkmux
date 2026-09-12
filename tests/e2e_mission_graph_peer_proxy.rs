//! E2E coverage for #1466 gate MUST FIX 4: `mission_graph_json_handler`
//! proxying a live peer's mission graph END TO END through the REAL
//! production wiring — two real `darkmux serve` daemon processes, real
//! Redis, a real roster file, and a real loopback HTTP call between them
//! (never a mocked listener standing in for a peer).
//!
//! **Why this binary exists (#1466 gate MUST FIX 4).** The proof of this
//! wiring used to live in-process as
//! `mission_graph_json_handler_proxies_a_live_peers_graph`
//! (`crates/darkmux-serve/src/lib_tests.rs`), gated on its own
//! `redis_server_available()` whose false arm silently `eprintln!`s and
//! returns. The ONLY workflow job that runs `crates/darkmux-serve`'s test
//! suite is `build-test-lint` (macOS, `cargo test --workspace`), and that
//! job installs no redis-server — so on every CI run since that test
//! landed, this exact proof of the production wiring reported `ok`
//! without ever executing a single assertion. `ci.yml`'s own header
//! memorializes the general shape of this defect class (#1662): "a
//! dependency that silently converts 'did not run' into 'passed' is the
//! same defect class as a status inferred rather than recorded" — and
//! it's exactly `#975`'s shape again (`docker docker run` riding four
//! releases of green CI because nothing ever executed the real
//! `Command`).
//!
//! This binary lives under `tests/e2e_*.rs`, which the `fleet-e2e` job
//! (ubuntu-latest, apt-installed redis, `DARKMUX_E2E_REQUIRED=1`) runs
//! with the skip converted into a HARD FAILURE — see `redis_available()`
//! below, mirroring every sibling `e2e_*.rs` binary's own guard. A rename
//! or an accidental skip here can no longer report green.
#![cfg(unix)]

#[path = "e2e/mod.rs"]
mod e2e;

use e2e::harness::{FleetHarness, FleetNode, NodeSpec};

fn redis_available() -> bool {
    let ok = std::process::Command::new("redis-server")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    // (#1662) Where the suite is REQUIRED, a missing redis is a HARD
    // FAILURE, never a skip. See every sibling `e2e_*.rs` binary's own
    // copy of this exact comment for the full rationale; kept verbatim so
    // a reader who has seen one has seen them all.
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

/// Hand-write a minimal `Mission` JSON directly onto `node`'s own crew
/// root — no dispatch, no AI call (the operator's "no real runs" rule for
/// this session applies here too, and there is no need for a real
/// dispatch just to prove an HTTP proxy path). Matches the on-disk shape
/// `darkmux_crew::loader::load_missions()` expects
/// (`<crew_root>/missions/<id>/mission.json`) — the exact shape
/// `darkmux-crew`'s own `loader_load_per_mission_tests.rs::seed_mission`
/// helper seeds in that crate's unit tests.
fn seed_minimal_mission_on(node: &FleetNode, mission_id: &str) {
    let dir = node.crew_root.join("missions").join(mission_id);
    std::fs::create_dir_all(&dir).expect("create mission dir");
    let mission = serde_json::json!({
        "id": mission_id,
        "description": "e2e #1466 gate MUST FIX 4 fixture — no real dispatch",
        "status": "active",
        "phase_ids": [],
        "created_ts": 1_700_000_000u64,
    });
    std::fs::write(
        dir.join("mission.json"),
        serde_json::to_string_pretty(&mission).unwrap(),
    )
    .expect("write mission.json");
}

/// Write a day-file flow record onto `reader`'s own flows_dir attributing
/// `mission_id` to `owner_machine_id` — the LOCAL-DISK half of
/// `mission_owner_machine`'s attribution union (the fleet-stream half is
/// not exercised here, same split the original in-process test drew).
fn attribute_mission_locally(reader: &FleetNode, mission_id: &str, owner_machine_id: &str) {
    let today = darkmux_flow::day_utc_now();
    let day_file = reader.flows_dir.join(format!("{today}.jsonl"));
    let record = format!(
        r#"{{"ts":"{today}T10:00:00Z","action":"mission start","mission_id":"{mission_id}","machine_id":"{owner_machine_id}"}}"#
    );
    std::fs::write(&day_file, record + "\n").expect("write day-file attribution record");
}

/// Register `peer` in `reader`'s roster via the real `machine add` CLI —
/// same helper shape as `e2e_fleet_status_deep.rs::populate_roster_via_cli`.
fn register_peer(reader: &FleetNode, peer: &FleetNode) {
    let out = reader
        .cmd()
        .args([
            "machine", "add", &peer.machine_id,
            "--address", &format!("127.0.0.1:{}", peer.daemon_port),
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

/// Publish a live presence beat for `peer` directly onto the harness's
/// shared Redis. Written directly rather than waited-for from `peer`'s
/// own emitter thread (`darkmux_flow::presence::spawn_emitter_thread`)
/// because that thread self-disables on any non-macOS host (no
/// `IOPlatformUUID`-derived `machine_uid`, #640) — this e2e binary runs
/// on the `fleet-e2e` job's ubuntu-latest runner. The emitter thread
/// itself is exercised elsewhere; what THIS test needs is only "presence
/// says peer is live", which is exactly what a real beat on the real
/// shared Redis proves regardless of who wrote it.
fn publish_live_presence(redis_url: &str, machine_id: &str) {
    let client = redis::Client::open(redis_url).expect("redis client");
    let beat = darkmux_flow::presence::PresenceBeat {
        machine_uid: format!("uid-{machine_id}"),
        display_name: machine_id.to_string(),
        schema_version: "1.0.0".to_string(),
        beat_ts_ms: darkmux_flow::presence::now_ms(),
        specs: None,
        loaded_models: Vec::new(),
        darkmux_version: None,
    };
    darkmux_flow::presence::write_beat(&client, &beat, 60).expect("write presence beat");
}

#[test]
fn mission_graph_json_handler_proxies_a_live_peers_graph_end_to_end() {
    if !redis_available() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }
    const MISSION_ID: &str = "peer-live-mission-e2e-1";

    let harness = FleetHarness::boot(vec![
        NodeSpec::new("reader-node"),
        NodeSpec::new("peer-node"),
    ])
    .expect("FleetHarness::boot");
    let reader = harness.node("reader-node").expect("reader-node");
    let peer = harness.node("peer-node").expect("peer-node");

    // The mission lives on the PEER's own disk only — the reader's local
    // `build_mission_graph` will genuinely miss it.
    seed_minimal_mission_on(peer, MISSION_ID);
    attribute_mission_locally(reader, MISSION_ID, &peer.machine_id);
    register_peer(reader, peer);
    publish_live_presence(harness.redis_url(), &peer.machine_id);

    let url = format!(
        "http://127.0.0.1:{}/mission/{MISSION_ID}/graph.json",
        reader.daemon_port
    );
    let resp = ureq::get(&url).call().expect("GET graph.json from reader");
    assert_eq!(
        resp.status(),
        200,
        "a local miss attributed to a live, rostered, reachable peer must render, not 404"
    );
    let body_str = resp.into_string().expect("response body is readable");
    let body: serde_json::Value =
        serde_json::from_str(&body_str).expect("response body is valid JSON");
    assert_eq!(body["mission_id"], MISSION_ID);
    assert_eq!(
        body["note"],
        format!("fetched live from peer `{}`", peer.machine_id),
        "provenance must name the peer that actually answered: {body}"
    );
}

#[test]
fn mission_graph_json_handler_refuses_to_proxy_an_already_relayed_request() {
    // (#1466 gate MUST FIX 1, belt-and-braces) End-to-end proof that the
    // PRODUCTION handler — not just `peer_graph::try_peer_graph` in
    // isolation — actually reads the incoming relay-marker header and
    // refuses to relay again. Everything about this scenario (attributed,
    // rostered, live peer with the mission genuinely on ITS disk) is
    // identical to the happy-path test above; the only difference is the
    // request itself carries the relay header, so it must 404 instead of
    // proxying, exactly as an incoming request forwarded by a peer's own
    // copy of this same guard would.
    if !redis_available() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }
    const MISSION_ID: &str = "peer-live-mission-e2e-2";

    let harness = FleetHarness::boot(vec![
        NodeSpec::new("reader-node"),
        NodeSpec::new("peer-node"),
    ])
    .expect("FleetHarness::boot");
    let reader = harness.node("reader-node").expect("reader-node");
    let peer = harness.node("peer-node").expect("peer-node");

    seed_minimal_mission_on(peer, MISSION_ID);
    attribute_mission_locally(reader, MISSION_ID, &peer.machine_id);
    register_peer(reader, peer);
    publish_live_presence(harness.redis_url(), &peer.machine_id);

    let url = format!(
        "http://127.0.0.1:{}/mission/{MISSION_ID}/graph.json",
        reader.daemon_port
    );
    // The exact header name is `peer_graph::PEER_RELAY_HEADER` — not
    // imported here (this binary doesn't link `darkmux-serve` as a lib
    // dependency the way `crates/darkmux-serve` itself does; it drives
    // the real compiled binary over HTTP instead), so the literal is
    // duplicated. Any drift between the two is caught by the OTHER half
    // of this proof: the happy-path test above would start failing too,
    // since both requests hit the exact same route.
    let result = ureq::get(&url)
        .set("x-darkmux-peer-relay", "1")
        .call();
    match result {
        Err(ureq::Error::Status(404, _)) => {
            // expected — the relay guard refused to proxy.
        }
        Ok(resp) => panic!(
            "an already-relayed request must never be proxied — got HTTP {} instead of 404",
            resp.status()
        ),
        Err(e) => panic!("unexpected error calling reader daemon: {e}"),
    }
}
