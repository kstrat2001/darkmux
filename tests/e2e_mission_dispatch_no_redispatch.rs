//! E2E TDD scenario for Wave-E.3 (#255): a second `darkmux mission
//! dispatch` of the same mission MUST NOT re-publish phases that are
//! already in flight from a prior dispatch.
//!
//! Pre-fix (Wave-E.2 main): PR-D.1's filter drops `Running` phases,
//! BUT mission dispatch doesn't call `lifecycle::phase_start` so the
//! phases stay `Planned` in their JSON after the first dispatch. A
//! second invocation of `mission dispatch` finds the same Planned
//! phases and re-publishes them — two runners race on the same work.
//!
//! Post-fix (this PR): `cmd_mission_dispatch` calls
//! `lifecycle::phase_start` on each filtered phase BEFORE publishing.
//! The Planned→Running flip is the state-machine gate; the second
//! invocation finds 0 dispatchable phases (they're all Running now)
//! and exits with the "nothing to fan out" exit code 2.

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

fn write_mission_fixture(crew_root: &std::path::Path, mission_id: &str, phase_ids: &[&str]) {
    let mission_dir = crew_root.join("missions").join(mission_id);
    std::fs::create_dir_all(mission_dir.join("phases")).unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    let mission_json = serde_json::json!({
        "id": mission_id,
        "description": "test mission for re-dispatch gating",
        "status": "active",
        "phase_ids": phase_ids,
        "created_ts": now_ms,
        "started_ts": now_ms,
    });
    std::fs::write(
        mission_dir.join("mission.json"),
        serde_json::to_string_pretty(&mission_json).unwrap(),
    )
    .unwrap();

    for sid in phase_ids {
        let phase_json = serde_json::json!({
            "id": sid,
            "mission_id": mission_id,
            "description": format!("phase {sid} — implement the {sid} thing"),
            "status": "planned",
            "depends_on": [],
            "created_ts": now_ms,
        });
        std::fs::write(
            mission_dir.join("phases").join(format!("{sid}.json")),
            serde_json::to_string_pretty(&phase_json).unwrap(),
        )
        .unwrap();
    }
}

#[test]
fn second_mission_dispatch_finds_nothing_to_fan_out() {
    if !redis_available() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }

    // The harness's openclaw config stub has no roles. Post-#590, mission
    // dispatch only checks the role EXISTS before fanning phases onto the
    // single `darkmux:work` stream (no tier requirement), so we register one
    // role to reach the publish + phase_start path (vs. bailing at
    // role-not-found).
    let harness = FleetHarness::boot(vec![NodeSpec::new("node-a")])
        .expect("FleetHarness::boot");
    let node = harness.node("node-a").unwrap();

    // Write a role manifest into node-a's crew root.
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
    std::fs::write(role_dir.join("tdd-coder.md"), "test system prompt").unwrap();

    // Write the mission + 2 phase fixtures.
    write_mission_fixture(&node.crew_root, "m-test", &["phase-alpha", "phase-beta"]);

    // First mission dispatch: --no-wait so we exit after publish.
    let out1 = node
        .cmd()
        .args([
            "mission", "dispatch", "m-test",
            "--role", "tdd-coder",
            "--no-wait",
        ])
        .output()
        .expect("first mission dispatch");
    let stdout1 = String::from_utf8_lossy(&out1.stdout);
    let stderr1 = String::from_utf8_lossy(&out1.stderr);
    assert!(
        out1.status.success(),
        "first mission dispatch should succeed; stdout={stdout1}\nstderr={stderr1}"
    );
    assert!(
        stderr1.contains("phases=2") || stderr1.contains("phase=phase-alpha"),
        "expected 2 phases published; stderr={stderr1}"
    );

    // Second mission dispatch — should find NOTHING to fan out
    // because the phases are now Running (phase_start flipped them).
    let out2 = node
        .cmd()
        .args([
            "mission", "dispatch", "m-test",
            "--role", "tdd-coder",
            "--no-wait",
        ])
        .output()
        .expect("second mission dispatch");
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    assert!(
        !out2.status.success(),
        "second mission dispatch should NOT succeed (exit code 2 = nothing to dispatch); \
         stderr={stderr2}"
    );
    assert_eq!(
        out2.status.code(),
        Some(2),
        "expected exit code 2 (nothing to fan out); stderr={stderr2}"
    );
    assert!(
        stderr2.contains("no phases") || stderr2.contains("nothing to fan out") || stderr2.contains("Nothing to fan out"),
        "expected 'no phases to dispatch' message; stderr={stderr2}"
    );
}
