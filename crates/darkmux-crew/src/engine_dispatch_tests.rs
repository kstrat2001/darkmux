//! (#3448) `prepare` against a stand-in Tensorfold binary and a fake
//! machine: no real engine, no real LM Studio, no real memory reading.

use super::*;
use darkmux_types::engine_record::ServerState;
use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

/// A stand-in `tensorfold-native`: `--version`, `info` (with the memory
/// floor the test asks for) and `serve`, which runs a tiny server answering
/// `GET /v1/models` with its `--name`.
fn fake_tensorfold(dir: &std::path::Path, version: &str, floor_gib: f64) -> PathBuf {
    let server = dir.join("serve.py");
    std::fs::write(
        &server,
        r#"
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer
a = sys.argv[1:]
port = int(a[a.index("--port") + 1]); name = a[a.index("--name") + 1]
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
    )
    .unwrap();
    let bin = dir.join("tensorfold-native");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'tensorfold-native {version}' ;;\n  info) printf 'weights: 1.00 GiB (1073741824 bytes)\\nmemory floor: {floor_gib:.2} GiB (weights plus a margin)\\n' ;;\n  serve) shift; exec python3 -u {} serve \"$@\" ;;\nesac\n",
            server.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn target(model: &str, port: u16) -> crate::target::Target {
    let mut reg: darkmux_types::ProfileRegistry = serde_json::from_value(serde_json::json!({
        "profiles": {"p": {"models": [{"id": model, "n_ctx": 8192, "endpoint": "tf"}]}},
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
        for rec in engine_record::list_in(&self.dirs.records).0 {
            if rec.state_now() == ServerState::Running {
                let _ = darkmux_types::child_registry::kill_pid(rec.pid, darkmux_types::child_registry::SIGKILL);
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
fn nothing_left_to_free_is_busy_or_wont_fit() {
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
    let alone = FakeMachine { reclaimable: RefCell::new(Some(8 * GIB)), ..Default::default() };
    let err = prepare_in(&target("TensorFold/m", free_port()), &r.dirs, &tf, quick(), &alone, &mut |_, _| {})
        .err()
        .expect("refused");
    assert_eq!(err.code, "wont_fit");
    assert!(err.message.contains("100.0 GiB"), "{err}");
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
    let leased = ThisMachine.leased(&own);
    drop(sibling);
    unsafe {
        match prev {
            Some(v) => std::env::set_var("DARKMUX_HOME", v),
            None => std::env::remove_var("DARKMUX_HOME"),
        }
    }
    assert!(leased.contains(&"darkmux:sibling".to_string()), "{leased:?}");
}
