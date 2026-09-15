//! (#2716) The regression test for the case the whole issue is about: a
//! harness process that dies WITHOUT unwinding.
//!
//! A clean shutdown already worked — `FleetHarness::drop` and
//! `FleetNode::drop` have always killed their children. What leaked was
//! every other ending: `Ctrl-C` on `cargo test` (SIGINT terminates; no
//! unwind, no `Drop`), a `SIGKILL`ed runner, an abort. 63 `redis-server`
//! processes on one developer machine, 60 of them orphaned, the oldest 76
//! days old, is what that produced. So the thing under test here is not
//! teardown — it is a hard kill.
//!
//! The shape: this binary re-executes ITSELF with `--exact` on a second
//! test that boots a one-node harness, reports its fixture pids, and then
//! sleeps. The parent verifies those pids are live, `SIGKILL`s the child
//! outright, and asserts that every fixture is gone shortly after. Nothing
//! in the child gets a chance to clean up, which is the point.

#[path = "e2e/mod.rs"]
mod e2e;

use std::io::Write;
use std::time::{Duration, Instant};

use e2e::fixture_reaper::{identify, registry_entries};
use e2e::harness::{FleetHarness, NodeSpec};

/// Names the file the re-executed child writes its fixture pids into.
/// Unset in an ordinary run, which is what makes the child-side test a
/// no-op for everyone but the parent.
const REPORT_ENV: &str = "DARKMUX_E2E_HARD_KILL_REPORT";

/// How long the child holds its harness up before giving in. Purely a
/// backstop so a parent that died before delivering its `SIGKILL` cannot
/// leave a harness running indefinitely — the parent normally kills the
/// child within a couple of seconds of the pids landing.
const CHILD_MAX_LIFETIME: Duration = Duration::from_secs(120);

/// See `e2e_harness_smoke.rs` for the full reasoning behind the
/// `DARKMUX_E2E_REQUIRED` escalation (#1662): a skip is for a contributor
/// without redis, never for the CI job that opted in.
fn redis_available() -> bool {
    let ok = std::process::Command::new("redis-server")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok && std::env::var("DARKMUX_E2E_REQUIRED").is_ok() {
        panic!(
            "redis-server is not on PATH, but DARKMUX_E2E_REQUIRED is set — the job that \
             opted in must never silently skip the fleet e2e suite (#1662)."
        );
    }
    ok
}

/// The CHILD half. A no-op unless the parent re-executed this binary with
/// `REPORT_ENV` set, so an ordinary `cargo test` run sees it return
/// immediately.
#[test]
fn hard_kill_child_harness() {
    let Ok(report_path) = std::env::var(REPORT_ENV) else {
        return;
    };

    let harness = FleetHarness::boot(vec![NodeSpec::new("node-a")])
        .expect("FleetHarness::boot in the hard-kill child");
    let pids = harness.fixture_pids();

    let body = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut f =
        std::fs::File::create(&report_path).expect("creating the hard-kill pid report");
    // The trailing newline is the parent's completeness marker — it must
    // never act on a partially-written line.
    writeln!(f, "{body}").expect("writing the hard-kill pid report");
    f.sync_all().expect("flushing the hard-kill pid report");
    drop(f);

    // Hold the fixtures up and wait to be killed. `harness` is still owned
    // here on purpose: the parent's SIGKILL has to land on a process that
    // WOULD have cleaned up, so that what survives (or does not) is
    // attributable to the die-with-parent guard rather than to a harness
    // that had already let go.
    let deadline = Instant::now() + CHILD_MAX_LIFETIME;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    drop(harness);
}

#[test]
fn fixtures_die_when_the_harness_is_sigkilled() {
    if !redis_available() {
        eprintln!(
            "skipping fixtures_die_when_the_harness_is_sigkilled: redis-server not on PATH"
        );
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir for the pid report");
    let report = tmp.path().join("fixture-pids");

    let exe = std::env::current_exe().expect("current_exe");
    let mut child = std::process::Command::new(&exe)
        .args(["hard_kill_child_harness", "--exact", "--nocapture"])
        .env(REPORT_ENV, &report)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("re-executing this test binary as the hard-kill child");

    // The child pays a `cargo build --release` fingerprint check plus a
    // redis and a daemon boot, and may block on the release-build flock
    // behind a sibling e2e binary.
    let pids = wait_for_report(&report, Duration::from_secs(300))
        .unwrap_or_else(|e| {
            let _ = child.kill();
            let _ = child.wait();
            panic!("hard-kill child never reported its fixture pids: {e}");
        });

    assert!(
        pids.len() >= 2,
        "expected at least a redis and one daemon pid, got {pids:?}"
    );
    for pid in &pids {
        assert!(
            identify(*pid).is_some(),
            "fixture {pid} should be running before the kill — the test proves nothing \
             otherwise"
        );
    }

    // The hard kill. SIGKILL, so the child gets no unwind, no `Drop`, no
    // chance to tell the watchdog to stand down.
    child.kill().expect("SIGKILLing the hard-kill child");
    child.wait().expect("reaping the hard-kill child");

    let deadline = Instant::now() + Duration::from_secs(30);
    let survivors = loop {
        let alive: Vec<u32> = pids
            .iter()
            .copied()
            .filter(|p| identify(*p).is_some())
            .collect();
        if alive.is_empty() || Instant::now() >= deadline {
            break alive;
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    assert!(
        survivors.is_empty(),
        "fixtures {survivors:?} outlived a SIGKILLed harness. That is the exact shape that \
         accumulated 60 orphaned redis-server processes over 76 days: `Drop` does not run on \
         the endings that leak, so the die-with-parent watchdog is the only thing standing \
         between a killed test runner and a permanent orphan (#2716)."
    );
}

/// (#2716) The OTHER half of the guard, wired to the real fixtures. The
/// watchdog handles a killed harness; it cannot handle a killed WATCHDOG,
/// a power cut, or a panic between spawn and teardown. For those the next
/// run's startup sweep is the only recourse, and it can only reap what the
/// registry names — so every process the harness spawns has to actually
/// reach the file. Removing either `fixtures.register(...)` call in
/// `harness.rs` makes this fail.
#[test]
fn every_fixture_reaches_this_run_s_registry_file() {
    if !redis_available() {
        eprintln!("skipping every_fixture_reaches_this_run_s_registry_file: no redis-server");
        return;
    }

    let harness =
        FleetHarness::boot(vec![NodeSpec::new("node-a")]).expect("FleetHarness::boot");
    let body = std::fs::read_to_string(harness.registry_path())
        .expect("the run's registry file must exist while the run is live");

    let recorded: Vec<u32> = body
        .lines()
        .filter(|l| l.starts_with("child\t"))
        .filter_map(|l| l.split('\t').nth(1))
        .filter_map(|p| p.parse::<u32>().ok())
        .collect();

    for pid in harness.fixture_pids() {
        assert!(
            recorded.contains(&pid),
            "fixture {pid} is not in the registry at {}. The startup sweep reaps only what \
             the registry names, so an unregistered fixture is one no future run can ever \
             reach — exactly the state the 60 orphans were in (#2716). Recorded: {recorded:?}",
            harness.registry_path().display()
        );
    }

    assert!(
        body.starts_with("owner\t"),
        "the registry must open with the owning test binary's identity — the sweep skips a \
         file whose owner is still alive, and without that line every concurrently-running \
         sibling e2e binary looks like an orphan (#2716)"
    );

    // (#2716) The inverted case, and the one a synthetic stand-in cannot
    // reach. `redis-server` REWRITES its own process title within a second
    // of exec — `redis-server --port N --save …` at spawn becomes
    // `redis-server 127.0.0.1:N`. An identity rule that compared command
    // lines would therefore accept a `sleep` fixture in a unit test and
    // reject the real redis here, which is a sweep that reaps everything
    // except the thing it was built for. Wait past the rewrite and assert
    // the sweep's OWN rule still matches every entry.
    std::thread::sleep(Duration::from_secs(2));
    for (role, proc) in registry_entries(harness.registry_path()) {
        if role != "child" {
            continue;
        }
        assert!(
            proc.still_running(),
            "registry entry for pid {} no longer matches the live process, so the startup \
             sweep would refuse to reap it if this run were killed. Recorded command was {:?}; \
             the process is still alive and is still ours (#2716).",
            proc.pid,
            proc.command
        );
    }
}

/// Poll for the child's pid report, returning only once the trailing
/// newline proves the line is complete.
fn wait_for_report(path: &std::path::Path, timeout: Duration) -> Result<Vec<u32>, String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(body) = std::fs::read_to_string(path) {
            if body.ends_with('\n') {
                let pids: Result<Vec<u32>, _> = body
                    .trim()
                    .split(',')
                    .map(|p| p.parse::<u32>())
                    .collect();
                return pids.map_err(|e| format!("unparseable pid report {body:?}: {e}"));
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("timed out after {timeout:?}"))
}
