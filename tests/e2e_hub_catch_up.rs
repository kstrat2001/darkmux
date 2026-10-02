//! (#3062) `darkmux serve` runs the hub catch-up tick: the periodic check that
//! backfills the outage a one-shot writer recorded into the hub, with no write
//! of the daemon's own to trigger it.
//!
//! The sink's unit tests call `tick()` by hand and `spawn_tick_thread`
//! directly, so deleting the daemon's one call to `spawn_hub_catch_up_thread`
//! left every one of them green. This boots the real release daemon (a binary
//! with no test-support cfg, which is what honors `DARKMUX_REDIS_URL`) on a
//! throwaway Redis. The outage watermark and the day file holding the missed
//! record are written AFTER the daemon has finished starting (its first write
//! is the only other thing that reads the watermark), so the record can reach
//! the hub only through the tick.
//!
//! RED-PROVED by hand: removing the `spawn_hub_catch_up_thread()` call from
//! `darkmux-serve`'s `run()` makes this test fail.

#![cfg(unix)]

#[path = "e2e/mod.rs"]
mod e2e;

use e2e::harness::{FleetHarness, NodeSpec};
use std::time::{Duration, Instant};

fn redis_available() -> bool {
    let ok = std::process::Command::new("redis-server")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    // A job that opted in must never silently skip (#1662).
    if !ok && std::env::var("DARKMUX_E2E_REQUIRED").is_ok() {
        panic!("redis-server is not on PATH, but DARKMUX_E2E_REQUIRED is set (#1662)");
    }
    ok
}

#[test]
fn serve_backfills_a_one_shot_writers_outage_on_its_catch_up_tick() {
    if !redis_available() {
        eprintln!("skipping: redis-server not on PATH");
        return;
    }
    let harness = FleetHarness::boot(vec![NodeSpec::new("node-a")]).expect("FleetHarness::boot");
    let node = harness.node("node-a").expect("node-a");
    // Past the daemon's startup writes, so its first-write watermark read is spent.
    std::thread::sleep(Duration::from_secs(3));

    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let missed_ts = darkmux_flow::ts_utc_at(now - 30);
    let day_file = node.flows_dir.join(format!("{}.jsonl", darkmux_flow::day_utc_at(now)));
    let mut body = std::fs::read_to_string(&day_file).unwrap_or_default();
    body.push_str(&serde_json::json!({"ts": missed_ts, "action": "operator.note", "handle": "one-shot-missed"}).to_string());
    body.push('\n');
    std::fs::write(&day_file, body).unwrap();
    let state_dir = node.home_dir.join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(state_dir.join("hub-outage.json"), serde_json::json!({"since": missed_ts, "seq": 1}).to_string()).unwrap();

    // The tick runs every 7s; allow several, polling for the real outcome.
    let client = redis::Client::open(node.redis_url.as_str()).unwrap();
    let mut conn = client.get_connection().unwrap();
    let stream = node.redis_stream.as_deref().unwrap_or("darkmux:flow");
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let entries: redis::Value = redis::cmd("XRANGE").arg(stream).arg("-").arg("+").query(&mut conn).unwrap();
        if format!("{entries:?}").contains("one-shot-missed") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the one-shot writer's missed record never reached the hub: the daemon is not running its catch-up tick"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}
