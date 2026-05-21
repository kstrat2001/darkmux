//! E2E TDD scenario for Wave-E.5 (#255): `darkmux mission dispatch`
//! MUST reject operator-supplied mission_ids containing path-traversal
//! or other non-identifier-shaped characters at the CLI boundary.
//!
//! Pre-fix: `mission_id` flows verbatim into the `session_id` format
//! string and into the WorkJob payload. PR-D.1 security-auditor MEDIUM
//! flagged: `mission_id="../etc"` reaches the mission-lookup code path,
//! which does exact-match against load_missions() (no traversal in
//! itself), but the value lands in audit-chain JSON unrendered + in
//! future "look up by mission" filters as a substring match.
//!
//! Post-fix (this PR): cmd_mission_dispatch validates mission_id via
//! `fleet::validate_identifier("mission_id", mission_id)` at the top
//! of the handler. Bails with operator-actionable error before reaching
//! load_missions / Redis / etc.

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
fn mission_dispatch_rejects_path_traversal_mission_id() {
    if !redis_available() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }

    let harness = FleetHarness::boot(vec![NodeSpec::inference("node-a")])
        .expect("FleetHarness::boot");
    let node = harness.node("node-a").unwrap();

    let out = node
        .cmd()
        .args([
            "mission", "dispatch",
            "../etc", // path-traversal mission_id
            "--role", "tdd-coder",
            "--no-wait",
        ])
        .output()
        .expect("running darkmux mission dispatch");

    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        !out.status.success(),
        "dispatch with path-traversal mission_id should fail; stdout={stdout}\nstderr={stderr}"
    );
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("invalid char") || combined.contains("mission_id"),
        "expected mission_id charset rejection; got:\n{combined}"
    );
}

#[test]
fn mission_dispatch_rejects_special_chars_in_mission_id() {
    if !redis_available() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }

    let harness = FleetHarness::boot(vec![NodeSpec::inference("node-a")])
        .expect("FleetHarness::boot");
    let node = harness.node("node-a").unwrap();

    // Whitespace + uppercase + special chars all violate [a-z0-9_-].
    let out = node
        .cmd()
        .args([
            "mission", "dispatch",
            "Foo Bar$!",
            "--role", "tdd-coder",
            "--no-wait",
        ])
        .output()
        .expect("running darkmux mission dispatch");

    assert!(
        !out.status.success(),
        "dispatch with special-char mission_id should fail"
    );
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        combined.contains("invalid char") || combined.contains("mission_id"),
        "expected mission_id charset rejection; got:\n{combined}"
    );
}
