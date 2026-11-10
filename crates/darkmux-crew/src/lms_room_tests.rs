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
    /// The standing utility binding.
    utility: Option<String>,
    /// Where LM Studio unloads are noted; `None` for a wave, whose room step
    /// never unloads anything.
    lms_unloads: Option<&'a Mutex<Vec<String>>>,
    /// The memory pressure each reading finds, in order; the last one
    /// stays. Empty: none can be read.
    pressure: Mutex<Vec<darkmux_platform::MemoryPressure>>,
}

/// A pressure reading at the kernel's `level`.
fn pressure_at(level: darkmux_platform::PressureLevel) -> darkmux_platform::MemoryPressure {
    darkmux_platform::MemoryPressure { level: Some(level), swap_used_bytes: Some(24 * GIB), swap_free_bytes: Some(GIB), compressor_pct: Some(40) }
}

impl<'a> FakeMachine<'a> {
    fn new(free: &'a (dyn Fn() -> Option<u64> + Sync)) -> Self {
        FakeMachine { free, leased: Mutex::new(vec![Vec::new()]), capacity: Some(128 * GIB), utility: None, lms_unloads: None, pressure: Mutex::new(Vec::new()) }
    }
    /// The pressure readings, in order.
    fn pressed(self, levels: &[darkmux_platform::PressureLevel]) -> Self {
        *self.pressure.lock().unwrap() = levels.iter().map(|l| pressure_at(*l)).collect();
        self
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
        let Some(unloads) = self.lms_unloads else { panic!("a wave's room step never unloads `{identifier}`: the plan does") };
        unloads.lock().unwrap().push(identifier.to_string());
        Ok(())
    }
    fn utility(&self) -> Option<String> {
        self.utility.clone()
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
    fn pressure(&self) -> Option<darkmux_platform::MemoryPressure> {
        let mut readings = self.pressure.lock().unwrap();
        if readings.len() > 1 {
            Some(readings.remove(0))
        } else {
            readings.first().copied()
        }
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
        // A load's memory has paged in the moment it is loaded.
        ServerRoom { dirs: self.dirs(), bounds: BOUNDS, machine, settle: |res, _| drop(res), workers: || Some(0) }
    }
    /// [`Self::room`], each load's reservation kept whole, as it is while
    /// its memory pages in, until the test settles it ([`settle_all`]) or
    /// the rig goes.
    fn room_settling<'a>(&self, machine: &'a (dyn Machine + Sync)) -> ServerRoom<'a> {
        ServerRoom { settle: |res, _| SETTLING.lock().unwrap().push(res), ..self.room(machine) }
    }
}

/// A stop's bounds, short for a test.
const BOUNDS: server::Bounds = server::Bounds { start: Duration::from_secs(5), stop_grace: Duration::from_secs(2), poll: Duration::from_millis(20) };

/// The loads' reservations a [`Rig::room`] was handed to settle.
static SETTLING: Mutex<Vec<Reserved>> = Mutex::new(Vec::new());

/// Every load handed to settle has paged in: its reservation ends.
fn settle_all() {
    SETTLING.lock().unwrap().clear();
}

/// What starts still paging in reserve, every footprint unread.
fn reserved_now() -> u64 {
    use darkmux_types::residency_reservation::{reserved, Servers};
    reserved(&Servers { alive: &server::process_alive, footprint: &|_| None })
}

impl Drop for Rig {
    fn drop(&mut self) {
        settle_all();
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

/// An idle server stopped to make room reserves nothing once it is gone:
/// its start's reservation (still paging in) is not held against the load
/// that replaced it.
#[test]
#[serial_test::serial]
fn stopping_an_idle_server_ends_what_its_start_reserved() {
    let rig = Rig::new();
    let mut tf = FakeServer::darkmux(&rig.dirs(), "tf", "TensorFold/q", 1_000);
    let rec = tf.rec.clone();
    let mut start = Reserved::begin(30 * GIB).unwrap();
    start.bind_server(rec.pid, rec.start_stamp, darkmux_types::residency_reservation::now_ms() + 600_000).unwrap();
    start.leave_to_server();
    assert_eq!(reserved_now(), 30 * GIB);
    let free = || Some(if server::alive(&rec) { 8 * GIB } else { 40 * GIB });
    let machine = FakeMachine::new(&free);
    let mut host = MockHost::new().cataloged("m", 20 * GIB);
    load(&mut host, &wave("m"), &rig.room(&machine)).expect("the load fits the 40 GiB free once the server stopped");
    assert!(!tf.running());
    assert_eq!(reserved_now(), 0, "the stopped server reserves nothing");
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
    // A model a preflight just loaded keeps that load's lease until the
    // run's own holds it.
    assert!(prepare.find("lease_lmstudio_job(").unwrap() < prepare.find("drop(preflight_hold)").unwrap());
    assert!(launch.find("hold_dispatch_models(").unwrap() < launch.find("drop(compactor_hold)").unwrap());
    // A compactor refused for memory fails the dispatch before the spawn.
    let compactor = body("prepare_compactor");
    assert!(compactor.contains("apply_compactor_setup(") && compactor.contains("})?;"), "{compactor}");
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
        self.at_load.push((residency_lock_is_free(), reserved_now()));
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
    ensure_wave_loaded(&wave("m"), &FixedEstimator::default(), &mut host, &lease, None, &rig.room_settling(&machine)).expect("loads");
    assert_eq!(host.at_unload, [false], "the orphan is unloaded under the lock");
    let (free_at_load, reserved_at_load) = host.at_load[0];
    assert!(free_at_load, "the lock is let go before the load begins");
    assert!(reserved_at_load > 20 * GIB, "the load's commitment is reserved while it loads: {reserved_at_load}");
    assert_eq!(reserved_now(), reserved_at_load, "and kept past the load, while its memory pages in");
    drop(lease);
    assert_eq!(reserved_now(), reserved_at_load, "apart from the dispatch's lease: it outlives it");
    settle_all();
    assert_eq!(reserved_now(), 0, "until it has paged in");
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
    assert_eq!(reserved_now(), 0);
}

/// A wave, and a preflight load, whose reservation cannot be written loads
/// nothing: a load no other decision counts would be admitted beside one it
/// does not fit with.
#[test]
#[serial_test::serial]
fn a_load_whose_reservation_cannot_be_written_is_refused_before_it_loads() {
    let rig = Rig::new();
    // The reservations directory's own name is a file.
    std::fs::create_dir_all(rig.dir.path().join("home").join("run")).unwrap();
    std::fs::write(rig.dir.path().join("home").join("run").join("reservations"), "").unwrap();
    let free = || Some(64 * GIB);
    let machine = FakeMachine::new(&free);
    let mut host = MockHost::new().cataloged("m", 20 * GIB);
    let err = load(&mut host, &wave("m"), &rig.room(&machine)).expect_err("refused");
    assert_eq!(refusal_code(&err), Some("model_start_failed"), "{err:#}");
    assert!(format!("{err:#}").contains("could not record this wave's reservation"), "{err:#}");
    assert!(!loaded(&host), "nothing loaded");
    let gate = FakePreflight::without_machine().priced("util", 5 * GIB);
    let loads = std::cell::Cell::new(0);
    let counting = |_: &str, _: &str, _: u32| -> anyhow::Result<()> {
        loads.set(loads.get() + 1);
        Ok(())
    };
    assert!(load_reserved(&gate, "util", "darkmux:util", 16_384, &counting).is_err(), "refused");
    assert_eq!(loads.get(), 0, "the preflight loaded nothing");
    assert!(LeaseGuard::acquire().all_live_leased_models().is_empty(), "and holds no lease after");
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
    let other = Reserved::begin(50 * GIB).unwrap();
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

/// A preflight stand-in: LM Studio as `lms ps` and its host report it, what
/// other dispatches hold, and the machine a load takes its memory from.
struct FakePreflight<'a> {
    held: Vec<String>,
    est: FixedEstimator,
    loaded: Vec<darkmux_types::LoadedModel>,
    residents: Vec<ResidentFact>,
    catalog: Vec<CatalogFact>,
    format: Format,
    dirs: Dirs,
    machine: &'a (dyn Machine + Sync),
    /// How often the residency lock was asked for.
    locks: std::cell::Cell<u32>,
    /// Every load and unload asked of LM Studio, in order.
    calls: Mutex<Vec<String>>,
    /// What other dispatches hold is read from the leases on disk.
    real_leases: bool,
}

impl FakePreflight<'static> {
    /// No machine to plan against, LM Studio empty.
    fn without_machine() -> Self {
        FakePreflight::on(Dirs { records: std::path::PathBuf::new(), logs: std::path::PathBuf::new() }, &NoMachine)
    }
}

impl<'a> FakePreflight<'a> {
    fn on(dirs: Dirs, machine: &'a (dyn Machine + Sync)) -> Self {
        FakePreflight {
            held: Vec::new(),
            est: FixedEstimator::default(),
            loaded: Vec::new(),
            residents: Vec::new(),
            catalog: Vec::new(),
            format: Format::Gguf,
            dirs,
            machine,
            locks: Default::default(),
            calls: Default::default(),
            real_leases: false,
        }
    }
    /// The caller prices `model_key` at `bytes`, both ways.
    fn priced(mut self, model_key: &str, bytes: u64) -> Self {
        self.est.0.insert(model_key.into(), bytes);
        self
    }
    fn reading_leases(mut self) -> Self {
        self.real_leases = true;
        self
    }
    fn holding(mut self, held: &[&str]) -> Self {
        self.held = held.iter().map(|s| s.to_string()).collect();
        self
    }
    fn cataloged(mut self, model_key: &str, size_bytes: u64) -> Self {
        self.catalog.push(CatalogFact { model_key: model_key.into(), size_bytes: Some(size_bytes) });
        self
    }
    fn formatted(mut self, format: Format) -> Self {
        self.format = format;
        self
    }
    /// Resident in LM Studio, as `lms ps` and the host both list it.
    fn resident(mut self, identifier: &str, model_key: &str, ctx: u64, bytes: u64) -> Self {
        self.loaded.push(darkmux_types::LoadedModel {
            identifier: identifier.into(),
            model: model_key.into(),
            status: "idle".into(),
            size: String::new(),
            context: ctx,
            queued: None,
        });
        self.residents.push(ResidentFact { identifier: identifier.into(), model_key: model_key.into(), ctx, est_bytes: Some(bytes), parallel: 0 });
        self
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Preflight for FakePreflight<'_> {
    fn held_by_others(&self) -> Vec<String> {
        if self.real_leases {
            return LeaseGuard::acquire().all_live_leased_models();
        }
        self.held.clone()
    }
    fn lock(&self) -> anyhow::Result<darkmux_types::residency_lock::ResidencyLock> {
        self.locks.set(self.locks.get() + 1);
        darkmux_types::residency_lock::acquire()
    }
    fn loaded(&self) -> Vec<darkmux_types::LoadedModel> {
        self.loaded.clone()
    }
    fn unload(&self, identifier: &str) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!("unload {identifier}"));
        Ok(())
    }
    fn load(&self, _model_key: &str, identifier: &str, n_ctx: u32) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!("load {identifier} {n_ctx}"));
        Ok(())
    }
    fn host(&self) -> Option<Box<dyn ModelHost>> {
        let inner = MockHost { residents: self.residents.clone(), catalog: self.catalog.clone(), ..Default::default() };
        Some(Box::new(ArchHost::new(inner, self.format)))
    }
    fn estimator(&self) -> &dyn FootprintEstimator {
        &self.est
    }
    fn room(&self) -> ServerRoom<'_> {
        // Each load's reservation kept whole until the test settles it.
        ServerRoom {
            dirs: self.dirs.clone(),
            bounds: BOUNDS,
            machine: self.machine,
            settle: |res, _| SETTLING.lock().unwrap().push(res),
            workers: || Some(0),
        }
    }
}

/// (#3514) A preflight load (the compactor, the utility model, a lab run's
/// model) follows the protocol: its reservation written under the lock,
/// the lock let go before the load, the reservation handed on to settle.
#[test]
#[serial_test::serial]
fn a_preflight_load_reserves_under_the_lock_and_loads_outside_it() {
    let _rig = Rig::new();
    let gate = FakePreflight::without_machine().priced("util", 5 * GIB);
    let seen = std::sync::Mutex::new(None);
    let load = |_: &str, _: &str, _: u32| -> anyhow::Result<()> {
        *seen.lock().unwrap() = Some((residency_lock_is_free(), reserved_now()));
        Ok(())
    };
    load_reserved(&gate, "util", "darkmux:util", 16_384, &load).unwrap();
    assert_eq!(*seen.lock().unwrap(), Some((true, 5 * GIB)), "loaded outside the lock, its 5 GiB reserved");
    let handed_on: Vec<u64> = SETTLING.lock().unwrap().iter().map(Reserved::need_bytes).collect();
    assert_eq!(handed_on, [5 * GIB], "the reservation is kept until it settles");
    let failing = |_: &str, _: &str, _: u32| -> anyhow::Result<()> { anyhow::bail!("no") };
    assert!(load_reserved(&gate, "util", "darkmux:util", 16_384, &failing).is_err());
    assert_eq!(reserved_now(), 5 * GIB, "a failed load reserves nothing after: only the first load's is left");
}

/// A model the preflight just loaded for nobody (the compactor, the utility
/// model) counts against free memory while it pages in, and that is all: no
/// lease outlives the load, so it reads as held by no dispatch, a reload of
/// it is not `busy`, and it can be evicted.
#[test]
#[serial_test::serial]
fn a_freshly_loaded_compactor_is_reserved_but_neither_held_nor_busy() {
    let _rig = Rig::new();
    let gate = FakePreflight::without_machine().priced("util", 5 * GIB);
    let held_during_load = std::sync::Mutex::new(Vec::new());
    let load = |_: &str, _: &str, _: u32| -> anyhow::Result<()> {
        *held_during_load.lock().unwrap() = LeaseGuard::acquire().all_live_leased_models();
        Ok(())
    };
    load_reserved(&gate, "util", "darkmux:util", 16_384, &load).unwrap();
    assert_eq!(*held_during_load.lock().unwrap(), ["darkmux:util"], "held while a dispatch is loading it");
    assert_eq!(reserved_now(), 5 * GIB, "still paging in: reserved");
    let held = LeaseGuard::acquire().all_live_leased_models();
    assert!(held.is_empty(), "and held by no dispatch: {held:?}");
    let unloaded = std::cell::Cell::new(false);
    unload_unless_held(&ThisPreflight, "darkmux:util", &|_| {
        unloaded.set(true);
        Ok(())
    })
    .expect("a reload is not `busy`: nothing runs on it");
    assert!(unloaded.get());
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
    let held = FakePreflight::without_machine().holding(&["darkmux:util"]);
    let err = unload_unless_held(&held, "darkmux:util", &unload).unwrap_err();
    assert_eq!(refusal_code(&err), Some("busy"), "{err:#}");
    assert!(unloaded.lock().unwrap().is_empty());
    let idle = FakePreflight::without_machine();
    unload_unless_held(&idle, "darkmux:util", &unload).unwrap();
    assert_eq!(*unloaded.lock().unwrap(), ["darkmux:util"]);
}

/// A profile model a dispatch's preflight makes resident at 8000 tokens.
fn profile_model(id: &str) -> darkmux_types::ProfileModel {
    darkmux_types::ProfileModel { id: id.into(), n_ctx: Some(8_000), ..Default::default() }
}

/// A preflight load as a dispatch makes it (a lab run's model, the
/// compactor, the utility model), against `gate`.
fn preflight(gate: &FakePreflight<'_>, id: &str) -> anyhow::Result<()> {
    crate::dispatch_internal::preflight_load(&profile_model(id), crate::dispatch_internal::WindowSource::Profile, gate).map(drop)
}

/// A preflight load decides memory as a wave does: an idle engine server
/// darkmux started holds the memory, and it is stopped before the load.
/// Before, the preflight loaded on top of it.
#[test]
#[serial_test::serial]
fn a_preflight_load_stops_an_idle_engine_server_to_make_room() {
    let rig = Rig::new();
    let mut tf = FakeServer::darkmux(&rig.dirs(), "tf", "TensorFold/q", 1_000);
    let rec = tf.rec.clone();
    let free = || Some(if server::alive(&rec) { 8 * GIB } else { 64 * GIB });
    let machine = FakeMachine::new(&free);
    let gate = FakePreflight::on(rig.dirs(), &machine).cataloged("m", 20 * GIB);
    preflight(&gate, "m").expect("the load fits once the idle server stopped");
    assert!(!tf.running(), "the idle server holding the memory was stopped");
    assert_eq!(gate.calls(), ["load darkmux:m 8000"]);
    assert_eq!(gate.locks.get(), 1, "decided under the residency lock");
}

/// Short of memory that darkmux did not take, with nothing of darkmux's
/// idle to free: the preflight is refused `memory_in_use`, and nothing loads.
#[test]
#[serial_test::serial]
fn a_preflight_load_short_of_memory_darkmux_did_not_take_is_memory_in_use() {
    let rig = Rig::new();
    let free = || Some(8 * GIB);
    let machine = FakeMachine::new(&free);
    let gate = FakePreflight::on(rig.dirs(), &machine).cataloged("m", 20 * GIB);
    let err = preflight(&gate, "m").expect_err("refused");
    assert_eq!(refusal_code(&err), Some("memory_in_use"), "{err:#}");
    assert!(format!("{err:#}").contains("programs darkmux did not start hold the rest"), "{err:#}");
    assert!(gate.calls().is_empty(), "nothing loaded: {:?}", gate.calls());
    assert!(LeaseGuard::acquire().all_live_leased_models().is_empty(), "and no lease is left behind");
}

/// Memory a running dispatch holds frees when it finishes: the preflight is
/// `busy`, the server it holds is never stopped, and nothing loads.
#[test]
#[serial_test::serial]
fn a_preflight_load_short_of_memory_a_running_dispatch_holds_is_busy() {
    let rig = Rig::new();
    let mut tf = FakeServer::darkmux(&rig.dirs(), "tf", "TensorFold/q", 1_000);
    let free = || Some(8 * GIB);
    let machine = FakeMachine::new(&free).leasing(&[&["darkmux:TensorFold/q"]]);
    let gate = FakePreflight::on(rig.dirs(), &machine).holding(&["darkmux:TensorFold/q"]).cataloged("m", 20 * GIB);
    let err = preflight(&gate, "m").expect_err("refused");
    assert_eq!(refusal_code(&err), Some("busy"), "{err:#}");
    assert!(format!("{err:#}").contains("running dispatches hold TensorFold/q (tf)"), "{err:#}");
    assert!(tf.running(), "the held server was not stopped");
    assert!(gate.calls().is_empty(), "nothing loaded: {:?}", gate.calls());
}

/// A load priced only by the size-based estimate (its architecture
/// unreadable) has no commitment, and is never refused: it loads, and LM
/// Studio decides.
#[test]
#[serial_test::serial]
fn a_preflight_load_priced_by_its_size_alone_is_never_refused() {
    let rig = Rig::new();
    let free = || Some(8 * GIB);
    let machine = FakeMachine::new(&free);
    let gate = FakePreflight::on(rig.dirs(), &machine).cataloged("m", 20 * GIB).formatted(Format::Unreadable);
    preflight(&gate, "m").expect("never refused on a guess");
    assert_eq!(gate.calls(), ["load darkmux:m 8000"]);
}

/// A preflight that reuses what is resident at its context decides nothing
/// about memory: it never asks for the residency lock.
#[test]
#[serial_test::serial]
fn a_preflight_reuse_takes_no_lock() {
    let rig = Rig::new();
    let free = || Some(8 * GIB);
    let machine = FakeMachine::new(&free);
    let gate = FakePreflight::on(rig.dirs(), &machine).resident("darkmux:m", "m", 8_000, 20 * GIB).cataloged("m", 20 * GIB);
    preflight(&gate, "m").expect("reused");
    assert_eq!(gate.locks.get(), 0, "a reuse takes no lock");
    assert!(gate.calls().is_empty(), "nothing loads or unloads: {:?}", gate.calls());
}

/// With no idle server left, darkmux's idle LM Studio loads are unloaded
/// to make room, as a wave's own pass unloads them; never one a running
/// dispatch holds, nor the standing utility binding.
#[test]
#[serial_test::serial]
fn a_preflight_load_unloads_darkmuxs_idle_lm_studio_loads_but_never_a_held_one_or_the_utility_binding() {
    let rig = Rig::new();
    let unloads = Mutex::new(Vec::new());
    let free = || Some(if unloads.lock().unwrap().is_empty() { 8 * GIB } else { 24 * GIB });
    let machine = FakeMachine { utility: Some("darkmux:util".into()), lms_unloads: Some(&unloads), ..FakeMachine::new(&free) }
        .leasing(&[&["darkmux:held"]]);
    let gate = FakePreflight::on(rig.dirs(), &machine)
        .holding(&["darkmux:held"])
        .resident("darkmux:held", "held", 8_000, 16 * GIB)
        .resident("darkmux:util", "util", 8_000, 16 * GIB)
        .resident("darkmux:orphan", "orphan", 8_000, 16 * GIB)
        .resident("theirs", "theirs", 8_000, 16 * GIB)
        .cataloged("m", 20 * GIB);
    preflight(&gate, "m").expect("8 GiB free plus the orphan's 16 GiB holds the load");
    assert_eq!(*unloads.lock().unwrap(), ["darkmux:orphan"]);
    assert_eq!(gate.calls(), ["load darkmux:m 8000"]);
}

/// What a preflight never unloads to make room: the instance it is loading
/// (resident by the time the lock is held: another process loaded it), one a
/// running dispatch holds, the standing utility binding. Short with only
/// those, it is `busy`, naming each holder once.
#[test]
#[serial_test::serial]
fn a_preflight_never_unloads_its_own_instance_a_held_one_or_the_utility_binding() {
    let rig = Rig::new();
    let unloads = Mutex::new(Vec::new());
    let free = || Some(8 * GIB);
    let machine = FakeMachine { utility: Some("darkmux:util".into()), lms_unloads: Some(&unloads), ..FakeMachine::new(&free) }
        .leasing(&[&["darkmux:held"]]);
    let mut gate = FakePreflight::on(rig.dirs(), &machine)
        .holding(&["darkmux:held"])
        .resident("darkmux:m", "m", 8_000, 16 * GIB)
        .resident("darkmux:held", "held", 8_000, 16 * GIB)
        .resident("darkmux:util", "util", 8_000, 16 * GIB)
        .cataloged("m", 20 * GIB);
    // `lms ps` read before the lock did not list it yet.
    gate.loaded.retain(|m| m.identifier != "darkmux:m");
    let err = preflight(&gate, "m").expect_err("refused");
    assert_eq!(refusal_code(&err), Some("busy"), "{err:#}");
    let text = format!("{err:#}");
    assert!(text.contains("running dispatches hold darkmux:held, darkmux:util;"), "{text}");
    assert!(unloads.lock().unwrap().is_empty(), "{:?}", unloads.lock().unwrap());
}

/// A model a preflight just loaded stays leased until its caller holds it
/// with its own lease: a peer dispatch's compactor, loaded and not yet held
/// by that dispatch, is never unloaded to make room, and the load that
/// wanted its memory is `busy`.
#[test]
#[serial_test::serial]
fn a_model_a_preflight_just_loaded_is_never_unloaded_before_its_caller_holds_it() {
    let rig = Rig::new();
    let peer = FakePreflight::without_machine();
    let peers_load = load_reserved(&peer, "peer-compactor", "darkmux:peer-compactor", 8_000, &|_, _, _| Ok(())).expect("loads");
    let unloads = Mutex::new(Vec::new());
    let free = || Some(if unloads.lock().unwrap().is_empty() { 8 * GIB } else { 24 * GIB });
    let machine = FakeMachine { leased: Mutex::new(Vec::new()), lms_unloads: Some(&unloads), ..FakeMachine::new(&free) };
    let gate = FakePreflight::on(rig.dirs(), &machine)
        .reading_leases()
        .resident("darkmux:peer-compactor", "peer-compactor", 8_000, 16 * GIB)
        .cataloged("m", 20 * GIB);
    let err = preflight(&gate, "m").expect_err("refused while the peer's compactor is held");
    assert_eq!(refusal_code(&err), Some("busy"), "{err:#}");
    assert!(unloads.lock().unwrap().is_empty(), "{:?}", unloads.lock().unwrap());
    drop(peers_load);
}

/// The stale-context reload through the same wiring: an instance a running
/// dispatch holds is never unloaded under it, and the preflight is `busy`.
#[test]
#[serial_test::serial]
fn a_preflight_reload_of_a_held_instance_is_busy_and_unloads_nothing() {
    let rig = Rig::new();
    let free = || Some(64 * GIB);
    let machine = FakeMachine::new(&free);
    let gate = FakePreflight::on(rig.dirs(), &machine).holding(&["darkmux:m"]).resident("darkmux:m", "m", 4_096, 20 * GIB);
    let err = preflight(&gate, "m").expect_err("refused");
    assert_eq!(refusal_code(&err), Some("busy"), "{err:#}");
    assert!(gate.calls().is_empty(), "{:?}", gate.calls());
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
        let room = ServerRoom { dirs, bounds: server::Bounds::default(), machine: &machine, settle: |_, _| {}, workers: || None };
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
    let ticks = std::cell::Cell::new(0u64);
    let seen = std::cell::RefCell::new(Vec::new());
    let observe = |_: Duration| {
        ticks.set(ticks.get() + 1);
        seen.borrow_mut().push(reserved_now());
    };
    // The workers stay at 8 GiB; the free memory falls 10 GiB a tick.
    let workers = || Some(8 * GIB);
    settle_load(Reserved::begin(need).unwrap(), Some(8 * GIB), &workers, &observe, Duration::from_millis(2_000));
    assert!(seen.borrow().iter().all(|r| *r == need) && seen.borrow().len() == 4, "held through every tick: {:?}", seen.borrow());
    assert_eq!(reserved_now(), 0, "ended at the bound");
    // The workers grow 10 GiB a tick: the reservation shrinks to nothing.
    seen.borrow_mut().clear();
    ticks.set(0);
    let growing = || Some(8 * GIB + ticks.get() * 10 * GIB);
    settle_load(Reserved::begin(need).unwrap(), Some(8 * GIB), &growing, &observe, Duration::from_millis(5_000));
    assert_eq!(*seen.borrow(), [need, 20 * GIB, 10 * GIB], "{:?}", seen.borrow());
    assert_eq!(reserved_now(), 0, "ended once the workers grew by the need");
    // Workers it cannot read keep the whole reservation.
    assert_eq!(grown_since(Some(GIB), None), None);
    assert_eq!(grown_since(None, Some(GIB)), None);
}

/// A local track makes several loads on one lease, each settling on a
/// thread of its own. An earlier wave's settle never touches a later wave's
/// reservation: while wave 1's 20 GiB settles (down to its end), wave 2's
/// 40 GiB in flight stays whole.
#[test]
#[serial_test::serial]
fn an_earlier_waves_settle_never_touches_a_later_waves_reservation() {
    let _rig = Rig::new();
    let wave_1 = Reserved::begin(20 * GIB).unwrap();
    let wave_2 = Reserved::begin(40 * GIB).unwrap();
    let ticks = std::cell::Cell::new(0u64);
    let seen = std::cell::RefCell::new(Vec::new());
    let observe = |_: Duration| {
        ticks.set(ticks.get() + 1);
        seen.borrow_mut().push(reserved_now());
    };
    let growing = || Some(ticks.get() * 10 * GIB);
    settle_load(wave_1, Some(0), &growing, &observe, Duration::from_secs(120));
    seen.borrow_mut().push(reserved_now());
    let gib: Vec<u64> = seen.borrow().iter().map(|b| b / GIB).collect();
    assert_eq!(gib, [60, 50, 40], "wave 1's shrinks and ends; wave 2's 40 GiB is whole throughout");
    drop(wave_2);
}

/// A load's reservation only shrinks: workers that fall back (the model was
/// unloaded while it settled) never bring back what had paged in.
#[test]
#[serial_test::serial]
fn a_loads_reservation_never_grows_back() {
    let _rig = Rig::new();
    let readings = std::cell::RefCell::new(vec![Some(25 * GIB), Some(0), Some(0)].into_iter());
    let seen = std::cell::RefCell::new(Vec::new());
    let workers = || readings.borrow_mut().next().flatten();
    settle_load(Reserved::begin(30 * GIB).unwrap(), Some(0), &workers, &|_| seen.borrow_mut().push(reserved_now() / GIB), Duration::from_millis(1_500));
    assert_eq!(*seen.borrow(), [5, 5, 5], "5 GiB was left, and stays the most it reserves");
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
        let room = ServerRoom { dirs, bounds: server::Bounds::default(), machine: &machine, settle: |_, _| {}, workers: || None };
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

// ── #3538: the pressure gate and the load watchdog on the LM Studio path ──

/// A wave whose load fits by the arithmetic is refused while the machine
/// is already under memory pressure, and nothing loads.
#[test]
#[serial_test::serial]
fn high_memory_pressure_refuses_a_wave_that_fits() {
    use darkmux_platform::PressureLevel;
    let rig = Rig::new();
    let free = || Some(64 * GIB);
    let machine = FakeMachine::new(&free).pressed(&[PressureLevel::Warning]);
    let mut host = MockHost::new().cataloged("m", 20 * GIB);
    let err = load(&mut host, &wave("m"), &rig.room(&machine)).expect_err("refused");
    assert_eq!(refusal_code(&err), Some("memory_in_use"), "{err:#}");
    assert!(format!("{err:#}").contains("`warning` memory pressure"), "the reading is named: {err:#}");
    assert!(!loaded(&host), "{:?}", host.ops);
    assert_eq!(reserved_now(), 0, "nothing stays reserved");
    // A wave that only reuses what is resident decides nothing, and goes on.
    let mut host = MockHost::new().resident("darkmux:m", "m", 8_000, Some(GIB));
    load(&mut host, &wave("m"), &rig.room(&machine)).expect("a reuse is never gated");
}

/// A preflight load (the compactor, the utility model, a lab run's model)
/// meets the same gate.
#[test]
#[serial_test::serial]
fn high_memory_pressure_refuses_a_preflight_load_that_fits() {
    use darkmux_platform::PressureLevel;
    let rig = Rig::new();
    let free = || Some(64 * GIB);
    let machine = FakeMachine::new(&free).pressed(&[PressureLevel::Critical]);
    let gate = FakePreflight::on(rig.dirs(), &machine).cataloged("m", 20 * GIB);
    let err = preflight(&gate, "m").expect_err("refused");
    assert_eq!(refusal_code(&err), Some("memory_in_use"), "{err:#}");
    assert!(format!("{err:#}").contains("`critical` memory pressure"), "{err:#}");
    assert!(gate.calls().is_empty(), "nothing loaded: {:?}", gate.calls());
    assert!(LeaseGuard::acquire().all_live_leased_models().is_empty(), "and no lease is left behind");
}

/// Pressure that turns critical while a preflight's `lms load` runs: the
/// instance just loaded is unloaded once the load returns, the dispatch is
/// refused, and neither its reservation nor its lease is left.
#[test]
#[serial_test::serial]
fn pressure_turning_critical_during_a_preflight_load_unloads_it_and_refuses() {
    use darkmux_platform::PressureLevel;
    let rig = Rig::new();
    let free = || Some(64 * GIB);
    let machine = FakeMachine::new(&free).pressed(&[PressureLevel::Normal, PressureLevel::Critical]);
    let gate = FakePreflight::on(rig.dirs(), &machine).priced("util", 5 * GIB);
    // The load outlasts several polls; the pressure is critical while it
    // runs and normal again by the time it returns, so only a reading
    // taken during the load sees it.
    let slow = |_: &str, id: &str, _: u32| -> anyhow::Result<()> {
        std::thread::sleep(BOUNDS.poll * 10);
        *machine.pressure.lock().unwrap() = vec![pressure_at(PressureLevel::Normal)];
        gate.calls.lock().unwrap().push(format!("load {id}"));
        Ok(())
    };
    let err = load_reserved(&gate, "util", "darkmux:util", 16_384, &slow).err().expect("refused");
    assert_eq!(refusal_code(&err), Some("memory_in_use"), "{err:#}");
    assert!(format!("{err:#}").contains("`critical` memory pressure") && format!("{err:#}").contains("unloaded"), "{err:#}");
    assert_eq!(gate.calls(), ["load darkmux:util", "unload darkmux:util"], "the load it made is cut");
    assert_eq!(reserved_now(), 0, "its reservation ends");
    assert!(LeaseGuard::acquire().all_live_leased_models().is_empty(), "and its lease");
}

/// The same for a wave: a load that returns onto a machine whose pressure
/// turned critical is unloaded, and the wave refused.
#[test]
#[serial_test::serial]
fn pressure_turning_critical_during_a_waves_load_unloads_it_and_refuses() {
    use darkmux_platform::PressureLevel;
    let rig = Rig::new();
    let free = || Some(64 * GIB);
    let machine = FakeMachine::new(&free).pressed(&[PressureLevel::Normal, PressureLevel::Critical]);
    let mut host = MockHost::new().cataloged("m", 20 * GIB);
    let err = load(&mut host, &wave("m"), &rig.room(&machine)).expect_err("refused");
    assert_eq!(refusal_code(&err), Some("memory_in_use"), "{err:#}");
    assert!(format!("{err:#}").contains("unloaded"), "{err:#}");
    let unloads: Vec<&HostOp> = host.ops.iter().filter(|op| matches!(op, HostOp::Unload { .. })).collect();
    assert_eq!(unloads.len(), 1, "the load it made is cut: {:?}", host.ops);
    assert!(loaded(&host), "after it loaded: {:?}", host.ops);
    assert_eq!(reserved_now(), 0, "its reservation ends");
}

/// A load under no critical pressure is left alone.
#[test]
#[serial_test::serial]
fn a_load_under_warning_pressure_mid_load_is_kept() {
    use darkmux_platform::PressureLevel;
    let rig = Rig::new();
    let free = || Some(64 * GIB);
    let machine = FakeMachine::new(&free).pressed(&[PressureLevel::Normal, PressureLevel::Warning]);
    let mut host = MockHost::new().cataloged("m", 20 * GIB);
    load(&mut host, &wave("m"), &rig.room(&machine)).expect("loads");
    assert!(!host.ops.iter().any(|op| matches!(op, HostOp::Unload { .. })), "{:?}", host.ops);
}
