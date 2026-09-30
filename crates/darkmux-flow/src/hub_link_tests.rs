//! The hub link against a real `redis-server` on loopback: a sink that loses
//! the hub, regains it, and what reaches the stream when it does.

use super::*;
use crate::hub_link::Backfill;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const STREAM: &str = "darkmux:flow";

struct Hub {
    child: Option<Child>,
    port: u16,
}

impl Hub {
    fn start() -> Hub {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut hub = Hub { child: None, port };
        hub.up();
        hub
    }

    fn url(&self) -> String {
        format!("redis://127.0.0.1:{}", self.port)
    }

    /// Bring the hub up (empty: no persistence), waiting until it answers.
    fn up(&mut self) {
        #[allow(clippy::zombie_processes)] // `Drop` and `down` kill and wait the child.
        let child = Command::new("redis-server")
            .args(["--port", &self.port.to_string(), "--save", "", "--appendonly", "no"])
            .args(["--bind", "127.0.0.1", "--protected-mode", "no"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("redis-server spawn");
        self.child = Some(child);
        let client = redis::Client::open(self.url()).unwrap();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            if client.get_connection().is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("redis-server did not come ready");
    }

    fn down(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Every entry on the stream, oldest first: `(record json, late marker)`.
    fn entries(&self) -> Vec<(serde_json::Value, bool)> {
        let client = redis::Client::open(self.url()).unwrap();
        let mut conn = client.get_connection().unwrap();
        let reply: redis::streams::StreamRangeReply =
            redis::cmd("XRANGE").arg(STREAM).arg("-").arg("+").query(&mut conn).unwrap();
        reply
            .ids
            .iter()
            .map(|e| {
                let rec: String = e.get("record").expect("record field");
                (serde_json::from_str(&rec).unwrap(), e.map.contains_key(LATE_FIELD))
            })
            .collect()
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.down();
    }
}

fn rec(handle: &str, secs_ago: i64) -> FlowRecord {
    FlowRecord {
        ts: ts_utc_at(current_epoch_secs() - secs_ago),
        level: Level::Info,
        category: Category::Work,
        tier: Tier::Operator,
        stage: Stage::Dispatch,
        action: crate::FlowAction::OperatorNote,
        handle: handle.to_string(),
        phase_id: None,
        session_id: None,
        execution_id: None,
        source: None,
        model: None,
        reasoning: None,
        mission_id: None,
        machine_id: None,
        machine_uid: None,
        prev_hash: None,
        hash: None,
        payload: None,
    }
}

/// What the default tee does for one record: the local day file first, then Redis.
fn write(sink: &RedisSink, dir: &TempDir, r: &FlowRecord) {
    let path = dir.path().join(format!("{}.jsonl", day_utc_at(current_epoch_secs())));
    record_at(r, &path).unwrap();
    sink.write(r).unwrap();
}

fn sink(hub: &Hub, dir: &TempDir, policy: SinkPolicy) -> RedisSink {
    RedisSink::new(&hub.url(), STREAM, Some(10_000))
        .unwrap()
        .with_policy(policy)
        .with_test_recovery(dir.path().to_path_buf(), Duration::ZERO, Duration::ZERO)
}

fn handles(entries: &[(serde_json::Value, bool)]) -> Vec<String> {
    entries.iter().map(|(v, _)| v["handle"].as_str().unwrap().to_string()).collect()
}

/// An outage: `before` reaches the hub, the hub goes away, `during` are
/// written (locally only) until the sink has disabled itself.
fn outage(hub: &mut Hub, dir: &TempDir, s: &RedisSink, before: &FlowRecord, during: &[FlowRecord]) {
    write(s, dir, before);
    hub.down();
    for r in during {
        write(s, dir, r);
    }
    assert!(s.is_disabled(), "the outage records must have tripped the disable threshold");
}

#[test]
fn a_long_lived_sink_recovers_when_the_hub_returns() {
    let dir = TempDir::new().unwrap();
    let mut hub = Hub::start();
    let s = sink(&hub, &dir, SinkPolicy::LongLived);
    let during = [rec("g1", 40), rec("g2", 30), rec("g3", 25)];
    outage(&mut hub, &dir, &s, &rec("b1", 50), &during);
    match s.hub_link() {
        Some(HubLink::Unreachable { since, reason }) => {
            assert_eq!(since, during[0].ts, "unreachable since the first record that failed");
            assert!(!reason.is_empty());
        }
        other => panic!("expected Unreachable, got {other:?}"),
    }
    hub.up();
    write(&s, &dir, &rec("after", 1));
    assert!(!s.is_disabled(), "the hub answered; the sink must re-enable");
    assert_eq!(s.hub_link(), Some(HubLink::Connected));
    assert!(handles(&hub.entries()).contains(&"after".to_string()));
}

#[test]
fn a_one_shot_sink_stays_off_when_the_hub_returns() {
    let dir = TempDir::new().unwrap();
    let mut hub = Hub::start();
    let s = sink(&hub, &dir, SinkPolicy::OneShot);
    outage(&mut hub, &dir, &s, &rec("b1", 50), &[rec("g1", 40), rec("g2", 30), rec("g3", 25)]);
    hub.up();
    write(&s, &dir, &rec("after", 1));
    assert!(s.is_disabled(), "OneShot keeps today's behavior: off for the process");
    assert!(hub.entries().is_empty(), "nothing reaches a hub a OneShot sink gave up on");
}

#[test]
fn the_backfill_publishes_exactly_the_gap_in_order() {
    let dir = TempDir::new().unwrap();
    let mut hub = Hub::start();
    let s = sink(&hub, &dir, SinkPolicy::LongLived);
    // A record older than the outage that only the file holds (a prior process's):
    // it is before the watermark, so it must not be re-sent.
    write_file_only(&dir, &rec("old-file-only", 90));
    let mut foreign = rec("other-machine", 35);
    foreign.machine_uid = Some("NOT-THIS-MACHINE".to_string());
    write_file_only(&dir, &foreign);
    let during = [rec("g1", 40), rec("g2", 30), rec("g3", 25)];
    outage(&mut hub, &dir, &s, &rec("b1", 50), &during);
    // Another local process wrote inside the gap, after g3 in the file but earlier by ts.
    write_file_only(&dir, &rec("mid", 28));
    hub.up(); // a restarted hub holds nothing: whatever arrives is what was re-sent
    write(&s, &dir, &rec("after", 1));
    let got = hub.entries();
    assert_eq!(handles(&got), ["g1", "g2", "mid", "g3", "after"], "exactly the gap, then the live record, in ts order");
    let late: Vec<bool> = got.iter().map(|(_, l)| *l).collect();
    assert_eq!(late, [true, true, true, true, false], "backfilled entries are marked late; the live one is not");
}

/// A record in the day file that no sink wrote (another process's, or a forwarded one).
fn write_file_only(dir: &TempDir, r: &FlowRecord) {
    let path = dir.path().join(format!("{}.jsonl", day_utc_at(current_epoch_secs())));
    record_at(r, &path).unwrap();
}

#[test]
fn a_resent_record_has_the_identity_readers_de_duplicate_on() {
    let dir = TempDir::new().unwrap();
    let hub = Hub::start();
    let s = sink(&hub, &dir, SinkPolicy::LongLived);
    let first = rec("dup", 10);
    write(&s, &dir, &first);
    // The hub holds `dup`. Claim an outage began at its ts: the next delivery re-sends it.
    s.link_state().record_failure(&first.ts, "synthetic".to_string());
    write(&s, &dir, &rec("next", 1));
    let got = hub.entries();
    assert_eq!(handles(&got), ["dup", "dup", "next"], "the backfill re-sent a record the hub already held");
    assert_eq!(
        flow_record_identity(&got[0].0),
        flow_record_identity(&got[1].0),
        "the re-sent copy must collapse with the original under the readers' identity"
    );
    assert_eq!(got.iter().map(|(_, l)| *l).collect::<Vec<_>>(), [false, true, false]);
}

#[test]
fn a_failed_backfill_keeps_the_outage_so_the_next_probe_retries() {
    let dir = TempDir::new().unwrap();
    let mut hub = Hub::start();
    let s = sink(&hub, &dir, SinkPolicy::LongLived);
    outage(&mut hub, &dir, &s, &rec("b1", 50), &[rec("g1", 40), rec("g2", 30), rec("g3", 25)]);
    // Still down: a probe fails, the outage (and its watermark) survive it.
    write(&s, &dir, &rec("g4", 20));
    assert!(s.is_disabled());
    assert!(matches!(s.hub_link(), Some(HubLink::Unreachable { .. })));
    hub.up();
    write(&s, &dir, &rec("after", 1));
    assert_eq!(handles(&hub.entries()), ["g1", "g2", "g3", "g4", "after"]);
}

#[test]
fn the_backfill_reads_two_days_and_keeps_only_the_newest_up_to_the_cap() {
    let dir = TempDir::new().unwrap();
    let now = current_epoch_secs();
    let today = dir.path().join(format!("{}.jsonl", day_utc_at(now)));
    let yesterday = dir.path().join(format!("{}.jsonl", day_utc_at(now - 86_400)));
    let three_days = dir.path().join(format!("{}.jsonl", day_utc_at(now - 3 * 86_400)));
    let at = |secs: i64, h: &str| {
        let mut r = rec(h, 0);
        r.ts = ts_utc_at(secs);
        r
    };
    record_at(&at(now - 86_400, "y"), &yesterday).unwrap();
    record_at(&at(now - 3 * 86_400, "ancient"), &three_days).unwrap();
    for (i, h) in ["a", "b", "c"].iter().enumerate() {
        record_at(&at(now - 30 + i as i64, h), &today).unwrap();
    }
    let since = ts_utc_at(now - 3 * 86_400);
    let q = |cap| Backfill { dir: dir.path(), now_secs: now, since: &since, own_uid: None, skip_identity: "", cap };
    let all: Vec<String> = q(None).lines();
    let names = |ls: &[String]| {
        ls.iter().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["handle"].as_str().unwrap().to_string()).collect::<Vec<_>>()
    };
    assert_eq!(names(&all), ["y", "a", "b", "c"], "yesterday and today only, oldest first; the 3-day-old file is out of scope");
    assert_eq!(names(&q(Some(2)).lines()), ["b", "c"], "a cap keeps the newest");
}
