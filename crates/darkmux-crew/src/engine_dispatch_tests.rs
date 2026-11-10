//! (#3448) `prepare` against a stand-in Tensorfold binary and a fake
//! machine: no real engine, no real LM Studio, no real memory reading.

use super::*;
use darkmux_types::engine_record::ServerState;
use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

/// What the stand-in prints for `capabilities --json` and `serve --help`: a
/// few flags, `--keep-warm` taking a value.
const FAKE_CAPS: &str = r#"{"schema":1,"serve":{"flags":{"--host":{},"--port":{},"--name":{},"--context":{},"--allowed-host":{},"--no-update-check":{},"--keep-warm":{},"--slide":{}}}}"#;
const FAKE_USAGE: &str = "usage: tensorfold serve [-h] [--host HOST] [--port PORT] [--name NAME] [--allowed-host NAME] [--context CONTEXT] [--keep-warm SECONDS] [--slide] [--no-update-check] model";

/// A stand-in `tensorfold-native`: `--version`, `info` (with the memory
/// floor the test asks for), `capabilities --json` and `serve --help`
/// ([`FAKE_CAPS`], [`FAKE_USAGE`]), and `serve`, which runs a tiny server
/// answering `GET /v1/models` with its `--name`.
fn fake_tensorfold(dir: &std::path::Path, version: &str, floor_gib: f64) -> PathBuf {
    // Stable scripts behind a per-test link (see `stable_script`): a freshly
    // written executable can wait seconds for macOS to assess it.
    let server = stable_script(
        "tf_serve.py",
        r#"
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
a = sys.argv[1:]
port = int(a[a.index("--port") + 1]); name = a[a.index("--name") + 1]
if "--slide" in a:
    # A slow start: the server answers 1.5 s after it is spawned.
    import time; time.sleep(1.5)
class H(BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({"object": "list", "data": [{"id": name}]}).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json"); self.end_headers(); self.wfile.write(body)
    def log_message(self, *args): pass
class S(HTTPServer):
    def server_bind(self):
        import socketserver
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = "localhost", self.server_address[1]
S(("127.0.0.1", port), H).serve_forever()
"#,
    );
    let stable = stable_script(
        "tensorfold-native",
        &format!(
            "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'tensorfold-native {version}' ;;\n  info) printf 'weights: 1.00 GiB (1073741824 bytes)\\nmemory floor: {floor_gib:.2} GiB (weights plus a margin)\\n' ;;\n  capabilities) echo '{FAKE_CAPS}' ;;\n  serve) if [ \"$2\" = --help ]; then echo '{FAKE_USAGE}'; exit 0; fi; shift; exec python3 -u {} serve \"$@\" ;;\nesac\n",
            server.display()
        ),
    );
    let bin = dir.join("tensorfold-native");
    let _ = std::fs::remove_file(&bin);
    std::os::unix::fs::symlink(&stable, &bin).unwrap();
    bin
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn target(model: &str, port: u16) -> crate::target::Target {
    target_with_args(model, port, &[])
}

fn target_with_args(model: &str, port: u16, engine_args: &[&str]) -> crate::target::Target {
    let mut reg: darkmux_types::ProfileRegistry = serde_json::from_value(serde_json::json!({
        "profiles": {"p": {"models": [{"id": model, "n_ctx": 8192, "endpoint": "tf", "engine_args": engine_args}]}},
        "endpoints": {"tf": {"managed": "tensorfold", "port": port}}
    }))
    .unwrap();
    reg.materialize_endpoints();
    let profile = reg.profiles["p"].clone();
    let m = profile.models[0].clone();
    crate::target::target_for("p".into(), profile, m).unwrap()
}

/// A machine whose memory and LM Studio the test sets.
#[derive(Default)]
struct FakeMachine {
    reclaimable: RefCell<Option<u64>>,
    /// Bytes each unload frees.
    frees_per_unload: u64,
    lms: RefCell<Vec<String>>,
    unloaded: RefCell<Vec<String>>,
    /// How many times LM Studio's residents were listed.
    listed: RefCell<u32>,
    leased: Vec<String>,
    /// The machine's memory; `None` = unknown.
    capacity: Option<u64>,
    /// The standing utility binding (#3507).
    utility: Option<String>,
}

impl Machine for FakeMachine {
    fn reclaimable(&self) -> Option<u64> {
        *self.reclaimable.borrow()
    }
    fn footprint(&self, _pid: u32) -> Option<u64> {
        None
    }
    fn lmstudio_residents(&self) -> Vec<String> {
        *self.listed.borrow_mut() += 1;
        self.lms.borrow().clone()
    }
    fn lmstudio_unload(&self, identifier: &str) -> anyhow::Result<()> {
        self.lms.borrow_mut().retain(|m| m != identifier);
        self.unloaded.borrow_mut().push(identifier.to_string());
        let mut r = self.reclaimable.borrow_mut();
        *r = r.map(|b| b + self.frees_per_unload);
        Ok(())
    }
    fn leased(&self, _own: &LeaseGuard) -> Vec<String> {
        self.leased.clone()
    }
    fn capacity(&self) -> Option<u64> {
        self.capacity
    }
    fn utility(&self) -> Option<String> {
        self.utility.clone()
    }
}

struct Rig {
    dir: tempfile::TempDir,
    dirs: Dirs,
    /// `DARKMUX_HOME` before the rig pointed it at `dir` (leases land there,
    /// never in the operator's real home).
    prev_home: Option<String>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        unsafe {
            match self.prev_home.take() {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        // A failed assertion must leave no stand-in server up: kill each
        // running one's whole group (it leads one), then the pid itself.
        for rec in engine_record::list_in(&self.dirs.records).0 {
            if rec.state_now() == ServerState::Running {
                use darkmux_types::child_registry::{kill_group, kill_pid, SIGKILL};
                let _ = kill_group(rec.pid, SIGKILL).or_else(|_| kill_pid(rec.pid, SIGKILL));
            }
        }
    }
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let dirs = Dirs { records: dir.path().join("run"), logs: dir.path().join("logs") };
    let prev_home = std::env::var("DARKMUX_HOME").ok();
    unsafe { std::env::set_var("DARKMUX_HOME", dir.path().join("home")) };
    Rig { dir, dirs, prev_home }
}

fn quick() -> server::Bounds {
    server::Bounds { start: Duration::from_secs(20), stop_grace: Duration::from_secs(2), poll: Duration::from_millis(50) }
}

const GIB: u64 = 1 << 30;

/// The first dispatch starts the server and says so (start, then ready);
/// the next one finds it running and writes no model record at all.
#[test]
#[serial_test::serial]
fn a_dispatch_starts_the_server_once_and_reuses_it_after() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 2.0).to_string_lossy().into_owned());
    let machine = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let t = target("TensorFold/m", free_port());
    let mut notes = Vec::new();
    let ready = prepare_in(&t, &r.dirs, &tf, quick(), &machine, &mut |n, p| notes.push((n, p))).unwrap();
    assert!(ready.started);
    assert_eq!(*machine.listed.borrow(), 0, "with room already, LM Studio is never asked");
    let kinds: Vec<Note> = notes.iter().map(|(n, _)| *n).collect();
    assert_eq!(kinds.first(), Some(&Note::Start));
    assert_eq!(kinds.last(), Some(&Note::Ready));
    assert_eq!(notes[0].1.endpoint, "tf");
    assert_eq!(notes[0].1.model, "TensorFold/m");
    assert!(notes.last().unwrap().1.elapsed_ms.is_some());
    drop(ready);
    let mut again = Vec::new();
    let reused = prepare_in(&t, &r.dirs, &tf, quick(), &machine, &mut |n, p| again.push((n, p))).unwrap();
    assert!(!reused.started, "the same command is reused");
    assert!(again.is_empty(), "a resident model writes no model record: {again:?}");
}

/// An unverified engine release fails `engine_unavailable` before anything
/// starts.
#[test]
#[serial_test::serial]
fn an_unverified_release_is_engine_unavailable() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.3.0", 2.0).to_string_lossy().into_owned());
    let machine = FakeMachine::default();
    let err = prepare_in(&target("TensorFold/m", free_port()), &r.dirs, &tf, quick(), &machine, &mut |_, _| {})
        .err()
        .expect("refused");
    assert_eq!(err.code, "engine_unavailable");
    assert!(err.message.contains("1.3.0"), "{err}");
    assert_eq!(engine_record::list_in(&r.dirs.records).0.len(), 0);
}

/// Short of memory, darkmux unloads its own idle LM Studio loads until the
/// model fits, never one a running dispatch holds, and names what it
/// evicted on the start record.
#[test]
#[serial_test::serial]
fn room_is_made_from_darkmuxs_idle_residents_only() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 20.0).to_string_lossy().into_owned());
    let machine = FakeMachine {
        reclaimable: RefCell::new(Some(8 * GIB)),
        frees_per_unload: 8 * GIB,
        lms: RefCell::new(vec!["darkmux:held".into(), "darkmux:a".into(), "darkmux:b".into()]),
        leased: vec!["darkmux:held".into()],
        ..Default::default()
    };
    let mut notes = Vec::new();
    let ready = prepare_in(&target("TensorFold/m", free_port()), &r.dirs, &tf, quick(), &machine, &mut |n, p| notes.push((n, p)))
        .unwrap();
    assert!(ready.started);
    let unloaded = machine.unloaded.borrow().clone();
    assert_eq!(unloaded.len(), 2, "two 8 GiB unloads take 8 GiB free past the 20 GiB floor: {unloaded:?}");
    assert!(!unloaded.contains(&"darkmux:held".to_string()), "a held resident is never evicted");
    let start = &notes.iter().find(|(n, _)| *n == Note::Start).unwrap().1;
    assert_eq!(start.evicted.len(), 2, "{start:?}");
}

/// With nothing idle left to free: `busy` when a running dispatch holds
/// memory that would free later, `wont_fit` when nothing would.
#[test]
#[serial_test::serial]
fn nothing_left_to_free_is_busy_or_memory_in_use_and_only_a_model_larger_than_the_machine_wont_fit() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 100.0).to_string_lossy().into_owned());
    let held = FakeMachine {
        reclaimable: RefCell::new(Some(8 * GIB)),
        lms: RefCell::new(vec!["darkmux:held".into()]),
        leased: vec!["darkmux:held".into()],
        ..Default::default()
    };
    let err = prepare_in(&target("TensorFold/m", free_port()), &r.dirs, &tf, quick(), &held, &mut |_, _| {})
        .err()
        .expect("refused");
    assert_eq!(err.code, "busy");
    assert!(err.message.contains("darkmux:held"), "{err}");
    // (#3510) Programs darkmux did not start hold the memory on a machine
    // the model fits: retryable, never `wont_fit`.
    let alone = FakeMachine { reclaimable: RefCell::new(Some(8 * GIB)), capacity: Some(128 * GIB), ..Default::default() };
    let err = prepare_in(&target("TensorFold/m", free_port()), &r.dirs, &tf, quick(), &alone, &mut |_, _| {})
        .err()
        .expect("refused");
    assert_eq!(err.code, "memory_in_use", "{err}");
    assert!(err.message.contains("100.0 GiB") && err.message.contains("did not start"), "{err}");
    // Only a model larger than the whole machine is `wont_fit`, and that is
    // decided before free memory is read: unread, it still says so.
    let small = FakeMachine { capacity: Some(64 * GIB), ..Default::default() };
    let err = prepare_in(&target("TensorFold/m", free_port()), &r.dirs, &tf, quick(), &small, &mut |_, _| {})
        .err()
        .expect("refused");
    assert_eq!(err.code, "wont_fit", "{err}");
    assert!(err.message.contains("64.0 GiB in all"), "{err}");
}

/// (#3510) A figure darkmux cannot read never lets a start through: no free
/// reading (on a machine the model fits) and no need figure both refuse,
/// naming what is missing, and nothing is started.
#[test]
#[serial_test::serial]
fn a_missing_memory_figure_refuses_rather_than_starting_blind() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 2.0).to_string_lossy().into_owned());
    let unread = FakeMachine { capacity: Some(128 * GIB), ..Default::default() };
    let err = prepare_in(&target("TensorFold/m", free_port()), &r.dirs, &tf, quick(), &unread, &mut |_, _| {})
        .err()
        .expect("refused");
    assert_eq!(err.code, "model_start_failed", "{err}");
    assert!(err.message.contains("free memory"), "{err}");
    let rec = fake_rec(r.dir.path(), "0.1.0");
    let bare = r.dir.path().join("share/rec/models/bb22");
    std::fs::create_dir_all(&bare).unwrap();
    std::fs::write(bare.join("flint-bare.json"), "{}").unwrap();
    let roomy = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let err = prepare_in(&flint_target("flint-bare", free_port()), &r.dirs, &rec, quick(), &roomy, &mut |_, _| {})
        .err()
        .expect("refused");
    assert_eq!(err.code, "model_start_failed", "{err}");
    assert!(err.message.contains("no memory figure"), "{err}");
    assert!(engine_record::list_in(&r.dirs.records).0.is_empty(), "nothing started");
}

/// A refusal's code is readable back off the dispatch's error.
#[test]
#[serial_test::serial]
fn a_refusal_code_survives_the_error_chain() {
    let e: anyhow::Error = refuse("busy", "x").into();
    let e = e.context("darkmux dispatch: making the model resident");
    assert_eq!(refusal_code(&e), Some("busy"));
    assert_eq!(refusal_code(&anyhow::anyhow!("other")), None);
}

/// A model already resident under the same command is reused without any
/// memory planning: even with memory short and an idle darkmux load to take,
/// nothing is evicted for a server that is already up.
#[test]
#[serial_test::serial]
fn a_resident_model_evicts_nothing_even_when_memory_is_short() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 20.0).to_string_lossy().into_owned());
    let t = target("TensorFold/m", free_port());
    let roomy = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    drop(prepare_in(&t, &r.dirs, &tf, quick(), &roomy, &mut |_, _| {}).unwrap());
    let short = FakeMachine {
        reclaimable: RefCell::new(Some(GIB)),
        frees_per_unload: 32 * GIB,
        lms: RefCell::new(vec!["darkmux:idle".into()]),
        ..Default::default()
    };
    let again = prepare_in(&t, &r.dirs, &tf, quick(), &short, &mut |_, _| {}).unwrap();
    assert!(!again.started);
    assert!(short.unloaded.borrow().is_empty(), "nothing evicted for a resident model: {:?}", short.unloaded.borrow());
}


/// (#3448 PR 4 review) A server another dispatch is using on this
/// dispatch's own endpoint is never replaced under it: the dispatch is
/// `busy`, and the running server is untouched.
#[test]
#[serial_test::serial]
fn a_held_server_on_the_same_endpoint_is_busy_never_replaced() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 2.0).to_string_lossy().into_owned());
    let port = free_port();
    let roomy = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let a = prepare_in(&target("TensorFold/a", port), &r.dirs, &tf, quick(), &roomy, &mut |_, _| {}).unwrap();
    let held = FakeMachine {
        reclaimable: RefCell::new(Some(64 * GIB)),
        leased: vec!["darkmux:TensorFold/a".into()],
        ..Default::default()
    };
    let err = prepare_in(&target("TensorFold/b", port), &r.dirs, &tf, quick(), &held, &mut |_, _| {})
        .err()
        .expect("refused");
    assert_eq!(err.code, "busy");
    assert!(err.message.contains("TensorFold/a"), "{err}");
    assert!(server::alive(&a.record), "the held server is untouched");
}

/// (#3448 PR 4 review) An idle engine server on another endpoint is evicted
/// to make room; one a running dispatch holds is not, and the dispatch is
/// `busy` instead.
#[test]
#[serial_test::serial]
fn an_idle_server_elsewhere_is_evicted_and_a_held_one_is_not() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 20.0).to_string_lossy().into_owned());
    let roomy = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let mut other = target("TensorFold/o", free_port());
    other.endpoint.source = darkmux_types::endpoint::EndpointSource::Named("other".into());
    let o = prepare_in(&other, &r.dirs, &tf, quick(), &roomy, &mut |_, _| {}).unwrap();
    let o_rec = o.record.clone();
    drop(o);
    let held = FakeMachine {
        reclaimable: RefCell::new(Some(GIB)),
        leased: vec!["darkmux:TensorFold/o".into()],
        ..Default::default()
    };
    let err = prepare_in(&target("TensorFold/m", free_port()), &r.dirs, &tf, quick(), &held, &mut |_, _| {})
        .err()
        .expect("refused");
    assert_eq!(err.code, "busy", "{err}");
    assert!(server::alive(&o_rec), "a held server elsewhere is never evicted");
    // Idle now: the same need evicts it, and the start record names it.
    struct Frees<'a>(&'a FakeMachine, &'a EngineRecord);
    impl Machine for Frees<'_> {
        fn reclaimable(&self) -> Option<u64> {
            Some(if server::alive(self.1) { GIB } else { 64 * GIB })
        }
        fn footprint(&self, _pid: u32) -> Option<u64> {
            None
        }
        fn lmstudio_residents(&self) -> Vec<String> {
            self.0.lmstudio_residents()
        }
        fn lmstudio_unload(&self, id: &str) -> anyhow::Result<()> {
            self.0.lmstudio_unload(id)
        }
        fn leased(&self, own: &LeaseGuard) -> Vec<String> {
            let _ = own;
            Vec::new()
        }
        fn capacity(&self) -> Option<u64> {
            None
        }
    }
    let idle = FakeMachine::default();
    let mut notes = Vec::new();
    let ready = prepare_in(&target("TensorFold/m", free_port()), &r.dirs, &tf, quick(), &Frees(&idle, &o_rec), &mut |n, p| {
        notes.push((n, p))
    })
    .unwrap();
    assert!(ready.started);
    assert!(!server::alive(&o_rec), "the idle server elsewhere was evicted");
    let start = &notes.iter().find(|(n, _)| *n == Note::Start).unwrap().1;
    assert_eq!(start.evicted, ["TensorFold/o (other)"]);
}

/// (#3448 PR 4 review) A model larger than the whole machine is `wont_fit`
/// even while something is held (retrying could never help), and nothing is
/// evicted for it.
#[test]
#[serial_test::serial]
fn a_model_larger_than_the_machine_is_wont_fit_and_evicts_nothing() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 200.0).to_string_lossy().into_owned());
    let machine = FakeMachine {
        reclaimable: RefCell::new(Some(8 * GIB)),
        lms: RefCell::new(vec!["darkmux:idle".into(), "darkmux:held".into()]),
        leased: vec!["darkmux:held".into()],
        capacity: Some(128 * GIB),
        ..Default::default()
    };
    let err = prepare_in(&target("TensorFold/m", free_port()), &r.dirs, &tf, quick(), &machine, &mut |_, _| {})
        .err()
        .expect("refused");
    assert_eq!(err.code, "wont_fit", "{err}");
    assert!(machine.unloaded.borrow().is_empty(), "nothing evicted for a model that cannot fit");
}

/// (#3448 PR 4 review) The dispatch holds a lease on its served model for as
/// long as it runs, visible to every other dispatch in this process.
#[test]
#[serial_test::serial]
fn the_dispatch_holds_a_lease_on_its_model_while_it_runs() {
    let r = rig();
    let home = tempfile::tempdir().unwrap();
    let prev = std::env::var("DARKMUX_HOME").ok();
    unsafe { std::env::set_var("DARKMUX_HOME", home.path()) };
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 2.0).to_string_lossy().into_owned());
    let roomy = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let ready = prepare_in(&target("TensorFold/m", free_port()), &r.dirs, &tf, quick(), &roomy, &mut |_, _| {}).unwrap();
    let sibling = LeaseGuard::acquire();
    let seen_while_running = sibling.all_live_leased_models();
    drop(ready);
    let seen_after = sibling.all_live_leased_models();
    unsafe {
        match prev {
            Some(v) => std::env::set_var("DARKMUX_HOME", v),
            None => std::env::remove_var("DARKMUX_HOME"),
        }
    }
    assert!(seen_while_running.contains(&"darkmux:TensorFold/m".to_string()), "{seen_while_running:?}");
    assert!(!seen_after.contains(&"darkmux:TensorFold/m".to_string()), "released when the dispatch ends");
}

/// (#3448 PR 4 review) On this machine, a sibling dispatch in the SAME
/// process is counted as holding its model (the LM Studio path's #2663 rule).
#[test]
#[serial_test::serial]
fn this_machine_counts_a_sibling_dispatch_in_the_same_process() {
    let home = tempfile::tempdir().unwrap();
    let prev = std::env::var("DARKMUX_HOME").ok();
    unsafe { std::env::set_var("DARKMUX_HOME", home.path()) };
    let sibling = LeaseGuard::acquire();
    sibling.write(&["darkmux:sibling".to_string()]).unwrap();
    let own = LeaseGuard::acquire();
    let leased = ThisMachine { profiles_file: None }.leased(&own);
    drop(sibling);
    unsafe {
        match prev {
            Some(v) => std::env::set_var("DARKMUX_HOME", v),
            None => std::env::remove_var("DARKMUX_HOME"),
        }
    }
    assert!(leased.contains(&"darkmux:sibling".to_string()), "{leased:?}");
}

// ─── (#3453) rec's Flint ────────────────────────────────────────────────────

/// A script at a stable path, written once per content: `exec` of a freshly
/// written file is assessed by macOS one file at a time (about 0.1 s each,
/// machine-wide), so a parallel run of tests that each write their own
/// stand-ins can push a 5 s probe past its bound. Each test links to these
/// instead, and the link's own path (`$0`) says where its state is.
pub(crate) fn stable_script(name: &str, content: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    use std::os::unix::fs::PermissionsExt;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut h);
    let dir = std::env::temp_dir().join("darkmux-test-stand-ins");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{:016x}-{name}", h.finish()));
    if !path.exists() {
        let tmp = dir.join(format!(".{name}.{}", std::process::id()));
        std::fs::write(&tmp, content).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&tmp, &path).unwrap();
    }
    path
}

/// The stand-in `rec-mlx serve flint ... --port P`: answers `GET /v1/models`
/// with `flint` (as rec's server does; it has no `--name`) and records each
/// `POST` body in `$REQUESTS` beside the wrapper's link when that file's
/// directory exists. Its wrapper `exec`s Python, as rec's does, so the
/// spawned pid is the server.
pub(crate) const REC_MLX_PY: &str = r#"
import json, os, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
a = sys.argv[2:]
assert a[:2] == ["serve", "flint"], a
port = int(a[a.index("--port") + 1])
requests = sys.argv[1]
class H(BaseHTTPRequestHandler):
    def reply(self, doc):
        body = json.dumps(doc).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def do_GET(self):
        self.reply({"object": "list", "data": [{"id": "flint"}]})
    def do_POST(self):
        req = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        open(requests, "ab").write(req + b"\n")
        self.reply({"choices": [{"message": {"content": "ok"}}], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
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

/// A stand-in rec 0.1.0 install under `dir`: `share/rec/runtimes/<sha>/rec-mlx`
/// (a link to a wrapper that `exec`s [`REC_MLX_PY`], recording requests in
/// `dir/requests.jsonl`), `share/rec/models/<hash>/flint-int4.json` beside
/// one small weights file, and a `rec` (a link) whose `--version` prints
/// `rec <dir/version>` and whose `status` prints `dir/status.json` (absent
/// prints nothing). Writing those two files changes what `rec` says.
pub(crate) fn fake_rec(dir: &std::path::Path, version: &str) -> darkmux_profiles::engine::flint::Rec {
    let data = dir.join("share").join("rec");
    let runtime = data.join("runtimes").join("0906aa");
    std::fs::create_dir_all(&runtime).unwrap();
    let py = stable_script("rec_mlx.py", REC_MLX_PY);
    let wrapper = stable_script(
        "rec-mlx",
        &format!(
            "#!/bin/sh\nhere=$(dirname \"$0\")\nexec python3 -u {} \"$here/../../../../requests.jsonl\" \"$@\"\n",
            py.display()
        ),
    );
    let link = runtime.join("rec-mlx");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&wrapper, &link).unwrap();
    let model = data.join("models").join("fdc852");
    std::fs::create_dir_all(&model).unwrap();
    std::fs::write(model.join("flint-int4.json"), "{}").unwrap();
    std::fs::write(model.join("model-00000.safetensors"), vec![0u8; 1024]).unwrap();
    std::fs::write(dir.join("version"), version).unwrap();
    let rec = stable_script(
        "rec",
        "#!/bin/sh\nhere=$(dirname \"$0\")\ncase \"$1\" in\n  --version) echo \"rec $(cat \"$here/version\")\" ;;\n  status) cat \"$here/status.json\" 2>/dev/null ;;\nesac\n",
    );
    let bin = dir.join("rec");
    let _ = std::fs::remove_file(&bin);
    std::os::unix::fs::symlink(&rec, &bin).unwrap();
    darkmux_profiles::engine::flint::Rec::at(bin.to_string_lossy().into_owned(), Some(data))
}

fn flint_target(id: &str, port: u16) -> crate::target::Target {
    let mut reg: darkmux_types::ProfileRegistry = serde_json::from_value(serde_json::json!({
        "profiles": {"p": {"models": [{"id": id, "n_ctx": 8192, "endpoint": "fl"}]}},
        "endpoints": {"fl": {"managed": "flint", "port": port}}
    }))
    .unwrap();
    reg.materialize_endpoints();
    let profile = reg.profiles["p"].clone();
    let m = profile.models[0].clone();
    crate::target::target_for("p".into(), profile, m).unwrap()
}

/// A Flint dispatch starts rec's own `rec-mlx serve flint` (never `rec
/// _supervise`) for the profile's revision, waits until it lists `flint`,
/// and holds a lease on `flint`; the next dispatch reuses it.
#[test]
#[serial_test::serial]
fn a_flint_dispatch_starts_recs_server_once_and_reuses_it() {
    let r = rig();
    let rec = fake_rec(r.dir.path(), "0.1.0");
    let roomy = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let t = flint_target("flint-int4", free_port());
    let mut notes = Vec::new();
    let ready = prepare_in(&t, &r.dirs, &rec, quick(), &roomy, &mut |n, p| notes.push((n, p))).unwrap();
    assert!(ready.started);
    assert!(ready.record.argv[0].ends_with("runtimes/0906aa/rec-mlx"), "{:?}", ready.record.argv);
    assert_eq!(&ready.record.argv[1..3], ["serve", "flint"]);
    assert_eq!((ready.record.engine.as_str(), ready.record.model.as_str()), ("flint", "flint-int4"));
    assert_eq!(notes.first().map(|(n, p)| (*n, p.engine.as_str())), Some((Note::Start, "flint")));
    let sibling = LeaseGuard::acquire();
    assert!(sibling.all_live_leased_models().contains(&"flint".to_string()), "the lease names the served model `flint`");
    drop(ready);
    let again = prepare_in(&t, &r.dirs, &rec, quick(), &roomy, &mut |_, _| {}).unwrap();
    assert!(!again.started, "the same command is reused");
}

/// rec's own server running (`rec status` says `ready: true`) refuses the
/// dispatch with `engine_in_use`, naming `rec stop`, and darkmux starts,
/// stops and records nothing.
#[test]
#[serial_test::serial]
fn recs_own_server_running_refuses_the_dispatch_naming_rec_stop() {
    let r = rig();
    let rec = fake_rec(r.dir.path(), "0.1.0");
    std::fs::write(r.dir.path().join("status.json"), r#"{"ready":true,"endpoint":"http://127.0.0.1:11500/v1"}"#).unwrap();
    let machine = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let err = prepare_in(&flint_target("flint-int4", free_port()), &r.dirs, &rec, quick(), &machine, &mut |_, _| {})
        .err()
        .expect("refused");
    assert_eq!(err.code, "engine_in_use");
    assert!(err.message.contains("rec stop") && err.message.contains("127.0.0.1:11500"), "{err}");
    assert!(engine_record::list_in(&r.dirs.records).0.is_empty(), "nothing started");
}

/// When `rec status` reports the server darkmux itself started (same port),
/// that is darkmux's own, not rec's: the dispatch reuses it.
#[test]
#[serial_test::serial]
fn rec_status_naming_darkmuxs_own_server_is_not_a_refusal() {
    let r = rig();
    let rec = fake_rec(r.dir.path(), "0.1.0");
    let port = free_port();
    let roomy = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    drop(prepare_in(&flint_target("flint-int4", port), &r.dirs, &rec, quick(), &roomy, &mut |_, _| {}).unwrap());
    std::fs::write(r.dir.path().join("status.json"), format!(r#"{{"ready":true,"endpoint":"http://127.0.0.1:{port}/v1"}}"#)).unwrap();
    let again = prepare_in(&flint_target("flint-int4", port), &r.dirs, &rec, quick(), &roomy, &mut |_, _| {}).unwrap();
    assert!(!again.started);
}

/// An unverified rec, or a revision rec's install does not hold, is
/// `engine_unavailable` before anything starts; the latter says `rec pull`.
#[test]
#[serial_test::serial]
fn an_unverified_rec_or_a_missing_revision_is_engine_unavailable() {
    let r = rig();
    let machine = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let newer = fake_rec(r.dir.path(), "0.2.0");
    let err = prepare_in(&flint_target("flint-int4", free_port()), &r.dirs, &newer, quick(), &machine, &mut |_, _| {})
        .err()
        .expect("refused");
    assert_eq!(err.code, "engine_unavailable");
    assert!(err.message.contains("rec 0.2.0") && err.message.contains("0.1.0"), "{err}");
    let rec = fake_rec(r.dir.path(), "0.1.0");
    let err = prepare_in(&flint_target("flint-int8", free_port()), &r.dirs, &rec, quick(), &machine, &mut |_, _| {})
        .err()
        .expect("refused");
    assert_eq!(err.code, "engine_unavailable");
    assert!(err.message.contains("flint-int8.json") && err.message.contains("rec pull"), "{err}");
    assert!(engine_record::list_in(&r.dirs.records).0.is_empty(), "nothing started");
}

/// A Flint server a running dispatch is using (its lease names `flint`) is
/// never replaced under it: another revision on the endpoint is `busy`.
#[test]
#[serial_test::serial]
fn a_held_flint_server_is_busy_never_replaced() {
    let r = rig();
    let rec = fake_rec(r.dir.path(), "0.1.0");
    let model = r.dir.path().join("share/rec/models/aa11");
    std::fs::create_dir_all(&model).unwrap();
    std::fs::write(model.join("flint-int8.json"), "{}").unwrap();
    let port = free_port();
    let roomy = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let a = prepare_in(&flint_target("flint-int4", port), &r.dirs, &rec, quick(), &roomy, &mut |_, _| {}).unwrap();
    let held = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), leased: vec!["flint".into()], ..Default::default() };
    let err = prepare_in(&flint_target("flint-int8", port), &r.dirs, &rec, quick(), &held, &mut |_, _| {})
        .err()
        .expect("refused");
    assert_eq!(err.code, "busy", "{err}");
    assert!(server::alive(&a.record), "the held server is untouched");
}

/// (#3453 review) The own-server exemption is for the port darkmux's own
/// server serves: with darkmux's server up on one port and `rec status`
/// naming another, rec's own server is up too, and the dispatch is refused.
#[test]
#[serial_test::serial]
fn rec_status_naming_another_port_than_darkmuxs_server_is_refused() {
    let r = rig();
    let rec = fake_rec(r.dir.path(), "0.1.0");
    let port = free_port();
    let roomy = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    drop(prepare_in(&flint_target("flint-int4", port), &r.dirs, &rec, quick(), &roomy, &mut |_, _| {}).unwrap());
    std::fs::write(r.dir.path().join("status.json"), r#"{"ready":true,"endpoint":"http://127.0.0.1:11500/v1"}"#).unwrap();
    let err = prepare_in(&flint_target("flint-int4", port), &r.dirs, &rec, quick(), &roomy, &mut |_, _| {})
        .err()
        .expect("refused");
    assert_eq!(err.code, "engine_in_use", "{err}");
}

/// (#3453 review) A live darkmux record on the port `rec status` names does
/// not exempt the answer when something else is the listener there: the
/// exemption is gated on darkmux's server being the listener.
#[test]
#[serial_test::serial]
fn rec_status_naming_darkmuxs_port_held_by_a_foreign_listener_is_refused() {
    let r = rig();
    let rec = fake_rec(r.dir.path(), "0.1.0");
    // darkmux's "server": alive, leading its own group, never listening.
    let mut ours = std::process::Command::new("sleep");
    ours.arg("30");
    std::os::unix::process::CommandExt::process_group(&mut ours, 0);
    let mut ours = ours.spawn().unwrap();
    // The listener: something darkmux did not start.
    let foreign = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = foreign.local_addr().unwrap().port();
    let stamp = (0..50).find_map(|_| {
        std::thread::sleep(Duration::from_millis(10));
        darkmux_platform::current().process_start_stamp(ours.id())
    });
    let record = EngineRecord {
        schema: darkmux_types::engine_record::ENGINE_RECORD_SCHEMA,
        endpoint: "fl".into(),
        engine: "flint".into(),
        model: "flint-int4".into(),
        n_ctx: 8192,
        port,
        pid: ours.id(),
        start_stamp: stamp.expect("a start stamp"),
        owner_pid: std::process::id(),
        argv: vec!["rec-mlx".into()],
        log: r.dirs.log_for("fl"),
        started_at_ms: 0,
        engine_version: "rec 0.1.0".into(),
    };
    engine_record::write_in(&r.dirs.records, &record).unwrap();
    assert!(server::running(&r.dirs, "fl").is_some(), "the fixture holds: darkmux's record is live");
    std::fs::write(r.dir.path().join("status.json"), format!(r#"{{"ready":true,"endpoint":"http://127.0.0.1:{port}/v1"}}"#)).unwrap();
    let machine = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let result = prepare_in(&flint_target("flint-int4", port), &r.dirs, &rec, quick(), &machine, &mut |_, _| {});
    let _ = ours.kill();
    let _ = ours.wait();
    drop(foreign);
    assert_eq!(result.err().map(|e| e.code), Some("engine_in_use"));
}

/// (#3453 re-review) `rec status` saying `ready: false`: rec's own server is
/// still starting when something listens on the endpoint it names (refused,
/// nothing started), and stopped when nothing does (the dispatch starts
/// darkmux's server).
#[test]
#[serial_test::serial]
fn rec_status_ready_false_is_recs_server_only_while_its_port_is_bound() {
    let r = rig();
    let rec = fake_rec(r.dir.path(), "0.1.0");
    let machine = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let bound = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let rec_port = bound.local_addr().unwrap().port();
    let status = format!(r#"{{"ready":false,"endpoint":"http://127.0.0.1:{rec_port}/v1"}}"#);
    std::fs::write(r.dir.path().join("status.json"), &status).unwrap();
    let err = prepare_in(&flint_target("flint-int4", free_port()), &r.dirs, &rec, quick(), &machine, &mut |_, _| {})
        .err()
        .expect("refused");
    assert_eq!(err.code, "engine_in_use");
    assert!(engine_record::list_in(&r.dirs.records).0.is_empty(), "nothing started");
    drop(bound);
    let ready = prepare_in(&flint_target("flint-int4", free_port()), &r.dirs, &rec, quick(), &machine, &mut |_, _| {}).unwrap();
    assert!(ready.started, "a stopped rec (`ready: false`, nothing on its port) leaves the engine free");
}

/// (#3453 review) Tensorfold's `serve_spec` failing (here a model with no
/// `n_ctx`) is `model_start_failed`, never `engine_unavailable`: the engine
/// is there, the model's start is what cannot be built.
#[test]
#[serial_test::serial]
fn a_tensorfold_serve_spec_failure_is_model_start_failed() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 2.0).to_string_lossy().into_owned());
    let mut t = target("TensorFold/m", free_port());
    t.model.n_ctx = None;
    let err = prepare_in(&t, &r.dirs, &tf, quick(), &FakeMachine::default(), &mut |_, _| {}).err().expect("refused");
    assert_eq!(err.code, "model_start_failed", "{err}");
}

/// (#3468) Every code a dispatch can write to `dispatch.error` (engine
/// dispatch's refusals, and the engine server's errors) is a client code an
/// app can branch on: read as spelled, never `dispatch_failed`. A new code
/// here that the client enum lacks fails this, before an app ever sees it.
#[test]
fn every_dispatch_refusal_code_is_a_client_code() {
    let src = |p: &str| std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(p)).unwrap();
    let dispatch = src("src/engine_dispatch.rs");
    let server = src("../darkmux-profiles/src/engine/server.rs");
    let mut codes: Vec<String> = Vec::new();
    for (i, _) in dispatch.match_indices("refuse(") {
        let rest = dispatch[i + "refuse(".len()..].trim_start();
        if let Some(code) = rest.strip_prefix('"').and_then(|r| r.split('"').next()) {
            codes.push(code.to_string());
        }
    }
    let code_fn = &server[server.find("pub fn code(&self)").expect("ServerError::code")..];
    let code_fn = &code_fn[..code_fn.find("\n    }\n").unwrap()];
    for line in code_fn.lines().filter(|l| l.contains("=> \"")) {
        codes.push(line.split('"').nth(1).unwrap().to_string());
    }
    codes.sort();
    codes.dedup();
    assert!(codes.len() >= 7, "the scan found the producers: {codes:?}");
    for code in &codes {
        let read = darkmux_types::client_code::ClientCode::of_dispatch(Some(code));
        assert_ne!(read, darkmux_types::client_code::ClientCode::DispatchFailed, "`{code}` is no client code");
    }
}

// ─── Concurrent dispatches on one endpoint ──────────────────────────────────

/// Start `target`'s server on `engine` with no lease and no residency lock.
fn start_unlocked(engine: &dyn ServerEngine, target: &crate::target::Target, dirs: &Dirs) -> EngineRecord {
    let endpoint = target.endpoint.named_id().unwrap().to_string();
    let spec = engine.plan(&endpoint, target.endpoint.port.unwrap(), &target.model).unwrap();
    server::ensure(&spec, dirs, quick(), &|_| false, &mut |_| {}).unwrap().record().clone()
}

/// Run `b` on its own thread once it has taken its first look at the
/// endpoint (cold) and is waiting for the residency lock, while `a` starts
/// another server there in between, as a starter that takes no residency
/// lock would (another darkmux's LM Studio path, an older darkmux). Returns
/// what `a` started and `b`'s outcome.
fn interleave<T: Send + 'static>(a: impl FnOnce() -> EngineRecord, b: impl FnOnce() -> T + Send + 'static) -> (EngineRecord, T) {
    let lock = darkmux_types::residency_lock::acquire().unwrap();
    let b = std::thread::spawn(b);
    std::thread::sleep(Duration::from_millis(300));
    let started = a();
    drop(lock);
    (started, b.join().unwrap())
}

/// Two dispatches on one Tensorfold endpoint, another model each: A starts
/// its server while B is between its first look (nothing there) and its own
/// start. B decides again under its locks, finds A's server held, and is
/// `busy`; A's server is never stopped or replaced under it.
#[test]
#[serial_test::serial]
fn a_dispatch_racing_another_on_its_endpoint_never_replaces_the_server_the_other_holds() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 2.0).to_string_lossy().into_owned());
    let port = free_port();
    let (tf_b, dirs) = (tf.clone(), r.dirs.clone());
    let (a, b) = interleave(
        || start_unlocked(&tf, &target("TensorFold/a", port), &r.dirs),
        move || {
            let held = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), leased: vec!["darkmux:TensorFold/a".into()], ..Default::default() };
            prepare_in(&target("TensorFold/b", port), &dirs, &tf_b, quick(), &held, &mut |_, _| {}).map(|_| ()).map_err(|e| e.code)
        },
    );
    assert!(server::alive(&a), "A's server is untouched");
    assert_eq!(server::running(&r.dirs, "tf").map(|r| r.model), Some("TensorFold/a".to_string()), "A's model still serves");
    assert_eq!(b, Err("busy"));
}

/// The same race on rec's Flint, where every server answers `flint`: had B
/// replaced A's server, A's next request would have been answered by B's
/// revision with nothing to tell them apart. B is `busy` instead, and the
/// endpoint still serves A's revision.
#[test]
#[serial_test::serial]
fn a_flint_dispatch_racing_another_never_gets_the_other_answered_by_its_model() {
    let r = rig();
    let rec = fake_rec(r.dir.path(), "0.1.0");
    let int8 = r.dir.path().join("share/rec/models/aa11");
    std::fs::create_dir_all(&int8).unwrap();
    std::fs::write(int8.join("flint-int8.json"), "{}").unwrap();
    std::fs::write(int8.join("model-00000.safetensors"), vec![0u8; 1024]).unwrap();
    let port = free_port();
    let (rec_b, dirs) = (rec.clone(), r.dirs.clone());
    let (a, b) = interleave(
        || start_unlocked(&rec, &flint_target("flint-int4", port), &r.dirs),
        move || {
            let held = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), leased: vec!["flint".into()], ..Default::default() };
            prepare_in(&flint_target("flint-int8", port), &dirs, &rec_b, quick(), &held, &mut |_, _| {}).map(|_| ()).map_err(|e| e.code)
        },
    );
    assert!(server::alive(&a), "A's server is untouched");
    assert_eq!(server::running(&r.dirs, "fl").map(|r| r.model), Some("flint-int4".to_string()), "A's revision still serves");
    assert_eq!(b, Err("busy"));
}

/// Send `sig` (`STOP`, `CONT`) to `pid`.
fn signal(pid: u32, sig: &str) {
    let _ = std::process::Command::new("kill").args([&format!("-{sig}"), &pid.to_string()]).status();
}

/// A server a running dispatch holds, busy enough that it does not answer
/// `GET /v1/models` in time (stopped here, still listening), is reused by a
/// second dispatch of the same model, never killed and restarted under the
/// first.
#[test]
#[serial_test::serial]
fn a_held_server_that_answers_slowly_is_reused_never_killed() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 2.0).to_string_lossy().into_owned());
    let port = free_port();
    let roomy = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let a = prepare_in(&target("TensorFold/a", port), &r.dirs, &tf, quick(), &roomy, &mut |_, _| {}).unwrap();
    signal(a.record.pid, "STOP");
    let held = FakeMachine {
        reclaimable: RefCell::new(Some(64 * GIB)),
        leased: vec!["darkmux:TensorFold/a".into()],
        ..Default::default()
    };
    let b = prepare_in(&target("TensorFold/a", port), &r.dirs, &tf, quick(), &held, &mut |_, _| {});
    signal(a.record.pid, "CONT");
    assert!(server::alive(&a.record), "the held server was not killed");
    let b = b.unwrap_or_else(|e| panic!("reused, not refused: {e}"));
    assert!(!b.started, "the held server is reused");
    assert_eq!(b.record.pid, a.record.pid);
}

/// A machine whose leases change once eviction has begun: the first read
/// answers `first`, every later read `then`. Memory and LM Studio are
/// `inner`'s, unless `reclaim` says otherwise.
struct LeasedLater<'a> {
    inner: FakeMachine,
    reclaim: Option<&'a dyn Fn() -> Option<u64>>,
    first: Vec<String>,
    then: Vec<String>,
    reads: RefCell<u32>,
}

impl Machine for LeasedLater<'_> {
    fn reclaimable(&self) -> Option<u64> {
        self.reclaim.map_or_else(|| self.inner.reclaimable(), |f| f())
    }
    fn footprint(&self, _pid: u32) -> Option<u64> {
        None
    }
    fn lmstudio_residents(&self) -> Vec<String> {
        self.inner.lmstudio_residents()
    }
    fn lmstudio_unload(&self, id: &str) -> anyhow::Result<()> {
        self.inner.lmstudio_unload(id)
    }
    fn leased(&self, _own: &LeaseGuard) -> Vec<String> {
        *self.reads.borrow_mut() += 1;
        if *self.reads.borrow() == 1 { self.first.clone() } else { self.then.clone() }
    }
    fn capacity(&self) -> Option<u64> {
        None
    }
}

/// An idle LM Studio load that a dispatch takes up while room is being made
/// (after the eviction list was read) is never unloaded: what is held is read
/// again before each eviction, and with nothing idle left the dispatch is
/// `busy`.
#[test]
#[serial_test::serial]
fn a_load_that_becomes_held_mid_eviction_is_never_unloaded() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 20.0).to_string_lossy().into_owned());
    let machine = LeasedLater {
        inner: FakeMachine {
            reclaimable: RefCell::new(Some(8 * GIB)),
            frees_per_unload: 8 * GIB,
            lms: RefCell::new(vec!["darkmux:a".into(), "darkmux:b".into()]),
            ..Default::default()
        },
        reclaim: None,
        first: Vec::new(),
        then: vec!["darkmux:a".into()],
        reads: RefCell::new(0),
    };
    let result = prepare_in(&target("TensorFold/m", free_port()), &r.dirs, &tf, quick(), &machine, &mut |_, _| {});
    assert_eq!(*machine.inner.unloaded.borrow(), ["darkmux:b"], "only the load still idle was unloaded");
    let err = result.err().expect("refused");
    assert_eq!(err.code, "busy", "{err}");
    assert!(err.message.contains("darkmux:a"), "{err}");
}

/// An idle engine server elsewhere that a dispatch takes up after the
/// eviction list was read is never stopped: the lease is checked under that
/// endpoint's lock, where the stop happens.
#[test]
#[serial_test::serial]
fn an_idle_server_that_becomes_held_before_its_eviction_is_never_stopped() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 20.0).to_string_lossy().into_owned());
    let roomy = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let mut other = target("TensorFold/o", free_port());
    other.endpoint.source = darkmux_types::endpoint::EndpointSource::Named("other".into());
    let o = prepare_in(&other, &r.dirs, &tf, quick(), &roomy, &mut |_, _| {}).unwrap();
    let o_rec = o.record.clone();
    drop(o);
    let frees = || Some(if server::alive(&o_rec) { GIB } else { 64 * GIB });
    let machine = LeasedLater {
        inner: FakeMachine::default(),
        reclaim: Some(&frees),
        first: Vec::new(),
        then: vec!["darkmux:TensorFold/o".into()],
        reads: RefCell::new(0),
    };
    let result = prepare_in(&target("TensorFold/m", free_port()), &r.dirs, &tf, quick(), &machine, &mut |_, _| {});
    assert!(server::alive(&o_rec), "the server held by then was not stopped");
    let err = result.err().expect("refused");
    assert_eq!(err.code, "busy", "{err}");
}

/// The dispatch checks `engine_args` against the installed engine's own
/// flags before it starts anything, as doctor and the listing do: a flag the
/// engine does not take, an abbreviation, or a flag missing its value is
/// refused, and no server is started or recorded.
#[test]
#[serial_test::serial]
fn a_dispatch_refuses_engine_args_the_engine_does_not_take_before_starting() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 2.0).to_string_lossy().into_owned());
    let roomy = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    for (args, names) in [
        (&["--turbo"][..], "--turbo"),
        (&["--keep"][..], "--keep"),
        (&["--keep-warm"][..], "--keep-warm"),
        (&["--slide", "stray"][..], "--slide"),
    ] {
        let err = prepare_in(&target_with_args("TensorFold/m", free_port(), args), &r.dirs, &tf, quick(), &roomy, &mut |_, _| {})
            .err()
            .unwrap_or_else(|| panic!("{args:?}: refused"));
        assert_eq!(err.code, "model_start_failed", "{args:?}: {err}");
        assert!(err.message.contains(names) && err.message.contains("engine_args"), "{args:?}: {err}");
        assert!(engine_record::list_in(&r.dirs.records).0.is_empty(), "{args:?}: nothing started");
    }
    let ok = prepare_in(&target_with_args("TensorFold/m", free_port(), &["--keep-warm", "30"]), &r.dirs, &tf, quick(), &roomy, &mut |_, _| {});
    assert!(ok.is_ok_and(|ready| ready.started), "flags the engine takes start the server");
}

/// A lease that cannot be written refuses the dispatch before anything
/// starts: without it, another darkmux process would read the server as
/// idle and could stop it under the dispatch.
#[test]
#[serial_test::serial]
fn a_lease_that_cannot_be_written_refuses_the_dispatch() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 2.0).to_string_lossy().into_owned());
    // The lease directory's own name is a file, so the lease cannot be written.
    std::fs::create_dir_all(r.dir.path().join("home")).unwrap();
    std::fs::write(r.dir.path().join("home").join("residency"), "").unwrap();
    let roomy = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let err = prepare_in(&target("TensorFold/m", free_port()), &r.dirs, &tf, quick(), &roomy, &mut |_, _| {})
        .err()
        .expect("refused");
    assert_eq!(err.code, "model_start_failed", "{err}");
    assert!(err.message.contains("lease"), "{err}");
    assert!(engine_record::list_in(&r.dirs.records).0.is_empty(), "nothing started");
}


// ─── A busy dispatch follows fleet.busy_policy (#3504) ──────────────────────

use crate::budget::tests::{solo_caller, FakeEnv};

fn wait_as(policy: BusyPolicy, bound_secs: Option<u64>) -> BusyWait<'static> {
    BusyWait { policy, bound: bound_secs.map(Duration::from_secs), caller: solo_caller() }
}

fn tf_base() -> DispatchModelPayload {
    DispatchModelPayload { engine: "tensorfold".into(), endpoint: "tf".into(), model: "TensorFold/b".into(), ..Default::default() }
}

/// An attempt that is `busy` for its first `n` tries, then succeeds; counts
/// every try.
fn busy_for(n: u32, tries: &std::cell::Cell<u32>) -> impl FnMut(&mut dyn FnMut(Note, DispatchModelPayload)) -> Result<(), EngineRefusal> + '_ {
    move |_| {
        tries.set(tries.get() + 1);
        if tries.get() <= n {
            Err(busy("endpoint `tf` is serving `TensorFold/a` for a running dispatch".into(), "retry when it finishes"))
        } else {
            Ok(())
        }
    }
}

/// Under `refuse` (the default) a busy dispatch is answered at once, exactly
/// as before: one try, no sleep, no line, no record.
#[test]
fn under_refuse_a_busy_dispatch_is_answered_at_once() {
    let env = FakeEnv::new(Vec::new());
    let tries = std::cell::Cell::new(0);
    let mut notes = Vec::new();
    let err = wait_while_busy(&wait_as(BusyPolicy::Refuse, Some(600)), &env, &tf_base(), &mut busy_for(u32::MAX, &tries), &mut |n, _| notes.push(n))
        .unwrap_err();
    assert_eq!(err.code(), Some("busy"));
    assert!(err.message().contains("retry when it finishes"), "{}", err.message());
    assert_eq!(tries.get(), 1);
    assert!(notes.is_empty() && env.said().is_empty() && env.paused_total() == 0, "{notes:?} {:?}", env.said());
}

/// Under `queue` a busy dispatch waits, tries again, and goes on once it is
/// no longer busy: one `queued` line and `dispatch.queue.wait` record when the
/// wait starts, a `dispatch.queue.resume` when it ends, and the pause
/// recorded so the run's wall-clock bound extends.
#[test]
fn under_queue_a_busy_dispatch_waits_then_goes_on() {
    let env = FakeEnv::new(Vec::new());
    let tries = std::cell::Cell::new(0);
    let mut notes = Vec::new();
    wait_while_busy(&wait_as(BusyPolicy::Queue, Some(600)), &env, &tf_base(), &mut busy_for(3, &tries), &mut |n, p| notes.push((n, p)))
        .unwrap();
    assert_eq!(tries.get(), 4, "tried again until it was free");
    let kinds: Vec<Note> = notes.iter().map(|(n, _)| *n).collect();
    assert_eq!(kinds, [Note::Queued, Note::Resumed]);
    assert_eq!(notes[0].1.elapsed_ms, Some(0), "the wait's start");
    assert_eq!(notes[0].1.wait_bound_ms, Some(600_000));
    assert!(notes[0].1.waiting_on.as_deref().is_some_and(|w| w.contains("TensorFold/a") && !w.contains("retry")), "{:?}", notes[0].1);
    assert_eq!(notes[1].1.elapsed_ms, Some(15_000), "three 5 s retries");
    assert_eq!(env.paused_total(), 15_000, "the wait is recorded as pause");
    let said = env.said();
    assert!(said[0].contains("queued: endpoint `tf` is serving `TensorFold/a`") && said[0].contains("Ctrl-C"), "{said:?}");
}

/// The wait is bounded by the dispatch's timeout and ends `busy`, saying so;
/// while it lasts it says it still waits every 20 s, no oftener.
#[test]
fn a_queued_wait_ends_busy_at_the_dispatchs_timeout_saying_so_every_20s() {
    let env = FakeEnv::new(Vec::new());
    let tries = std::cell::Cell::new(0);
    let mut notes = Vec::new();
    let err = wait_while_busy(&wait_as(BusyPolicy::Queue, Some(60)), &env, &tf_base(), &mut busy_for(u32::MAX, &tries), &mut |n, p| notes.push((n, p)))
        .unwrap_err();
    assert_eq!(err.code(), Some("busy"));
    assert!(err.message().contains("1m 0s, this dispatch's timeout"), "{}", err.message());
    let at: Vec<Option<u64>> = notes.iter().map(|(_, p)| p.elapsed_ms).collect();
    assert_eq!(at, [Some(0), Some(20_000), Some(40_000)], "a record at the start and every 20 s");
    assert!(notes.iter().all(|(n, _)| *n == Note::Queued), "no resume for a wait that timed out");
    assert_eq!(env.said().len(), 3, "a line with each record");
    assert_eq!(env.paused_total(), 60_000, "waited exactly its bound");
    assert_eq!(tries.get(), 13, "a last try at the bound, then no more");
}

/// A stop (Ctrl-C, a client's cancel, `mission abort`) ends the wait without
/// trying again: nothing is started or evicted after it, and no code is
/// written for it.
#[test]
fn a_stop_ends_the_wait_without_trying_again() {
    let env = FakeEnv::new(Vec::new()).stopped_after(7_000, "the run was interrupted");
    let tries = std::cell::Cell::new(0);
    let err = wait_while_busy(&wait_as(BusyPolicy::Queue, Some(600)), &env, &tf_base(), &mut busy_for(2, &tries), &mut |_, _| {})
        .unwrap_err();
    assert_eq!(err.code(), None, "a stop is not a refusal");
    assert!(err.message().contains("interrupted") && err.message().contains("nothing was started"), "{}", err.message());
    assert_eq!(tries.get(), 2, "the try that would have started (the third) never ran");
}

/// `wont_fit` never waits (no wait can make it fit), nor does any other
/// refusal: one try, answered at once.
#[test]
fn only_busy_waits() {
    for code in ["wont_fit", "memory_in_use", "model_start_failed", "port_in_use"] {
        let env = FakeEnv::new(Vec::new());
        let tries = std::cell::Cell::new(0);
        let err = wait_while_busy(
            &wait_as(BusyPolicy::Queue, Some(600)),
            &env,
            &tf_base(),
            &mut |_| -> Result<(), EngineRefusal> {
                tries.set(tries.get() + 1);
                Err(refuse(code, "no"))
            },
            &mut |_, _| {},
        )
        .unwrap_err();
        assert_eq!((err.code(), tries.get(), env.paused_total()), (Some(code), 1, 0), "{code}");
    }
}

/// A machine on which `held` is leased by another dispatch for the first
/// `until` lease reads, and free after. While it is held, it records
/// whether `holder` was still alive, and every lease this process counts as
/// held (a waiter's own must never be among them).
struct HeldUntil<'a> {
    held: String,
    until: u32,
    reads: RefCell<u32>,
    holder: &'a EngineRecord,
    holder_alive_while_held: RefCell<bool>,
    counted: RefCell<Vec<String>>,
}

impl Machine for HeldUntil<'_> {
    fn reclaimable(&self) -> Option<u64> {
        Some(64 * GIB)
    }
    fn footprint(&self, _pid: u32) -> Option<u64> {
        None
    }
    fn lmstudio_residents(&self) -> Vec<String> {
        Vec::new()
    }
    fn lmstudio_unload(&self, _id: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn leased(&self, _own: &LeaseGuard) -> Vec<String> {
        *self.reads.borrow_mut() += 1;
        if *self.reads.borrow() <= self.until {
            // A try that ends `busy` (and so waits): what is held now.
            self.counted.borrow_mut().extend(LeaseGuard::acquire().all_live_leased_models());
            let alive = server::alive(self.holder);
            *self.holder_alive_while_held.borrow_mut() &= alive;
            return vec![self.held.clone()];
        }
        Vec::new()
    }
    fn capacity(&self) -> Option<u64> {
        None
    }
}

/// End to end on the stand-in engine: under `queue`, a dispatch whose
/// endpoint serves another model for a running dispatch waits (deciding
/// again on every try, holding no lease another dispatch would count), never
/// touches the held server, and starts its own once the holder's lease goes.
#[test]
#[serial_test::serial]
fn a_queued_dispatch_starts_once_the_holders_lease_goes_never_evicting_it_while_held() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 2.0).to_string_lossy().into_owned());
    let port = free_port();
    let roomy = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let a = prepare_in(&target("TensorFold/a", port), &r.dirs, &tf, quick(), &roomy, &mut |_, _| {}).unwrap();
    let a_rec = a.record.clone();
    drop(a);
    let machine = HeldUntil {
        held: "darkmux:TensorFold/a".into(),
        until: 3,
        reads: RefCell::new(0),
        holder: &a_rec,
        holder_alive_while_held: RefCell::new(true),
        counted: RefCell::new(Vec::new()),
    };
    let env = FakeEnv::new(Vec::new());
    let mut notes = Vec::new();
    let b = target("TensorFold/b", port);
    let ready = wait_while_busy(
        &wait_as(BusyPolicy::Queue, Some(600)),
        &env,
        &queue_payload(&b),
        &mut |note| prepare_in(&b, &r.dirs, &tf, quick(), &machine, note),
        &mut |n, _| notes.push(n),
    )
    .unwrap();
    assert!(*machine.holder_alive_while_held.borrow(), "the held server was never touched while held");
    assert!(ready.started && !server::alive(&a_rec), "B replaced A's server once A's lease went");
    let kinds: Vec<Note> = notes.iter().copied().filter(|n| matches!(n, Note::Queued | Note::Resumed | Note::Start)).collect();
    assert_eq!(kinds, [Note::Queued, Note::Resumed, Note::Start]);
    assert!(
        !machine.counted.borrow().iter().any(|m| m == "darkmux:TensorFold/b"),
        "the waiter's own lease was never counted as held: {:?}",
        machine.counted.borrow()
    );
}

/// A machine whose leases are this process's real ones, with room to spare.
struct RealLeases;

impl Machine for RealLeases {
    fn reclaimable(&self) -> Option<u64> {
        Some(64 * GIB)
    }
    fn footprint(&self, _pid: u32) -> Option<u64> {
        None
    }
    fn lmstudio_residents(&self) -> Vec<String> {
        Vec::new()
    }
    fn lmstudio_unload(&self, _id: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn leased(&self, own: &LeaseGuard) -> Vec<String> {
        own.all_live_leased_models()
    }
    fn capacity(&self) -> Option<u64> {
        None
    }
}

/// Four dispatches queued behind one holder on one endpoint, each for
/// another model: when the holder ends they run one at a time, never two at
/// once, and every one of them runs (none is starved or lost).
#[test]
#[serial_test::serial]
fn four_queued_dispatches_behind_one_holder_start_one_at_a_time() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 2.0).to_string_lossy().into_owned());
    let port = free_port();
    let holder = prepare_in(&target("TensorFold/a", port), &r.dirs, &tf, quick(), &RealLeases, &mut |_, _| {}).unwrap();
    let spans = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let waiters: Vec<_> = ["b", "c", "d", "e"]
        .into_iter()
        .map(|m| {
            let (tf, dirs, spans) = (tf.clone(), r.dirs.clone(), spans.clone());
            std::thread::spawn(move || {
                let t = target(&format!("TensorFold/{m}"), port);
                let env = FakeEnv::new(Vec::new());
                let ready = wait_while_busy(
                    &wait_as(BusyPolicy::Queue, None),
                    &env,
                    &queue_payload(&t),
                    &mut |note| prepare_in(&t, &dirs, &tf, quick(), &RealLeases, note),
                    &mut |_, _| {},
                )
                .unwrap_or_else(|e| panic!("{m}: {}", e.message()));
                let began = std::time::Instant::now();
                assert!(ready.started, "{m} started its own server");
                std::thread::sleep(Duration::from_millis(150));
                spans.lock().unwrap().push((began, std::time::Instant::now(), m));
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(300));
    drop(holder);
    for w in waiters {
        w.join().unwrap();
    }
    let mut spans = spans.lock().unwrap().clone();
    spans.sort();
    assert_eq!(spans.len(), 4, "every waiter ran");
    for pair in spans.windows(2) {
        assert!(pair[1].0 >= pair[0].1, "{} began before {} ended", pair[1].2, pair[0].2);
    }
}

// ─── The machine-wide residency lock and reservations (#3509) ──────────────

/// A start still in flight counts against free memory though its memory has
/// not paged in: with 40 GiB free and another start reserving 30 GiB, a
/// 30 GiB start is `busy` (it frees when that start's dispatch ends), never
/// admitted beside it.
#[test]
#[serial_test::serial]
fn a_start_in_flight_reservation_counts_against_free_memory() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 30.0).to_string_lossy().into_owned());
    let in_flight = LeaseGuard::acquire();
    in_flight.write_starting(&["darkmux:TensorFold/other".to_string()], 30 * GIB).unwrap();
    let machine = FakeMachine { reclaimable: RefCell::new(Some(40 * GIB)), capacity: Some(128 * GIB), ..Default::default() };
    let err = prepare_in(&target("TensorFold/m", free_port()), &r.dirs, &tf, quick(), &machine, &mut |_, _| {})
        .err()
        .expect("refused");
    assert_eq!(err.code, "busy", "{err}");
    assert!(err.message.contains("starts in flight reserve 30.0 GiB"), "{err}");
    assert!(engine_record::list_in(&r.dirs.records).0.is_empty(), "nothing started");
    drop(in_flight);
    let ready = prepare_in(&target("TensorFold/m", free_port()), &r.dirs, &tf, quick(), &machine, &mut |_, _| {}).unwrap();
    assert!(ready.started, "with no start in flight it fits");
}

/// A start decides and spawns under the machine-wide residency lock: while
/// another holder has it, nothing is started.
#[test]
#[serial_test::serial]
fn a_start_waits_for_the_residency_lock() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 2.0).to_string_lossy().into_owned());
    let lock = darkmux_types::residency_lock::acquire().unwrap();
    let (dirs, port) = (r.dirs.clone(), free_port());
    let start = std::thread::spawn(move || {
        let roomy = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
        prepare_in(&target("TensorFold/m", port), &dirs, &tf, quick(), &roomy, &mut |_, _| {}).map(|r| r.started)
    });
    std::thread::sleep(Duration::from_millis(500));
    assert!(engine_record::list_in(&r.dirs.records).0.is_empty(), "nothing is spawned while another holds the lock");
    drop(lock);
    assert_eq!(start.join().unwrap(), Ok(true), "it starts once the lock is free");
}

/// (#3507) The standing utility binding is never evicted to make room,
/// though no lease names it: a running dispatch loads it on demand to
/// compact. The idle load beside it is evicted instead.
#[test]
#[serial_test::serial]
fn the_utility_binding_is_never_evicted_to_make_room() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 20.0).to_string_lossy().into_owned());
    let machine = FakeMachine {
        reclaimable: RefCell::new(Some(16 * GIB)),
        frees_per_unload: 8 * GIB,
        lms: RefCell::new(vec!["darkmux:idle".into(), "darkmux:qwen/qwen3-4b-2507".into()]),
        utility: Some("darkmux:qwen/qwen3-4b-2507".into()),
        ..Default::default()
    };
    let ready = prepare_in(&target("TensorFold/m", free_port()), &r.dirs, &tf, quick(), &machine, &mut |_, _| {}).unwrap();
    assert!(ready.started);
    assert_eq!(*machine.unloaded.borrow(), ["darkmux:idle"], "only the idle load was evicted");
}

/// (#3511) A server judged reusable that is gone by the time the reuse is
/// decided under its endpoint's lock (here: it no longer answers, and
/// nothing holds it) is started through the memory decision, never restarted
/// blind: on a machine with no room, the dispatch is refused and nothing
/// new is started.
#[test]
#[serial_test::serial]
fn a_reusable_server_lost_before_its_reuse_is_restarted_only_through_the_memory_decision() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 20.0).to_string_lossy().into_owned());
    let t = target("TensorFold/m", free_port());
    let roomy = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let first = prepare_in(&t, &r.dirs, &tf, quick(), &roomy, &mut |_, _| {}).unwrap();
    let first_rec = first.record.clone();
    drop(first);
    signal(first_rec.pid, "STOP");
    let short = FakeMachine { reclaimable: RefCell::new(Some(GIB)), capacity: Some(128 * GIB), ..Default::default() };
    let result = prepare_in(&t, &r.dirs, &tf, quick(), &short, &mut |_, _| {});
    signal(first_rec.pid, "CONT");
    let err = result.err().expect("refused: there is no room to start it again");
    assert_eq!(err.code, "memory_in_use", "{err}");
    assert!(server::running(&r.dirs, "tf").is_none(), "nothing was started blind");
}

// ─── A start in flight on the endpoint (#3509 review) ───────────────────────

/// A start that answers 1.5 s after its spawn (the stand-in's `--slide`).
fn slow(model: &str, port: u16) -> crate::target::Target {
    target_with_args(model, port, &["--slide"])
}

/// Hold the residency lock while `racers` all take their first look (each
/// reads the endpoint cold), then let them race.
fn race<T: Send + 'static>(racers: Vec<Box<dyn FnOnce() -> T + Send>>) -> Vec<T> {
    let lock = darkmux_types::residency_lock::acquire().unwrap();
    let handles: Vec<_> = racers.into_iter().map(std::thread::spawn).collect();
    std::thread::sleep(Duration::from_millis(300));
    drop(lock);
    handles.into_iter().map(|h| h.join().unwrap()).collect()
}

/// Two dispatches for one cold 30 GiB model with 40 GiB free: one starts it,
/// the other waits for that start (off the machine lock) and reuses the
/// server. It is never `busy` against its own model's reservation.
#[test]
#[serial_test::serial]
fn two_dispatches_for_one_cold_model_start_it_once_and_both_run() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 30.0).to_string_lossy().into_owned());
    let port = free_port();
    let racer = |tf: Tensorfold, dirs: Dirs| -> Box<dyn FnOnce() -> Result<bool, String> + Send> {
        Box::new(move || {
            let machine = FakeMachine { reclaimable: RefCell::new(Some(40 * GIB)), capacity: Some(128 * GIB), ..Default::default() };
            let ready = prepare_in(&slow("TensorFold/m", port), &dirs, &tf, quick(), &machine, &mut |_, _| {}).map_err(|e| e.to_string())?;
            std::thread::sleep(Duration::from_millis(200));
            Ok(ready.started)
        })
    };
    let mut outcomes = race(vec![racer(tf.clone(), r.dirs.clone()), racer(tf.clone(), r.dirs.clone())]);
    outcomes.sort();
    assert_eq!(outcomes, [Ok(false), Ok(true)], "one started it, the other reused it");
}

/// While a start is in flight on an endpoint, a dispatch for another model
/// there never holds the machine-wide lock waiting on that endpoint: a third
/// party takes the lock at once, not once the start answers. The other
/// model's dispatch is `busy` (the endpoint serves a running dispatch).
#[test]
#[serial_test::serial]
fn a_start_in_flight_never_keeps_the_machine_lock_from_a_third_party() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 2.0).to_string_lossy().into_owned());
    let port = free_port();
    let racer = |model: &'static str, tf: Tensorfold, dirs: Dirs| -> Box<dyn FnOnce() -> Result<bool, String> + Send> {
        Box::new(move || {
            let ready = prepare_in(&slow(model, port), &dirs, &tf, quick(), &RealLeases, &mut |_, _| {}).map_err(|e| e.code.to_string())?;
            // The winner's dispatch runs on a while, holding its server.
            std::thread::sleep(Duration::from_millis(1_000));
            Ok(ready.started)
        })
    };
    let records = r.dirs.records.clone();
    let third = std::thread::spawn(move || {
        while engine_record::list_in(&records).0.is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
        // The start is spawned and has 1.5 s to go; the loser has looked.
        std::thread::sleep(Duration::from_millis(300));
        let asked = std::time::Instant::now();
        drop(darkmux_types::residency_lock::acquire().unwrap());
        asked.elapsed()
    });
    let mut outcomes = race(vec![racer("TensorFold/a", tf.clone(), r.dirs.clone()), racer("TensorFold/b", tf.clone(), r.dirs.clone())]);
    let waited = third.join().unwrap();
    outcomes.sort();
    assert_eq!(outcomes, [Ok(true), Err("busy".to_string())]);
    assert!(waited < Duration::from_millis(500), "the third party waited {waited:?} for the machine lock");
}

/// The reservation outlives the start's answer until the server's memory
/// pages in: right after a start, another start still sees it reserved.
#[test]
#[serial_test::serial]
fn a_started_servers_reservation_outlives_its_answer() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 30.0).to_string_lossy().into_owned());
    let machine = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let ready = prepare_in(&target("TensorFold/m", free_port()), &r.dirs, &tf, quick(), &machine, &mut |_, _| {}).unwrap();
    assert!(ready.started);
    let other = LeaseGuard::acquire();
    let reserved = other.reserved_by_others();
    assert!(reserved > 29 * GIB && reserved <= 30 * GIB, "its memory has not paged in (the stand-in is tiny): {reserved}");
    drop(ready);
    assert_eq!(other.reserved_by_others(), 0, "a dispatch that ends reserves nothing");
}

/// The reservation shrinks as the server's footprint grows and ends when it
/// reaches the need, or at the bound when it never does (an unreadable
/// footprint keeps the whole need until then).
#[test]
#[serial_test::serial]
fn a_reservation_settles_as_the_footprint_grows_or_at_its_bound() {
    let _r = rig();
    let reserved = |footprints: Vec<Option<u64>>, bound: Duration| {
        let guard = LeaseGuard::acquire();
        guard.write_starting(&["darkmux:x".to_string()], 30 * GIB).unwrap();
        let res = guard.mark_loaded_reserving(&["darkmux:x".to_string()], 30 * GIB).unwrap();
        let other = LeaseGuard::acquire();
        let seen = RefCell::new(Vec::new());
        let reads = RefCell::new(footprints.into_iter());
        settle_reservation(res, 30 * GIB, &|| reads.borrow_mut().next().flatten(), &|_| seen.borrow_mut().push(other.reserved_by_others()), bound);
        (seen.into_inner(), other.reserved_by_others())
    };
    let (seen, after) = reserved(vec![Some(10 * GIB), Some(25 * GIB), Some(30 * GIB)], Duration::from_secs(120));
    assert_eq!(seen, [20 * GIB, 5 * GIB], "shrinks as it pages in");
    assert_eq!(after, 0, "ends at the need");
    let (seen, after) = reserved(vec![None; 1_000], Duration::from_secs(2));
    assert!(seen.iter().all(|b| *b == 30 * GIB) && seen.len() == 4, "unread: the whole need, polled to the bound: {seen:?}");
    assert_eq!(after, 0, "ends at the bound");
}
