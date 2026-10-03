//! The hub link against a fake hub on loopback: a sink that loses the hub,
//! regains it, and what reaches the stream when it does.
//!
//! The hub is an in-process stand-in that speaks the slice of Redis the sink
//! uses (the connection-setup `CLIENT SETINFO` pair, `PING`, `XADD`) and keeps
//! what it is sent in memory. It is not a `redis-server`: these are unit tests,
//! and the one CI job that installs a `redis-server` runs `tests/e2e_*` binaries
//! only, so a unit test that needed the binary would not run in the build job.
//! Going down closes the listener and every open connection, so the sink sees a
//! refused connect, as against a dead host; coming back up starts EMPTY, as a
//! restarted hub with no persistence does.

use super::*;
use crate::hub_link::Backfill;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tempfile::TempDir;

const STREAM: &str = "darkmux:flow";

/// One `XADD` the hub accepted: the stream it went to, its `MAXLEN ~` cap if
/// it carried one, its `record` field, and whether it carried `late`.
struct Stored {
    stream: String,
    maxlen: Option<usize>,
    record: String,
    late: bool,
}

struct Hub {
    port: u16,
    stored: Arc<Mutex<Vec<Stored>>>,
    conns: Arc<Mutex<Vec<TcpStream>>>,
    stop: Arc<AtomicBool>,
    /// While set, every `XADD` is answered with an error (a hub that accepts
    /// the connection and refuses the write).
    reject_xadd: Arc<AtomicBool>,
    acceptor: Option<JoinHandle<()>>,
}

impl Hub {
    fn start() -> Hub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        let mut hub = Hub {
            port,
            stored: Arc::new(Mutex::new(Vec::new())),
            conns: Arc::new(Mutex::new(Vec::new())),
            stop: Arc::new(AtomicBool::new(false)),
            reject_xadd: Arc::new(AtomicBool::new(false)),
            acceptor: None,
        };
        hub.serve(listener);
        hub
    }

    fn url(&self) -> String {
        format!("redis://127.0.0.1:{}", self.port)
    }

    /// Bring the hub back on its port. It holds nothing: whatever it is sent
    /// from here on is all it has.
    fn up(&mut self) {
        let listener = TcpListener::bind(("127.0.0.1", self.port)).expect("rebind the hub's port");
        self.serve(listener);
    }

    fn serve(&mut self, listener: TcpListener) {
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        self.stored.lock().unwrap().clear();
        self.stop = Arc::new(AtomicBool::new(false));
        let (stop, stored, conns, reject) = (
            Arc::clone(&self.stop),
            Arc::clone(&self.stored),
            Arc::clone(&self.conns),
            Arc::clone(&self.reject_xadd),
        );
        self.acceptor = Some(std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // An accepted socket can inherit the listener's non-blocking mode.
                        let _ = stream.set_nonblocking(false);
                        if let Ok(kept) = stream.try_clone() {
                            conns.lock().unwrap().push(kept);
                        }
                        let stored = Arc::clone(&stored);
                        let reject = Arc::clone(&reject);
                        std::thread::spawn(move || serve_connection(stream, &stored, &reject));
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
            // `listener` drops here: the port stops accepting.
        }));
    }

    /// Go away: stop accepting, join the acceptor (so the port is released
    /// before any `up`), and cut every open connection.
    fn down(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(acceptor) = self.acceptor.take() {
            let _ = acceptor.join();
        }
        for conn in self.conns.lock().unwrap().drain(..) {
            let _ = conn.shutdown(Shutdown::Both);
        }
    }

    /// Every entry on the work stream, oldest first: `(record json, late marker)`.
    fn entries(&self) -> Vec<(serde_json::Value, bool)> {
        self.entries_on(STREAM)
    }

    /// Every entry on `stream`, oldest first.
    fn entries_on(&self, stream: &str) -> Vec<(serde_json::Value, bool)> {
        self.stored
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.stream == stream)
            .map(|e| (serde_json::from_str(&e.record).unwrap(), e.late))
            .collect()
    }

    /// The `MAXLEN ~` cap each `XADD` to `stream` carried.
    fn maxlens_on(&self, stream: &str) -> Vec<Option<usize>> {
        self.stored.lock().unwrap().iter().filter(|e| e.stream == stream).map(|e| e.maxlen).collect()
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.down();
    }
}

/// One Redis command as RESP: `*N\r\n` then N bulk strings `$len\r\n<bytes>\r\n`.
fn read_command(r: &mut impl BufRead) -> Option<Vec<String>> {
    let mut line = String::new();
    if r.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let n: usize = line.trim_end().strip_prefix('*')?.parse().ok()?;
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        line.clear();
        r.read_line(&mut line).ok()?;
        let len: usize = line.trim_end().strip_prefix('$')?.parse().ok()?;
        let mut buf = vec![0u8; len + 2];
        r.read_exact(&mut buf).ok()?;
        buf.truncate(len);
        args.push(String::from_utf8_lossy(&buf).into_owned());
    }
    Some(args)
}

/// `XADD key [MAXLEN ~ n] * field value ...`: keep the `record` field and
/// whether `late` was set. The MAXLEN cap is recorded, not applied.
fn xadd_fields(args: &[String]) -> Option<Stored> {
    let mut i = 2;
    let mut maxlen = None;
    if args.get(i)?.eq_ignore_ascii_case("MAXLEN") {
        i += 1;
        if matches!(args.get(i)?.as_str(), "~" | "=") {
            i += 1;
        }
        maxlen = args.get(i)?.parse().ok();
        i += 1; // the count
    }
    i += 1; // the id (`*`)
    let (mut record, mut late) = (None, false);
    for pair in args.get(i..)?.chunks(2) {
        match pair {
            [k, v] if k == "record" => record = Some(v.clone()),
            [k, _] if k == LATE_FIELD => late = true,
            _ => {}
        }
    }
    Some(Stored { stream: args.get(1)?.clone(), maxlen, record: record?, late })
}

fn serve_connection(stream: TcpStream, stored: &Mutex<Vec<Stored>>, reject_xadd: &AtomicBool) {
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    while let Some(args) = read_command(&mut reader) {
        let reply = match args.first().map(|c| c.to_ascii_uppercase()).as_deref() {
            Some("CLIENT") => "+OK\r\n".to_string(),
            Some("PING") => "+PONG\r\n".to_string(),
            Some("XADD") if reject_xadd.load(Ordering::SeqCst) => "-ERR hub refuses writes\r\n".to_string(),
            Some("XADD") => match xadd_fields(&args) {
                Some(entry) => {
                    let mut stored = stored.lock().unwrap();
                    stored.push(entry);
                    let id = format!("{}-0", stored.len());
                    format!("${}\r\n{id}\r\n", id.len())
                }
                None => "-ERR malformed XADD\r\n".to_string(),
            },
            _ => "-ERR unknown command\r\n".to_string(),
        };
        if out.write_all(reply.as_bytes()).is_err() {
            return;
        }
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
    let path = dir
        .path()
        .join(format!("{}.jsonl", day_utc_at(current_epoch_secs())));
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
    entries
        .iter()
        .map(|(v, _)| v["handle"].as_str().unwrap().to_string())
        .collect()
}

/// An outage: `before` reaches the hub, the hub goes away, `during` are
/// written (locally only) until the sink has disabled itself.
fn outage(hub: &mut Hub, dir: &TempDir, s: &RedisSink, before: &FlowRecord, during: &[FlowRecord]) {
    write(s, dir, before);
    hub.down();
    for r in during {
        write(s, dir, r);
    }
    assert!(
        s.is_disabled(),
        "the outage records must have tripped the disable threshold"
    );
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
            assert_eq!(
                since, during[0].ts,
                "unreachable since the first record that failed"
            );
            assert!(!reason.is_empty());
        }
        other => panic!("expected Unreachable, got {other:?}"),
    }
    hub.up();
    write(&s, &dir, &rec("after", 1));
    assert!(
        !s.is_disabled(),
        "the hub answered; the sink must re-enable"
    );
    assert_eq!(s.hub_link(), Some(HubLink::Connected));
    assert!(handles(&hub.entries()).contains(&"after".to_string()));
}

#[test]
fn a_one_shot_sink_stays_off_when_the_hub_returns() {
    let dir = TempDir::new().unwrap();
    let mut hub = Hub::start();
    let s = sink(&hub, &dir, SinkPolicy::OneShot);
    outage(
        &mut hub,
        &dir,
        &s,
        &rec("b1", 50),
        &[rec("g1", 40), rec("g2", 30), rec("g3", 25)],
    );
    hub.up();
    write(&s, &dir, &rec("after", 1));
    assert!(
        s.is_disabled(),
        "OneShot keeps today's behavior: off for the process"
    );
    assert!(
        hub.entries().is_empty(),
        "nothing reaches a hub a OneShot sink gave up on"
    );
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
    assert_eq!(
        handles(&got),
        ["g1", "g2", "mid", "g3", "after"],
        "exactly the gap, then the live record, in ts order"
    );
    let late: Vec<bool> = got.iter().map(|(_, l)| *l).collect();
    assert_eq!(
        late,
        [true, true, true, true, false],
        "backfilled entries are marked late; the live one is not"
    );
}

/// A record in the day file that no sink wrote (another process's, or a forwarded one).
fn write_file_only(dir: &TempDir, r: &FlowRecord) {
    let path = dir
        .path()
        .join(format!("{}.jsonl", day_utc_at(current_epoch_secs())));
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
    s.link_state()
        .record_failure(&first.ts, "synthetic".to_string());
    write(&s, &dir, &rec("next", 1));
    let got = hub.entries();
    assert_eq!(
        handles(&got),
        ["dup", "dup", "next"],
        "the backfill re-sent a record the hub already held"
    );
    assert_eq!(
        flow_record_identity(&got[0].0),
        flow_record_identity(&got[1].0),
        "the re-sent copy must collapse with the original under the readers' identity"
    );
    assert_eq!(
        got.iter().map(|(_, l)| *l).collect::<Vec<_>>(),
        [false, true, false]
    );
}

#[test]
fn a_failed_backfill_keeps_the_outage_so_the_next_probe_retries() {
    let dir = TempDir::new().unwrap();
    let mut hub = Hub::start();
    let s = sink(&hub, &dir, SinkPolicy::LongLived);
    outage(
        &mut hub,
        &dir,
        &s,
        &rec("b1", 50),
        &[rec("g1", 40), rec("g2", 30), rec("g3", 25)],
    );
    // Still down: a probe fails, the outage (and its watermark) survive it.
    write(&s, &dir, &rec("g4", 20));
    assert!(s.is_disabled());
    assert!(matches!(s.hub_link(), Some(HubLink::Unreachable { .. })));
    hub.up();
    write(&s, &dir, &rec("after", 1));
    assert_eq!(handles(&hub.entries()), ["g1", "g2", "g3", "g4", "after"]);
}

fn since_a_day_ago(now: i64) -> String {
    ts_utc_at(now - 86_400)
}

#[test]
fn the_backfill_reads_every_day_from_the_outage_start_and_keeps_only_the_newest_up_to_the_cap() {
    let dir = TempDir::new().unwrap();
    let now = current_epoch_secs();
    let today = dir.path().join(format!("{}.jsonl", day_utc_at(now)));
    let yesterday = dir
        .path()
        .join(format!("{}.jsonl", day_utc_at(now - 86_400)));
    let three_days = dir
        .path()
        .join(format!("{}.jsonl", day_utc_at(now - 3 * 86_400)));
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
    let q = |cap| Backfill {
        dir: dir.path(),
        now_secs: now,
        since: &since,
        own_uid: None,
        skip_identity: "",
        cap,
    };
    let all: Vec<String> = q(None).lines();
    let names = |ls: &[String]| {
        ls.iter()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["handle"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(&all),
        ["ancient", "y", "a", "b", "c"],
        "every day file from the outage start (#3073), oldest first"
    );
    let recent = since_a_day_ago(now);
    let narrow = Backfill { dir: dir.path(), now_secs: now, since: &recent, own_uid: None, skip_identity: "", cap: None }.lines();
    assert_eq!(names(&narrow), ["y", "a", "b", "c"], "a day file wholly before the outage start is not read");
    assert_eq!(
        names(&q(Some(2)).lines()),
        ["b", "c"],
        "a cap keeps the newest"
    );
}

/// (#3073) Only the day files from the outage start's day through today are
/// opened: a multi-day outage reaches back, an older file stays unread.
#[test]
fn the_backfill_opens_day_files_from_the_outage_start_day_through_today() {
    let dir = TempDir::new().unwrap();
    let now = current_epoch_secs();
    let days: Vec<String> = (0..5).map(|n| day_utc_at(now - n * 86_400)).collect();
    for d in &days {
        std::fs::write(dir.path().join(format!("{d}.jsonl")), "").unwrap();
    }
    std::fs::write(dir.path().join("notes.txt"), "").unwrap();
    let since = ts_utc_at(now - 2 * 86_400);
    let files = Backfill { dir: dir.path(), now_secs: now, since: &since, own_uid: None, skip_identity: "", cap: None }.day_files();
    let names: Vec<String> = files.iter().map(|p| p.file_stem().unwrap().to_string_lossy().to_string()).collect();
    assert_eq!(names, [days[2].clone(), days[1].clone(), days[0].clone()]);
}

/// (#3074, contract 3) A lab run's records are in the local day file but
/// never re-sent to the hub.
#[test]
fn backfill_skips_lab_session_records() {
    use darkmux_types::session_id::{RunId, SessionId};
    let dir = TempDir::new().unwrap();
    let now = current_epoch_secs();
    let today = dir.path().join(format!("{}.jsonl", day_utc_at(now)));
    let line = |handle: &str, session: String| {
        serde_json::json!({"ts": ts_utc_at(now - 10), "action": "dispatch.tool", "handle": handle, "session_id": session}).to_string()
    };
    let lab = SessionId::adhoc(RunId::lab("l-1").unwrap(), "coder", "n").wire();
    let solo = SessionId::adhoc(RunId::standalone("s-1").unwrap(), "coder", "n").wire();
    std::fs::write(&today, [line("lab", lab), line("solo", solo)].join("\n")).unwrap();
    let since = ts_utc_at(now - 60);
    let lines = Backfill { dir: dir.path(), now_secs: now, since: &since, own_uid: None, skip_identity: "", cap: None }.lines();
    let handles: Vec<String> =
        lines.iter().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["handle"].as_str().unwrap().to_string()).collect();
    assert_eq!(handles, ["solo"]);
}

/// (#2101) Backfill re-sends the local day file's records, but never the
/// heartbeats: they are local-only liveness detail and would flush the stream.
#[test]
fn backfill_skips_heartbeats_and_still_sends_the_rest() {
    let dir = TempDir::new().unwrap();
    let now = current_epoch_secs();
    let today = dir.path().join(format!("{}.jsonl", day_utc_at(now)));
    // Written raw: the point is what the filter does with a day file's lines.
    let line = |handle: &str, action: &str| {
        serde_json::json!({"ts": ts_utc_at(now - 10), "action": action, "handle": handle}).to_string()
    };
    let body = [line("beat", "dispatch.turn.heartbeat"), line("tool", "dispatch.tool"), line("turn", "dispatch.turn")].join("\n");
    std::fs::write(&today, body).unwrap();
    let since = ts_utc_at(now - 60);
    let lines = Backfill { dir: dir.path(), now_secs: now, since: &since, own_uid: None, skip_identity: "", cap: None }.lines();
    let handles: Vec<String> =
        lines.iter().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["handle"].as_str().unwrap().to_string()).collect();
    assert_eq!(handles, ["tool", "turn"], "heartbeat skipped, the rest sent: {handles:?}");
}

/// (#3035) A link state or health value a newer darkmux wrote reads as
/// `Unknown`, so the status that carries it still parses.
#[test]
fn a_state_from_a_newer_darkmux_reads_as_unknown() {
    let link: HubLink = serde_json::from_str(r#"{"state":"from_the_future","detail":1}"#).unwrap();
    assert_eq!(link, HubLink::Unknown);
    let health: crate::status::HealthState = serde_json::from_str("\"from_the_future\"").unwrap();
    assert_eq!(health, crate::status::HealthState::Unknown);
    let known: HubLink = serde_json::from_str(r#"{"state":"connected"}"#).unwrap();
    assert_eq!(known, HubLink::Connected);
}

fn watermark_file(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join("hub-outage.json")
}

/// A daemon that restarts mid-outage must still owe the hub what the previous
/// process failed to send.
#[test]
fn a_restarted_sink_backfills_the_outage_its_predecessor_saw() {
    let dir = TempDir::new().unwrap();
    let mut hub = Hub::start();
    let a = sink(&hub, &dir, SinkPolicy::LongLived);
    outage(&mut hub, &dir, &a, &rec("b1", 50), &[rec("g1", 40), rec("g2", 30), rec("g3", 25)]);
    drop(a);
    assert!(hub_link::OutageWatermark::new(watermark_file(&dir)).load().is_some(), "the outage must be on disk before the process goes");
    hub.up();
    let b = sink(&hub, &dir, SinkPolicy::LongLived);
    write(&b, &dir, &rec("after", 1));
    assert_eq!(handles(&hub.entries()), ["g1", "g2", "g3", "after"]);
    assert!(hub_link::OutageWatermark::new(watermark_file(&dir)).load().is_none(), "a landed backfill clears the watermark");
}

/// Records a one-shot CLI wrote during an outage predate the daemon's own
/// first failure; the daemon must still re-send them.
#[test]
fn a_one_shot_writers_outage_records_are_backfilled_by_the_daemon() {
    let dir = TempDir::new().unwrap();
    let mut hub = Hub::start();
    let daemon = sink(&hub, &dir, SinkPolicy::LongLived);
    write(&daemon, &dir, &rec("b1", 60));
    hub.down();
    let cli = sink(&hub, &dir, SinkPolicy::OneShot);
    for r in [rec("cli1", 50), rec("cli2", 45), rec("cli3", 40)] {
        write(&cli, &dir, &r);
    }
    assert!(cli.is_disabled());
    for r in [rec("d1", 30), rec("d2", 25), rec("d3", 20)] {
        write(&daemon, &dir, &r);
    }
    hub.up();
    write(&daemon, &dir, &rec("after", 1));
    assert_eq!(
        handles(&hub.entries()),
        ["cli1", "cli2", "cli3", "d1", "d2", "d3", "after"],
        "the daemon's own outage began at d1, but the CLI's failures started earlier"
    );
}

/// A healthy daemon must pick up an outage a one-shot writer recorded, on its
/// periodic tick, without waiting for a restart or an outage of its own.
#[test]
fn a_healthy_daemons_tick_backfills_a_one_shot_writers_watermark() {
    let dir = TempDir::new().unwrap();
    let hub = Hub::start();
    let daemon = sink(&hub, &dir, SinkPolicy::LongLived);
    write(&daemon, &dir, &rec("b1", 60));
    daemon.tick();
    assert_eq!(handles(&hub.entries()), ["b1"], "a tick with no watermark sends nothing");
    // A one-shot CLI wrote while the hub was unreachable from where it ran.
    let missed = rec("cli1", 30);
    write_file_only(&dir, &missed);
    hub_link::OutageWatermark::new(watermark_file(&dir)).record(&missed.ts).unwrap();
    daemon.tick();
    assert_eq!(handles(&hub.entries()), ["b1", "cli1"], "the tick backfills what the CLI missed");
    assert!(hub_link::OutageWatermark::new(watermark_file(&dir)).load().is_none(), "a landed backfill clears the watermark");
    daemon.tick();
    assert_eq!(hub.entries().len(), 2, "a tick after the clear sends nothing");
}

/// The real call site: the catch-up thread the daemon spawns drives the tick,
/// so a healthy daemon backfills a one-shot writer's watermark with no
/// manual `tick()` and no write of its own.
#[test]
fn the_catch_up_thread_backfills_a_watermark_without_a_manual_tick() {
    let dir = TempDir::new().unwrap();
    let hub = Hub::start();
    let daemon = Arc::new(sink(&hub, &dir, SinkPolicy::LongLived));
    write(&daemon, &dir, &rec("b1", 60));
    let stop = Arc::new(AtomicBool::new(false));
    let handle = hub_link::spawn_tick_thread(daemon.clone(), Duration::from_millis(20), stop.clone());
    let missed = rec("cli1", 30);
    write_file_only(&dir, &missed);
    hub_link::OutageWatermark::new(watermark_file(&dir)).record(&missed.ts).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while handles(&hub.entries()) != ["b1", "cli1"] && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    stop.store(true, Ordering::Release);
    handle.join().unwrap();
    assert_eq!(handles(&hub.entries()), ["b1", "cli1"]);
}

/// (#3075) The backfill rides a connection of its own: a writer stuck holding
/// the live connection (a silent hub) does not hold the backfill up.
#[test]
fn the_backfill_does_not_wait_for_the_live_connection() {
    let dir = TempDir::new().unwrap();
    let hub = Hub::start();
    let daemon = sink(&hub, &dir, SinkPolicy::LongLived);
    let missed = rec("cli1", 30);
    write_file_only(&dir, &missed);
    hub_link::OutageWatermark::new(watermark_file(&dir)).record(&missed.ts).unwrap();
    let _held = daemon.conn.lock().unwrap();
    daemon.tick();
    assert_eq!(handles(&hub.entries()), ["cli1"], "the tick backfilled while the live connection was held");
}

/// (#3075) A writer that wins the connection after the sink disabled itself
/// does not touch the hub.
#[test]
fn a_disabled_sink_skips_the_hub_after_taking_the_connection() {
    let dir = TempDir::new().unwrap();
    let hub = Hub::start();
    let s = sink(&hub, &dir, SinkPolicy::OneShot);
    s.disabled.store(true, Ordering::Release);
    let delivery = s.deliver(&rec("late", 1), crate::HubStream::Work).unwrap();
    assert!(matches!(delivery, Delivery::Skipped), "got {delivery:?}");
    assert!(hub.entries().is_empty());
}

/// A backfill the hub refuses must leave the watermark for the next recovery.
#[test]
fn a_failed_backfill_keeps_the_persisted_watermark() {
    let dir = TempDir::new().unwrap();
    let mut hub = Hub::start();
    let s = sink(&hub, &dir, SinkPolicy::LongLived);
    outage(&mut hub, &dir, &s, &rec("b1", 50), &[rec("g1", 40), rec("g2", 30), rec("g3", 25)]);
    let owed = hub_link::OutageWatermark::new(watermark_file(&dir)).load().unwrap();
    hub.up();
    hub.reject_xadd.store(true, Ordering::SeqCst);
    write(&s, &dir, &rec("g4", 20));
    assert!(hub.entries().is_empty());
    let w = hub_link::OutageWatermark::new(watermark_file(&dir));
    let kept = w.load().unwrap();
    let before = owed;
    assert_eq!(kept.since, before.since, "the watermark keeps the earliest unsent ts");
    hub.reject_xadd.store(false, Ordering::SeqCst);
    write(&s, &dir, &rec("after", 1));
    assert_eq!(handles(&hub.entries()), ["g1", "g2", "g3", "g4", "after"]);
    assert!(hub_link::OutageWatermark::new(watermark_file(&dir)).load().is_none());
}

/// A failure recorded while a backfill runs is not cleared by it.
#[test]
fn clearing_a_stale_generation_keeps_a_newer_watermark() {
    let dir = TempDir::new().unwrap();
    let w = hub_link::OutageWatermark::new(watermark_file(&dir));
    w.record("2026-01-01T00:00:10Z").unwrap();
    let read = w.load().unwrap();
    w.record("2026-01-01T00:00:20Z").unwrap();
    w.clear_if(read.seq).unwrap();
    let kept = w.load().expect("a record landed after the read; the watermark stays");
    assert_eq!(kept.since, "2026-01-01T00:00:10Z", "earliest ts wins");
    w.clear_if(kept.seq).unwrap();
    assert!(w.load().is_none());
}

/// The generation must survive a clear: a backfill that read an old
/// generation, run after another backfill cleared and a CLI recorded a new
/// outage, must not erase that newer outage.
#[test]
fn a_stale_clear_after_a_clear_and_a_new_record_keeps_the_newer_outage() {
    let dir = TempDir::new().unwrap();
    let w = hub_link::OutageWatermark::new(watermark_file(&dir));
    w.record("2026-01-01T00:00:10Z").unwrap();
    let tick_read = w.load().unwrap();
    let write_read = w.load().unwrap();
    w.clear_if(write_read.seq).unwrap();
    w.record("2026-01-01T00:05:00Z").unwrap();
    w.clear_if(tick_read.seq).unwrap();
    let kept = w.load().expect("the newer outage must survive the stale clear");
    assert_eq!(kept.since, "2026-01-01T00:05:00Z");
}

const TELEMETRY_STREAM: &str = "darkmux:flow:telemetry";

fn telemetry_rec(handle: &str, secs_ago: i64) -> FlowRecord {
    let mut r = rec(handle, secs_ago);
    r.action = crate::FlowAction::MachineTelemetry;
    r
}

/// (#2101) `machine.telemetry` is most of the hub's records by volume, so it
/// rides its own stream with its own cap; every other record stays on the
/// work stream under that stream's cap.
#[test]
fn machine_telemetry_rides_its_own_stream_with_its_own_cap() {
    let dir = TempDir::new().unwrap();
    let hub = Hub::start();
    let s = sink(&hub, &dir, SinkPolicy::LongLived).with_telemetry(TELEMETRY_STREAM, Some(500));
    write(&s, &dir, &rec("work", 5));
    write(&s, &dir, &telemetry_rec("sample", 4));
    assert_eq!(handles(&hub.entries_on(STREAM)), ["work"], "telemetry must not touch the work stream");
    assert_eq!(handles(&hub.entries_on(TELEMETRY_STREAM)), ["sample"]);
    assert_eq!(hub.maxlens_on(STREAM), [Some(10_000)]);
    assert_eq!(hub.maxlens_on(TELEMETRY_STREAM), [Some(500)]);
}

/// Telemetry is live state, so a backfill after an outage does not re-send it
/// to either stream: the next sample supersedes the missed ones.
#[test]
fn the_backfill_never_resends_telemetry() {
    let dir = TempDir::new().unwrap();
    let mut hub = Hub::start();
    let s = sink(&hub, &dir, SinkPolicy::LongLived);
    let during = [rec("g1", 40), telemetry_rec("t1", 35), rec("g2", 30), rec("g3", 25)];
    outage(&mut hub, &dir, &s, &rec("b1", 50), &during);
    hub.up();
    write(&s, &dir, &rec("after", 1));
    assert_eq!(handles(&hub.entries_on(STREAM)), ["g1", "g2", "g3", "after"]);
    assert!(hub.entries_on(TELEMETRY_STREAM).is_empty(), "a missed sample is not backfilled");
}

/// The generation must not restart at 1 when the state file is gone or
/// unreadable: a backfill that read generation N must never see a newer
/// outage reuse N.
#[test]
fn a_lost_state_file_does_not_restart_the_generation() {
    let dir = TempDir::new().unwrap();
    let w = hub_link::OutageWatermark::new(watermark_file(&dir));
    w.record("2026-01-01T00:00:10Z").unwrap();
    let first = w.load().unwrap().seq;
    std::fs::remove_file(watermark_file(&dir)).unwrap();
    w.record("2026-01-01T00:00:20Z").unwrap();
    let after_missing = w.load().unwrap().seq;
    assert!(after_missing > first, "generation {after_missing} after a missing file must be past {first}");
    std::fs::write(watermark_file(&dir), "{ not json").unwrap();
    w.record("2026-01-01T00:00:30Z").unwrap();
    let after_corrupt = w.load().unwrap().seq;
    assert!(after_corrupt > after_missing, "generation {after_corrupt} after a corrupt file must be past {after_missing}");
}
