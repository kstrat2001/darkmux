//! E2E TDD scenario for Wave-E.8 (#255): pre-validation is all-or-
//! nothing. If any phase trips `WorkJob::validate()` (e.g. oversize
//! description), `mission dispatch` MUST NOT publish any phases —
//! not even the ones whose own jobs would have passed validation.
//!
//! This is regression coverage for the fix already in main (PR-D.1
//! HIGH-2 follow-up). The fix pre-builds + pre-validates every WorkJob
//! BEFORE the publish loop runs, so a mid-loop validation failure
//! never leaves orphan jobs on Redis.
//!
//! Test asserts the invariant directly: query Redis `XLEN
//! darkmux:work` after a known-failing dispatch and confirm
//! it's still 0. A future refactor that switched to a "publish-then-
//! validate-each" pattern would silently leak orphan jobs; this test
//! catches that regression.

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

fn write_mission_with_oversize_phase(
    crew_root: &std::path::Path,
    mission_id: &str,
    oversize_phase_id: &str,
    normal_phase_id: &str,
) {
    let mission_dir = crew_root.join("missions").join(mission_id);
    std::fs::create_dir_all(mission_dir.join("phases")).unwrap();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let mission_json = serde_json::json!({
        "id": mission_id,
        "description": "all-or-nothing pre-publish validation test",
        "status": "active",
        "phase_ids": [oversize_phase_id, normal_phase_id],
        "created_ts": now_ms,
        "started_ts": now_ms,
    });
    std::fs::write(
        mission_dir.join("mission.json"),
        serde_json::to_string_pretty(&mission_json).unwrap(),
    )
    .unwrap();

    // Phase with a 300 KiB description — over `MAX_WORK_MESSAGE_BYTES`
    // (256 KiB). Will trip `WorkJob::validate()` at pre-build time.
    let oversize = "x".repeat(300 * 1024);
    let phase_oversize = serde_json::json!({
        "id": oversize_phase_id,
        "mission_id": mission_id,
        "description": oversize,
        "status": "planned",
        "depends_on": [],
        "created_ts": now_ms,
    });
    std::fs::write(
        mission_dir.join("phases").join(format!("{oversize_phase_id}.json")),
        serde_json::to_string_pretty(&phase_oversize).unwrap(),
    )
    .unwrap();

    // The other phase is well within limits. Pre-fix (publish-each-
    // and-validate-each), this phase would land on Redis as an orphan
    // before phase_oversize tripped validation.
    let phase_normal = serde_json::json!({
        "id": normal_phase_id,
        "mission_id": mission_id,
        "description": "small phase",
        "status": "planned",
        "depends_on": [],
        "created_ts": now_ms,
    });
    std::fs::write(
        mission_dir.join("phases").join(format!("{normal_phase_id}.json")),
        serde_json::to_string_pretty(&phase_normal).unwrap(),
    )
    .unwrap();
}

fn xlen(redis_url: &str, stream: &str) -> u64 {
    let client = redis::Client::open(redis_url).expect("open redis");
    let mut conn = client.get_connection().expect("redis connection");
    // XLEN returns 0 for a non-existent stream — that's the
    // "nothing published" case we want to detect.
    redis::cmd("XLEN").arg(stream).query(&mut conn).unwrap_or(0)
}

#[test]
fn oversize_phase_aborts_publish_with_no_orphans() {
    if !redis_available() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }

    let harness = FleetHarness::boot(vec![NodeSpec::new("node-a")])
        .expect("FleetHarness::boot");
    let node = harness.node("node-a").unwrap();

    // Set up a tdd-coder role + the mission + phases.
    let role_json = serde_json::json!({
        "id": "tdd-coder",
        "description": "test role",
        "skills": [],
        "tool_palette": {"allow": [], "deny": []},
        "escalation_contract": "bail-with-explanation",
        "tier": "inference"
    });
    let role_dir = node.crew_root.join("roles");
    std::fs::create_dir_all(&role_dir).unwrap();
    std::fs::write(
        role_dir.join("tdd-coder.json"),
        serde_json::to_string_pretty(&role_json).unwrap(),
    )
    .unwrap();
    std::fs::write(role_dir.join("tdd-coder.md"), "test prompt").unwrap();

    write_mission_with_oversize_phase(
        &node.crew_root,
        "m-oversize-test",
        "phase-oversize",
        "phase-normal",
    );

    // Sanity: stream is empty before dispatch.
    let pre_xlen = xlen(harness.redis_url(), "darkmux:work");

    let out = node
        .cmd()
        .args([
            "mission", "dispatch", "m-oversize-test",
            "--role", "tdd-coder",
            "--no-wait",
        ])
        .output()
        .expect("running mission dispatch");

    let stderr = String::from_utf8_lossy(&out.stderr);

    // The dispatch MUST fail (validation error from pre-build step).
    assert!(
        !out.status.success(),
        "expected dispatch to fail at pre-validate; stderr={stderr}"
    );
    assert!(
        stderr.contains("validation failed") || stderr.contains("exceeds") || stderr.contains("message"),
        "expected validation error message; stderr={stderr}"
    );

    // Critical assertion: NO phases published. The all-or-nothing
    // invariant means even the well-formed `phase-normal` did not
    // land on Redis. Net XADD count for our stream after the failed
    // dispatch must equal the pre-dispatch count (typically 0).
    let post_xlen = xlen(harness.redis_url(), "darkmux:work");
    assert_eq!(
        post_xlen, pre_xlen,
        "all-or-nothing pre-publish invariant violated: \
         XLEN(darkmux:work) went from {pre_xlen} to {post_xlen}. \
         A future refactor must NOT publish ANY phase when ANY phase trips validation."
    );

    // Bonus: ORPHAN-list message must NOT appear (we never had orphans
    // to report; if it does appear, the test fixture / fix is inconsistent).
    assert!(
        !stderr.contains("ORPHAN"),
        "no ORPHAN message expected when validation prevents any publish; stderr={stderr}"
    );
}
