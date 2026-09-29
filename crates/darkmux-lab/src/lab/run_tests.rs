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
    /// The `verify` value written into `manifest.json` (when written).
    pub(crate) manifest_verify: Option<serde_json::Value>,
    /// Fail `run` on this call (1-based, counted from `script`) only.
    pub(crate) run_err_on_call: Option<u32>,
}

static SCRIPT: Mutex<Option<Script>> = Mutex::new(None);

pub(crate) fn script(s: Script) {
    *SCRIPT.lock().unwrap() = Some(s);
    SCRIPT_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
}

/// `run` calls since the last `script`.
static SCRIPT_CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

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
        loaded: &LoadedWorkload,
        run_dir: &Path,
        _: &Path,
        _: &darkmux_types::Profile,
        _: &str,
        _: Option<&str>,
        _: Option<&crate::lab::loop_report::LoopCompactionOverride>,
        run: &darkmux_types::session_id::RunId,
        on_session_id: &mut dyn FnMut(&darkmux_types::session_id::SessionId),
    ) -> Result<RunResult> {
        let s = SCRIPT.lock().unwrap().clone().unwrap_or_default();
        on_session_id(&darkmux_types::session_id::SessionId::adhoc(run.clone(), "stub", "darkmux-stub-scripted"));
        let nth = SCRIPT_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if s.run_err_on_call == Some(nth) {
            return Err(anyhow!("scripted failure on run {nth}"));
        }
        if let Some(p) = &loaded.manifest.workload.prompt {
            fs::write(run_dir.join("prompt"), p)?;
        }
        if s.run_err {
            return Err(anyhow!("scripted run failure"));
        }
        // Which call wrote this dir, so a test can tell whose artifacts it holds.
        let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        fs::write(run_dir.join("marker"), call.to_string())?;
        if s.write_manifest {
            let mut m = serde_json::json!({ "schema_version": 2 });
            if let Some(v) = s.manifest_verify {
                m["verify"] = v;
            }
            fs::write(run_dir.join("manifest.json"), m.to_string())?;
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

    /// The sandbox a scripted workload's run is cloned from (no
    /// `requires_fixture`, so the `sandboxes/<id>` fallback).
    fn source_sandbox(&self, workload: &str) -> std::path::PathBuf {
        paths::resolve(ResolveScope::Auto).sandboxes.join(workload)
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
        // The manifest records the same tri-state, so `lab run list` reads
        // a failed verify as FAIL rather than a plain tick.
        let listed = crate::lab::list::list_runs(None).unwrap();
        let row = listed.iter().find(|r| r.run_id == o.run_id).unwrap();
        assert_eq!(row.verify_passed, want, "{id}: listed");
    }
    // A manifest from before the field existed reads as not checked.
    let old = darkmux_types::config_access::lab_dir().join("p2982-old-stub-1-1");
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("manifest.json"), r#"{"schema_version":2,"workload":"p2982-none","ok":true}"#).unwrap();
    let listed = crate::lab::list::list_runs(None).unwrap();
    assert_eq!(listed.iter().find(|r| r.run_id == "p2982-old-stub-1-1").unwrap().verify_passed, None);
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
        provider_error: None,
    };
    assert_eq!(exit_code(&[]), 0);
    assert_eq!(exit_code(&[o(true, None)]), 0);
    assert_eq!(exit_code(&[o(true, Some(true))]), 0);
    assert_eq!(exit_code(&[o(true, Some(false))]), 1);
    assert_eq!(exit_code(&[o(false, None)]), 1);
    assert_eq!(exit_code(&[o(false, Some(true))]), 1);
    assert_eq!(exit_code(&[o(true, None), o(true, Some(false))]), 1);
}

// ─── lab_run characterization: one test per harness branch ───────────

fn manifest(o: &RunOutcome) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(o.run_dir.join("manifest.json")).unwrap()).unwrap()
}

/// N runs: N outcomes with ordinal ids, each completed, each with the
/// notes line `lab run` prints; `runs: 0` still runs once.
#[test]
#[serial_test::serial]
fn every_run_completes_with_its_ordinal_and_notes() {
    let lab = Lab::scripted(&["wn"]);
    script(Script { ok: true, verify: Some(true), ..Default::default() });
    let out = lab.run("wn", 3).unwrap();
    assert_eq!(out.len(), 3);
    for (k, o) in out.iter().enumerate() {
        assert!(o.run_id.starts_with("wn-fast-") && o.run_id.ends_with(&format!("-{}", k + 1)), "{}", o.run_id);
        assert_eq!(o.run_dir.file_name().unwrap().to_str().unwrap(), o.run_id);
        assert!(o.ok);
        assert_eq!(o.verify_passed, Some(true));
        assert_eq!(o.duration_ms, 2_000);
        assert_eq!(o.notes, ["provider=stub-lab-run-scripted", "wall=2s", "ok", "verify=pass (scripted)"]);
        let rec = lifecycle::read(&o.run_dir).unwrap();
        assert_eq!(rec.status, lifecycle::LifecycleStatus::Complete);
        assert!(o.run_dir.join("sandbox").is_dir(), "an empty per-run sandbox exists");
    }
    assert_eq!(lab.run("wn", 0).unwrap().len(), 1);
}

/// A failed dispatch is still a completed run: the error is a note, and a
/// run with no verify has no verify note and `verify_passed: None`.
#[test]
#[serial_test::serial]
fn a_failed_dispatch_is_a_completed_run_with_an_error_note() {
    let lab = Lab::scripted(&["wf"]);
    script(Script { ok: false, verify: None, ..Default::default() });
    let o = lab.run("wf", 1).unwrap().remove(0);
    assert!(!o.ok);
    assert_eq!(o.verify_passed, None);
    assert_eq!(o.notes, ["provider=stub-lab-run-scripted", "wall=2s", "error: scripted error"]);
    assert_eq!(lifecycle::read(&o.run_dir).unwrap().status, lifecycle::LifecycleStatus::Complete);
}

/// (#2986) A provider that errors in setup or run, or is not registered at
/// all, fails that run: the run is recorded as errored and returned as a
/// failed outcome naming the error, and the batch goes on to the next run.
#[test]
#[serial_test::serial]
fn a_provider_error_fails_the_run_and_the_batch_goes_on() {
    let lab = Lab::new(
        r#"{"default_profile":"fast","profiles":{"fast":{"models":[{"id":"model-a","n_ctx":32000}]}}}"#,
        &[
            serde_json::json!({ "id": "we", "provider": SCRIPTED, "prompt": "hi" }),
            serde_json::json!({ "id": "wu", "provider": "no-such-provider", "prompt": "hi" }),
        ],
    );
    for (s, want) in [
        (Script { setup_err: true, ..Default::default() }, "scripted setup failure"),
        (Script { run_err: true, ..Default::default() }, "scripted run failure"),
    ] {
        script(s);
        let out = lab.run("we", 2).unwrap();
        assert_eq!(out.len(), 2, "both runs are attempted");
        for o in &out {
            assert!(!o.ok && !o.passed());
            assert_eq!(o.verify_passed, None);
            assert_eq!(o.provider_error.as_deref(), Some(want));
            assert_eq!(o.notes, [format!("provider={SCRIPTED}"), format!("error: {want}")]);
            let rec = lifecycle::read(&o.run_dir).unwrap();
            assert_eq!(rec.status, lifecycle::LifecycleStatus::Error);
            assert_eq!(rec.error.as_deref(), Some(want));
        }
        assert_eq!(exit_code(&out), 1);
    }
    let o = lab.run("wu", 1).unwrap().remove(0);
    let err = o.provider_error.unwrap();
    assert!(err.contains("unknown workload provider: \"no-such-provider\""), "{err}");
    assert_eq!(lifecycle::read(&o.run_dir).unwrap().status, lifecycle::LifecycleStatus::Error);
}

/// (#2462) A provider error while a signal is set records the run as
/// interrupted, not as a provider failure.
#[test]
#[serial_test::serial]
fn a_provider_error_under_a_signal_records_the_run_interrupted() {
    let lab = Lab::scripted(&["wi"]);
    script(Script { run_err: true, ..Default::default() });
    darkmux_types::interrupt::mark_interrupted();
    let err = lab.run("wi", 1).unwrap_err();
    darkmux_types::interrupt::reset_for_test();
    assert_eq!(err.to_string(), "scripted run failure");
    let root = darkmux_types::config_access::lab_dir();
    let dir = fs::read_dir(&root).unwrap().flatten().next().unwrap().path();
    assert_eq!(lifecycle::read(&dir).unwrap().status, lifecycle::LifecycleStatus::Interrupted);
}

/// With a source sandbox, the run works on a clone of it (run-artifact dirs
/// pruned) and the manifest records the canonical source path and the
/// clone's hash; without one, the per-run sandbox starts empty and the
/// manifest records a null source.
#[test]
#[serial_test::serial]
fn the_run_clones_its_source_sandbox_and_records_its_provenance() {
    let lab = Lab::scripted(&["ws", "wempty"]);
    script(Script { ok: true, write_manifest: true, ..Default::default() });
    let src = lab.source_sandbox("ws");
    fs::create_dir_all(src.join(".git")).unwrap();
    fs::write(src.join("a.txt"), "baseline").unwrap();
    fs::write(src.join(".git").join("HEAD"), "x").unwrap();

    let o = lab.run("ws", 1).unwrap().remove(0);
    let sandbox = o.run_dir.join("sandbox");
    assert_eq!(fs::read_to_string(sandbox.join("a.txt")).unwrap(), "baseline");
    assert!(!sandbox.join(".git").exists(), "run-artifact dirs are pruned from the clone");
    let m = manifest(&o);
    assert_eq!(m["schema_version"], 4);
    assert_eq!(m["fixture"]["source_path"], src.canonicalize().unwrap().display().to_string());
    assert_eq!(m["fixture"]["baseline_hash"], hash_sandbox_dir(&sandbox).unwrap());

    let o = lab.run("wempty", 1).unwrap().remove(0);
    assert_eq!(fs::read_dir(o.run_dir.join("sandbox")).unwrap().count(), 0);
    let m = manifest(&o);
    assert!(m["fixture"]["source_path"].is_null() && m["fixture"]["baseline_hash"].is_null(), "{m}");
}

/// A provider that writes no manifest still completes: enrichment is
/// best-effort observability, never a run failure.
#[test]
#[serial_test::serial]
fn a_missing_manifest_does_not_fail_the_run() {
    let lab = Lab::scripted(&["wm"]);
    script(Script { ok: true, write_manifest: false, ..Default::default() });
    let o = lab.run("wm", 1).unwrap().remove(0);
    assert!(o.ok);
    assert!(!o.run_dir.join("manifest.json").exists());
}

/// (#2833) When the fixture declares a baseline, the work gate's verdict
/// replaces the provider's raw verify in the outcome and its notes; when
/// the gate cannot be applied at all, the outcome fails closed.
#[test]
#[serial_test::serial]
fn the_work_gate_overrides_the_raw_verify_and_fails_closed() {
    let lab = Lab::scripted(&["wg"]);
    let src = lab.source_sandbox("wg");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join(".fixture.json"), r#"{"name":"wg","baseline":{"test_count":"many"}}"#).unwrap();

    script(Script {
        ok: true,
        verify: Some(true),
        write_manifest: true,
        manifest_verify: Some(serde_json::json!({ "passed": true, "details": "raw" })),
        ..Default::default()
    });
    let o = lab.run("wg", 1).unwrap().remove(0);
    assert_eq!(o.verify_passed, Some(false), "the gate's forced failure wins: {:?}", o.notes);
    assert!(o.notes[3].starts_with("verify=fail (") && o.notes[3].contains("malformed"), "{:?}", o.notes);

    script(Script {
        ok: true,
        verify: Some(true),
        write_manifest: true,
        manifest_verify: Some(serde_json::json!(5)),
        ..Default::default()
    });
    let o = lab.run("wg", 1).unwrap().remove(0);
    assert_eq!(o.verify_passed, Some(false), "{:?}", o.notes);
    assert!(o.notes[3].starts_with("verify=fail (verify gate could not be applied: "), "{:?}", o.notes);
}

/// (#1004) The injected context reaches the workload the provider runs.
#[test]
#[serial_test::serial]
fn an_injected_context_is_prepended_to_the_prompt() {
    let lab = Lab::scripted(&["wc"]);
    script(Script { ok: true, ..Default::default() });
    let o = lab_run(RunOpts {
        workload_id: "wc".into(),
        profile_name: Some("fast".into()),
        runs: 1,
        config_path: Some(lab.profiles.clone()),
        quiet: false,
        loop_override: None,
        inject_context: Some("<ctx/>".into()),
    })
    .unwrap()
    .remove(0);
    assert_eq!(fs::read_to_string(o.run_dir.join("prompt")).unwrap(), "<ctx/>\n\nhi");
}

/// An explicit profile the registry does not define is refused before any
/// run dir is claimed.
#[test]
#[serial_test::serial]
fn an_undefined_profile_is_refused_before_any_run() {
    let lab = Lab::scripted(&["wp"]);
    let err = lab_run(RunOpts {
        workload_id: "wp".into(),
        profile_name: Some("ghost".into()),
        runs: 1,
        config_path: Some(lab.profiles.clone()),
        quiet: true,
        loop_override: None,
        inject_context: None,
    })
    .unwrap_err();
    assert!(err.to_string().contains("ghost"), "{err}");
    assert!(fs::read_dir(darkmux_types::config_access::lab_dir()).map(|d| d.count() == 0).unwrap_or(true));
}


// ─── lab_run's pure pieces ───────────────────────────────────────────

/// (#365) A mismatch that holds across runs warns once; a changed picture
/// warns again; an empty picture after a warning clears it.
#[test]
fn envelope_warnings_print_once_per_distinct_picture() {
    let mut e = EnvelopeWarnings::default();
    let w = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert_eq!(e.fresh(w(&["a"])), w(&["a"]));
    assert!(e.fresh(w(&["a"])).is_empty());
    assert_eq!(e.fresh(w(&["b"])), w(&["b"]));
    assert!(e.fresh(w(&[])).is_empty());
    assert_eq!(e.fresh(w(&["b"])), w(&["b"]));
}

/// (#2833) The gate's verdict replaces the raw verify; no gate leaves it;
/// a gate error fails it closed and names why; with no verify there is
/// nothing to settle, and the error still warns.
#[test]
fn settle_verify_takes_the_gates_verdict_and_fails_closed_on_error() {
    let raw = || VerifyOutcome { passed: true, details: "raw".into() };
    let gate = |passed| crate::lab::verify_gate::WorkGateResult { passed, details: "gated".into(), tests_added: None };

    let mut v = raw();
    assert_eq!(settle_verify(Some(&mut v), Ok(None)), None);
    assert!(v.passed && v.details == "raw");

    let mut v = raw();
    assert_eq!(settle_verify(Some(&mut v), Ok(Some(gate(false)))), None);
    assert!(!v.passed && v.details == "gated");

    let mut v = raw();
    let w = settle_verify(Some(&mut v), Err(anyhow!("boom"))).unwrap();
    assert!(w.contains("boom") && w.contains("failing verify closed"), "{w}");
    assert!(!v.passed && v.details == "verify gate could not be applied: boom");

    assert!(settle_verify(None, Err(anyhow!("boom"))).is_some());
    assert_eq!(settle_verify(None, Ok(Some(gate(true)))), None);
}

/// A failed dispatch with no error text still names itself.
#[test]
fn run_notes_name_an_unexplained_failure() {
    let r = RunResult {
        ok: false,
        duration_ms: 61_999,
        payload_text: None,
        trajectory_path: None,
        verify: Some(VerifyOutcome { passed: false, details: "2 failing".into() }),
        error: None,
    };
    assert_eq!(run_notes("p", &r), ["provider=p", "wall=61s", "error: unknown", "verify=fail (2 failing)"]);
}

/// (#365) An `lms ps` that cannot run is named as an unverified profile
/// tag, not silently skipped.
#[test]
#[serial_test::serial]
fn a_failed_lms_ps_is_an_unverified_profile_warning() {
    let _lab = Lab::scripted(&[]);
    // SAFETY: serial; `Lab`'s drop restores the variable.
    unsafe { std::env::set_var("DARKMUX_LMS_BIN", "/nonexistent/lms") };
    let profile: darkmux_types::Profile = serde_json::from_str(r#"{"models":[{"id":"m","n_ctx":1}]}"#).unwrap();
    let w = envelope_check(&profile, "fast");
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(w[0].starts_with("could not verify profile-load match — `lms ps` failed ("), "{w:?}");
    assert!(w[0].ends_with("this run's `profile=fast` tag is unverified. (#365)"), "{w:?}");
}

/// (review of #2986) `lab run inspect` on a run whose provider errored, which
/// has a lifecycle record but no manifest, shows the recorded error instead
/// of failing: the characterize and tune reports point there.
#[test]
#[serial_test::serial]
fn inspect_shows_an_errored_runs_recorded_error() {
    let lab = Lab::scripted(&["wx"]);
    script(Script { run_err: true, ..Default::default() });
    let o = lab.run("wx", 1).unwrap().remove(0);
    let report = crate::lab::inspect::lab_inspect(&o.run_id).unwrap();
    assert_eq!(report.run_id, o.run_id);
    assert_eq!(report.workload_id, "wx");
    assert!(report.verify.is_none());
    assert_eq!(
        report.notes,
        [
            "the run ended before its provider finished, so it has no manifest".to_string(),
            "status: error".to_string(),
            "error: scripted run failure".to_string(),
        ]
    );
    // A dir with neither record is still refused.
    let bare = darkmux_types::config_access::lab_dir().join("wx-bare");
    fs::create_dir_all(&bare).unwrap();
    assert!(crate::lab::inspect::lab_inspect("wx-bare").is_err());
}

/// (review of #2986) The `lab run` summary never calls an errored run
/// complete: it counts the batch, the runs that completed, and those that
/// errored.
#[test]
fn the_batch_summary_counts_completed_and_errored_runs() {
    let o = |provider_error: Option<&str>| RunOutcome {
        run_id: "r".into(),
        run_dir: std::path::PathBuf::new(),
        ok: provider_error.is_none(),
        verify_passed: None,
        duration_ms: 0,
        notes: vec![],
        provider_error: provider_error.map(str::to_string),
    };
    assert_eq!(batch_summary(&[o(None), o(Some("boom")), o(None)]), "3 run(s): 2 completed, 1 errored");
    assert_eq!(batch_summary(&[o(None)]), "1 run(s): 1 completed, 0 errored");
}
