//! TDD coverage for #247 PR-B — `darkmux crew dispatch <role>` (no
//! `--machine`) auto-routes to a fleet peer when role.tier doesn't
//! match the local DARKMUX_MACHINE_TIER, OR bails loud when no fleet
//! peer matches.
//!
//! Pre-fix: dispatch runs locally regardless of tier mismatch. A
//! `dispatch coder` on Studio (hub tier) executes locally even when
//! the role wants inference and Laptop is right there in the roster.
//!
//! Post-fix:
//! 1. local tier == role.tier OR role.tier in {None, "any"} → local
//!    (today's behavior)
//! 2. local tier != role.tier AND fleet has matching peer → publish
//!    to `darkmux:work:<role.tier>` stream (consumer group claims)
//! 3. local tier != role.tier AND fleet has NO matching peer → bail
//!    with operator-actionable hint

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

fn write_role(crew_root: &std::path::Path, id: &str, tier: &str) {
    let role_json = serde_json::json!({
        "id": id,
        "description": format!("test role {id}"),
        "capabilities": [],
        "tool_palette": {"allow": [], "deny": []},
        "escalation_contract": "bail-with-explanation",
        "tier": tier,
    });
    let role_dir = crew_root.join("roles");
    std::fs::create_dir_all(&role_dir).unwrap();
    std::fs::write(
        role_dir.join(format!("{id}.json")),
        serde_json::to_string_pretty(&role_json).unwrap(),
    )
    .unwrap();
    std::fs::write(role_dir.join(format!("{id}.md")), "test system prompt").unwrap();
}

fn xlen(redis_url: &str, stream: &str) -> u64 {
    let client = redis::Client::open(redis_url).expect("open redis");
    let mut conn = client.get_connection().expect("redis connection");
    redis::cmd("XLEN").arg(stream).query(&mut conn).unwrap_or(0)
}

/// Hub-tier machine asks for an inference role; a fleet peer matches
/// the role's tier; the dispatch MUST publish to the inference work
/// stream (auto-route) rather than running locally.
#[test]
fn dispatch_from_hub_for_inference_role_auto_routes_to_inference_peer() {
    if !redis_available() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }
    let harness = FleetHarness::boot(vec![
        NodeSpec::hub("studio"),
        NodeSpec::inference("laptop"),
    ])
    .expect("FleetHarness::boot");

    let studio = harness.node("studio").expect("studio");
    let laptop = harness.node("laptop").expect("laptop");

    // Role lives on studio's crew dir; declared tier = inference.
    write_role(&studio.crew_root, "auto-route-coder", "inference");

    // Add laptop to studio's roster so studio knows about the
    // inference peer.
    let add = studio
        .cmd()
        .args([
            "fleet", "add", "laptop",
            "--tier", "inference",
            "--address", &format!("127.0.0.1:{}", laptop.daemon_port),
        ])
        .output()
        .expect("running `darkmux fleet add laptop`");
    assert!(
        add.status.success(),
        "fleet add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let pre_xlen = xlen(harness.redis_url(), "darkmux:work:inference");

    // Auto-route case — no --machine; role.tier=inference, local
    // tier=hub. Expected: publish to darkmux:work:inference.
    let out = studio
        .cmd()
        .args([
            "crew", "dispatch", "auto-route-coder",
            "--message", "hello from studio",
            "--no-wait",
        ])
        .output()
        .expect("running `darkmux crew dispatch`");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "dispatch should succeed (auto-route to laptop); stdout={stdout}\nstderr={stderr}"
    );

    let post_xlen = xlen(harness.redis_url(), "darkmux:work:inference");
    assert!(
        post_xlen > pre_xlen,
        "auto-route must publish to darkmux:work:inference (XLEN was {pre_xlen}, now {post_xlen}); \
         stderr={stderr}"
    );

    // The banner should make the routing decision visible.
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("tier") && (combined.contains("auto") || combined.contains("route")),
        "expected an auto-route banner mentioning tier; got:\n{combined}"
    );
}

/// Hub-tier machine asks for an inference role, but NO inference peer
/// is in the roster. The dispatch MUST fail loud — operator wants the
/// operator-actionable hint pointing at `darkmux fleet add`. Today
/// (pre-fix), this case silently dispatches locally and runs on the
/// wrong-tier machine.
#[test]
fn dispatch_bails_when_no_fleet_peer_matches_role_tier() {
    if !redis_available() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }
    let harness = FleetHarness::boot(vec![NodeSpec::hub("studio")])
        .expect("FleetHarness::boot");
    let studio = harness.node("studio").expect("studio");

    // Role tier = inference; no inference peer in studio's roster.
    write_role(&studio.crew_root, "auto-route-coder", "inference");

    let out = studio
        .cmd()
        .args([
            "crew", "dispatch", "auto-route-coder",
            "--message", "hi",
            "--no-wait",
        ])
        .output()
        .expect("running `darkmux crew dispatch`");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !out.status.success(),
        "dispatch should fail when no fleet peer matches the role's tier; \
         stdout={stdout}\nstderr={stderr}"
    );
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("inference") || combined.contains("tier"),
        "expected operator-actionable hint mentioning the missing tier; got:\n{combined}"
    );
    assert!(
        combined.contains("fleet add") || combined.contains("add a peer"),
        "expected hint to point at `darkmux fleet add` for the fix; got:\n{combined}"
    );
}

/// Local tier matches the role's tier: dispatch runs locally — no
/// queue routing, no XADD. (Regression for today's behavior on the
/// happy path.)
#[test]
fn dispatch_runs_locally_when_role_tier_matches_local_tier() {
    if !redis_available() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }
    let harness = FleetHarness::boot(vec![NodeSpec::inference("laptop")])
        .expect("FleetHarness::boot");
    let laptop = harness.node("laptop").expect("laptop");

    // Role tier matches local (both inference).
    write_role(&laptop.crew_root, "auto-route-coder", "inference");

    let pre_xlen = xlen(harness.redis_url(), "darkmux:work:inference");

    let out = laptop
        .cmd()
        .args([
            "crew", "dispatch", "auto-route-coder",
            "--message", "hi",
            "--no-wait",
        ])
        .output()
        .expect("running `darkmux crew dispatch`");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // The actual local dispatch may fail (no openclaw available in
    // the e2e env) — that's OK. What we're asserting is the absence
    // of cross-machine routing.
    let post_xlen = xlen(harness.redis_url(), "darkmux:work:inference");
    assert_eq!(
        post_xlen, pre_xlen,
        "matching local tier MUST NOT route via queue; XLEN went from {pre_xlen} to {post_xlen}\n\
         stdout={stdout}\nstderr={stderr}"
    );
}
