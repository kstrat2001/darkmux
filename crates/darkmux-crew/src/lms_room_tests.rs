//! (#3506) An LM Studio wave's load against darkmux's engine servers: real
//! processes standing in for the servers (each recorded as darkmux's, as a
//! start records it), a mock LM Studio and a fake machine memory. No real
//! engine, LM Studio or memory reading.

use super::*;
use crate::concurrent_dispatch::ensure_wave_loaded;
use crate::engine_dispatch::refusal_code;
use darkmux_gestalt::mock::{HostOp, MockHost};
use darkmux_gestalt::{FixedEstimator, Placement};
use darkmux_types::engine_record::{self, EngineRecord, ENGINE_RECORD_SCHEMA};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::Duration;

const GIB: u64 = 1 << 30;

/// A process darkmux's records name as one of its engine servers.
struct FakeServer {
    child: Child,
    rec: EngineRecord,
}

impl FakeServer {
    /// A live process, recorded as darkmux's server on `endpoint` serving
    /// `model` (its record matches the process: pid and start stamp).
    fn darkmux(dirs: &Dirs, endpoint: &str, model: &str, started_at_ms: u64) -> Self {
        let mut s = Self::spawn(endpoint, model, started_at_ms);
        engine_record::write_in(&dirs.records, &s.rec).unwrap();
        s.rec = engine_record::read_in(&dirs.records, endpoint).unwrap().unwrap();
        s
    }

    /// A live process with a record that does not match it (another start
    /// stamp): a server darkmux did not start, as contract 4 reads it.
    fn foreign(dirs: &Dirs, endpoint: &str, model: &str) -> Self {
        let mut s = Self::spawn(endpoint, model, 1);
        s.rec.start_stamp ^= 1;
        engine_record::write_in(&dirs.records, &s.rec).unwrap();
        s
    }

    fn spawn(endpoint: &str, model: &str, started_at_ms: u64) -> Self {
        use std::os::unix::process::CommandExt;
        let child = Command::new("sleep").arg("600").process_group(0).spawn().unwrap();
        let pid = child.id();
        let start_stamp = darkmux_platform::current().process_start_stamp(pid).expect("a start stamp");
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let rec = EngineRecord {
            schema: ENGINE_RECORD_SCHEMA,
            endpoint: endpoint.into(),
            engine: "tensorfold".into(),
            model: model.into(),
            n_ctx: 8192,
            port,
            pid,
            start_stamp,
            owner_pid: std::process::id(),
            argv: ["tensorfold-native", "serve", model, "--name", &format!("darkmux:{model}")].map(String::from).to_vec(),
            log: std::path::PathBuf::from("/dev/null"),
            started_at_ms,
            engine_version: "tensorfold-native 1.2.4".into(),
        };
        FakeServer { child, rec }
    }

    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The machine's memory as a function of the test's own state, and the
/// leases other dispatches hold, answered in turn (the last one repeats).
pub(crate) struct FakeMachine<'a> {
    free: &'a (dyn Fn() -> Option<u64> + Sync),
    leased: Mutex<Vec<Vec<String>>>,
    capacity: Option<u64>,
}

impl<'a> FakeMachine<'a> {
    fn new(free: &'a (dyn Fn() -> Option<u64> + Sync)) -> Self {
        FakeMachine { free, leased: Mutex::new(vec![Vec::new()]), capacity: Some(128 * GIB) }
    }
    fn leasing(mut self, answers: &[&[&str]]) -> Self {
        self.leased = Mutex::new(answers.iter().map(|a| a.iter().map(|s| s.to_string()).collect()).collect());
        self
    }
}

impl Machine for FakeMachine<'_> {
    fn reclaimable(&self) -> Option<u64> {
        (self.free)()
    }
    fn footprint(&self, _pid: u32) -> Option<u64> {
        None
    }
    fn lmstudio_residents(&self) -> Vec<String> {
        panic!("the LM Studio path's room step never lists LM Studio for eviction")
    }
    fn lmstudio_unload(&self, identifier: &str) -> anyhow::Result<()> {
        panic!("the LM Studio path's room step never unloads `{identifier}`: the plan does")
    }
    fn leased(&self, own: &LeaseGuard) -> Vec<String> {
        let mut answers = self.leased.lock().unwrap();
        if answers.is_empty() {
            return own.all_live_leased_models();
        }
        if answers.len() > 1 {
            answers.remove(0)
        } else {
            answers[0].clone()
        }
    }
    fn capacity(&self) -> Option<u64> {
        self.capacity
    }
}

struct Rig {
    dir: tempfile::TempDir,
    prev_home: Option<String>,
}

impl Rig {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let prev_home = std::env::var("DARKMUX_HOME").ok();
        // SAFETY: #[serial]; restored on drop.
        unsafe { std::env::set_var("DARKMUX_HOME", dir.path().join("home")) };
        Rig { dir, prev_home }
    }
    fn dirs(&self) -> Dirs {
        Dirs { records: self.dir.path().join("run"), logs: self.dir.path().join("logs") }
    }
    fn room<'a>(&self, machine: &'a (dyn Machine + Sync)) -> ServerRoom<'a> {
        let bounds = server::Bounds { start: Duration::from_secs(5), stop_grace: Duration::from_secs(2), poll: Duration::from_millis(20) };
        ServerRoom { dirs: self.dirs(), bounds, machine }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        unsafe {
            match self.prev_home.take() {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }
}

fn wave(model_key: &str) -> Vec<Placement> {
    vec![Placement { model_key: model_key.into(), identifier: format!("darkmux:{model_key}"), min_ctx: 8_000, seat: "s".into() }]
}

/// Load the wave the way every LM Studio dispatch does, against `room`.
fn load(host: &mut MockHost, wave: &[Placement], room: &ServerRoom<'_>) -> anyhow::Result<()> {
    let lease = LeaseGuard::acquire();
    ensure_wave_loaded(wave, &FixedEstimator::default(), host, &lease, None, room)
}

fn loaded(host: &MockHost) -> bool {
    host.ops.iter().any(|op| matches!(op, HostOp::Load { .. }))
}

/// The first finding (what the LM Studio path did before #3506): an idle
/// server darkmux started holds 56 GiB and 8 GiB is free; a 20 GiB LM Studio
/// load must stop it, then load. Before, the planner never saw the server:
/// it planned the load into the memory the server held and left it running.
#[test]
#[serial_test::serial]
fn an_idle_engine_server_counts_against_an_lm_studio_load_and_is_stopped_to_make_room() {
    let rig = Rig::new();
    let mut tf = FakeServer::darkmux(&rig.dirs(), "tf", "TensorFold/q", 1_000);
    let rec = tf.rec.clone();
    let free = || Some(if server::alive(&rec) { 8 * GIB } else { 64 * GIB });
    let machine = FakeMachine::new(&free);
    let mut host = MockHost::new().cataloged("m", 20 * GIB);
    load(&mut host, &wave("m"), &rig.room(&machine)).expect("the load fits once the idle server stopped");
    assert!(!tf.running(), "the idle server holding the memory was stopped");
    assert!(engine_record::read_in(&rig.dirs().records, "tf").unwrap().is_none(), "and its record removed");
    assert_eq!(
        host.ops,
        [HostOp::ListResident, HostOp::ListCatalog, HostOp::Load { model_key: "m".into(), identifier: "darkmux:m".into(), min_ctx: 8_000 }],
        "the server stopped before anything loaded"
    );
}

/// Least recently started first, and no more than the load needs: two idle
/// servers, one stop makes room, and the older one is the one stopped.
#[test]
#[serial_test::serial]
fn the_least_recently_started_idle_server_is_stopped_first_and_only_as_many_as_needed() {
    let rig = Rig::new();
    let mut old = FakeServer::darkmux(&rig.dirs(), "old", "TensorFold/old", 1_000);
    let mut new = FakeServer::darkmux(&rig.dirs(), "new", "TensorFold/new", 2_000);
    let (old_rec, new_rec) = (old.rec.clone(), new.rec.clone());
    let free = || Some(if server::alive(&old_rec) && server::alive(&new_rec) { 8 * GIB } else { 40 * GIB });
    let machine = FakeMachine::new(&free);
    let mut host = MockHost::new().cataloged("m", 20 * GIB);
    load(&mut host, &wave("m"), &rig.room(&machine)).expect("one stop makes room");
    assert!(!old.running(), "the server started first is stopped first");
    assert!(new.running(), "the newer one is left: one stop was enough");
}

/// A server a running dispatch holds is never stopped, and memory it holds
/// is `busy` (retry later), never a load planned over it. The code reads
/// back off the wave's error; nothing is loaded.
#[test]
#[serial_test::serial]
fn a_server_a_running_dispatch_holds_is_never_stopped_and_the_load_is_busy() {
    let rig = Rig::new();
    let mut tf = FakeServer::darkmux(&rig.dirs(), "tf", "TensorFold/q", 1_000);
    let free = || Some(8 * GIB);
    let machine = FakeMachine::new(&free).leasing(&[&["darkmux:TensorFold/q"]]);
    let mut host = MockHost::new().cataloged("m", 20 * GIB);
    let err = load(&mut host, &wave("m"), &rig.room(&machine)).expect_err("refused");
    assert_eq!(refusal_code(&err), Some("busy"), "{err:#}");
    let text = format!("{err:#}");
    assert!(text.contains("GiB and 8.0 GiB is free after evicting every idle darkmux resident"), "{text}");
    assert!(text.contains("running dispatches hold TensorFold/q (tf), so retry when they finish"), "{text}");
    assert!(tf.running(), "the held server was not stopped");
    assert!(!loaded(&host), "nothing was loaded over the memory it holds: {:?}", host.ops);
}

/// The lease is read again under the endpoint's lock, where the stop is
/// decided: a server a dispatch took up after the list was read is never
/// stopped, and the load is `busy`.
#[test]
#[serial_test::serial]
fn a_server_taken_up_after_the_list_was_read_is_never_stopped() {
    let rig = Rig::new();
    let mut tf = FakeServer::darkmux(&rig.dirs(), "tf", "TensorFold/q", 1_000);
    let free = || Some(8 * GIB);
    // Idle when the list is read; held by the time the stop is decided.
    let machine = FakeMachine::new(&free).leasing(&[&[], &["darkmux:TensorFold/q"]]);
    let mut host = MockHost::new().cataloged("m", 20 * GIB);
    let err = load(&mut host, &wave("m"), &rig.room(&machine)).expect_err("refused");
    assert_eq!(refusal_code(&err), Some("busy"), "{err:#}");
    assert!(tf.running(), "the server held by then was not stopped");
}

/// A server darkmux did not start (its record does not match the live
/// process) is never stopped, however short memory is: darkmux stops its
/// own idle one, and when that is not enough the rest is memory darkmux did
/// not take, `memory_in_use` (retry later), never `wont_fit`: the load fits
/// this machine.
#[test]
#[serial_test::serial]
fn a_server_darkmux_did_not_start_is_never_touched_and_the_load_is_memory_in_use() {
    let rig = Rig::new();
    let mut theirs = FakeServer::foreign(&rig.dirs(), "theirs", "TensorFold/theirs");
    let mut ours = FakeServer::darkmux(&rig.dirs(), "ours", "TensorFold/ours", 1_000);
    let free = || Some(8 * GIB);
    let machine = FakeMachine::new(&free);
    let mut host = MockHost::new().cataloged("m", 20 * GIB);
    let err = load(&mut host, &wave("m"), &rig.room(&machine)).expect_err("refused");
    assert_eq!(refusal_code(&err), Some("memory_in_use"), "{err:#}");
    assert!(format!("{err:#}").contains("memory darkmux did not take holds the rest, so retry when it frees"), "{err:#}");
    assert!(format!("{err:#}").contains("stopped the idle engine server TensorFold/ours (ours)"), "the wave's error names what it stopped: {err:#}");
    assert!(!ours.running(), "darkmux's own idle server was stopped");
    assert!(theirs.running(), "the server darkmux did not start was never touched");
    assert!(!loaded(&host), "{:?}", host.ops);
}

/// Loads larger than the whole machine are `wont_fit` at once: evicting or
/// waiting can never help, so nothing is stopped.
#[test]
#[serial_test::serial]
fn loads_larger_than_the_machine_are_wont_fit_and_stop_nothing() {
    let rig = Rig::new();
    let mut tf = FakeServer::darkmux(&rig.dirs(), "tf", "TensorFold/q", 1_000);
    let free = || Some(8 * GIB);
    let machine = FakeMachine { capacity: Some(64 * GIB), ..FakeMachine::new(&free) };
    let mut host = MockHost::new().cataloged("m", 100 * GIB);
    let err = load(&mut host, &wave("m"), &rig.room(&machine)).expect_err("refused");
    assert_eq!(refusal_code(&err), Some("wont_fit"), "{err:#}");
    assert!(format!("{err:#}").contains("GiB and this machine has 64.0 GiB in all"), "{err:#}");
    assert!(tf.running());
}

/// (#3508) The LM Studio path checks memory with no engine server at all:
/// a load the reclaimable memory cannot hold, with nothing of darkmux's to
/// free, is `memory_in_use`, and nothing is loaded.
#[test]
#[serial_test::serial]
fn an_lm_studio_load_is_checked_against_free_memory_with_no_server_running() {
    let rig = Rig::new();
    let free = || Some(8 * GIB);
    let machine = FakeMachine::new(&free);
    let mut host = MockHost::new().cataloged("m", 20 * GIB);
    let err = load(&mut host, &wave("m"), &rig.room(&machine)).expect_err("refused");
    assert_eq!(refusal_code(&err), Some("memory_in_use"), "{err:#}");
    assert!(!loaded(&host), "{:?}", host.ops);
    // With the memory free, the same load goes ahead.
    let free = || Some(64 * GIB);
    let machine = FakeMachine::new(&free);
    let mut host = MockHost::new().cataloged("m", 20 * GIB);
    load(&mut host, &wave("m"), &rig.room(&machine)).expect("loads");
    assert_eq!(host.ops, [HostOp::ListResident, HostOp::ListCatalog, HostOp::Load { model_key: "m".into(), identifier: "darkmux:m".into(), min_ctx: 8_000 }]);
}

/// (#3508) A load is priced as the ledger prices a resident: the weights
/// plus the KV cache at the context and a margin, never the weights alone.
/// 20 GiB of weights with 20.5 GiB free does not fit at 8000 tokens.
#[test]
#[serial_test::serial]
fn a_load_is_priced_with_its_kv_cache_not_its_weights_alone() {
    let rig = Rig::new();
    let free = || Some(20 * GIB + GIB / 2);
    let machine = FakeMachine::new(&free);
    let mut host = MockHost::new().cataloged("m", 20 * GIB);
    let err = load(&mut host, &wave("m"), &rig.room(&machine)).expect_err("the weights fit, the load does not");
    assert_eq!(refusal_code(&err), Some("memory_in_use"), "{err:#}");
}

/// (#3510) A load larger than the machine is `wont_fit` even when free
/// memory cannot be read: capacity is checked first, so an unreadable figure
/// never lets it go ahead.
#[test]
#[serial_test::serial]
fn capacity_is_checked_before_free_memory() {
    let rig = Rig::new();
    let free = || None;
    let machine = FakeMachine { capacity: Some(16 * GIB), ..FakeMachine::new(&free) };
    let mut host = MockHost::new().cataloged("m", 64 * GIB);
    let err = load(&mut host, &wave("m"), &rig.room(&machine)).expect_err("refused");
    assert_eq!(refusal_code(&err), Some("wont_fit"), "{err:#}");
}

/// What the plan's own unloads free counts first: an idle LM Studio load
/// the wave unloads makes the room, and the idle server is left running.
#[test]
#[serial_test::serial]
fn the_plans_own_unloads_count_before_any_server_is_stopped() {
    let rig = Rig::new();
    let mut tf = FakeServer::darkmux(&rig.dirs(), "tf", "TensorFold/q", 1_000);
    let free = || Some(8 * GIB);
    let machine = FakeMachine::new(&free);
    let mut host = MockHost::new().resident("darkmux:orphan", "orphan", 8_000, Some(16 * GIB)).cataloged("m", 20 * GIB);
    load(&mut host, &wave("m"), &rig.room(&machine)).expect("8 GiB free plus the orphan's 16 GiB holds 20 GiB");
    assert!(tf.running(), "no server stopped: the plan's own unload made room");
    assert!(host.ops.contains(&HostOp::Unload { identifier: "darkmux:orphan".into() }), "{:?}", host.ops);
    assert!(loaded(&host));
}

/// An LM Studio load another dispatch holds (pinned, so the plan keeps it)
/// is memory that frees later: with every idle server stopped and the load
/// still short, the answer is `busy` naming it, not `wont_fit`.
#[test]
#[serial_test::serial]
fn a_held_lm_studio_load_makes_a_short_wave_busy() {
    let rig = Rig::new();
    let mut tf = FakeServer::darkmux(&rig.dirs(), "tf", "TensorFold/q", 1_000);
    let free = || Some(8 * GIB);
    let machine = FakeMachine::new(&free);
    let mut host = MockHost::new().resident("darkmux:busy", "busy", 8_000, Some(30 * GIB)).cataloged("m", 20 * GIB);
    // Another process's dispatch holds `darkmux:busy`.
    let other = LeaseGuard::acquire();
    other.write(&["darkmux:busy".to_string()]).unwrap();
    let err = load(&mut host, &wave("m"), &rig.room(&machine)).expect_err("refused");
    drop(other);
    assert_eq!(refusal_code(&err), Some("busy"), "{err:#}");
    assert!(format!("{err:#}").contains("running dispatches hold darkmux:busy"), "{err:#}");
    assert!(!tf.running(), "the idle server was stopped first");
    assert!(!host.ops.contains(&HostOp::Unload { identifier: "darkmux:busy".into() }), "the held load was kept: {:?}", host.ops);
}

/// Contract 2 at the wave's seam: a wave whose load is refused runs none of
/// its jobs, and each comes back as an error carrying the code (the step's
/// `step.error`, and the run's terminal, are written from it). This uses
/// the production room: this machine's own memory, which no test machine
/// has a terabyte of, and the records under the test's `DARKMUX_HOME`.
#[test]
#[serial_test::serial]
fn a_refused_wave_runs_none_of_its_jobs_and_each_ends_in_an_error() {
    use crate::concurrent_dispatch::{run_bounded, QueuedJob};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    let rig = Rig::new();
    let home_dirs = Dirs::under_home();
    assert!(home_dirs.records.starts_with(rig.dir.path()), "the records are the test's own: {:?}", home_dirs.records);
    let mut tf = FakeServer::darkmux(&home_dirs, "tf", "TensorFold/q", 1_000);
    let ran = Arc::new(AtomicU32::new(0));
    let job = |i: usize| -> crate::concurrent_dispatch::DispatchJob<usize> {
        let ran = ran.clone();
        Box::new(move || {
            ran.fetch_add(1, Ordering::SeqCst);
            Ok((i, vec![]))
        })
    };
    let seat = |s: &str| crate::step_kinds::SeatClaim::LocalModel(Placement { seat: s.into(), ..wave("huge")[0].clone() });
    let jobs = vec![QueuedJob { index: 0, seat: seat("a"), job: job(0) }, QueuedJob { index: 1, seat: seat("b"), job: job(1) }];
    let factory = || -> Box<dyn ModelHost> { Box::new(MockHost::new().cataloged("huge", 1 << 40)) };
    let room = ServerRoom::this_machine();
    let results = run_bounded(jobs, &Facts::default(), &FixedEstimator::default(), 4, &factory, &room).expect("planned");
    assert_eq!(ran.load(Ordering::SeqCst), 0, "no job of the refused wave ran");
    assert_eq!(results.len(), 2, "every job has its terminal result");
    for (_, r) in &results {
        let e = r.as_ref().err().expect("an error");
        assert!(format!("{e:#}").contains("[wont_fit]"), "the code reaches the step's error: {e:#}");
    }
    assert!(tf.running(), "a load larger than the machine stops nothing");
}

/// A wave that loads nothing (its model is already resident) asks nothing
/// of the budget: the servers stay, and LM Studio's catalog is not asked
/// (pricing it would cost every reused dispatch an `lms ls`).
#[test]
#[serial_test::serial]
fn a_wave_that_loads_nothing_stops_nothing() {
    let rig = Rig::new();
    let mut tf = FakeServer::darkmux(&rig.dirs(), "tf", "TensorFold/q", 1_000);
    let free = || Some(GIB);
    let machine = FakeMachine::new(&free);
    let mut host = MockHost::new().resident("darkmux:m", "m", 8_000, Some(20 * GIB)).cataloged("m", 20 * GIB);
    load(&mut host, &wave("m"), &rig.room(&machine)).expect("reused");
    assert!(tf.running());
    assert_eq!(host.ops, [HostOp::ListResident], "the resident is reused: nothing else is asked");
}

/// A held LM Studio load this wave itself uses stays whoever finishes
/// first, so it never makes a short wave `busy`.
#[test]
#[serial_test::serial]
fn a_held_load_the_wave_itself_uses_is_not_memory_that_frees() {
    let rig = Rig::new();
    let _tf = FakeServer::darkmux(&rig.dirs(), "tf", "TensorFold/q", 1_000);
    let free = || Some(8 * GIB);
    let machine = FakeMachine::new(&free);
    let mut host = MockHost::new()
        .resident("darkmux:shared", "shared", 8_000, Some(30 * GIB))
        .cataloged("shared", 30 * GIB)
        .cataloged("m", 20 * GIB);
    let other = LeaseGuard::acquire();
    other.write(&["darkmux:shared".to_string()]).unwrap();
    let mut both = wave("shared");
    both.extend(wave("m"));
    let err = load(&mut host, &both, &rig.room(&machine)).expect_err("refused");
    drop(other);
    assert_eq!(refusal_code(&err), Some("memory_in_use"), "{err:#}");
}

/// (#3508) A mission's waves are packed against the machine: three 20 GiB
/// models (about 21.5 GiB each with their KV cache) with 50 GiB reclaimable
/// run as two waves, not one wave of three
/// (which every launcher planned before, with no pool and no estimate).
/// An idle darkmux load the first wave's Exclusive pass unloads counts as
/// room, and sibling seats on one model share its wave.
#[test]
#[serial_test::serial]
fn a_missions_waves_are_packed_against_the_machines_memory() {
    use crate::concurrent_dispatch::schedule_waves;
    let rig = Rig::new();
    let free = || Some(50 * GIB);
    let machine = FakeMachine::new(&free);
    let room = rig.room(&machine);
    let factory = || -> Box<dyn ModelHost> {
        Box::new(MockHost::new().cataloged("a", 20 * GIB).cataloged("b", 20 * GIB).cataloged("c", 20 * GIB))
    };
    let three: Vec<Placement> = ["a", "b", "c"].iter().flat_map(|m| wave(m)).collect();
    let schedule = schedule_waves(&three, &Facts::default(), &FixedEstimator::default(), &factory, &room).unwrap();
    assert_eq!(schedule.waves.len(), 2, "{schedule:?}");
    // The same three with an idle 30 GiB darkmux load to free: one wave.
    let with_idle = || -> Box<dyn ModelHost> {
        Box::new(
            MockHost::new()
                .resident("darkmux:idle", "idle", 8_000, Some(30 * GIB))
                .cataloged("a", 20 * GIB)
                .cataloged("b", 20 * GIB)
                .cataloged("c", 20 * GIB),
        )
    };
    let schedule = schedule_waves(&three, &Facts::default(), &FixedEstimator::default(), &with_idle, &room).unwrap();
    assert_eq!(schedule.waves.len(), 1, "{schedule:?}");
    // Sibling seats on `a` ride together.
    let siblings: Vec<Placement> = ["x", "y", "z"].iter().map(|s| Placement { seat: s.to_string(), ..wave("a")[0].clone() }).collect();
    let schedule = schedule_waves(&siblings, &Facts::default(), &FixedEstimator::default(), &factory, &room).unwrap();
    assert_eq!(schedule.waves.len(), 1, "{schedule:?}");
    // A caller's own pool is planned on as given.
    let unknown = ServerRoom::unknown_machine();
    let schedule = schedule_waves(&three, &Facts::default(), &FixedEstimator::default(), &factory, &unknown).unwrap();
    assert_eq!(schedule.waves.len(), 1, "no machine figures, no constraint: {schedule:?}");
}

/// (#3507) A container dispatch on LM Studio holds its job model and its
/// compactor for as long as it runs, so another dispatch's Exclusive pass
/// (or an engine server's start) never unloads them under it. A lab run
/// calls `dispatch()` directly and has no wave lease of its own: this is
/// what protects it. Only darkmux's own loads are leased.
#[test]
#[serial_test::serial]
fn a_running_dispatch_holds_its_job_model_and_its_compactor() {
    let _rig = Rig::new();
    let lab_run = LeaseGuard::acquire();
    hold_dispatch_models(&lab_run, Some("darkmux:job"), Some("darkmux:qwen/qwen3-4b-2507")).unwrap();
    hold_dispatch_models(&LeaseGuard::acquire(), Some("user-loaded"), None).unwrap();
    let mut host = MockHost::new()
        .resident("darkmux:job", "job", 8_000, Some(GIB))
        .resident("darkmux:qwen/qwen3-4b-2507", "qwen/qwen3-4b-2507", 8_000, Some(GIB))
        .resident("darkmux:idle", "idle", 8_000, Some(GIB))
        .cataloged("other", GIB);
    let free = || Some(64 * GIB);
    let machine = FakeMachine::new(&free).leasing(&[]);
    let rig = Rig::new();
    load(&mut host, &wave("other"), &rig.room(&machine)).expect("loads");
    let unloaded: Vec<_> = host.ops.iter().filter_map(|op| match op { HostOp::Unload { identifier } => Some(identifier.as_str()), _ => None }).collect();
    assert_eq!(unloaded, ["darkmux:idle"], "the running dispatch's job model and compactor stay");
    drop(lab_run);
}

/// (#3507) The wiring: the container path leases its LM Studio job model
/// once it is resident, and its compactor once that is.
#[test]
fn the_container_path_takes_the_dispatch_lease() {
    let src = include_str!("dispatch_internal.rs");
    let body = |f: &str| {
        let start = src.find(&format!("fn {f}(")).unwrap_or_else(|| panic!("{f}"));
        &src[start..start + src[start..].find("\n}\n").unwrap()]
    };
    let prepare = body("prepare_container_run");
    assert!(prepare.find("resolve_container_model(").unwrap() < prepare.find("hold_dispatch_models(").unwrap());
    let launch = body("launch_container");
    assert!(launch.find("prepare_compactor(").unwrap() < launch.find("hold_dispatch_models(").unwrap());
}
