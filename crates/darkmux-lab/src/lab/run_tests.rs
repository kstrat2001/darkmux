//! `lab_run`'s per-run contract, driven end to end with no model and no
//! Docker: a scripted stub provider for the harness branches, and a real
//! `prompt` workload against a stub chat endpoint (a tool-less role, so the
//! dispatch is a host `curl`) for the verify contract.

use super::tests::HomeGuard;
use super::*;
use crate::workloads::types::{InspectionReport, LoadedWorkload, RunResult, VerifyOutcome, WorkloadProvider};
use std::sync::Mutex;
use tempfile::TempDir;

/// What the scripted provider does on its next run. One process-global
/// script, so every test using it holds `#[serial_test::serial]`.
#[derive(Clone, Default)]
pub(crate) struct Script {
    pub(crate) setup_err: bool,
    pub(crate) run_err: bool,
    pub(crate) ok: bool,
    pub(crate) verify: Option<bool>,
    pub(crate) write_manifest: bool,
}

static SCRIPT: Mutex<Option<Script>> = Mutex::new(None);

pub(crate) fn script(s: Script) {
    *SCRIPT.lock().unwrap() = Some(s);
}

static CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

const SCRIPTED: &str = "stub-lab-run-scripted";

struct ScriptedProvider;
impl WorkloadProvider for ScriptedProvider {
    fn id(&self) -> &'static str {
        SCRIPTED
    }
    fn description(&self) -> &'static str {
        "scripted stub for lab_run's harness branches"
    }
    fn setup(&self, _: &LoadedWorkload, _: &Path, _: &Path) -> Result<()> {
        let s = SCRIPT.lock().unwrap().clone().unwrap_or_default();
        if s.setup_err {
            return Err(anyhow!("scripted setup failure"));
        }
        Ok(())
    }
    fn run(
        &self,
        _: &LoadedWorkload,
        run_dir: &Path,
        _: &Path,
        _: &darkmux_types::Profile,
        _: &str,
        _: Option<&str>,
        _: Option<&crate::lab::loop_report::LoopCompactionOverride>,
        on_session_id: &mut dyn FnMut(&str),
    ) -> Result<RunResult> {
        let s = SCRIPT.lock().unwrap().clone().unwrap_or_default();
        on_session_id("darkmux-stub-scripted");
        if s.run_err {
            return Err(anyhow!("scripted run failure"));
        }
        // Which call wrote this dir, so a test can tell whose artifacts it holds.
        let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        fs::write(run_dir.join("marker"), call.to_string())?;
        if s.write_manifest {
            fs::write(run_dir.join("manifest.json"), r#"{"schema_version":2}"#)?;
        }
        Ok(RunResult {
            ok: s.ok,
            duration_ms: 2_000,
            payload_text: None,
            trajectory_path: None,
            verify: s.verify.map(|passed| VerifyOutcome { passed, details: "scripted".into() }),
            error: if s.ok { None } else { Some("scripted error".into()) },
        })
    }
    fn inspect(&self, _: &LoadedWorkload, _: &Path) -> Result<InspectionReport> {
        Ok(InspectionReport::default())
    }
}

/// An isolated darkmux root holding one profile registry and the named
/// workload documents.
pub(crate) struct Lab {
    _tmp: TempDir,
    _home: HomeGuard,
    prev_lms_bin: Option<std::ffi::OsString>,
    pub(crate) profiles: String,
}

impl Drop for Lab {
    fn drop(&mut self) {
        // SAFETY: every caller holds `#[serial_test::serial]`.
        unsafe {
            match &self.prev_lms_bin {
                Some(v) => std::env::set_var("DARKMUX_LMS_BIN", v),
                None => std::env::remove_var("DARKMUX_LMS_BIN"),
            }
        }
    }
}

impl Lab {
    fn new(profiles_json: &str, workloads: &[serde_json::Value]) -> Self {
        // Registration is process-global and refuses a second call; an
        // earlier test in this process may already have registered these.
        let _ = crate::providers::register_builtins();
        let _ = crate::workloads::registry::register(Box::new(ScriptedProvider));
        let tmp = TempDir::new().unwrap();
        let profiles = tmp.path().join("profiles.json");
        fs::write(&profiles, profiles_json).unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join("workloads")).unwrap();
        for w in workloads {
            let id = w["id"].as_str().unwrap();
            fs::write(
                home.join("workloads").join(format!("{id}.json")),
                serde_json::json!({ "workload": w }).to_string(),
            )
            .unwrap();
        }
        let guard = HomeGuard::set(&home);
        // A non-quiet run lists the loaded models; never reach a real `lms`.
        let prev_lms_bin = std::env::var_os("DARKMUX_LMS_BIN");
        // SAFETY: every caller holds `#[serial_test::serial]`.
        unsafe { std::env::set_var("DARKMUX_LMS_BIN", "/usr/bin/true") };
        Self { _tmp: tmp, _home: guard, prev_lms_bin, profiles: profiles.to_str().unwrap().to_string() }
    }

    pub(crate) fn scripted(workloads: &[&str]) -> Self {
        let docs: Vec<_> = workloads
            .iter()
            .map(|id| serde_json::json!({ "id": id, "provider": SCRIPTED, "prompt": "hi" }))
            .collect();
        Self::new(
            r#"{"default_profile":"fast","profiles":{"fast":{"models":[{"id":"model-a","n_ctx":32000}]}}}"#,
            &docs,
        )
    }

    fn run(&self, workload: &str, runs: u32) -> Result<Vec<RunOutcome>> {
        lab_run(RunOpts {
            workload_id: workload.into(),
            profile_name: None,
            runs,
            config_path: Some(self.profiles.clone()),
            quiet: true,
            loop_override: None,
            inject_context: None,
        })
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

// ─── #2981: a run never reuses a run dir ─────────────────────────────

/// (#2981) A run dir that already exists is never written into. Plants a
/// dir under every id a run could mint in the next few seconds, each with a
/// sentinel; the run must land in a dir of its own, and every planted dir
/// must hold exactly what it held before.
#[test]
#[serial_test::serial]
fn a_run_never_writes_into_an_existing_run_dir() {
    let lab = Lab::scripted(&["w2981"]);
    script(Script { ok: true, write_manifest: true, ..Default::default() });
    let root = darkmux_types::config_access::lab_dir();
    let now = now_secs();
    let planted: Vec<_> = (now..now + 5).map(|s| root.join(format!("w2981-fast-{s}-1"))).collect();
    for d in &planted {
        fs::create_dir_all(d).unwrap();
        fs::write(d.join("sentinel"), "earlier run").unwrap();
    }

    let out = lab.run("w2981", 1).unwrap();

    assert!(!planted.contains(&out[0].run_dir), "reused {}", out[0].run_dir.display());
    for d in &planted {
        let names: Vec<_> = fs::read_dir(d).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("sentinel")], "{} was written into", d.display());
    }
    assert!(out[0].run_dir.join("marker").is_file());
}

/// (#2981) The reported case: back-to-back runs of one workload, well
/// inside one second, each keep their own artifacts.
#[test]
#[serial_test::serial]
fn back_to_back_runs_keep_their_own_artifacts() {
    let lab = Lab::scripted(&["w2981b"]);
    script(Script { ok: true, ..Default::default() });
    let a = lab.run("w2981b", 1).unwrap().remove(0);
    let b = lab.run("w2981b", 1).unwrap().remove(0);
    assert_ne!(a.run_id, b.run_id);
    let marker = |o: &RunOutcome| fs::read_to_string(o.run_dir.join("marker")).unwrap();
    assert_ne!(marker(&a), marker(&b), "the second run overwrote the first run's artifacts");
}

/// (#2981) The claim itself: the ordinal first, then `.2`, `.3` past each
/// directory that already exists, and a real I/O error surfaces instead of
/// being retried.
#[test]
fn a_claim_skips_every_existing_dir_and_surfaces_other_errors() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("runs");
    let claim = || claim_run_dir(&root, "w", "p", 100, 3).unwrap();
    assert_eq!(claim().0, "w-p-100-3");
    let second = claim().0;
    assert_eq!(second, "w-p-100-3.2");
    // The stamp still reads back from a later claim's id.
    assert_eq!(crate::lab::stats::run_id_epoch_ms(&second), Some(100_000));
    let (id, dir) = claim();
    assert_eq!(id, "w-p-100-3.3");
    assert_eq!(dir, root.join("w-p-100-3.3"));
    assert!(dir.is_dir());

    let file_root = tmp.path().join("a-file");
    fs::write(&file_root, "").unwrap();
    let err = claim_run_dir(&file_root, "w", "p", 100, 1).unwrap_err();
    assert!(format!("{err:#}").contains("creating"), "{err:#}");
}

// ─── #2982: verify tri-state and the exit gate ───────────────────────

/// A chat-completions stub that answers every request with `ack`.
fn ack_endpoint() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let mut buf = [0u8; 16384];
                let _ = stream.read(&mut buf);
                let body = serde_json::json!({
                    "choices": [{ "message": { "content": "ack" } }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
                })
                .to_string();
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            });
        }
    });
    port
}

/// (#2982a) A `prompt` workload that declares no verify reports
/// `verify_passed: None`, as its doc says, not a pass. The inverse cases
/// pin that a declared verify still reports its real verdict.
#[test]
#[serial_test::serial]
fn a_prompt_workload_reports_verify_none_without_a_spec_and_the_verdict_with_one() {
    let port = ack_endpoint();
    let prompt = |id: &str, verify: serde_json::Value| {
        let mut w = serde_json::json!({
            "id": id, "provider": "prompt", "role": "dialectic-judge", "prompt": "say ack"
        });
        if !verify.is_null() {
            w["verify"] = verify;
        }
        w
    };
    let lab = Lab::new(
        &format!(
            r#"{{"default_profile":"stub","profiles":{{"stub":{{"models":[
                {{"id":"stub-model","n_ctx":8000,"endpoint":{{"url":"http://127.0.0.1:{port}"}}}}]}}}}}}"#
        ),
        &[
            prompt("p2982-none", serde_json::Value::Null),
            prompt("p2982-pass", serde_json::json!({ "must_contain": ["ack"] })),
            prompt("p2982-fail", serde_json::json!({ "must_contain": ["zzz-never"] })),
        ],
    );
    for (id, want) in [("p2982-none", None), ("p2982-pass", Some(true)), ("p2982-fail", Some(false))] {
        let o = lab.run(id, 1).unwrap().remove(0);
        assert!(o.ok, "{id}: the stub dispatch must succeed: {:?}", o.notes);
        assert_eq!(o.verify_passed, want, "{id}: {:?}", o.notes);
    }
}
/// (#2982b, #2494) ONE exit gate for every lab verb that runs a workload:
/// a failed dispatch or a failed verify exits 1; a verify nothing declared
/// does not.
#[test]
fn the_exit_gate_fails_on_a_failed_dispatch_or_a_failed_verify_only() {
    let o = |ok: bool, verify_passed: Option<bool>| RunOutcome {
        run_id: String::new(),
        run_dir: std::path::PathBuf::new(),
        ok,
        verify_passed,
        duration_ms: 0,
        notes: vec![],
    };
    assert_eq!(exit_code(&[]), 0);
    assert_eq!(exit_code(&[o(true, None)]), 0);
    assert_eq!(exit_code(&[o(true, Some(true))]), 0);
    assert_eq!(exit_code(&[o(true, Some(false))]), 1);
    assert_eq!(exit_code(&[o(false, None)]), 1);
    assert_eq!(exit_code(&[o(false, Some(true))]), 1);
    assert_eq!(exit_code(&[o(true, None), o(true, Some(false))]), 1);
}
