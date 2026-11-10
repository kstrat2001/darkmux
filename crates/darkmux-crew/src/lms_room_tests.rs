//! (#3506) An LM Studio wave's load against darkmux's engine servers: real
//! processes standing in for the servers (each recorded as darkmux's, as a
//! start records it), a mock LM Studio and a fake machine memory. No real
//! engine, LM Studio or memory reading.

use super::*;
use crate::concurrent_dispatch::ensure_wave_loaded;
use crate::engine_dispatch::refusal_code;
use darkmux_gestalt::mock::{HostOp, MockHost};
use darkmux_gestalt::{CatalogFact, FixedEstimator, Placement, ResidentFact};
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
        // A load's reservation is kept, as it is while memory pages in, until
        // its lease drops.
        ServerRoom { dirs: self.dirs(), bounds, machine, settle: |res, need, _| { res.set(need); }, workers: || Some(0) }
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
/// The weights' formats a test host reports, as the readers would name
/// them.
#[derive(Clone, Copy)]
pub(crate) enum Format {
    /// A GGUF header was read: the KV cache is paid at load.
    Gguf,
    /// A `config.json` was read (MLX): the cache grows as the context fills.
    Mlx,
    /// Neither: the size-based estimate.
    Unreadable,
}

/// A mock LM Studio that also reports each model's architecture.
pub(crate) struct ArchHost {
    pub(crate) inner: MockHost,
    format: Format,
    arch: darkmux_gestalt::ArchFacts,
    /// Answers as LM Studio would, that its loads take this machine's
    /// memory (so a wave takes the residency lock for them).
    on_machine: bool,
}

/// A dense model's KV per token: 48 layers x 8 KV heads x 128 x 2 (K, V)
/// x 2 bytes, 192 KiB a token.
const DENSE: darkmux_gestalt::ArchFacts =
    darkmux_gestalt::ArchFacts { total_layers: 48, full_attention_layers: 48, kv_heads: 8, head_dim: 128, kv_bytes_per_element: 2 };

impl ArchHost {
    pub(crate) fn new(inner: MockHost, format: Format) -> Self {
        ArchHost { inner, format, arch: DENSE, on_machine: false }
    }

    /// As LM Studio: its loads take this machine's memory.
    fn on_this_machine_too(mut self) -> Self {
        self.on_machine = true;
        self
    }
}

impl ModelHost for ArchHost {
    fn list_resident(&mut self) -> Result<Vec<ResidentFact>, darkmux_gestalt::HostError> {
        self.inner.list_resident()
    }
    fn list_catalog(&mut self) -> Result<Vec<CatalogFact>, darkmux_gestalt::HostError> {
        self.inner.list_catalog()
    }
    fn load(&mut self, k: &str, id: &str, ctx: u32, d: darkmux_gestalt::Deadline) -> Result<darkmux_gestalt::LoadReport, darkmux_gestalt::HostError> {
        self.inner.load(k, id, ctx, d)
    }
    fn unload(&mut self, t: &darkmux_gestalt::OwnedTarget, d: darkmux_gestalt::Deadline) -> Result<(), darkmux_gestalt::HostError> {
        self.inner.unload(t, d)
    }
    fn on_this_machine(&self) -> bool {
        self.on_machine
    }
    fn arch_facts(&mut self, keys: &[&str]) -> darkmux_gestalt::ModelArch {
        let mut arch = darkmux_gestalt::ModelArch::default();
        if matches!(self.format, Format::Unreadable) {
            return arch;
        }
        for k in keys {
            arch.facts.insert(k.to_string(), self.arch);
            if matches!(self.format, Format::Gguf) {
                arch.kv_at_load.insert(k.to_string());
            }
        }
        arch
    }
}

/// Load the wave the way every LM Studio dispatch does, against `room`,
/// its models GGUF: their whole cache is committed at load.
fn load(host: &mut MockHost, wave: &[Placement], room: &ServerRoom<'_>) -> anyhow::Result<()> {
    load_as(host, Format::Gguf, wave, room)
}

fn load_as(host: &mut MockHost, format: Format, wave: &[Placement], room: &ServerRoom<'_>) -> anyhow::Result<()> {
    let lease = LeaseGuard::acquire();
    let mut arch = ArchHost::new(std::mem::take(host), format);
    let result = ensure_wave_loaded(wave, &FixedEstimator::default(), &mut arch, &lease, None, room);
    *host = arch.inner;
    result
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
    assert!(text.contains("running dispatches hold TensorFold/q (tf); retry when they finish"), "{text}");
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
    assert!(format!("{err:#}").contains("programs darkmux did not start hold the rest, so retry once they free it"), "{err:#}");
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

/// Contract 2 at the wave's seam, and #3514's CI failure made
/// deterministic: a map step's 5 GB model on a machine with 1 GiB free (a
/// small CI runner). The wave is refused `memory_in_use`, none of its jobs
/// runs, and each comes back as an error carrying the code, from which the
/// step's `step.error` and the run's terminal are written. Larger than the
/// machine, the same wave is `wont_fit`.
#[test]
#[serial_test::serial]
fn a_refused_wave_runs_none_of_its_jobs_and_each_ends_in_an_error() {
    use crate::concurrent_dispatch::{run_bounded, QueuedJob};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    let rig = Rig::new();
    for (capacity, code) in [(128 * GIB, "memory_in_use"), (4 * GIB, "wont_fit")] {
        let ran = Arc::new(AtomicU32::new(0));
        let job = |i: usize| -> crate::concurrent_dispatch::DispatchJob<usize> {
            let ran = ran.clone();
            Box::new(move || {
                ran.fetch_add(1, Ordering::SeqCst);
                Ok((i, vec![]))
            })
        };
        let seat = |s: &str| crate::step_kinds::SeatClaim::LocalModel(Placement { seat: s.into(), ..wave("map-model")[0].clone() });
        let jobs = vec![QueuedJob { index: 0, seat: seat("a"), job: job(0) }, QueuedJob { index: 1, seat: seat("b"), job: job(1) }];
        let factory = || -> Box<dyn ModelHost> { Box::new(MockHost::new()) };
        let free = || Some(GIB);
        let machine = FakeMachine { capacity: Some(capacity), ..FakeMachine::new(&free) };
        let est = FixedEstimator(std::collections::BTreeMap::from([("map-model".to_string(), 5_000_000_000)]));
        let results = run_bounded(jobs, &Facts::default(), &est, 4, &factory, Some(&rig.room(&machine))).expect("planned");
        assert_eq!(ran.load(Ordering::SeqCst), 0, "no job of the refused wave ran");
        assert_eq!(results.len(), 2, "every job has its terminal result");
        for (_, r) in &results {
            let e = r.as_ref().expect_err("an error");
            assert_eq!(refusal_code(e), Some(code), "{e:#}");
            assert!(format!("{e:#}").contains(&format!("[{code}]")), "the code reaches the step's error text: {e:#}");
        }
    }
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
    let schedule = schedule_waves(&three, &Facts::default(), &FixedEstimator::default(), &factory, Some(&room)).unwrap();
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
    let schedule = schedule_waves(&three, &Facts::default(), &FixedEstimator::default(), &with_idle, Some(&room)).unwrap();
    assert_eq!(schedule.waves.len(), 1, "{schedule:?}");
    // Sibling seats on `a` ride together.
    let siblings: Vec<Placement> = ["x", "y", "z"].iter().map(|s| Placement { seat: s.to_string(), ..wave("a")[0].clone() }).collect();
    let schedule = schedule_waves(&siblings, &Facts::default(), &FixedEstimator::default(), &factory, Some(&room)).unwrap();
    assert_eq!(schedule.waves.len(), 1, "{schedule:?}");
    // A caller's own pool is planned on as given.
    let unknown = ServerRoom::without_machine();
    let schedule = schedule_waves(&three, &Facts::default(), &FixedEstimator::default(), &factory, Some(&unknown)).unwrap();
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
    let user = LeaseGuard::acquire();
    hold_dispatch_models(&user, Some("user-loaded"), None).unwrap();
    assert!(!LeaseGuard::acquire().all_live_leased_models().contains(&"user-loaded".to_string()), "a load darkmux did not make is not leased");
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
    assert!(prepare.find("resolve_container_model(").unwrap() < prepare.find("lease_lmstudio_job(").unwrap());
    assert!(body("lease_lmstudio_job").contains("hold_dispatch_models("));
    let launch = body("launch_container");
    assert!(launch.find("prepare_compactor(").unwrap() < launch.find("hold_dispatch_models(").unwrap());
}

/// (#3508) The planner plans against the machine's memory too: a copy of a
/// model a user loaded that darkmux's own copy cannot sit beside is refused
/// naming the user's instance, `memory_in_use`.
#[test]
#[serial_test::serial]
fn the_planner_names_a_user_load_whose_memory_darkmux_cannot_free() {
    let rig = Rig::new();
    let free = || Some(8 * GIB);
    let machine = FakeMachine::new(&free);
    let mut host = MockHost::new().resident("m", "m", 8_000, Some(20 * GIB)).cataloged("m", 20 * GIB);
    let err = load(&mut host, &wave("m"), &rig.room(&machine)).expect_err("refused");
    assert_eq!(refusal_code(&err), Some("memory_in_use"), "{err:#}");
    assert!(format!("{err:#}").contains("user-loaded instance \"m\""), "the planner's pool arm names it: {err:#}");
}

/// (#3514 B1) The review's probe, on a 128 GiB machine: a load is refused
/// only when what it takes at the moment it is made exceeds what darkmux can
/// free. A model's potential (its whole context filled) drives eviction and
/// earns a warning, never a refusal; an unreadable architecture is never a
/// refusal at all.
#[test]
#[serial_test::serial]
fn a_long_context_load_is_refused_only_on_what_it_commits_at_load() {
    let rig = Rig::new();
    let at = |model: &str, ctx: u32| vec![Placement { min_ctx: ctx, ..wave(model)[0].clone() }];
    // (format, weights GiB, context, free GiB, refused with)
    let rows: [(Format, u64, u32, u64, Option<&str>); 6] = [
        // A 15 GiB dense MLX model at 200k: its cache grows as it fills (36.6
        // GiB at 200k), so with 40 GiB free it loads.
        (Format::Mlx, 15, 200_000, 40, None),
        // The same model as GGUF commits that cache at load: 52 GiB does not
        // fit 40 GiB free, which is a real shortfall.
        (Format::Gguf, 15, 200_000, 40, Some("memory_in_use")),
        // As GGUF with 64 GiB free it fits.
        (Format::Gguf, 15, 200_000, 64, None),
        // 48 GiB of weights at 262k with the architecture unreadable: the
        // size-based guess exceeds the machine, and is never a refusal.
        (Format::Unreadable, 48, 262_144, 100, None),
        // A 2.5 GiB 4B at 120k with 24 GiB free, unreadable or MLX: loads.
        (Format::Unreadable, 2, 120_000, 24, None),
        (Format::Mlx, 2, 120_000, 24, None),
    ];
    for (format, weights, ctx, free_gib, refused) in rows {
        let free = move || Some(free_gib * GIB);
        let machine = FakeMachine::new(&free);
        let mut host = MockHost::new().cataloged("m", weights * GIB);
        let result = load_as(&mut host, format, &at("m", ctx), &rig.room(&machine));
        match refused {
            None => {
                result.unwrap_or_else(|e| panic!("{weights} GiB at {ctx} with {free_gib} GiB free loads: {e:#}"));
                assert!(loaded(&host));
            }
            Some(code) => {
                let err = result.expect_err("refused");
                assert_eq!(refusal_code(&err), Some(code), "{err:#}");
                assert!(!loaded(&host));
            }
        }
    }
}

/// (#3514 B1) Only a commitment larger than the whole machine is
/// `wont_fit`: an MLX model whose potential is larger than the machine
/// loads (its cache may never fill), a GGUF one whose commitment is does not.
#[test]
#[serial_test::serial]
fn wont_fit_rests_on_the_commitment_alone() {
    let rig = Rig::new();
    let free = || Some(120 * GIB);
    let machine = FakeMachine::new(&free);
    let big = vec![Placement { min_ctx: 600_000, ..wave("m")[0].clone() }];
    let mut host = MockHost::new().cataloged("m", 20 * GIB);
    load_as(&mut host, Format::Mlx, &big, &rig.room(&machine)).expect("its potential (130 GiB) is past the machine, its commitment is not");
    let mut host = MockHost::new().cataloged("m", 20 * GIB);
    let err = load_as(&mut host, Format::Gguf, &big, &rig.room(&machine)).expect_err("a GGUF commits it at load");
    assert_eq!(refusal_code(&err), Some("wont_fit"), "{err:#}");
}

/// (#3514 B1) The potential drives eviction: an MLX load whose commitment
/// already fits still stops darkmux's idle server to reach its potential.
#[test]
#[serial_test::serial]
fn idle_servers_are_stopped_to_reach_the_potential() {
    let rig = Rig::new();
    let mut tf = FakeServer::darkmux(&rig.dirs(), "tf", "TensorFold/q", 1_000);
    let rec = tf.rec.clone();
    let free = || Some(if server::alive(&rec) { 30 * GIB } else { 90 * GIB });
    let machine = FakeMachine::new(&free);
    let mut host = MockHost::new().cataloged("m", 15 * GIB);
    let long = vec![Placement { min_ctx: 200_000, ..wave("m")[0].clone() }];
    load_as(&mut host, Format::Mlx, &long, &rig.room(&machine)).expect("loads");
    assert!(!tf.running(), "stopped to make room for the cache the load may grow into");
}

/// (#3514 B2) A resident a placement reuses is not credited to the wave
/// pool: the packer already prices the reuse at nothing. Resident `a` (40
/// GiB) and `b`, `c` to load (40 GiB each) with 50 GiB free is two waves,
/// not one the room step would then refuse.
#[test]
#[serial_test::serial]
fn a_resident_the_wave_reuses_is_not_credited_to_its_pool() {
    use crate::concurrent_dispatch::schedule_waves;
    let rig = Rig::new();
    let free = || Some(50 * GIB);
    let machine = FakeMachine::new(&free);
    let room = rig.room(&machine);
    let est = FixedEstimator(["a", "b", "c"].iter().map(|k| (k.to_string(), 40 * GIB)).collect());
    let factory = || -> Box<dyn ModelHost> { Box::new(MockHost::new().resident("darkmux:a", "a", 8_000, Some(40 * GIB))) };
    let abc: Vec<Placement> = ["a", "b", "c"].iter().flat_map(|m| wave(m)).collect();
    let schedule = schedule_waves(&abc, &Facts::default(), &est, &factory, Some(&room)).unwrap();
    assert_eq!(schedule.waves, vec![abc[..2].to_vec(), vec![abc[2].clone()]], "{schedule:?}");
}

/// (#3514 B1) The planner refuses on the commitment too: an MLX model
/// loaded beside a user's copy of it, whose commitment fits but whose
/// potential would not, loads.
#[test]
#[serial_test::serial]
fn the_planner_refuses_a_load_beside_a_user_copy_only_on_its_commitment() {
    let rig = Rig::new();
    let free = || Some(40 * GIB);
    let machine = FakeMachine::new(&free);
    let long = vec![Placement { min_ctx: 200_000, ..wave("m")[0].clone() }];
    let mut host = MockHost::new().resident("m", "m", 8_000, Some(15 * GIB)).cataloged("m", 15 * GIB);
    load_as(&mut host, Format::Mlx, &long, &rig.room(&machine)).expect("15.7 GiB at load fits 40 GiB");
    assert!(loaded(&host));
}

/// (#3514 B1) Waves pack on the potential but refuse only on the
/// commitment: the same load beside a user's copy is scheduled, not refused.
#[test]
#[serial_test::serial]
fn a_wave_is_refused_only_on_the_commitment() {
    use crate::concurrent_dispatch::schedule_waves;
    let rig = Rig::new();
    let free = || Some(40 * GIB);
    let machine = FakeMachine::new(&free);
    let room = rig.room(&machine);
    let factory = || -> Box<dyn ModelHost> {
        Box::new(ArchHost::new(MockHost::new().resident("m", "m", 8_000, Some(15 * GIB)).cataloged("m", 15 * GIB), Format::Mlx))
    };
    let long = vec![Placement { min_ctx: 200_000, ..wave("m")[0].clone() }];
    let schedule = schedule_waves(&long, &Facts::default(), &FixedEstimator::default(), &factory, Some(&room)).unwrap();
    assert!(schedule.refusals.is_empty(), "{schedule:?}");
    assert_eq!(schedule.waves, vec![long]);
}

/// Whether another thread can take the machine-wide residency lock within
/// a short wait (the lock is per open file, so a thread of this process
/// excludes as another process would).
fn residency_lock_is_free() -> bool {
    darkmux_types::residency_lock::try_acquire().unwrap().is_some()
}

/// A mock LM Studio that notes, at each unload and load, whether the
/// residency lock is free, and what other dispatches reserve.
struct LockProbe {
    inner: ArchHost,
    at_unload: Vec<bool>,
    at_load: Vec<(bool, u64)>,
}

impl ModelHost for LockProbe {
    fn list_resident(&mut self) -> Result<Vec<ResidentFact>, darkmux_gestalt::HostError> {
        self.inner.list_resident()
    }
    fn list_catalog(&mut self) -> Result<Vec<CatalogFact>, darkmux_gestalt::HostError> {
        self.inner.list_catalog()
    }
    fn load(&mut self, k: &str, id: &str, ctx: u32, d: darkmux_gestalt::Deadline) -> Result<darkmux_gestalt::LoadReport, darkmux_gestalt::HostError> {
        self.at_load.push((residency_lock_is_free(), LeaseGuard::acquire().reserved_by_others()));
        self.inner.load(k, id, ctx, d)
    }
    fn unload(&mut self, t: &darkmux_gestalt::OwnedTarget, d: darkmux_gestalt::Deadline) -> Result<(), darkmux_gestalt::HostError> {
        self.at_unload.push(residency_lock_is_free());
        self.inner.unload(t, d)
    }
    fn arch_facts(&mut self, keys: &[&str]) -> darkmux_gestalt::ModelArch {
        self.inner.arch_facts(keys)
    }
    fn on_this_machine(&self) -> bool {
        self.inner.on_this_machine()
    }
}

/// (#3509) An LM Studio wave decides, evicts and unloads under the
/// machine-wide residency lock, reserves what its load commits, and lets
/// the lock go before the load begins; the reservation outlives the load.
#[test]
#[serial_test::serial]
fn a_wave_decides_under_the_residency_lock_and_loads_outside_it_with_a_reservation() {
    let rig = Rig::new();
    let free = || Some(64 * GIB);
    let machine = FakeMachine::new(&free);
    let inner = ArchHost::new(MockHost::new().resident("darkmux:orphan", "orphan", 8_000, Some(GIB)).cataloged("m", 20 * GIB), Format::Gguf)
        .on_this_machine_too();
    let mut host = LockProbe { inner, at_unload: Vec::new(), at_load: Vec::new() };
    let lease = LeaseGuard::acquire();
    ensure_wave_loaded(&wave("m"), &FixedEstimator::default(), &mut host, &lease, None, &rig.room(&machine)).expect("loads");
    assert_eq!(host.at_unload, [false], "the orphan is unloaded under the lock");
    let (free_at_load, reserved_at_load) = host.at_load[0];
    assert!(free_at_load, "the lock is let go before the load begins");
    assert!(reserved_at_load > 20 * GIB, "the load's commitment is reserved while it loads: {reserved_at_load}");
    assert_eq!(LeaseGuard::acquire().reserved_by_others(), reserved_at_load, "and kept past the load, while its memory pages in");
    drop(lease);
    assert_eq!(LeaseGuard::acquire().reserved_by_others(), 0, "until the dispatch's lease goes");
}

/// (#3509) A wave that does not load ends its reservation.
#[test]
#[serial_test::serial]
fn a_wave_that_fails_to_load_reserves_nothing_after() {
    let rig = Rig::new();
    let free = || Some(64 * GIB);
    let machine = FakeMachine::new(&free);
    let mut inner = MockHost::new().cataloged("m", 20 * GIB);
    inner.fail_next_load = Some(darkmux_gestalt::HostError::CommandFailed { detail: "no".into() });
    let mut host = ArchHost::new(inner, Format::Gguf);
    let lease = LeaseGuard::acquire();
    ensure_wave_loaded(&wave("m"), &FixedEstimator::default(), &mut host, &lease, None, &rig.room(&machine)).expect_err("fails");
    assert_eq!(LeaseGuard::acquire().reserved_by_others(), 0);
}

/// (#3509) What starts still in flight reserve counts against the load:
/// another dispatch's 50 GiB start leaves 14 GiB of 64 GiB for this one, so
/// a 21 GiB load is `busy`, naming the reservation.
#[test]
#[serial_test::serial]
fn another_dispatchs_starting_reservation_counts_against_the_load() {
    let rig = Rig::new();
    let free = || Some(64 * GIB);
    let machine = FakeMachine::new(&free);
    let other = LeaseGuard::acquire();
    other.write_starting(&["darkmux:TensorFold/big".to_string()], 50 * GIB).unwrap();
    let mut host = MockHost::new().cataloged("m", 20 * GIB);
    let err = load(&mut host, &wave("m"), &rig.room(&machine)).expect_err("refused");
    drop(other);
    assert_eq!(refusal_code(&err), Some("busy"), "{err:#}");
    assert!(format!("{err:#}").contains("starts in flight reserve 50.0 GiB"), "{err:#}");
}

/// (#3507) The standing utility binding counts as held: memory it takes
/// frees when no dispatch needs it, so a short wave is `busy`, never
/// `memory_in_use`.
#[test]
#[serial_test::serial]
fn the_utility_binding_counts_as_held() {
    let rig = Rig::new();
    let free = || Some(8 * GIB);
    let machine = FakeMachine::new(&free);
    let inner = MockHost::new().resident("darkmux:util", "util", 8_000, Some(30 * GIB)).cataloged("m", 20 * GIB);
    let mut host = ArchHost::new(inner, Format::Gguf);
    let lease = LeaseGuard::acquire();
    let err = ensure_wave_loaded(&wave("m"), &FixedEstimator::default(), &mut host, &lease, Some("darkmux:util"), &rig.room(&machine))
        .expect_err("refused");
    assert_eq!(refusal_code(&err), Some("busy"), "{err:#}");
    assert!(format!("{err:#}").contains("darkmux:util"), "{err:#}");
}

/// A preflight stand-in: what it holds, prices and reserves.
struct FakePreflight {
    held: Vec<String>,
    commitment: Option<u64>,
    settled: std::sync::Mutex<Vec<u64>>,
}

impl Preflight for FakePreflight {
    fn held_by_others(&self) -> Vec<String> {
        self.held.clone()
    }
    fn lock(&self) -> anyhow::Result<darkmux_types::residency_lock::ResidencyLock> {
        darkmux_types::residency_lock::acquire()
    }
    fn commitment(&self, _model_key: &str, _n_ctx: u32) -> Option<u64> {
        self.commitment
    }
    fn workers(&self) -> Option<u64> {
        Some(0)
    }
    fn settle(&self, lease: LeaseGuard, res: Reservation, need: u64, _before: Option<u64>) {
        self.settled.lock().unwrap().push(need);
        drop((res, lease));
    }
}

/// (#3514) A preflight load (the compactor, the utility model, a lab run's
/// model) follows the protocol: its reservation written under the lock,
/// the lock let go before the load, the reservation handed on to settle.
#[test]
#[serial_test::serial]
fn a_preflight_load_reserves_under_the_lock_and_loads_outside_it() {
    let _rig = Rig::new();
    let gate = FakePreflight { held: Vec::new(), commitment: Some(5 * GIB), settled: Default::default() };
    let seen = std::sync::Mutex::new(None);
    let load = |_: &str, _: &str, _: u32| -> anyhow::Result<()> {
        *seen.lock().unwrap() = Some((residency_lock_is_free(), LeaseGuard::acquire().reserved_by_others()));
        Ok(())
    };
    load_reserved(&gate, "util", "darkmux:util", 16_384, &load).unwrap();
    assert_eq!(*seen.lock().unwrap(), Some((true, 5 * GIB)), "loaded outside the lock, its 5 GiB reserved");
    assert_eq!(*gate.settled.lock().unwrap(), [5 * GIB], "the reservation is kept until it settles");
    let failing = |_: &str, _: &str, _: u32| -> anyhow::Result<()> { anyhow::bail!("no") };
    load_reserved(&gate, "util", "darkmux:util", 16_384, &failing).unwrap_err();
    assert_eq!(LeaseGuard::acquire().reserved_by_others(), 0, "a failed load reserves nothing after");
}

/// (#3514) A preflight reload never unloads the stale instance while a
/// running dispatch holds it: that is `busy`, and nothing is unloaded.
#[test]
#[serial_test::serial]
fn a_preflight_reload_never_unloads_an_instance_a_dispatch_holds() {
    let unloaded = std::sync::Mutex::new(Vec::new());
    let unload = |id: &str| -> anyhow::Result<()> {
        unloaded.lock().unwrap().push(id.to_string());
        Ok(())
    };
    let held = FakePreflight { held: vec!["darkmux:util".into()], commitment: None, settled: Default::default() };
    let err = unload_unless_held(&held, "darkmux:util", &unload).unwrap_err();
    assert_eq!(refusal_code(&err), Some("busy"), "{err:#}");
    assert!(unloaded.lock().unwrap().is_empty());
    let idle = FakePreflight { held: Vec::new(), commitment: None, settled: Default::default() };
    unload_unless_held(&idle, "darkmux:util", &unload).unwrap();
    assert_eq!(*unloaded.lock().unwrap(), ["darkmux:util"]);
}

/// (#3514) A wave that only reuses what is resident takes no residency
/// lock: a resident dispatch goes ahead while another dispatch's decision
/// (which may wait on a slow `lms unload`) holds it.
#[test]
#[serial_test::serial]
fn a_reuse_only_wave_never_waits_on_the_residency_lock() {
    let rig = Rig::new();
    let held = darkmux_types::residency_lock::acquire().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let dirs = rig.dirs();
    std::thread::spawn(move || {
        let free = || Some(64 * GIB);
        let machine = FakeMachine::new(&free);
        let room = ServerRoom { dirs, bounds: server::Bounds::default(), machine: &machine, settle: |_, _, _| {}, workers: || None };
        let inner = MockHost::new().resident("darkmux:m", "m", 8_000, Some(GIB)).cataloged("m", GIB);
        let mut host = ArchHost::new(inner, Format::Gguf).on_this_machine_too();
        let lease = LeaseGuard::acquire();
        let _ = tx.send(ensure_wave_loaded(&wave("m"), &FixedEstimator::default(), &mut host, &lease, None, &room).map(|()| host.inner.ops));
    });
    let reused = rx.recv_timeout(Duration::from_secs(5)).expect("the reuse did not wait on the lock");
    drop(held);
    assert_eq!(reused.unwrap(), [HostOp::ListResident], "reused, and nothing else asked");
}

/// (#3514) The reviewer's case: while a load's memory pages in, another
/// consumer takes 10 GiB a tick and the load itself does not grow. Its
/// reservation is measured on LM Studio's workers, which did not grow, so
/// it holds; measured on the machine's falling free memory it would end.
/// When the workers grow, it shrinks and ends.
#[test]
#[serial_test::serial]
fn a_loads_reservation_is_measured_on_lm_studios_workers_not_free_memory() {
    let _rig = Rig::new();
    let need = 30 * GIB;
    let reserve = |lease: &LeaseGuard| {
        lease.write_starting(&["darkmux:a".to_string()], need).unwrap();
        lease.mark_loaded_reserving(&["darkmux:a".to_string()], need).unwrap()
    };
    let ticks = std::cell::Cell::new(0u64);
    let seen = std::cell::RefCell::new(Vec::new());
    let observe = |_: Duration| {
        ticks.set(ticks.get() + 1);
        seen.borrow_mut().push(LeaseGuard::acquire().reserved_by_others());
    };
    // The workers stay at 8 GiB; the free memory falls 10 GiB a tick.
    let lease = LeaseGuard::acquire();
    let workers = || Some(8 * GIB);
    settle_load(reserve(&lease), need, Some(8 * GIB), &workers, &observe, Duration::from_millis(2_000));
    assert!(seen.borrow().iter().all(|r| *r == need), "held through every tick: {:?}", seen.borrow());
    drop(lease);
    // The workers grow 10 GiB a tick: the reservation shrinks to nothing.
    seen.borrow_mut().clear();
    ticks.set(0);
    let lease = LeaseGuard::acquire();
    let growing = || Some(8 * GIB + ticks.get() * 10 * GIB);
    settle_load(reserve(&lease), need, Some(8 * GIB), &growing, &observe, Duration::from_millis(5_000));
    assert_eq!(*seen.borrow(), [need, 20 * GIB, 10 * GIB], "{:?}", seen.borrow());
    assert_eq!(LeaseGuard::acquire().reserved_by_others(), 0, "ended once the workers grew by the need");
    // Workers it cannot read keep the whole reservation.
    assert_eq!(grown_since(Some(GIB), None), None);
    assert_eq!(grown_since(None, Some(GIB)), None);
}

/// (#3514) Only a reuse goes ahead without the lock: a wave that loads
/// waits for it, and decides under it.
#[test]
#[serial_test::serial]
fn a_wave_that_loads_waits_for_the_residency_lock() {
    let rig = Rig::new();
    let held = darkmux_types::residency_lock::acquire().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let dirs = rig.dirs();
    std::thread::spawn(move || {
        let free = || Some(64 * GIB);
        let machine = FakeMachine::new(&free);
        let room = ServerRoom { dirs, bounds: server::Bounds::default(), machine: &machine, settle: |_, _, _| {}, workers: || None };
        let mut host = ArchHost::new(MockHost::new().cataloged("m", GIB), Format::Gguf).on_this_machine_too();
        let lease = LeaseGuard::acquire();
        let _ = tx.send(ensure_wave_loaded(&wave("m"), &FixedEstimator::default(), &mut host, &lease, None, &room).map(|()| host.inner.ops));
    });
    assert!(rx.recv_timeout(Duration::from_millis(500)).is_err(), "a load does not go ahead while another dispatch decides");
    drop(held);
    let ops = rx.recv_timeout(Duration::from_secs(5)).expect("it goes ahead once the lock is free").unwrap();
    assert!(ops.iter().any(|op| matches!(op, HostOp::Load { .. })), "{ops:?}");
}

/// (#3514) A host's room: LM Studio's loads take this machine's memory; a
/// double's take none, so a test driving the wave loader with one is never
/// planned against the memory of the machine it runs on.
#[test]
fn only_a_host_on_this_machine_is_planned_against_its_memory() {
    let lms = darkmux_profiles::gestalt_host::LmsHost::with_bin("/nonexistent/lms");
    assert!(ServerRoom::for_host(&lms).machine.capacity().is_some(), "LM Studio's room reads this machine");
    let room = ServerRoom::for_host(&MockHost::new());
    assert_eq!((room.machine.capacity(), room.machine.reclaimable(), (room.workers)()), (None, None, None));
    assert!(room.pools().is_empty(), "no pool: the planner plans as the test's facts say");
}

/// (#3514) A wave on a host whose loads take none of this machine's memory
/// (a test's double) decides nothing about it: it never takes the
/// residency lock, nor creates its file.
#[test]
#[serial_test::serial]
fn a_doubles_wave_never_creates_the_residency_lock() {
    let rig = Rig::new();
    let lock = darkmux_types::residency_lock::lock_path();
    assert!(lock.starts_with(rig.dir.path()), "{}", lock.display());
    let mut host = MockHost::new().resident("darkmux:orphan", "orphan", 8_000, Some(GIB)).cataloged("m", GIB);
    let lease = LeaseGuard::acquire();
    ensure_wave_loaded(&wave("m"), &FixedEstimator::default(), &mut host, &lease, None, &ServerRoom::without_machine()).expect("loads");
    assert!(host.ops.iter().any(|op| matches!(op, HostOp::Load { .. })));
    assert!(!lock.exists(), "no lock file: {}", lock.display());
}
