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
//!   darkmux started it under.
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

/// Why a server could not be started or stopped. Each has the stable
/// `code` an app branches on (DESIGN.md, "What an app reads").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerError {
    /// Something darkmux did not start answers on the endpoint's port.
    PortInUse { endpoint: String, port: u16 },
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
            ServerError::StartFailed { .. } => "model_start_failed",
            ServerError::StopFailed { .. } => "model_stop_failed",
        }
    }
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerError::PortInUse { endpoint, port } => write!(
                f,
                "endpoint `{endpoint}`: something darkmux did not start already answers on 127.0.0.1:{port}, and \
                 darkmux never takes over a server it did not start. Stop it, or give the endpoint another `port`"
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
fn alive(rec: &EngineRecord) -> bool {
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
/// belongs to someone else) is removed and reads as none; an unreadable one
/// claims nothing.
pub fn running(dirs: &Dirs, endpoint: &str) -> Option<EngineRecord> {
    let rec = engine_record::read_in(&dirs.records, endpoint).ok().flatten()?;
    if alive(&rec) {
        return Some(rec);
    }
    let _ = engine_record::remove_if_in(&dirs.records, &rec);
    None
}

/// Make `spec`'s server the one running on its endpoint, and wait until it
/// answers. See the module doc for the steps.
pub fn ensure(spec: &ServerSpec, dirs: &Dirs, bounds: Bounds, progress: &mut dyn FnMut(Progress)) -> Result<Ensured, ServerError> {
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
    if let Some(rec) = running(dirs, &spec.endpoint) {
        if rec.argv == spec.argv && answers(spec.port, &spec.served_name) {
            return Ok(Ensured::Reused(rec));
        }
        progress(Progress::Replacing { model: rec.model.clone() });
        stop_locked(dirs, &rec, bounds)?;
    }
    if port_taken(spec.port) {
        return Err(ServerError::PortInUse { endpoint: spec.endpoint.clone(), port: spec.port });
    }
    progress(Progress::Starting);
    let rec = spawn_and_record(spec, dirs).map_err(start_failed)?;
    wait_ready(spec, dirs, &rec, bounds, progress).map_err(|detail| {
        let _ = stop_locked(dirs, &rec, bounds);
        start_failed(detail)
    })?;
    Ok(Ensured::Started(rec))
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

fn wait_ready(spec: &ServerSpec, dirs: &Dirs, rec: &EngineRecord, bounds: Bounds, progress: &mut dyn FnMut(Progress)) -> Result<(), String> {
    let began = Instant::now();
    let mut last_beat = began;
    loop {
        if !alive(rec) {
            return Err(format!("it exited during its start{}", log_tail(&dirs.log_for(&spec.endpoint))));
        }
        if answers(spec.port, &spec.served_name) {
            progress(Progress::Ready { elapsed: began.elapsed() });
            return Ok(());
        }
        if began.elapsed() >= bounds.start {
            return Err(format!("it did not answer within {} s", bounds.start.as_secs()));
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
    let lock_path = engine_record::path_in(&dirs.records, &rec.endpoint).with_extension("server.lock");
    let _lock = darkmux_types::flock::lock_exclusive(&lock_path)
        .map_err(|e| ServerError::StopFailed { endpoint: rec.endpoint.clone(), pid: rec.pid, detail: format!("{e:#}") })?;
    stop_locked(dirs, rec, bounds)
}

/// [`stop`] with the lock already held. Signals only a process the record
/// still matches; a stale record is just removed.
fn stop_locked(dirs: &Dirs, rec: &EngineRecord, bounds: Bounds) -> Result<(), ServerError> {
    use darkmux_types::child_registry::{kill_group, SIGKILL, SIGTERM};
    let failed = |detail: String| ServerError::StopFailed { endpoint: rec.endpoint.clone(), pid: rec.pid, detail };
    for (sig, name) in [(SIGTERM, "SIGTERM"), (SIGKILL, "SIGKILL")] {
        if !alive(rec) {
            break;
        }
        // The server leads its own process group (`spawn_and_record`), so
        // the group is the server and every worker it forked.
        kill_group(rec.pid, sig).map_err(|e| failed(format!("{name}: {e}")))?;
        if gone_within(rec, bounds) {
            break;
        }
    }
    if alive(rec) {
        return Err(failed("it is still running after SIGKILL".into()));
    }
    let until = Instant::now() + bounds.stop_grace;
    while port_taken(rec.port) && Instant::now() < until {
        std::thread::sleep(bounds.poll);
    }
    let _ = engine_record::remove_if_in(&dirs.records, rec);
    Ok(())
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
fn port_taken(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Whether the server on `port` lists `served_name` at `GET /v1/models`.
fn answers(port: u16, served_name: &str) -> bool {
    let agent = ureq::AgentBuilder::new().redirects(0).timeout(Duration::from_secs(2)).build();
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
if "--silent-name" in a:
    name = "someone-else"
class H(BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({"object": "list", "data": [{"id": name}]}).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json"); self.end_headers(); self.wfile.write(body)
    def log_message(self, *args): pass
HTTPServer(("127.0.0.1", port), H).serve_forever()
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
                    let _ = darkmux_types::child_registry::kill_pid(rec.pid, darkmux_types::child_registry::SIGKILL);
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

    /// Start, reuse, stop: a second ensure of the same command reuses the
    /// server; a stop takes it down, frees the port and removes the record.
    #[test]
    fn a_server_starts_is_reused_and_stops() {
        let f = fixture();
        let s = spec(&f, "tf", "m/a", free_port(), &[]);
        let mut seen = Vec::new();
        let first = ensure(&s, &f.dirs, quick(), &mut |p| seen.push(p)).unwrap();
        let Ensured::Started(rec) = first else { panic!("started: {first:?}") };
        assert!(matches!(seen.first(), Some(Progress::Starting)) && matches!(seen.last(), Some(Progress::Ready { .. })), "{seen:?}");
        assert!(answers(s.port, &s.served_name));
        assert_eq!(running(&f.dirs, "tf"), Some(rec.clone()));
        let again = ensure(&s, &f.dirs, quick(), &mut no_progress()).unwrap();
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
        let a = ensure(&spec(&f, "tf", "m/a", port, &[]), &f.dirs, quick(), &mut no_progress()).unwrap();
        let mut seen = Vec::new();
        let b = ensure(&spec(&f, "tf", "m/b", port, &[]), &f.dirs, quick(), &mut |p| seen.push(p)).unwrap();
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
        let a = ensure(&spec(&f, "tf", "m/a", port, &[]), &f.dirs, quick(), &mut no_progress()).unwrap();
        let b = ensure(&spec(&f, "tf", "m/a", port, &["--keep-warm"]), &f.dirs, quick(), &mut no_progress()).unwrap();
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
        let err = ensure(&spec(&f, "tf", "m/a", port, &[]), &f.dirs, quick(), &mut no_progress()).unwrap_err();
        assert_eq!(err.code(), "port_in_use");
        assert!(err.to_string().contains(&port.to_string()), "{err}");
        assert_eq!(engine_record::read_in(&f.dirs.records, "tf").unwrap(), None);
    }

    /// A server that exits during its start fails the start naming its log
    /// and its last output, and leaves no record.
    #[test]
    fn a_server_that_exits_during_its_start_fails_naming_its_log() {
        let f = fixture();
        let err = ensure(&spec(&f, "tf", "m/a", free_port(), &["--fail"]), &f.dirs, quick(), &mut no_progress()).unwrap_err();
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
        let err = ensure(&s, &f.dirs, bounds, &mut no_progress()).unwrap_err();
        assert!(err.to_string().contains("did not answer within"), "{err}");
        assert_eq!(engine_record::read_in(&f.dirs.records, "tf").unwrap(), None, "no record survives a failed start");
    }

    /// A server that ignores SIGTERM is SIGKILLed after the grace.
    #[test]
    fn a_server_that_ignores_sigterm_is_killed_after_the_grace() {
        let f = fixture();
        let s = spec(&f, "tf", "m/a", free_port(), &["--ignore-term"]);
        let rec = ensure(&s, &f.dirs, quick(), &mut no_progress()).unwrap().record().clone();
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
        let rec = ensure(&s, &f.dirs, quick(), &mut no_progress()).unwrap().record().clone();
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
        let result = ensure(&silent, &f.dirs, Bounds { start: Duration::from_millis(800), ..quick() }, &mut no_progress());
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
        let err = ensure(&spec(&f, "tf", "m/a", port, &[]), &f.dirs, quick(), &mut no_progress()).unwrap_err();
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
}
