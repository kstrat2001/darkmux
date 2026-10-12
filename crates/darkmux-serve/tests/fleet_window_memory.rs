//! (#3217) What a `/runs`-shaped read of a 10,000-entry fleet stream costs in
//! memory and time: the bytes the daemon keeps between requests, the peak a
//! request allocates, and the request's latency, cold, warm, after the cache
//! window, and after new entries arrive.
//!
//! In-process, against a `redis-server` this test spawns on a free port (never
//! the fleet hub), through the public entry points every caller uses
//! (`fleet_records_for_runs`, `build_runs_with_usage`). Memory is measured by a
//! counting global allocator: live bytes and the high-water mark, in this
//! binary only. Skips when `redis-server` is not on PATH.
//!
//! `the_fleet_window_stays_inside_its_memory_budget` is the budget check the
//! issue asks for. `fleet_window_cost_report` prints the full table (it sleeps
//! past the 2 s cache window twice, so it is `#[ignore]`d):
//!
//! ```text
//! cargo nextest run -p darkmux-serve --test fleet_window_memory --run-ignored all --no-capture
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[path = "support/fleet_fixture.rs"]
mod fleet_fixture;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

const MB: f64 = 1024.0 * 1024.0;

struct Redis {
    child: std::process::Child,
    url: String,
}

impl Drop for Redis {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_redis() -> Option<Redis> {
    let available = std::process::Command::new("redis-server")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !available {
        eprintln!("skipping: redis-server not on PATH");
        return None;
    }
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    #[allow(clippy::zombie_processes)] // `Redis`'s Drop kills and waits.
    let child = std::process::Command::new("redis-server")
        .args([
            "--port",
            &port.to_string(),
            "--save",
            "",
            "--appendonly",
            "no",
            "--bind",
            "127.0.0.1",
            "--protected-mode",
            "no",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("redis-server spawn");
    let mut redis = Redis {
        child,
        url: format!("redis://127.0.0.1:{port}"),
    };
    darkmux_flow::test_redis::wait_until_redis_answers(
        &mut redis.child,
        port,
        darkmux_flow::test_redis::REDIS_READY_TIMEOUT,
    )
    .unwrap_or_else(|why| panic!("{why}"));
    Some(redis)
}

fn xadd_all(url: &str, records: &[serde_json::Value]) {
    let mut conn = redis::Client::open(url).unwrap().get_connection().unwrap();
    for chunk in records.chunks(500) {
        let mut pipe = redis::pipe();
        for r in chunk {
            pipe.cmd("XADD")
                .arg("darkmux:flow")
                .arg("*")
                .arg("schema")
                .arg("1.8.0")
                .arg("record")
                .arg(r.to_string())
                .ignore();
        }
        let () = pipe.query(&mut conn).unwrap();
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// One `/runs`-shaped request, as `runs_handler` makes it: the fleet read,
/// then the union over it. Returns (latency, peak bytes above the bytes live
/// before it, bytes still live after it, rows).
fn request(flows: &std::path::Path) -> (Duration, usize, usize, usize) {
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let started = Instant::now();
    let rows = {
        let fleet = darkmux_serve::fleet_records_for_runs();
        assert_eq!(
            fleet.state,
            darkmux_serve::source_state::SourceState::Ok,
            "the fleet read must succeed"
        );
        darkmux_serve::build_runs_with_usage(flows, None, &fleet.records, None)
            .runs
            .len()
    };
    let elapsed = started.elapsed();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(before);
    let after = LIVE.load(Ordering::Relaxed);
    (elapsed, peak, after.saturating_sub(before), rows)
}

/// Sets up the fixture: `(redis, isolation guard, flows dir, record text bytes)`.
fn fixture(
    n: usize,
) -> Option<(
    Redis,
    darkmux_types::test_isolation::IsolatedState,
    tempfile::TempDir,
    usize,
)> {
    let isolated = darkmux_types::test_isolation::IsolatedState::new();
    let redis = spawn_redis()?;
    let records = fleet_fixture::realistic_records(n, now_secs());
    let text: usize = records.iter().map(|r| r.to_string().len()).sum();
    xadd_all(&redis.url, &records);
    // After the isolation guard, which clears it.
    unsafe { std::env::set_var("DARKMUX_REDIS_URL", &redis.url) };
    Some((redis, isolated, tempfile::tempdir().unwrap(), text))
}

/// The budget, measured against the form the window had before #3217 (the
/// same records parsed whole into `serde_json::Value`, in this process): a
/// cold `/runs` request keeps at most three quarters of that, and a repeated
/// request inside the cache window allocates at most a tenth of it at its
/// peak (before, each request cloned the whole window out of the cache).
#[test]
fn the_fleet_window_stays_inside_its_memory_budget() {
    let Some((_redis, _isolated, flows, text)) = fixture(10_000) else {
        return;
    };
    let whole = {
        let records = fleet_fixture::realistic_records(10_000, now_secs());
        let lines: Vec<String> = records.iter().map(|r| r.to_string()).collect();
        drop(records);
        let before = LIVE.load(Ordering::Relaxed);
        let parsed: Vec<serde_json::Value> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let bytes = LIVE.load(Ordering::Relaxed) - before;
        drop(parsed);
        bytes
    };
    let (_, cold_peak, kept, rows) = request(flows.path());
    let (_, warm_peak, _, _) = request(flows.path());
    assert!(rows > 50, "the fixture builds real rows: {rows}");
    eprintln!(
        "text {:.1} MB, whole-value window {:.1} MB, kept {:.1} MB, cold peak {:.1} MB, warm peak {:.1} MB",
        text as f64 / MB,
        whole as f64 / MB,
        kept as f64 / MB,
        cold_peak as f64 / MB,
        warm_peak as f64 / MB
    );
    assert!(
        (kept as f64) < 0.75 * whole as f64,
        "kept {kept} bytes; the whole-value window is {whole}"
    );
    assert!(
        (warm_peak as f64) < 0.1 * whole as f64,
        "a repeated request peaked at {warm_peak} bytes; the whole-value window is {whole}"
    );
}

#[test]
#[ignore = "measurement: sleeps past the cache window; run with --run-ignored all --no-capture"]
fn fleet_window_cost_report() {
    let Some((redis, _isolated, flows, text)) = fixture(10_000) else {
        return;
    };
    eprintln!(
        "fixture: 10,000 entries, {:.1} MB of record text",
        text as f64 / MB
    );
    let report = |label: &str, (elapsed, peak, kept, rows): (Duration, usize, usize, usize)| {
        eprintln!(
            "{label:<34} {:>8.1} ms   peak +{:>6.1} MB   kept +{:>6.1} MB   rows {rows}",
            elapsed.as_secs_f64() * 1000.0,
            peak as f64 / MB,
            kept as f64 / MB
        );
    };
    report("cold (first request)", request(flows.path()));
    report("warm (inside the 2 s window)", request(flows.path()));
    std::thread::sleep(Duration::from_millis(2_100));
    report("repeat after the window", request(flows.path()));
    xadd_all(&redis.url, &fleet_fixture::realistic_records(5, now_secs()));
    std::thread::sleep(Duration::from_millis(2_100));
    report("after 5 new entries", request(flows.path()));
}
