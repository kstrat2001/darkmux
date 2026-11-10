//! (#3448) The lifecycle of an engine server darkmux starts (DESIGN.md,
//! "Engine servers darkmux starts"): an engine that holds one model per
//! server process, so loading a model is starting a server and unloading it
//! is stopping one.
//!
//! - **Start** ([`ensure`]): under a per-endpoint lock, reuse the endpoint's
//!   server when darkmux's record says it is running the same command and it
//!   answers; otherwise stop darkmux's old server there, refuse a port
//!   something else holds, spawn the command in its own process group with
//!   its output in a log file, record it, and wait until it lists the name
//!   darkmux started it under. A server a running dispatch holds (the
//!   caller's `held`, asked under the same lock) is never stopped: the same
//!   command still listening is reused however slowly it answers, and any
//!   other is refused `busy`. That holds for workers an exited leader left
//!   on the port too. A name proves nothing on its own (every Flint
//!   server answers `flint`, #3453 review): a server is reused, and a start
//!   is ready, only when darkmux's own server is what listens on the port
//!   ([`listens`]); a start answered by anything else fails `port_in_use`.
//! - **Stop** ([`stop`]): only a server whose record still matches the live
//!   process (pid and start stamp), `SIGTERM`, then `SIGKILL` after a grace,
//!   then wait for its port to close.
//!
//! The server outlives the darkmux process that started it and is never
//! registered for the shutdown sweep (`child_registry`), which would take a
//! warm model down with every CLI exit. Any darkmux process can find it again
//! through its record ([`running`]).

use darkmux_types::engine_record::{self, EngineRecord, ServerState, ENGINE_RECORD_SCHEMA};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a start may take before darkmux gives up and stops it. Not
/// measured yet for a large model at a long context (#3448 open question):
/// a 62 GB model reads from disk for minutes, so this is generous.
pub const START_BOUND: Duration = Duration::from_secs(15 * 60);

/// How long a stop waits after `SIGTERM` before `SIGKILL`, and again for
/// the port to close.
pub const STOP_GRACE: Duration = Duration::from_secs(15);

/// One server to run: the command darkmux builds for the engine, and what
/// the record says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    pub endpoint: String,
    /// The `managed` token (`tensorfold`).
    pub engine: String,
    pub model: String,
    pub n_ctx: u32,
    pub port: u16,
    /// The model name the server answers to (`darkmux:<id>`, contract 4).
    pub served_name: String,
    /// The whole command; `argv[0]` is the binary.
    pub argv: Vec<String>,
    pub engine_version: String,
}

/// Where records and logs live: `<darkmux-home>/run/engines` and
/// `<darkmux-home>/logs/engines` by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    pub records: PathBuf,
    pub logs: PathBuf,
}

impl Dirs {
    pub fn under_home() -> Self {
        let home = darkmux_types::paths::user_root_guarded();
        Dirs { records: engine_record::dir(), logs: home.join("logs").join("engines") }
    }

    /// The log file of `endpoint`'s server.
    pub fn log_for(&self, endpoint: &str) -> PathBuf {
        engine_record::path_in(&self.logs, endpoint).with_extension("log")
    }
}

/// The waits a start and a stop are bounded by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub start: Duration,
    pub stop_grace: Duration,
    pub poll: Duration,
}

impl Default for Bounds {
    fn default() -> Self {
        Bounds { start: START_BOUND, stop_grace: STOP_GRACE, poll: Duration::from_millis(500) }
    }
}

/// What a start reports while it runs, for the dispatch's live records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// darkmux is stopping its own server on the endpoint, which runs another
    /// model or context.
    Replacing { model: String },
    Starting,
    /// Still waiting; how long so far.
    Waiting { elapsed: Duration },
    Ready { elapsed: Duration },
}

/// Which check found the endpoint's port held by something darkmux did not
/// start, so a refusal says why it was read that way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortHolder {
    /// The port accepted a connection before darkmux started its server.
    BeforeStart,
    /// The port answered, and these processes listen there: none is
    /// darkmux's server or in the group it leads.
    Foreign(Vec<u32>),
    /// The port answered, and no process listened on it when darkmux asked.
    NoListener,
    /// The port answered, and this machine could not say who listens on it,
    /// asked twice.
    Unreadable,
}

impl std::fmt::Display for PortHolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PortHolder::BeforeStart => write!(f, "it accepted a connection before darkmux started its server"),
            PortHolder::Foreign(pids) => {
                let list: Vec<String> = pids.iter().map(u32::to_string).collect();
                let noun = if pids.len() == 1 { "pid" } else { "pids" };
                write!(f, "{noun} {} listen there, and none is darkmux's server", list.join(", "))
            }
            PortHolder::NoListener => write!(f, "it answered, and no process listened on it when darkmux asked"),
            PortHolder::Unreadable => write!(f, "it answered, and this machine could not say which process listens on it"),
        }
    }
}

/// Why a server could not be started or stopped. Each has the stable
/// `code` an app branches on (DESIGN.md, "What an app reads").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerError {
    /// Something darkmux did not start answers on the endpoint's port;
    /// `holder` says which check found it.
    PortInUse { endpoint: String, port: u16, holder: PortHolder },
    /// darkmux's server on the endpoint runs another command, and a running
    /// dispatch is using it.
    Busy { endpoint: String, model: String },
    /// The server exited, did not answer in time, or could not be spawned
    /// or recorded. `log` holds its output.
    StartFailed { endpoint: String, log: PathBuf, detail: String },
    /// darkmux's server would not stop.
    StopFailed { endpoint: String, pid: u32, detail: String },
}

impl ServerError {
    pub fn code(&self) -> &'static str {
        match self {
            ServerError::PortInUse { .. } => "port_in_use",
            ServerError::Busy { .. } => "busy",
            ServerError::StartFailed { .. } => "model_start_failed",
            ServerError::StopFailed { .. } => "model_stop_failed",
        }
    }
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerError::PortInUse { endpoint, port, holder } => write!(
                f,
                "endpoint `{endpoint}`: something darkmux did not start already answers on 127.0.0.1:{port} ({holder}), \
                 and darkmux never takes over a server it did not start. Stop it, or give the endpoint another `port`"
            ),
            ServerError::Busy { endpoint, model } => write!(
                f,
                "endpoint `{endpoint}` is serving `{model}` for a running dispatch; retry when it finishes"
            ),
            ServerError::StartFailed { endpoint, log, detail } => {
                write!(f, "endpoint `{endpoint}`: the server did not start: {detail}. Its output is in {}", log.display())
            }
            ServerError::StopFailed { endpoint, pid, detail } => {
                write!(f, "endpoint `{endpoint}`: darkmux's server (pid {pid}) did not stop: {detail}")
            }
        }
    }
}

impl std::error::Error for ServerError {}

/// What [`ensure`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ensured {
    /// darkmux's server was already running this command and answering.
    Reused(EngineRecord),
    Started(EngineRecord),
}

impl Ensured {
    pub fn record(&self) -> &EngineRecord {
        match self {
            Ensured::Reused(r) | Ensured::Started(r) => r,
        }
    }
}

/// The servers this process spawned and has not reaped. A server that exits
/// while its starter is still alive (the daemon) stays a zombie, still
/// holding its pid and start stamp, until reaped here.
static SPAWNED: Mutex<Vec<Child>> = Mutex::new(Vec::new());

/// Whether `pid` is a zombie: exited, but its parent (another process than
/// this one, which reaps its own) has not reaped it yet. It still holds its
/// pid and start stamp, so the record alone would read it as running.
fn is_zombie(pid: u32) -> bool {
    let mut cmd = Command::new("ps");
    cmd.args(["-o", "stat=", "-p", &pid.to_string()]);
    crate::process::pin_cwd(&mut cmd);
    cmd.output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim_start().starts_with('Z'))
}

/// darkmux's server `rec` is alive: its record matches the live process,
/// and that process has not exited.
pub fn alive(rec: &EngineRecord) -> bool {
    reap_exited();
    rec.state_now() == ServerState::Running && !is_zombie(rec.pid)
}

fn reap_exited() {
    if let Ok(mut spawned) = SPAWNED.lock() {
        spawned.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
    }
}

/// darkmux's running server on `endpoint`, if it has one: its record matches
/// the live process. A stale record (the process exited, or its pid now
/// belongs to someone else) is removed and reads as none, unless workers its
/// exited server left still hold the port ([`left_workers`]): [`ensure`]
/// clears those under the endpoint's lock. An unreadable record claims
/// nothing.
pub fn running(dirs: &Dirs, endpoint: &str) -> Option<EngineRecord> {
    let rec = engine_record::read_in(&dirs.records, endpoint).ok().flatten()?;
    if alive(&rec) {
        return Some(rec);
    }
    if !left_workers(&rec) {
        let _ = engine_record::remove_if_in(&dirs.records, &rec);
    }
    None
}

/// darkmux's server on `endpoint` whose leader exited while workers in its
/// group still serve the port ([`left_workers`]), read only: a dispatch
/// clears it, or reuses it when a running dispatch holds it.
pub fn left_behind(dirs: &Dirs, endpoint: &str) -> Option<EngineRecord> {
    engine_record::read_in(&dirs.records, endpoint).ok().flatten().filter(|r| !alive(r) && left_workers(r))
}

/// Whether darkmux's exited server `rec` left workers in the group it led
/// still listening on its port. Never when its pid now names another
/// process: that pid's group could be the new process's. A group outlives
/// its leader, and its id is not reused as a pid while any member is left.
fn left_workers(rec: &EngineRecord) -> bool {
    // A closed port answers at once; only a taken one is worth asking the
    // host who listens (a socket listing is the slow part).
    rec.state_now() != ServerState::PidReused && port_taken(rec.port) && listener(rec) == Listener::Darkmuxs
}

/// darkmux's live server on `endpoint`, read only: a stale record is left
/// for [`running`] (a dispatch) to clear, so a listing changes nothing.
pub fn running_now(dirs: &Dirs, endpoint: &str) -> Option<EngineRecord> {
    engine_record::read_in(&dirs.records, endpoint).ok().flatten().filter(alive)
}

/// (#3450) One server darkmux started that is running now, as every machine
/// surface lists it beside darkmux's LM Studio loads: `machine status`,
/// `/machine/status`, `/machine/specs` (and so the machine card), and a row
/// of the residency ledger.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct EngineServer {
    /// The `endpoints` id it serves.
    pub endpoint: String,
    /// The `managed` token (`tensorfold`, `flint`).
    pub engine: String,
    /// The model it was started with (a profile model's `id`).
    pub model: String,
    /// The model name it answers to on the wire: `darkmux:<id>` on
    /// Tensorfold, `flint` on rec's Flint (which takes no name).
    pub served_name: String,
    pub n_ctx: u32,
    pub port: u16,
    pub pid: u32,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub started_at_ms: u64,
    /// Whether it answered `GET /v1/models` under `served_name` when it was
    /// listed. `false` while its model is still starting, or when it was too
    /// busy to answer within a second.
    pub answering: bool,
}

/// How long a listing waits for a server to answer `GET /v1/models`.
const LISTING_ANSWER_BOUND: Duration = Duration::from_secs(1);

/// The model name darkmux's server `rec` answers to: `flint` for rec's (it
/// has no `--name`, #3453), else the `--name` it was started under.
pub fn served_name(rec: &EngineRecord) -> String {
    if rec.engine == super::flint::ENGINE {
        return super::flint::SERVED_NAME.to_string();
    }
    rec.argv.iter().skip_while(|a| *a != "--name").nth(1).cloned().unwrap_or_default()
}

/// (#3450) darkmux's servers running now, by endpoint id. Read only, like
/// [`running_now`]: a stale record is left for a dispatch to clear, and an
/// unreadable one claims nothing.
pub fn listed(dirs: &Dirs) -> Vec<EngineServer> {
    let mut out: Vec<EngineServer> = engine_record::list_in(&dirs.records)
        .0
        .into_iter()
        .filter(alive)
        .map(|rec| {
            let served_name = served_name(&rec);
            EngineServer {
                answering: answers_within(rec.port, &served_name, LISTING_ANSWER_BOUND),
                endpoint: rec.endpoint,
                engine: rec.engine,
                model: rec.model,
                served_name,
                n_ctx: rec.n_ctx,
                port: rec.port,
                pid: rec.pid,
                started_at_ms: rec.started_at_ms,
            }
        })
        .collect();
    out.sort_by(|a, b| a.endpoint.cmp(&b.endpoint));
    out
}

/// (#3450) darkmux's live server serving `model` (a profile model on an
/// endpoint darkmux starts servers for): the one on its endpoint, started
/// with its id. The context is the caller's to compare. Read only.
pub fn serving(dirs: &Dirs, model: &darkmux_types::ProfileModel) -> Option<EngineRecord> {
    let endpoint = model.endpoint.as_ref().and_then(|e| e.named_id())?;
    running_now(dirs, endpoint).filter(|r| r.model == model.id)
}

/// The reuse rule [`ensure`] applies, and a listing's `resident`: darkmux's
/// server `rec` runs exactly `argv`, answers under `served_name` on `port`,
/// and is what listens there (never a server darkmux did not start).
pub fn serves(rec: &EngineRecord, argv: &[String], port: u16, served_name: &str) -> bool {
    rec.argv == argv && answers(port, served_name) && listens(rec)
}

/// Whether a running dispatch is using darkmux's server `rec`: asked under
/// the endpoint's lock, right before a stop it would forbid.
pub type Held<'a> = &'a dyn Fn(&EngineRecord) -> bool;

/// Make `spec`'s server the one running on its endpoint, and wait until it
/// answers. See the module doc for the steps; `held` is asked under the
/// endpoint's lock before darkmux's server there is stopped.
pub fn ensure(
    spec: &ServerSpec,
    dirs: &Dirs,
    bounds: Bounds,
    held: Held<'_>,
    progress: &mut dyn FnMut(Progress),
) -> Result<Ensured, ServerError> {
    let start_failed = |detail: String| ServerError::StartFailed {
        endpoint: spec.endpoint.clone(),
        log: dirs.log_for(&spec.endpoint),
        detail,
    };
    // `<stem>.server.lock`, never a name the record's own `<stem>.json.lock`
    // can take for another endpoint (holding both in one process would
    // otherwise block on itself, since flock locks per open file).
    let lock_path = engine_record::path_in(&dirs.records, &spec.endpoint).with_extension("server.lock");
    let _lock = darkmux_types::flock::lock_exclusive(&lock_path).map_err(|e| start_failed(format!("{e:#}")))?;
    if let Some(rec) = reuse_or_stop(spec, dirs, bounds, held, progress)? {
        return Ok(Ensured::Reused(rec));
    }
    if port_taken(spec.port) {
        return Err(ServerError::PortInUse { endpoint: spec.endpoint.clone(), port: spec.port, holder: PortHolder::BeforeStart });
    }
    progress(Progress::Starting);
    let rec = spawn_and_record(spec, dirs).map_err(start_failed)?;
    wait_ready(spec, dirs, &rec, bounds, progress).map_err(|fail| {
        let _ = stop_locked(dirs, &rec, bounds);
        match fail {
            NotReady::Foreign(holder) => ServerError::PortInUse { endpoint: spec.endpoint.clone(), port: spec.port, holder },
            NotReady::Failed(detail) => start_failed(detail),
        }
    })?;
    Ok(Ensured::Started(rec))
}

/// darkmux's running server on `spec`'s endpoint when it runs this exact
/// command, answers under its name and is what listens on the port; any other
/// server of darkmux's there is stopped, and `None` returned. One a running
/// dispatch holds is never stopped: the same command still listening is
/// reused however slowly it answers (it is busy), and another command is
/// refused `busy`. A stale record is cleared with any workers it left.
fn reuse_or_stop(
    spec: &ServerSpec,
    dirs: &Dirs,
    bounds: Bounds,
    held: Held<'_>,
    progress: &mut dyn FnMut(Progress),
) -> Result<Option<EngineRecord>, ServerError> {
    let Some(rec) = engine_record::read_in(&dirs.records, &spec.endpoint).ok().flatten() else { return Ok(None) };
    if !alive(&rec) {
        return keep_or_clear_left(spec, dirs, bounds, held, rec);
    }
    let same = rec.argv == spec.argv;
    if same && answers(spec.port, &spec.served_name) && listens(&rec) {
        return Ok(Some(rec));
    }
    if held(&rec) {
        if same && listens(&rec) {
            return Ok(Some(rec));
        }
        return Err(ServerError::Busy { endpoint: spec.endpoint.clone(), model: rec.model.clone() });
    }
    progress(Progress::Replacing { model: rec.model.clone() });
    stop_locked(dirs, &rec, bounds)?;
    Ok(None)
}

/// [`reuse_or_stop`] for a record whose leader exited: the workers it left
/// on the port are a dispatch's when it holds them, kept as it would keep
/// the leader (reused for the same command, else `busy`); otherwise they are
/// cleared with the record.
fn keep_or_clear_left(
    spec: &ServerSpec,
    dirs: &Dirs,
    bounds: Bounds,
    held: Held<'_>,
    rec: EngineRecord,
) -> Result<Option<EngineRecord>, ServerError> {
    if left_workers(&rec) && held(&rec) {
        return if rec.argv == spec.argv {
            Ok(Some(rec))
        } else {
            Err(ServerError::Busy { endpoint: spec.endpoint.clone(), model: rec.model.clone() })
        };
    }
    stop_locked(dirs, &rec, bounds)?;
    Ok(None)
}

/// Spawn `spec.argv` detached in its own process group, its output appended
/// to the endpoint's log, and write its record.
fn spawn_and_record(spec: &ServerSpec, dirs: &Dirs) -> Result<EngineRecord, String> {
    let (bin, args) = spec.argv.split_first().ok_or("the command is empty")?;
    let log_path = dirs.log_for(&spec.endpoint);
    std::fs::create_dir_all(&dirs.logs).map_err(|e| format!("creating {}: {e}", dirs.logs.display()))?;
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("opening {}: {e}", log_path.display()))?;
    let started_at_ms = now_ms();
    use std::io::Write;
    let _ = writeln!(log, "--- darkmux started at {started_at_ms} ms: {}", spec.argv.join(" "));
    let err = log.try_clone().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(bin);
    cmd.args(args).stdin(Stdio::null()).stdout(log).stderr(err);
    crate::process::pin_cwd(&mut cmd);
    // Its own group: a terminal's Ctrl-C to the dispatch that started it
    // must not take the server down with it.
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn().map_err(|e| format!("could not run `{bin}`: {e}"))?;
    let pid = child.id();
    let kill_spawned = |child: &mut Child| {
        let _ = darkmux_types::child_registry::kill_group(child.id(), darkmux_types::child_registry::SIGKILL);
        let _ = child.wait();
    };
    let Some(start_stamp) = start_stamp_of(pid) else {
        kill_spawned(&mut child);
        return Err("darkmux could not read the server's start time, so it could not record it as its own".into());
    };
    let rec = EngineRecord {
        schema: ENGINE_RECORD_SCHEMA,
        endpoint: spec.endpoint.clone(),
        engine: spec.engine.clone(),
        model: spec.model.clone(),
        n_ctx: spec.n_ctx,
        port: spec.port,
        pid,
        start_stamp,
        owner_pid: std::process::id(),
        argv: spec.argv.clone(),
        log: log_path,
        started_at_ms,
        engine_version: spec.engine_version.clone(),
    };
    if let Err(e) = engine_record::write_in(&dirs.records, &rec) {
        kill_spawned(&mut child);
        return Err(format!("could not write its record: {e}"));
    }
    if let Ok(mut spawned) = SPAWNED.lock() {
        spawned.push(child);
    }
    Ok(rec)
}

/// A freshly spawned process may not show a start time for a moment.
fn start_stamp_of(pid: u32) -> Option<u64> {
    (0..20).find_map(|_| {
        let s = darkmux_platform::current().process_start_stamp(pid);
        if s.is_none() {
            std::thread::sleep(Duration::from_millis(10));
        }
        s
    })
}

/// Why a started server is not ready.
enum NotReady {
    /// Something darkmux did not start answers on the port (#3453 review).
    Foreign(PortHolder),
    Failed(String),
}

fn wait_ready(spec: &ServerSpec, dirs: &Dirs, rec: &EngineRecord, bounds: Bounds, progress: &mut dyn FnMut(Progress)) -> Result<(), NotReady> {
    let began = Instant::now();
    let mut last_beat = began;
    loop {
        if !alive(rec) {
            return Err(NotReady::Failed(format!("it exited during its start{}", log_tail(&dirs.log_for(&spec.endpoint)))));
        }
        if answers(spec.port, &spec.served_name) {
            // The answer is darkmux's only when its own server is the listener.
            if let Some(holder) = listener(rec).holder() {
                return Err(NotReady::Foreign(holder));
            }
            progress(Progress::Ready { elapsed: began.elapsed() });
            return Ok(());
        }
        if began.elapsed() >= bounds.start {
            return Err(NotReady::Failed(format!(
                "it did not answer within {} s{}",
                bounds.start.as_secs(),
                log_tail(&dirs.log_for(&spec.endpoint))
            )));
        }
        if last_beat.elapsed() >= Duration::from_secs(10) {
            last_beat = Instant::now();
            progress(Progress::Waiting { elapsed: began.elapsed() });
        }
        std::thread::sleep(bounds.poll);
    }
}

/// The last lines of the server's log, for a failed start's message.
fn log_tail(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.starts_with("--- darkmux started")).collect();
    let tail = &lines[lines.len().saturating_sub(5)..];
    if tail.is_empty() {
        String::new()
    } else {
        format!("; its last output: {}", tail.join(" / "))
    }
}

/// Stop darkmux's server `rec`, taking the endpoint's lock.
pub fn stop(dirs: &Dirs, rec: &EngineRecord, bounds: Bounds) -> Result<(), ServerError> {
    let _lock = lock_for_stop(dirs, rec)?;
    stop_locked(dirs, rec, bounds)
}

/// Stop darkmux's server `rec` to make room, unless `held` says a running
/// dispatch uses it, asked under the endpoint's lock so a dispatch that took
/// it up since the caller looked is seen. `Ok(false)`: held, left running.
pub fn stop_unless_held(dirs: &Dirs, rec: &EngineRecord, bounds: Bounds, held: Held<'_>) -> Result<bool, ServerError> {
    let _lock = lock_for_stop(dirs, rec)?;
    if held(rec) {
        return Ok(false);
    }
    stop_locked(dirs, rec, bounds)?;
    Ok(true)
}

fn lock_for_stop(dirs: &Dirs, rec: &EngineRecord) -> Result<darkmux_types::flock::FlockGuard, ServerError> {
    let lock_path = engine_record::path_in(&dirs.records, &rec.endpoint).with_extension("server.lock");
    darkmux_types::flock::lock_exclusive(&lock_path)
        .map_err(|e| ServerError::StopFailed { endpoint: rec.endpoint.clone(), pid: rec.pid, detail: format!("{e:#}") })
}

/// [`stop`] with the lock already held. Signals only a process the record
/// still matches, or the workers its exited server left on the port
/// ([`left_workers`]); a stale record is then removed.
fn stop_locked(dirs: &Dirs, rec: &EngineRecord, bounds: Bounds) -> Result<(), ServerError> {
    use darkmux_types::child_registry::{SIGKILL, SIGTERM};
    let failed = |detail: String| ServerError::StopFailed { endpoint: rec.endpoint.clone(), pid: rec.pid, detail };
    let mut signaled = false;
    // Only a server the record still matches is ever signaled; a stale
    // record names a pid (and so a group) that may be someone else's now.
    if alive(rec) {
        signaled = true;
        signal_server(rec.pid, SIGTERM).map_err(|e| failed(format!("SIGTERM: {e}")))?;
        let exited = gone_within(rec, bounds);
        // Whether or not the server exited on SIGTERM, a worker in its group
        // may have ignored it: SIGKILL the group. Its id cannot be reused
        // while any member is left, and with none left this is a no-op.
        let _ = darkmux_types::child_registry::kill_group(rec.pid, SIGKILL);
        // That SIGKILL may already have ended the server; signal it only if
        // it is still alive.
        if !exited && alive(rec) {
            signal_server(rec.pid, SIGKILL).map_err(|e| failed(format!("SIGKILL: {e}")))?;
            gone_within(rec, bounds);
        }
    } else if left_workers(rec) {
        // Its server exited first (a crash, or a server that daemonized):
        // the group it led still holds the port, and is darkmux's.
        signaled = true;
        let _ = darkmux_types::child_registry::kill_group(rec.pid, SIGTERM);
        port_closed_within(rec.port, bounds);
        let _ = darkmux_types::child_registry::kill_group(rec.pid, SIGKILL);
    }
    if alive(rec) {
        return Err(failed("it is still running after SIGKILL".into()));
    }
    if signaled {
        port_closed_within(rec.port, bounds);
    }
    let _ = engine_record::remove_if_in(&dirs.records, rec);
    Ok(())
}

/// Wait up to the stop grace for nothing to accept connections on `port`.
fn port_closed_within(port: u16, bounds: Bounds) {
    let until = Instant::now() + bounds.stop_grace;
    while port_taken(port) && Instant::now() < until {
        std::thread::sleep(bounds.poll);
    }
}

/// Signal the server's process group (it leads one: `spawn_and_record`, so
/// the group is the server and every worker it forked) and the server
/// itself, which may have left that group. Called only right after
/// `alive(rec)`, so the pid is darkmux's; "no such process" for either is
/// not an error (it already exited, or it leads no group).
fn signal_server(pid: u32, sig: i32) -> std::io::Result<()> {
    use darkmux_types::child_registry::{kill_group, kill_pid};
    ok_if_gone(kill_group(pid, sig), || is_zombie(pid)).and_then(|()| ok_if_gone(kill_pid(pid, sig), || is_zombie(pid)))
}

/// A signal's result, with "the process is gone" read as success: ESRCH,
/// and EPERM when `zombie` says the target is a zombie (macOS answers EPERM,
/// not ESRCH, for a group whose only member is a zombie: exited, not yet
/// reaped). `zombie` is asked only for an EPERM.
fn ok_if_gone(result: std::io::Result<()>, zombie: impl FnOnce() -> bool) -> std::io::Result<()> {
    use darkmux_types::child_registry::{EPERM, ESRCH};
    match result {
        Err(e) if e.raw_os_error() == Some(ESRCH) => Ok(()),
        Err(e) if e.raw_os_error() == Some(EPERM) && zombie() => Ok(()),
        other => other,
    }
}

fn gone_within(rec: &EngineRecord, bounds: Bounds) -> bool {
    let until = Instant::now() + bounds.stop_grace;
    while Instant::now() < until {
        if !alive(rec) {
            return true;
        }
        std::thread::sleep(bounds.poll.min(Duration::from_millis(100)));
    }
    false
}

/// Whether anything accepts a connection on loopback `port`.
pub fn port_taken(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Who listens on the port of darkmux's server `rec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listener {
    /// Every listener is the recorded server or a process in the group it
    /// leads.
    Darkmuxs,
    Nobody,
    /// These listeners are neither.
    Foreign(Vec<u32>),
    /// The host could not say, asked twice.
    Unknown,
}

impl Listener {
    /// The [`PortHolder`] a start reports when the answer on the port is not
    /// darkmux's server's; `None` when it is.
    fn holder(self) -> Option<PortHolder> {
        match self {
            Listener::Darkmuxs => None,
            Listener::Nobody => Some(PortHolder::NoListener),
            Listener::Foreign(pids) => Some(PortHolder::Foreign(pids)),
            Listener::Unknown => Some(PortHolder::Unreadable),
        }
    }
}

/// How long darkmux waits before asking the host again who listens, when
/// it could not say: a listing of sockets can fail for a moment.
const LISTENERS_RETRY: Duration = Duration::from_millis(100);

/// [`Listener`] on `rec`'s port, asked of this machine.
pub fn listener(rec: &EngineRecord) -> Listener {
    listener_by(rec, darkmux_platform::current())
}

/// [`listener`], asked of `host`. A read that cannot say is asked again
/// once before it reads as [`Listener::Unknown`].
pub fn listener_by(rec: &EngineRecord, host: &dyn darkmux_platform::Platform) -> Listener {
    let read = || host.tcp_listeners(rec.port);
    let pids = read().or_else(|| {
        std::thread::sleep(LISTENERS_RETRY);
        read()
    });
    let Some(pids) = pids else { return Listener::Unknown };
    if pids.is_empty() {
        return Listener::Nobody;
    }
    let foreign: Vec<u32> = pids.into_iter().filter(|&pid| pid != rec.pid && host.process_group(pid) != Some(rec.pid)).collect();
    if foreign.is_empty() {
        Listener::Darkmuxs
    } else {
        Listener::Foreign(foreign)
    }
}

/// (#3453 review) Whether darkmux's server `rec` is what listens on its
/// port: every listener there is the recorded server or a process in the
/// group it leads (it is spawned leading its own; Tensorfold may serve from
/// a worker). A name proves nothing (every Flint server answers `flint`), so
/// this is what makes an answer on the port darkmux's. Nothing listening,
/// or a platform that cannot say (asked twice), is not darkmux's.
pub fn listens(rec: &EngineRecord) -> bool {
    listens_by(rec, darkmux_platform::current())
}

/// [`listens`], asked of `host`.
pub fn listens_by(rec: &EngineRecord, host: &dyn darkmux_platform::Platform) -> bool {
    listener_by(rec, host) == Listener::Darkmuxs
}

/// Whether the server on `port` lists `served_name` at `GET /v1/models`.
fn answers(port: u16, served_name: &str) -> bool {
    answers_within(port, served_name, Duration::from_secs(2))
}

/// [`answers`], waiting at most `bound`.
fn answers_within(port: u16, served_name: &str, bound: Duration) -> bool {
    let agent = ureq::AgentBuilder::new().redirects(0).timeout(bound).build();
    let Ok(resp) = agent.get(&format!("http://127.0.0.1:{port}/v1/models")).call() else { return false };
    let Ok(text) = resp.into_string() else { return false };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else { return false };
    doc.get("data")
        .and_then(|d| d.as_array())
        .is_some_and(|rows| rows.iter().any(|r| r.get("id").and_then(|i| i.as_str()) == Some(served_name)))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in engine: `serve MODEL --port P --name N [--fail|--slow S|--ignore-term]`.
    /// It answers `GET /v1/models` with `N`, the shape Tensorfold answers.
    const FAKE: &str = r#"
import json, signal, sys, time
from http.server import BaseHTTPRequestHandler, HTTPServer
a = sys.argv[1:]
port = int(a[a.index("--port") + 1]); name = a[a.index("--name") + 1]
if "--fail" in a:
    print("fake engine: refusing to start", flush=True); sys.exit(3)
if "--slow" in a:
    time.sleep(float(a[a.index("--slow") + 1]))
if "--ignore-term" in a:
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
if "--fork-child" in a:
    import subprocess
    child = subprocess.Popen(["sleep", "300"])
    open(a[a.index("--fork-child") + 1], "w").write(str(child.pid))
if "--stubborn-worker" in a:
    import os
    pid = os.fork()
    if pid == 0:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        time.sleep(300); os._exit(0)
    open(a[a.index("--stubborn-worker") + 1], "w").write(str(pid))
if "--leave-group-keep-worker" in a:
    import os
    if os.fork() == 0:
        time.sleep(300); os._exit(0)
    os.setpgid(0, os.getpgid(os.getppid()))
if "--leave-group" in a:
    import os
    os.setpgid(0, os.getpgid(os.getppid()))
if "--daemonize" in a:
    import os
    if os.fork() > 0:
        os._exit(0)
if "--silent-name" in a:
    name = "someone-else"
if "--no-bind" in a:
    time.sleep(300); sys.exit(0)
class H(BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({"object": "list", "data": [{"id": name}]}).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json"); self.end_headers(); self.wfile.write(body)
    def log_message(self, *args): pass
# HTTPServer's server_bind looks up the host's FQDN, which can stall for
# tens of seconds on a CI runner; bind without it.
class S(HTTPServer):
    def server_bind(self):
        import socketserver
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = "localhost", self.server_address[1]
S(("127.0.0.1", port), H).serve_forever()
"#;

    struct Fixture {
        _dir: tempfile::TempDir,
        dirs: Dirs,
        script: PathBuf,
    }

    /// A failed assertion must not leave a stand-in server running: every
    /// server the fixture's records still name as running is killed when the
    /// test ends, pass or fail.
    impl Drop for Fixture {
        fn drop(&mut self) {
            for rec in engine_record::list_in(&self.dirs.records).0 {
                if rec.state_now() == ServerState::Running {
                    use darkmux_types::child_registry::{kill_group, kill_pid, SIGKILL};
                    let _ = kill_group(rec.pid, SIGKILL).or_else(|_| kill_pid(rec.pid, SIGKILL));
                }
            }
            reap_exited();
        }
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake_engine.py");
        std::fs::write(&script, FAKE).unwrap();
        let dirs = Dirs { records: dir.path().join("run"), logs: dir.path().join("logs") };
        Fixture { _dir: dir, dirs, script }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    fn spec(f: &Fixture, endpoint: &str, model: &str, port: u16, extra: &[&str]) -> ServerSpec {
        let served_name = format!("darkmux:{model}");
        let mut argv: Vec<String> = ["python3", "-u", f.script.to_str().unwrap(), "serve", model, "--port"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        argv.extend([port.to_string(), "--name".into(), served_name.clone()]);
        argv.extend(extra.iter().map(|s| s.to_string()));
        ServerSpec {
            endpoint: endpoint.into(),
            engine: "tensorfold".into(),
            model: model.into(),
            n_ctx: 8192,
            port,
            served_name,
            argv,
            engine_version: "fake 1.2.4".into(),
        }
    }

    fn quick() -> Bounds {
        Bounds { start: Duration::from_secs(20), stop_grace: Duration::from_secs(2), poll: Duration::from_millis(50) }
    }

    fn no_progress() -> impl FnMut(Progress) {
        |_| {}
    }

    fn not_held(_: &EngineRecord) -> bool {
        false
    }

    /// Start, reuse, stop: a second ensure of the same command reuses the
    /// server; a stop takes it down, frees the port and removes the record.
    #[test]
    fn a_server_starts_is_reused_and_stops() {
        let f = fixture();
        let s = spec(&f, "tf", "m/a", free_port(), &[]);
        let mut seen = Vec::new();
        let first = ensure(&s, &f.dirs, quick(), &not_held, &mut |p| seen.push(p)).unwrap();
        let Ensured::Started(rec) = first else { panic!("started: {first:?}") };
        assert!(matches!(seen.first(), Some(Progress::Starting)) && matches!(seen.last(), Some(Progress::Ready { .. })), "{seen:?}");
        assert!(answers(s.port, &s.served_name));
        assert_eq!(running(&f.dirs, "tf"), Some(rec.clone()));
        let again = ensure(&s, &f.dirs, quick(), &not_held, &mut no_progress()).unwrap();
        assert_eq!(again, Ensured::Reused(rec.clone()), "the same command is reused, not restarted");
        stop(&f.dirs, &rec, quick()).unwrap();
        assert_eq!(rec.state_now(), ServerState::Exited);
        assert!(!port_taken(s.port), "the port is free after a stop");
        assert_eq!(running(&f.dirs, "tf"), None);
        assert_eq!(engine_record::read_in(&f.dirs.records, "tf").unwrap(), None, "the record is gone");
    }

    /// Another model on the endpoint replaces darkmux's server there.
    #[test]
    fn another_model_on_the_endpoint_replaces_darkmuxs_server() {
        let f = fixture();
        let port = free_port();
        let a = ensure(&spec(&f, "tf", "m/a", port, &[]), &f.dirs, quick(), &not_held, &mut no_progress()).unwrap();
        let mut seen = Vec::new();
        let b = ensure(&spec(&f, "tf", "m/b", port, &[]), &f.dirs, quick(), &not_held, &mut |p| seen.push(p)).unwrap();
        assert!(matches!(&b, Ensured::Started(r) if r.model == "m/b" && r.pid != a.record().pid), "{b:?}");
        assert_eq!(seen.first(), Some(&Progress::Replacing { model: "m/a".into() }));
        assert_eq!(a.record().state_now(), ServerState::Exited, "the old server is stopped");
        stop(&f.dirs, b.record(), quick()).unwrap();
    }

    /// The same model under a changed command (other `engine_args`) is
    /// restarted, never reused: what the server runs must be what the
    /// profile says.
    #[test]
    fn a_changed_command_for_the_same_model_restarts_the_server() {
        let f = fixture();
        let port = free_port();
        let a = ensure(&spec(&f, "tf", "m/a", port, &[]), &f.dirs, quick(), &not_held, &mut no_progress()).unwrap();
        let b = ensure(&spec(&f, "tf", "m/a", port, &["--keep-warm"]), &f.dirs, quick(), &not_held, &mut no_progress()).unwrap();
        assert!(matches!(&b, Ensured::Started(r) if r.pid != a.record().pid), "{b:?}");
        assert_eq!(a.record().state_now(), ServerState::Exited);
        stop(&f.dirs, b.record(), quick()).unwrap();
    }

    /// A port something else holds is refused, never reused or taken over,
    /// and nothing is spawned or recorded.
    #[test]
    fn a_port_something_else_holds_is_refused() {
        let f = fixture();
        let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = other.local_addr().unwrap().port();
        let err = ensure(&spec(&f, "tf", "m/a", port, &[]), &f.dirs, quick(), &not_held, &mut no_progress()).unwrap_err();
        assert_eq!(err.code(), "port_in_use");
        assert!(err.to_string().contains(&port.to_string()), "{err}");
        assert!(err.to_string().contains("before darkmux started its server"), "it says which check refused: {err}");
        assert_eq!(engine_record::read_in(&f.dirs.records, "tf").unwrap(), None);
    }

    /// A server that exits during its start fails the start naming its log
    /// and its last output, and leaves no record.
    #[test]
    fn a_server_that_exits_during_its_start_fails_naming_its_log() {
        let f = fixture();
        let err = ensure(&spec(&f, "tf", "m/a", free_port(), &["--fail"]), &f.dirs, quick(), &not_held, &mut no_progress()).unwrap_err();
        assert_eq!(err.code(), "model_start_failed");
        let msg = err.to_string();
        assert!(msg.contains("refusing to start") && msg.contains("tf.log"), "{msg}");
        assert_eq!(engine_record::read_in(&f.dirs.records, "tf").unwrap(), None);
    }

    /// A server that does not answer in time is stopped, and the start fails.
    #[test]
    fn a_server_that_does_not_answer_in_time_is_stopped() {
        let f = fixture();
        let s = spec(&f, "tf", "m/a", free_port(), &["--slow", "30"]);
        let bounds = Bounds { start: Duration::from_millis(600), ..quick() };
        let err = ensure(&s, &f.dirs, bounds, &not_held, &mut no_progress()).unwrap_err();
        assert!(err.to_string().contains("did not answer within"), "{err}");
        assert_eq!(engine_record::read_in(&f.dirs.records, "tf").unwrap(), None, "no record survives a failed start");
    }

    /// A server that ignores SIGTERM is SIGKILLed after the grace.
    #[test]
    fn a_server_that_ignores_sigterm_is_killed_after_the_grace() {
        let f = fixture();
        let s = spec(&f, "tf", "m/a", free_port(), &["--ignore-term"]);
        let rec = ensure(&s, &f.dirs, quick(), &not_held, &mut no_progress()).unwrap().record().clone();
        let bounds = Bounds { stop_grace: Duration::from_millis(500), ..quick() };
        stop(&f.dirs, &rec, bounds).unwrap();
        assert_eq!(rec.state_now(), ServerState::Exited);
    }

    /// A record whose pid now belongs to another process is stale: it is
    /// removed, and that process is never signaled. The "other process" is
    /// this test itself, so a wrong signal would end the test run.
    #[test]
    fn a_record_whose_pid_was_reused_is_never_signaled() {
        let f = fixture();
        let me = std::process::id();
        let stamp = darkmux_platform::current().process_start_stamp(me).expect("own start stamp");
        let mut stale = EngineRecord {
            schema: ENGINE_RECORD_SCHEMA,
            endpoint: "tf".into(),
            engine: "tensorfold".into(),
            model: "m/a".into(),
            n_ctx: 8192,
            port: free_port(),
            pid: me,
            start_stamp: stamp.wrapping_add(1),
            owner_pid: me,
            argv: vec![],
            log: f.dirs.log_for("tf"),
            started_at_ms: 0,
            engine_version: String::new(),
        };
        engine_record::write_in(&f.dirs.records, &stale).unwrap();
        assert_eq!(stale.state_now(), ServerState::PidReused);
        assert_eq!(running(&f.dirs, "tf"), None, "a reused pid is not darkmux's server");
        assert_eq!(engine_record::read_in(&f.dirs.records, "tf").unwrap(), None, "and its record is removed");
        engine_record::write_in(&f.dirs.records, &stale).unwrap();
        stop(&f.dirs, &stale, quick()).unwrap();
        stale.start_stamp = stamp;
        assert_eq!(stale.state_now(), ServerState::Running, "still alive: never signaled");
    }

    fn pid_alive(pid: u32) -> bool {
        darkmux_types::residency_lease::process_alive(pid) && !is_zombie(pid)
    }

    /// (#3458 review) A stop takes down the server's whole process group, so a
    /// worker it forked never outlives it.
    #[test]
    fn a_stop_takes_down_the_servers_forked_workers() {
        let f = fixture();
        let child_file = f.dirs.logs.with_extension("child");
        let s = spec(&f, "tf", "m/a", free_port(), &["--fork-child", child_file.to_str().unwrap()]);
        let rec = ensure(&s, &f.dirs, quick(), &not_held, &mut no_progress()).unwrap().record().clone();
        let child: u32 = std::fs::read_to_string(&child_file).unwrap().trim().parse().unwrap();
        assert!(pid_alive(child));
        stop(&f.dirs, &rec, quick()).unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        while pid_alive(child) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!pid_alive(child), "the forked worker died with its server");
    }

    /// (#3458 review) A server that is alive and runs the same command but
    /// does not answer under the `darkmux:` name is never reused: darkmux
    /// replaces it (and here the replacement never answers either, so the
    /// start fails rather than returning the silent server as ready).
    #[test]
    fn a_server_that_does_not_answer_under_its_name_is_not_reused() {
        let f = fixture();
        let silent = spec(&f, "tf", "m/a", free_port(), &["--silent-name"]);
        let up = spawn_and_record(&silent, &f.dirs).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while !port_taken(silent.port) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(alive(&up), "the silent server is up under the same command");
        let result = ensure(&silent, &f.dirs, Bounds { start: Duration::from_millis(800), ..quick() }, &not_held, &mut no_progress());
        assert!(!matches!(result, Ok(Ensured::Reused(_))), "never reused: {result:?}");
    }

    /// (#3458 review) A record that cannot be written leaves no server
    /// running: the spawned process is killed, never orphaned.
    #[test]
    fn a_record_that_cannot_be_written_leaves_no_server() {
        let f = fixture();
        std::fs::create_dir_all(&f.dirs.records).unwrap();
        std::fs::create_dir_all(&f.dirs.logs).unwrap();
        // Make the record file's own name a directory, so the rename fails.
        std::fs::create_dir_all(engine_record::path_in(&f.dirs.records, "tf")).unwrap();
        let port = free_port();
        let err = ensure(&spec(&f, "tf", "m/a", port, &[]), &f.dirs, quick(), &not_held, &mut no_progress()).unwrap_err();
        assert_eq!(err.code(), "model_start_failed");
        std::thread::sleep(Duration::from_millis(300));
        assert!(!port_taken(port), "no server survives a failed record write");
    }

    /// (#3458 review) A dead server whose parent is another process (a
    /// zombie it has not reaped) is not darkmux's running server, and a stop
    /// of it ends at once instead of timing out.
    #[test]
    fn a_dead_server_another_process_has_not_reaped_is_not_running() {
        let f = fixture();
        // A parent that spawns a child and never reaps it: the child exits
        // at once and stays a zombie of that parent.
        let mut parent = Command::new("python3")
            .args(["-c", "import os,time,sys\npid=os.fork()\nif pid==0: os._exit(0)\nprint(pid,flush=True)\ntime.sleep(30)"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(parent.stdout.take().unwrap()), &mut line).unwrap();
        let zombie: u32 = line.trim().parse().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let stamp = darkmux_platform::current().process_start_stamp(zombie);
        let rec = EngineRecord {
            schema: ENGINE_RECORD_SCHEMA,
            endpoint: "tf".into(),
            engine: "tensorfold".into(),
            model: "m/a".into(),
            n_ctx: 8192,
            port: free_port(),
            pid: zombie,
            start_stamp: stamp.unwrap_or(1),
            owner_pid: parent.id(),
            argv: vec![],
            log: f.dirs.log_for("tf"),
            started_at_ms: 0,
            engine_version: String::new(),
        };
        engine_record::write_in(&f.dirs.records, &rec).unwrap();
        assert_eq!(running(&f.dirs, "tf"), None, "a zombie is not a running server");
        let began = Instant::now();
        stop(&f.dirs, &rec, quick()).unwrap();
        assert!(began.elapsed() < Duration::from_secs(1), "a stop of a dead server does not wait out the grace");
        let _ = parent.kill();
        let _ = parent.wait();
    }

    /// (#3458 re-review) A worker that ignores SIGTERM is SIGKILLed with its
    /// group even when the server itself exits on SIGTERM.
    #[test]
    fn a_worker_that_ignores_sigterm_dies_with_the_stop() {
        let f = fixture();
        let file = f.dirs.logs.with_extension("worker");
        let s = spec(&f, "tf", "m/a", free_port(), &["--stubborn-worker", file.to_str().unwrap()]);
        let rec = ensure(&s, &f.dirs, quick(), &not_held, &mut no_progress()).unwrap().record().clone();
        let worker: u32 = std::fs::read_to_string(&file).unwrap().trim().parse().unwrap();
        stop(&f.dirs, &rec, Bounds { stop_grace: Duration::from_millis(500), ..quick() }).unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        while pid_alive(worker) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        let survived = pid_alive(worker);
        if survived {
            let _ = darkmux_types::child_registry::kill_pid(worker, darkmux_types::child_registry::SIGKILL);
        }
        assert!(!survived, "the SIGTERM-ignoring worker died with the stop");
    }

    /// (#3458 re-review) A server that moved itself out of its own process
    /// group is still stopped, by its pid.
    #[test]
    fn a_server_that_left_its_group_is_still_stopped() {
        let f = fixture();
        let s = spec(&f, "tf", "m/a", free_port(), &["--leave-group"]);
        let rec = ensure(&s, &f.dirs, quick(), &not_held, &mut no_progress()).unwrap().record().clone();
        stop(&f.dirs, &rec, quick()).unwrap();
        assert_eq!(rec.state_now(), ServerState::Exited);
    }

    /// A process darkmux did not start, serving the stand-in's answer for
    /// `name` on `port` after `delay` seconds; killed when dropped.
    struct Foreign(Child);

    impl Drop for Foreign {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn foreign(f: &Fixture, port: u16, name: &str, delay: &str) -> Foreign {
        let child = Command::new("python3")
            .args(["-u", f.script.to_str().unwrap(), "serve", "x", "--port", &port.to_string(), "--name", name, "--slow", delay])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Foreign(child)
    }

    /// (#3453 review, probe 1) A foreign server that binds the port while
    /// darkmux's own server is still starting, answering under the very name
    /// darkmux waits for (every Flint server answers `flint`), is not
    /// darkmux's: the start fails `port_in_use`, darkmux's server is
    /// stopped and unrecorded, and the foreign one is untouched.
    #[test]
    fn a_foreign_server_that_binds_during_the_start_is_never_taken_as_ready() {
        let f = fixture();
        let port = free_port();
        let s = spec(&f, "fl", "m/a", port, &["--no-bind"]);
        let mut other = foreign(&f, port, &s.served_name, "0.3");
        let err = ensure(&s, &f.dirs, quick(), &not_held, &mut no_progress()).unwrap_err();
        assert_eq!(err.code(), "port_in_use", "{err}");
        assert!(err.to_string().contains(&format!("pid {}", other.0.id())), "it names the listener: {err}");
        assert_eq!(engine_record::read_in(&f.dirs.records, "fl").unwrap(), None, "darkmux's server is unrecorded");
        assert!(matches!(other.0.try_wait(), Ok(None)), "the foreign server is untouched");
        assert!(answers(port, &s.served_name), "and still serving");
    }

    /// (#3453 review, probe 2) darkmux's recorded server is alive and runs the
    /// same command but is not what listens on the port; a foreign server
    /// answering the same name is. It is never reused: darkmux stops its own
    /// and refuses the port.
    #[test]
    fn a_live_record_behind_a_foreign_listener_is_never_reused() {
        let f = fixture();
        let port = free_port();
        let s = spec(&f, "fl", "m/a", port, &["--no-bind"]);
        let ours = spawn_and_record(&s, &f.dirs).unwrap();
        let _other = foreign(&f, port, &s.served_name, "0");
        let until = Instant::now() + Duration::from_secs(10);
        while !answers(port, &s.served_name) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(alive(&ours) && answers(port, &s.served_name), "the fixture holds: our record live, a foreign answer");
        let result = ensure(&s, &f.dirs, quick(), &not_held, &mut no_progress());
        assert!(!matches!(result, Ok(Ensured::Reused(_))), "never reused: {result:?}");
        assert_eq!(result.unwrap_err().code(), "port_in_use");
        assert!(!alive(&ours), "darkmux's own non-listening server was stopped");
    }

    /// (#3453 review) `listens` is darkmux's server or a process in the group
    /// it leads, never another process on the port.
    #[test]
    fn listens_is_the_recorded_servers_group_and_nothing_else() {
        let f = fixture();
        let s = spec(&f, "fl", "m/a", free_port(), &[]);
        let rec = ensure(&s, &f.dirs, quick(), &not_held, &mut no_progress()).unwrap().record().clone();
        assert!(listens(&rec));
        let other_port = free_port();
        let _other = foreign(&f, other_port, "x", "0");
        let until = Instant::now() + Duration::from_secs(10);
        while !port_taken(other_port) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!listens(&EngineRecord { port: other_port, ..rec.clone() }), "a foreign listener is not darkmux's");
        assert!(!listens(&EngineRecord { port: free_port(), ..rec.clone() }), "nothing listening is not darkmux's");
        stop(&f.dirs, &rec, quick()).unwrap();
    }

    /// (#3453 re-review) `listens_by` against a fake host: the recorded pid or
    /// a process in its group is darkmux's; a foreign listener, nothing
    /// listening, or a host that cannot say (`None`) is not.
    #[test]
    fn listens_is_false_when_the_host_cannot_say() {
        use darkmux_platform::FakePlatform;
        let f = fixture();
        let rec = EngineRecord {
            schema: ENGINE_RECORD_SCHEMA,
            endpoint: "fl".into(),
            engine: "flint".into(),
            model: "m".into(),
            n_ctx: 8,
            port: 9100,
            pid: 40,
            start_stamp: 1,
            owner_pid: 1,
            argv: vec![],
            log: f.dirs.log_for("fl"),
            started_at_ms: 0,
            engine_version: String::new(),
        };
        let host = |listeners: Vec<u32>| FakePlatform {
            tcp_listeners: vec![(9100, listeners)],
            process_groups: vec![(41, 40), (99, 99)],
            ..Default::default()
        };
        assert!(listens_by(&rec, &host(vec![40])), "the server itself");
        assert!(listens_by(&rec, &host(vec![41])), "a worker in its group");
        assert!(!listens_by(&rec, &host(vec![41, 99])), "a foreign listener beside it");
        assert!(!listens_by(&rec, &host(vec![])), "nothing listening");
        let cannot_say = FakePlatform { tcp_listeners_unreadable: true, ..host(vec![40]) };
        assert!(!listens_by(&rec, &cannot_say), "a host that cannot say is not darkmux's");
    }

    /// (#3458 re-review) A server that left its group while a worker stayed
    /// in it is stopped too: the group and the server are each signaled.
    #[test]
    fn a_server_that_left_its_group_behind_a_worker_is_still_stopped() {
        let f = fixture();
        let s = spec(&f, "tf", "m/a", free_port(), &["--leave-group-keep-worker"]);
        let rec = ensure(&s, &f.dirs, quick(), &not_held, &mut no_progress()).unwrap().record().clone();
        stop(&f.dirs, &rec, Bounds { stop_grace: Duration::from_millis(500), ..quick() }).unwrap();
        assert_eq!(rec.state_now(), ServerState::Exited);
    }

    /// SIGKILLs the process group `0` leads when dropped, so a worker a
    /// failed assertion left behind does not outlive the test.
    struct KillGroup(u32);

    impl Drop for KillGroup {
        fn drop(&mut self) {
            let _ = darkmux_types::child_registry::kill_group(self.0, darkmux_types::child_registry::SIGKILL);
        }
    }

    /// A server whose leader exited while a worker it forked still serves
    /// its port: spawned and recorded, then waited on until the leader is
    /// gone and the worker answers.
    fn leader_gone_worker_serving(f: &Fixture, endpoint: &str, port: u16) -> (EngineRecord, KillGroup) {
        let rec = spawn_and_record(&spec(f, endpoint, "m/a", port, &["--daemonize"]), &f.dirs).unwrap();
        let guard = KillGroup(rec.pid);
        let until = Instant::now() + Duration::from_secs(10);
        while (alive(&rec) || !port_taken(port)) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(&rec) && port_taken(port), "the fixture holds: the leader is gone and its worker serves");
        (rec, guard)
    }

    /// A stop of a server whose leader already exited still takes down the
    /// workers left in its group: none outlives the stop holding the port.
    #[test]
    fn a_stop_takes_down_the_workers_a_gone_leader_left() {
        let f = fixture();
        let port = free_port();
        let (rec, _guard) = leader_gone_worker_serving(&f, "tf", port);
        stop(&f.dirs, &rec, quick()).unwrap();
        assert!(!port_taken(port), "the worker its leader left is gone with the stop");
    }

    /// darkmux's own server whose leader exited, its worker still on the
    /// port, is darkmux's to clear: a start on the endpoint stops that
    /// worker and starts, rather than refusing the port as someone else's.
    #[test]
    fn a_gone_leaders_worker_is_cleared_not_read_as_someone_elses() {
        let f = fixture();
        let port = free_port();
        let (rec, _guard) = leader_gone_worker_serving(&f, "tf", port);
        assert_eq!(running(&f.dirs, "tf"), None, "the gone leader is not a running server");
        assert_eq!(engine_record::read_in(&f.dirs.records, "tf").unwrap(), Some(rec), "its record stays for the start to clear");
        let started = ensure(&spec(&f, "tf", "m/b", port, &[]), &f.dirs, quick(), &not_held, &mut no_progress());
        let started = started.unwrap_or_else(|e| panic!("started: {e}"));
        assert!(matches!(&started, Ensured::Started(r) if r.model == "m/b"), "{started:?}");
        stop(&f.dirs, started.record(), quick()).unwrap();
    }

    /// One read that cannot say who listens is asked again once: a host
    /// that answers on the second read finds darkmux's server; two
    /// unreadable reads in a row are still not darkmux's.
    #[test]
    fn listens_asks_the_host_again_once_when_it_cannot_say() {
        use darkmux_platform::FakePlatform;
        let f = fixture();
        let rec = EngineRecord {
            schema: ENGINE_RECORD_SCHEMA,
            endpoint: "fl".into(),
            engine: "flint".into(),
            model: "m".into(),
            n_ctx: 8,
            port: 9100,
            pid: 40,
            start_stamp: 1,
            owner_pid: 1,
            argv: vec![],
            log: f.dirs.log_for("fl"),
            started_at_ms: 0,
            engine_version: String::new(),
        };
        let host = |unreadable: u32| FakePlatform {
            tcp_listeners: vec![(9100, vec![40])],
            tcp_listeners_unreadable_reads: std::sync::atomic::AtomicU32::new(unreadable),
            ..Default::default()
        };
        assert!(listens_by(&rec, &host(1)), "the second read answers");
        assert!(!listens_by(&rec, &host(2)), "two reads that cannot say are not darkmux's");
    }

    /// A server a running dispatch holds is never stopped for another
    /// command on its endpoint: the start is refused `busy`, naming the
    /// model it serves, and the server is untouched.
    #[test]
    fn a_held_server_is_never_replaced_by_another_command() {
        let f = fixture();
        let port = free_port();
        let a = ensure(&spec(&f, "tf", "m/a", port, &[]), &f.dirs, quick(), &not_held, &mut no_progress()).unwrap();
        let mut seen = Vec::new();
        let err = ensure(&spec(&f, "tf", "m/b", port, &[]), &f.dirs, quick(), &|_| true, &mut |p| seen.push(p)).unwrap_err();
        assert_eq!(err.code(), "busy", "{err}");
        assert!(err.to_string().contains("`m/a`"), "{err}");
        assert!(alive(a.record()), "the held server is untouched");
        assert!(seen.is_empty(), "nothing is replaced or started: {seen:?}");
        stop(&f.dirs, a.record(), quick()).unwrap();
    }

    /// A held server running the same command that does not answer under
    /// its name in time (busy) is reused, never stopped under the dispatch
    /// using it.
    #[test]
    fn a_held_server_that_does_not_answer_in_time_is_reused() {
        let f = fixture();
        let silent = spec(&f, "tf", "m/a", free_port(), &["--silent-name"]);
        let up = spawn_and_record(&silent, &f.dirs).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while !port_taken(silent.port) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        let again = ensure(&silent, &f.dirs, quick(), &|_| true, &mut no_progress()).unwrap();
        assert_eq!(again, Ensured::Reused(up.clone()));
        assert!(alive(&up));
        stop(&f.dirs, &up, quick()).unwrap();
    }

    /// An idle server is stopped to make room; one held by the time the
    /// endpoint's lock is taken is left running.
    #[test]
    fn a_stop_to_make_room_leaves_a_held_server_running() {
        let f = fixture();
        let a = ensure(&spec(&f, "tf", "m/a", free_port(), &[]), &f.dirs, quick(), &not_held, &mut no_progress()).unwrap();
        assert_eq!(stop_unless_held(&f.dirs, a.record(), quick(), &|_| true), Ok(false));
        assert!(alive(a.record()), "held: left running");
        assert_eq!(stop_unless_held(&f.dirs, a.record(), quick(), &not_held), Ok(true));
        assert_eq!(a.record().state_now(), ServerState::Exited);
    }

    /// A record whose pid now names another process is never read as
    /// having left workers, even when that process leads a group listening
    /// on the recorded port: its group is the new process's, and a stop
    /// signals nothing.
    #[test]
    fn a_reused_pids_group_on_the_port_is_never_signaled() {
        let f = fixture();
        let port = free_port();
        let mut other = Command::new("python3");
        other
            .args(["-u", f.script.to_str().unwrap(), "serve", "x", "--port", &port.to_string(), "--name", "x"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        std::os::unix::process::CommandExt::process_group(&mut other, 0);
        let other = Foreign(other.spawn().unwrap());
        let _guard = KillGroup(other.0.id());
        let until = Instant::now() + Duration::from_secs(10);
        while !port_taken(port) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        let pid = other.0.id();
        let stamp = darkmux_platform::current().process_start_stamp(pid).expect("its start stamp");
        let rec = EngineRecord {
            schema: ENGINE_RECORD_SCHEMA,
            endpoint: "tf".into(),
            engine: "tensorfold".into(),
            model: "m/a".into(),
            n_ctx: 8192,
            port,
            pid,
            start_stamp: stamp.wrapping_add(1),
            owner_pid: pid,
            argv: vec![],
            log: f.dirs.log_for("tf"),
            started_at_ms: 0,
            engine_version: String::new(),
        };
        assert_eq!(rec.state_now(), ServerState::PidReused);
        engine_record::write_in(&f.dirs.records, &rec).unwrap();
        stop(&f.dirs, &rec, quick()).unwrap();
        assert!(port_taken(port), "the other process's server still answers: never signaled");
    }

    /// A held server whose leader exited while its worker still serves the
    /// port is the dispatch's server all the same: another command on the
    /// endpoint is refused `busy` (asked under the lock), never started in
    /// its place; the same command is reused.
    #[test]
    fn a_held_server_whose_leader_is_gone_is_never_replaced() {
        let f = fixture();
        let port = free_port();
        let (rec, _guard) = leader_gone_worker_serving(&f, "tf", port);
        let before = darkmux_platform::current().tcp_listeners(port);
        let asked = std::cell::Cell::new(0);
        let held = |_: &EngineRecord| {
            asked.set(asked.get() + 1);
            true
        };
        let other = ensure(&spec(&f, "tf", "m/b", port, &[]), &f.dirs, quick(), &held, &mut no_progress());
        assert!(asked.get() > 0, "the lease was asked");
        assert_eq!(darkmux_platform::current().tcp_listeners(port), before, "the worker still serves the port");
        assert_eq!(other.map_err(|e| e.code()), Err("busy"));
        let same = ensure(&spec(&f, "tf", "m/a", port, &["--daemonize"]), &f.dirs, quick(), &held, &mut no_progress());
        assert_eq!(same, Ok(Ensured::Reused(rec)));
    }

    /// A signal to a process that is gone succeeds: ESRCH always, EPERM only
    /// for a zombie (asked only then); any other error, and EPERM for a live
    /// process, stays an error.
    #[test]
    fn a_signal_to_a_gone_process_is_success_and_nothing_else_is() {
        use darkmux_types::child_registry::{EPERM, ESRCH};
        let err = |code| Err(std::io::Error::from_raw_os_error(code));
        let never = || -> bool { panic!("zombie is asked only for EPERM") };
        assert!(ok_if_gone(Ok(()), never).is_ok());
        assert!(ok_if_gone(err(ESRCH), never).is_ok(), "no such process: already gone");
        assert!(ok_if_gone(err(EPERM), || true).is_ok(), "EPERM for a zombie: gone");
        assert_eq!(ok_if_gone(err(EPERM), || false).unwrap_err().raw_os_error(), Some(EPERM), "EPERM for a live process");
        // EINVAL (an invalid signal number): 22 on macOS and Linux alike.
        assert_eq!(ok_if_gone(err(22), never).unwrap_err().raw_os_error(), Some(22));
    }

    /// A child leading its own group, killed with it and reaped when dropped.
    struct Reaped(Child);

    impl Drop for Reaped {
        fn drop(&mut self) {
            let _ = darkmux_types::child_registry::kill_group(self.0.id(), darkmux_types::child_registry::SIGKILL);
            let _ = self.0.wait();
        }
    }

    /// A record for `pid` on `endpoint`, as darkmux writes one, read now.
    fn record_of(f: &Fixture, endpoint: &str, engine: &str, pid: u32, port: u16, argv: &[&str]) -> EngineRecord {
        let stamp = (0..50).find_map(|_| {
            std::thread::sleep(Duration::from_millis(10));
            darkmux_platform::current().process_start_stamp(pid)
        });
        EngineRecord {
            schema: ENGINE_RECORD_SCHEMA,
            endpoint: endpoint.into(),
            engine: engine.into(),
            model: "m/a".into(),
            n_ctx: 8192,
            port,
            pid,
            start_stamp: stamp.unwrap_or(1),
            owner_pid: std::process::id(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            log: f.dirs.log_for(endpoint),
            started_at_ms: 1_000,
            engine_version: "fake 1.2.4".into(),
        }
    }

    /// (#3450) A listing names darkmux's live servers, answering or still
    /// starting, and never a stale record, which it leaves on disk for a
    /// dispatch to clear.
    #[test]
    fn a_listing_names_darkmuxs_live_servers_and_changes_nothing() {
        let f = fixture();
        let s = spec(&f, "tf", "m/a", free_port(), &[]);
        let up = ensure(&s, &f.dirs, quick(), &not_held, &mut no_progress()).unwrap();
        // A server whose model is still starting: alive, not yet bound.
        let mut starting = Command::new("sleep");
        starting.arg("30");
        std::os::unix::process::CommandExt::process_group(&mut starting, 0);
        let starting = Reaped(starting.spawn().unwrap());
        let starting_rec = record_of(&f, "zz", "tensorfold", starting.0.id(), free_port(), &["tf", "--name", "darkmux:m/a"]);
        engine_record::write_in(&f.dirs.records, &starting_rec).unwrap();
        // A stale record: its process exited.
        let mut gone = Command::new("true").spawn().unwrap();
        let gone_pid = gone.id();
        gone.wait().unwrap();
        let stale = EngineRecord { start_stamp: 7, ..record_of(&f, "old", "tensorfold", gone_pid, free_port(), &[]) };
        engine_record::write_in(&f.dirs.records, &stale).unwrap();

        let listed = listed(&f.dirs);
        let rows: Vec<(&str, bool)> = listed.iter().map(|l| (l.endpoint.as_str(), l.answering)).collect();
        assert_eq!(rows, vec![("tf", true), ("zz", false)], "{listed:?}");
        let tf = &listed[0];
        assert_eq!((tf.model.as_str(), tf.served_name.as_str(), tf.port, tf.pid), ("m/a", "darkmux:m/a", s.port, up.record().pid));
        assert_eq!(tf.n_ctx, 8192);
        assert_eq!(listed[1].served_name, "darkmux:m/a");
        assert!(engine_record::read_in(&f.dirs.records, "old").unwrap().is_some(), "a listing removes nothing");
        stop(&f.dirs, up.record(), quick()).unwrap();
    }

    /// (#3450) rec's Flint takes no name, so its server answers `flint`;
    /// every other engine answers the `--name` darkmux started it under.
    #[test]
    fn a_servers_wire_name_is_flint_for_rec_and_its_name_flag_otherwise() {
        let f = fixture();
        let tf = record_of(&f, "tf", "tensorfold", 2, 1, &["tf", "serve", "m", "--name", "darkmux:m"]);
        assert_eq!(served_name(&tf), "darkmux:m");
        let fl = record_of(&f, "fl", "flint", 2, 1, &["rec-mlx", "serve", "flint", "--name", "x"]);
        assert_eq!(served_name(&fl), "flint");
        assert_eq!(served_name(&record_of(&f, "tf", "tensorfold", 2, 1, &["tf"])), "", "no name flag, no name");
    }

    /// (#3450) The server serving a profile model is the live one on its
    /// endpoint started with its id, never another endpoint's or model's.
    #[test]
    fn the_server_serving_a_model_is_its_endpoints_with_its_id() {
        let f = fixture();
        let mut live = Command::new("sleep");
        live.arg("30");
        std::os::unix::process::CommandExt::process_group(&mut live, 0);
        let live = Reaped(live.spawn().unwrap());
        engine_record::write_in(&f.dirs.records, &record_of(&f, "tf", "tensorfold", live.0.id(), 1, &[])).unwrap();
        let model = |id: &str, endpoint: &str| darkmux_types::ProfileModel {
            id: id.into(),
            endpoint: Some(darkmux_types::ModelEndpoint::reference(endpoint)),
            ..Default::default()
        };
        assert_eq!(serving(&f.dirs, &model("m/a", "tf")).map(|r| r.pid), Some(live.0.id()));
        assert_eq!(serving(&f.dirs, &model("m/b", "tf")), None, "another model on the endpoint");
        assert_eq!(serving(&f.dirs, &model("m/a", "other")), None, "another endpoint");
    }
}
