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

/// A machine on which another dispatch runs its whole `prepare` between this
/// dispatch's first look at the endpoint and its start: the first lease read
/// starts `other` (keeping it running) and still answers with the world
/// before it, nothing leased; every later read sees `other`'s lease, `held`.
struct Interleaved<'a> {
    other: &'a dyn Fn() -> EngineReady,
    held: String,
    running: RefCell<Option<EngineReady>>,
    reads: RefCell<u32>,
}

impl Machine for Interleaved<'_> {
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
        if *self.reads.borrow() == 1 {
            *self.running.borrow_mut() = Some((self.other)());
            return Vec::new();
        }
        vec![self.held.clone()]
    }
    fn capacity(&self) -> Option<u64> {
        None
    }
}

/// Two dispatches on one Tensorfold endpoint, another model each: A starts
/// its server while B is between its first look (nothing there) and its own
/// start. B must find A's server held under the endpoint's lock and refuse
/// `busy`; A's server is never stopped or replaced under it.
#[test]
#[serial_test::serial]
fn a_dispatch_racing_another_on_its_endpoint_never_replaces_the_server_the_other_holds() {
    let r = rig();
    let tf = Tensorfold::at(fake_tensorfold(r.dir.path(), "1.2.4", 2.0).to_string_lossy().into_owned());
    let port = free_port();
    let roomy = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let start_a = || prepare_in(&target("TensorFold/a", port), &r.dirs, &tf, quick(), &roomy, &mut |_, _| {}).unwrap();
    let race = Interleaved { other: &start_a, held: "darkmux:TensorFold/a".into(), running: RefCell::new(None), reads: RefCell::new(0) };
    let b = prepare_in(&target("TensorFold/b", port), &r.dirs, &tf, quick(), &race, &mut |_, _| {});
    let a = race.running.borrow().as_ref().map(|a| a.record.clone()).expect("A started in between");
    assert!(server::alive(&a), "A's server is untouched");
    assert_eq!(server::running(&r.dirs, "tf").map(|r| r.model), Some("TensorFold/a".to_string()), "A's model still serves");
    let err = b.err().expect("B refused");
    assert_eq!(err.code, "busy", "{err}");
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
    let port = free_port();
    let roomy = FakeMachine { reclaimable: RefCell::new(Some(64 * GIB)), ..Default::default() };
    let start_a = || prepare_in(&flint_target("flint-int4", port), &r.dirs, &rec, quick(), &roomy, &mut |_, _| {}).unwrap();
    let race = Interleaved { other: &start_a, held: "flint".into(), running: RefCell::new(None), reads: RefCell::new(0) };
    let b = prepare_in(&flint_target("flint-int8", port), &r.dirs, &rec, quick(), &race, &mut |_, _| {});
    let a = race.running.borrow().as_ref().map(|a| a.record.clone()).expect("A started in between");
    assert!(server::alive(&a), "A's server is untouched");
    assert_eq!(server::running(&r.dirs, "fl").map(|r| r.model), Some("flint-int4".to_string()), "A's revision still serves");
    let err = b.err().expect("B refused");
    assert_eq!(err.code, "busy", "{err}");
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
