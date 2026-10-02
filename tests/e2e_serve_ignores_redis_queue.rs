//! (#2916) The Redis work queue is retired: a running daemon with Redis
//! configured must not create the old consumer group on `darkmux:work`, so it
//! cannot be a consumer of jobs XADDed there. Before 4.0 any node that could
//! write the hub's Redis could make a daemon run work this way.
//!
//! This lives on the release-binary harness because the test binary of
//! `tests/cli.rs` is built with test-support, which scrubs `DARKMUX_REDIS_URL`:
//! a daemon spawned from it never connected to Redis, so the check it held
//! here passed whether or not the daemon still consumed the queue.

#![cfg(unix)]

#[path = "e2e/mod.rs"]
mod e2e;

use e2e::harness::{FleetHarness, NodeSpec};
use std::time::Duration;

fn redis_available() -> bool {
    let ok = std::process::Command::new("redis-server")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok && std::env::var("DARKMUX_E2E_REQUIRED").is_ok() {
        panic!("redis-server is not on PATH, but DARKMUX_E2E_REQUIRED is set (#1662)");
    }
    ok
}

#[test]
fn serve_no_longer_takes_work_off_the_redis_queue() {
    if !redis_available() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }
    let harness = FleetHarness::boot(vec![NodeSpec::new("node-a")]).expect("FleetHarness::boot");
    let node = harness.node("node-a").expect("node-a");
    let client = redis::Client::open(node.redis_url.as_str()).unwrap();
    let mut conn = client.get_connection().unwrap();
    // Anti-vacuity: the release binary really talks to this Redis (a CLI note
    // lands on the flow stream), so a missing consumer group means the daemon
    // chose not to consume, not that it never connected.
    let note = node.cmd().args(["flow", "note", "--text", "queue probe", "--source", "orchestrator"]).output().unwrap();
    assert!(note.status.success(), "{}", String::from_utf8_lossy(&note.stderr));
    let stream = node.redis_stream.as_deref().unwrap_or("darkmux:flow");
    let len: u64 = redis::cmd("XLEN").arg(stream).query(&mut conn).unwrap();
    assert!(len > 0, "the release binary never wrote to the test Redis");
    // A job in the last queue shape (v4), exactly what a pre-4.0 peer would XADD.
    let record = r#"{"role_id":"pr-reviewer","message":"hang please","session_id":"s-queue","timeout_seconds":60,"published_at_unix_ms":1,"attempt":1}"#;
    let _: String = redis::cmd("XADD")
        .arg("darkmux:work").arg("*").arg("schema").arg("4").arg("record").arg(record)
        .query(&mut conn)
        .unwrap();
    // The old runner blocked in 2 s rounds; give a live consumer every chance
    // to create its group and claim the job.
    std::thread::sleep(Duration::from_secs(6));
    let groups: redis::Value = redis::cmd("XINFO").arg("GROUPS").arg("darkmux:work").query(&mut conn).unwrap();
    assert!(
        !format!("{groups:?}").contains("darkmux-runners"),
        "the daemon created the retired consumer group: {groups:?}"
    );
}
