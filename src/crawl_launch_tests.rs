//! Tests for the crawl launcher (#1959 packet 2). **No real model, no
//! container.** Every test injects a scripted `dispatch_fn` closure that
//! emits the SAME `dispatch start`/`dispatch complete`/`dispatch error`
//! flow records `crew::dispatch::dispatch` would (via `build_dispatch_
//! record_with_payload`, the identical builder that path uses) and returns
//! a `DispatchResult` pointing at a tempdir the closure pre-seeds with
//! `.darkmux-runtime/findings.jsonl` — proving this module's own
//! orchestration without spawning Docker or touching LMStudio. See this
//! module's own doc comment for the full rationale.

use super::*;
use anyhow::anyhow;
use darkmux_lab::crawl::rules::RuleKind;
use std::cell::RefCell;
use std::env;
use std::process::Command;
use tempfile::TempDir;

// ── env isolation (mirrors mission_launch.rs's LaunchTestGuard) ─────────

struct TestGuard {
    _crew: TempDir,
    _flows: TempDir,
    prev_crew: Option<String>,
    prev_flows: Option<String>,
}

impl TestGuard {
    fn new() -> Self {
        let crew_dir = TempDir::new().unwrap();
        let flows_dir = TempDir::new().unwrap();
        let prev_crew = env::var("DARKMUX_CREW_DIR").ok();
        let prev_flows = env::var("DARKMUX_FLOWS_DIR").ok();
        // SAFETY: every caller is #[serial_test::serial].
        unsafe {
            env::set_var("DARKMUX_CREW_DIR", crew_dir.path());
            env::set_var("DARKMUX_FLOWS_DIR", flows_dir.path());
        }
        Self { _crew: crew_dir, _flows: flows_dir, prev_crew, prev_flows }
    }
}

impl Drop for TestGuard {
    fn drop(&mut self) {
        // SAFETY: every caller is #[serial_test::serial].
        unsafe {
            match &self.prev_crew {
                Some(v) => env::set_var("DARKMUX_CREW_DIR", v),
                None => env::remove_var("DARKMUX_CREW_DIR"),
            }
            match &self.prev_flows {
                Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
                None => env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }
}

/// Every flow record written to the isolated `DARKMUX_FLOWS_DIR` so far —
/// read raw off disk, mirroring `mission_launch.rs`'s own test helper of
/// the same shape.
fn read_all_flow_records() -> Vec<Value> {
    let dir = env::var("DARKMUX_FLOWS_DIR").expect("DARKMUX_FLOWS_DIR must be set by an active TestGuard");
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    for entry in rd.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                out.push(v);
            }
        }
    }
    out
}

fn mission_id_from_records(records: &[Value]) -> String {
    records
        .iter()
        .find_map(|r| {
            if r["action"] == "crawl.mission.started" {
                r["mission_id"].as_str().map(String::from)
            } else {
                None
            }
        })
        .expect("expected a crawl.mission.started record carrying mission_id")
}

/// Only the actions this module's own tests care about — excludes the
/// `mission start`/`phase start`/`mission close`/`phase complete`
/// lifecycle bookkeeping `crew::lifecycle` ALSO emits on the same stream.
fn crawl_relevant_actions(records: &[Value]) -> Vec<String> {
    records
        .iter()
        .filter_map(|r| r["action"].as_str())
        .filter(|a| a.starts_with("crawl.") || *a == "dispatch start" || *a == "dispatch complete" || *a == "dispatch error")
        .map(String::from)
        .collect()
}

// ── corpus fixture: two tiny local git repos, one `catch` site each ─────

fn init_source_repo(dir: &std::path::Path, filename: &str, contents: &str) {
    let run = |args: &[&str]| {
        let out = Command::new("git").current_dir(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    };
    std::fs::create_dir_all(dir).unwrap();
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "test"]);
    std::fs::write(dir.join(filename), contents).unwrap();
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "init"]);
}

struct Fixture {
    _workdir: TempDir,
    root: TempDir,
    manifest_path: PathBuf,
}

/// Two sources ("app1"/"app2"), one `catch` site each, `swallowed-error`
/// (the embedded site rule) — planning this manifest deterministically
/// yields exactly two `Unit::Site` units, one per source (`plan::plan`
/// collects site units per-source, so two sources never merge into one
/// unit), which is what every unit-count-dependent test below relies on.
fn two_source_fixture() -> Fixture {
    let workdir = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let app1 = workdir.path().join("app1");
    let app2 = workdir.path().join("app2");
    let body = "function f() {\n  try {\n    g();\n  }\n  catch (e) {\n    void 0;\n  }\n}\n";
    init_source_repo(&app1, "x.ts", body);
    init_source_repo(&app2, "y.ts", body);

    let manifest = serde_json::json!({
        "name": "fixture",
        "root": root.path().to_string_lossy(),
        "sources": [
            {"id": "app1", "path": app1.to_string_lossy(), "ref": "main"},
            {"id": "app2", "path": app2.to_string_lossy(), "ref": "main"}
        ],
        "rules": ["swallowed-error"]
    });
    let manifest_path = workdir.path().join("corpus.json");
    std::fs::write(&manifest_path, manifest.to_string()).unwrap();

    Fixture { _workdir: workdir, root, manifest_path }
}

fn params_for(fx: &Fixture) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    m.insert("corpus".to_string(), Value::String(fx.manifest_path.to_string_lossy().to_string()));
    m
}

// ── scripted dispatch ─────────────────────────────────────────────────

#[derive(Clone)]
struct ScriptedUnit {
    findings: Vec<Value>,
    result: &'static str,
    exit_code: i32,
    err: bool,
    prompt_tokens: u64,
    completion_tokens: u64,
    wall_ms: u64,
    model: &'static str,
}

impl Default for ScriptedUnit {
    fn default() -> Self {
        Self {
            findings: Vec::new(),
            result: "stop",
            exit_code: 0,
            err: false,
            prompt_tokens: 10,
            completion_tokens: 5,
            wall_ms: 100,
            model: "darkmux:test-model",
        }
    }
}

fn finding(file: &str, line: u32, evidence: &str, why: &str) -> Value {
    json!({
        "file": file,
        "line": line,
        "pattern": "swallowed-error",
        "evidence": evidence,
        "context": evidence,
        "context_start": line,
        "context_end": line,
        "why": why,
        "ts": 0,
    })
}

/// The unit id (`u-0001`, …) a production `session_id` (`crawl-<mission_id>-
/// u-0001`) or `phase_id` (`<mission_id>-crawl`) encodes. Unit ids are
/// always exactly `u-\d{4}`, so scanning for the LAST `-u-` occurrence is
/// robust even though `mission_id` itself is an unpredictable minted value.
fn unit_id_from_session(session_id: &str) -> String {
    match session_id.rfind("-u-") {
        Some(idx) => session_id[idx + 1..].to_string(),
        None => session_id.to_string(),
    }
}

fn mission_id_from_phase_id(phase_id: Option<&str>) -> Option<String> {
    phase_id.and_then(|p| p.strip_suffix("-crawl")).map(String::from)
}

fn scripted_ok_result(unit_id: &str, script: &ScriptedUnit, session_id: String) -> DispatchResult {
    let out_dir = TempDir::new().unwrap();
    let out_dir_path = out_dir.path().to_path_buf();
    // Leaked deliberately: the launcher reads from this path AFTER the
    // closure returns, so the TempDir must outlive this call. Test
    // processes are short-lived; the OS reclaims it at process exit.
    std::mem::forget(out_dir);
    let runtime_dir = out_dir_path.join(".darkmux-runtime");
    std::fs::create_dir_all(&runtime_dir).unwrap();
    if !script.findings.is_empty() {
        let mut body = String::new();
        for f in &script.findings {
            body.push_str(&serde_json::to_string(f).unwrap());
            body.push('\n');
        }
        std::fs::write(runtime_dir.join("findings.jsonl"), body).unwrap();
    }

    let envelope = json!({
        "result": script.result,
        "final_assistant": format!("covered {unit_id}"),
        "metrics": {
            "model": script.model,
            "wall_ms": script.wall_ms,
            "prompt_tokens": script.prompt_tokens,
            "completion_tokens": script.completion_tokens,
        },
        "detections": [],
    });

    DispatchResult {
        exit_code: script.exit_code,
        stdout: serde_json::to_string(&envelope).unwrap(),
        stderr: String::new(),
        session_id,
        out_dir: Some(out_dir_path),
    }
}

fn emit_scripted_bookend(action: &str, opts: &DispatchOpts, session_id: &str, model: Option<&str>) {
    let mission_id = mission_id_from_phase_id(opts.phase_id.as_deref());
    let _ = darkmux_flow::record(darkmux_crew::dispatch::build_dispatch_record_with_payload(
        darkmux_flow::Level::Info,
        action,
        &opts.role_id,
        session_id,
        model,
        mission_id.as_deref(),
        opts.phase_id.as_deref(),
        None,
    ));
}

/// Build a scripted `dispatch_fn`. `before(unit_id)` runs before the
/// scripted work — the hook the kill-file test uses to drop the STOP file
/// mid-run.
fn make_dispatch_fn<'a>(
    scripts: BTreeMap<String, ScriptedUnit>,
    calls: &'a RefCell<Vec<String>>,
    mut before: impl FnMut(&str) + 'a,
) -> impl FnMut(DispatchOpts) -> Result<DispatchResult> + 'a {
    move |opts: DispatchOpts| {
        let session_id = opts.session_id.clone().unwrap_or_default();
        let unit_id = unit_id_from_session(&session_id);
        before(&unit_id);
        calls.borrow_mut().push(unit_id.clone());

        emit_scripted_bookend(darkmux_flow::DISPATCH_START, &opts, &session_id, None);

        let script = scripts.get(&unit_id).cloned().unwrap_or_default();
        if script.err {
            emit_scripted_bookend(darkmux_flow::DISPATCH_ERROR, &opts, &session_id, None);
            return Err(anyhow!("scripted dispatch failure for {unit_id}"));
        }

        let result = scripted_ok_result(&unit_id, &script, session_id.clone());
        emit_scripted_bookend(darkmux_flow::DISPATCH_COMPLETE, &opts, &session_id, Some(script.model));
        Ok(result)
    }
}

// ── test 1: per-unit event ordering + mission_id on every record ───────

#[test]
#[serial_test::serial]
fn unit_loop_emits_started_dispatch_findings_completed_in_order_with_mission_id() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let mut scripts = BTreeMap::new();
    scripts.insert(
        "u-0001".to_string(),
        ScriptedUnit { findings: vec![finding("app1/x.ts", 5, "catch (e) {", "swallowed")], ..Default::default() },
    );
    scripts.insert("u-0002".to_string(), ScriptedUnit::default());
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(scripts, &calls, |_| {});

    let code = run(&params_for(&fx), None, &mut dispatch).unwrap();
    assert_eq!(code, 0);
    assert_eq!(calls.borrow().clone(), vec!["u-0001".to_string(), "u-0002".to_string()]);

    let records = read_all_flow_records();
    let actions = crawl_relevant_actions(&records);
    assert_eq!(
        actions,
        vec![
            "crawl.mission.started",
            "crawl.unit.started",
            "dispatch start",
            "dispatch complete",
            "crawl.finding",
            "crawl.unit.completed",
            "crawl.unit.started",
            "dispatch start",
            "dispatch complete",
            "crawl.unit.completed",
            "crawl.mission.completed",
        ],
        "{actions:#?}"
    );

    for r in records.iter().filter(|r| r["action"].as_str().unwrap_or("").starts_with("crawl.")) {
        assert!(r["mission_id"].is_string(), "record missing mission_id: {r:#?}");
        // (#1959 merge-gate finding 3) No `crawl.*` record may ever carry
        // an empty sha — that's the exact tell of a unit whose source
        // silently fell through `the_plan.sources.iter().find(...).
        // unwrap_or_default()` unvalidated.
        if let Some(sha) = r["payload"]["sha"].as_str() {
            assert_ne!(sha, "", "record must never carry an empty sha: {r:#?}");
        }
    }

    // (#1959 merge-gate finding 6) `model` is pinned on the top-level
    // FlowRecord for a `crawl.finding` and a `crawl.unit.completed`
    // record, not just buried in the payload.
    let finding_record = records.iter().find(|r| r["action"] == "crawl.finding").expect("expected one finding");
    assert_eq!(finding_record["model"], "darkmux:test-model");
    let completed_record =
        records.iter().find(|r| r["action"] == "crawl.unit.completed").expect("expected a completed record");
    assert_eq!(completed_record["model"], "darkmux:test-model");

    // (#1959 merge-gate finding 3) The ledger line for the one finding must
    // carry the real source sha too, never "".
    let mission_id = mission_id_from_records(&records);
    let ledger_path = fx.root.path().join("runs").join(&mission_id).join("ledger.jsonl");
    let ledger_body = std::fs::read_to_string(&ledger_path).unwrap();
    let mut ledger_lines = 0;
    for line in ledger_body.lines().filter(|l| !l.trim().is_empty()) {
        let rec: Value = serde_json::from_str(line).unwrap();
        assert_ne!(rec["sha"].as_str().unwrap_or(""), "", "ledger line must never carry an empty sha: {rec:#?}");
        ledger_lines += 1;
    }
    assert_eq!(ledger_lines, 1, "expected exactly u-0001's one finding in the ledger");
}

// ── test 2: finding path rewrite + ledger + per-unit copy ──────────────

#[test]
#[serial_test::serial]
fn findings_get_path_rewritten_and_land_in_ledger_and_per_unit_copy() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let mut scripts = BTreeMap::new();
    scripts.insert(
        "u-0001".to_string(),
        ScriptedUnit { findings: vec![finding("/workspace/app1/x.ts", 5, "catch (e) {", "swallowed")], ..Default::default() },
    );
    scripts.insert("u-0002".to_string(), ScriptedUnit::default());
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(scripts, &calls, |_| {});
    run(&params_for(&fx), None, &mut dispatch).unwrap();

    let records = read_all_flow_records();
    let mission_id = mission_id_from_records(&records);
    let finding_rec = records.iter().find(|r| r["action"] == "crawl.finding").expect("one crawl.finding record");
    assert_eq!(finding_rec["payload"]["file"], "x.ts");
    assert_eq!(finding_rec["payload"]["file_raw"], "/workspace/app1/x.ts");
    assert_eq!(finding_rec["payload"]["source"], "app1");
    assert_eq!(finding_rec["payload"]["evidence"], "catch (e) {");

    let runs_dir = fx.root.path().join("runs").join(&mission_id);
    let ledger = std::fs::read_to_string(runs_dir.join("ledger.jsonl")).unwrap();
    let ledger_rec: Value = serde_json::from_str(ledger.lines().next().unwrap()).unwrap();
    assert_eq!(ledger_rec["file"], "x.ts");
    assert_eq!(ledger_rec["file_raw"], "/workspace/app1/x.ts");

    let copy = std::fs::read_to_string(runs_dir.join("u-0001.findings.jsonl")).unwrap();
    let copy_rec: Value = serde_json::from_str(copy.lines().next().unwrap()).unwrap();
    // The per-unit copy is the RAW findings file, unmodified — file is
    // still the container path, no file_raw/corpus/unit/etc. added.
    assert_eq!(copy_rec["file"], "/workspace/app1/x.ts");
    assert!(copy_rec.get("file_raw").is_none(), "{copy_rec:#?}");
}

// ── test 3: kill file present before the second unit ────────────────────

#[test]
#[serial_test::serial]
fn kill_file_present_before_second_unit_stops_the_crawl() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let root_path = fx.root.path().to_path_buf();
    let mut scripts = BTreeMap::new();
    scripts.insert("u-0001".to_string(), ScriptedUnit::default());
    scripts.insert("u-0002".to_string(), ScriptedUnit::default());
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(scripts, &calls, move |unit_id| {
        // Simulate the operator dropping STOP while u-0001 is in flight —
        // the loop's kill-file check runs BEFORE the next unit, so u-0002
        // must never be dispatched.
        if unit_id == "u-0001" {
            std::fs::write(root_path.join("STOP"), "").unwrap();
        }
    });

    let code = run(&params_for(&fx), None, &mut dispatch).unwrap();
    assert_eq!(code, 3);
    assert_eq!(calls.borrow().clone(), vec!["u-0001".to_string()], "u-0002 must never be dispatched");

    let records = read_all_flow_records();
    let completed = records.iter().find(|r| r["action"] == "crawl.mission.completed").unwrap();
    assert_eq!(completed["payload"]["stopped_by"], "kill_file");
    assert_eq!(completed["payload"]["units_completed"], 1);
    assert_eq!(completed["payload"]["units_skipped"], 1);
}

// ── test 4: limit + units filter ────────────────────────────────────────

#[test]
#[serial_test::serial]
fn limit_caps_the_unit_count_and_reports_stopped_by_limit() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let mut params = params_for(&fx);
    params.insert("limit".to_string(), Value::String("1".to_string()));
    let mut scripts = BTreeMap::new();
    scripts.insert("u-0001".to_string(), ScriptedUnit::default());
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(scripts, &calls, |_| {});

    let code = run(&params, None, &mut dispatch).unwrap();
    assert_eq!(code, 0);
    assert_eq!(calls.borrow().clone(), vec!["u-0001".to_string()]);

    let records = read_all_flow_records();
    let completed = records.iter().find(|r| r["action"] == "crawl.mission.completed").unwrap();
    assert_eq!(completed["payload"]["stopped_by"], "limit");
}

#[test]
#[serial_test::serial]
fn units_filter_selects_by_id_and_bails_on_unknown_id() {
    {
        let _guard = TestGuard::new();
        let fx = two_source_fixture();
        let mut params = params_for(&fx);
        params.insert("units".to_string(), Value::String("u-0002".to_string()));
        let mut scripts = BTreeMap::new();
        scripts.insert("u-0002".to_string(), ScriptedUnit::default());
        let calls = RefCell::new(Vec::new());
        let mut dispatch = make_dispatch_fn(scripts, &calls, |_| {});
        let code = run(&params, None, &mut dispatch).unwrap();
        assert_eq!(code, 0);
        assert_eq!(calls.borrow().clone(), vec!["u-0002".to_string()]);
    }
    {
        let _guard = TestGuard::new();
        let fx = two_source_fixture();
        let mut params = params_for(&fx);
        params.insert("units".to_string(), Value::String("u-9999".to_string()));
        let calls = RefCell::new(Vec::new());
        let mut dispatch = make_dispatch_fn(BTreeMap::new(), &calls, |_| {});
        let err = run(&params, None, &mut dispatch).unwrap_err();
        assert!(err.to_string().contains("u-9999"), "{err}");
        assert!(calls.borrow().clone().is_empty(), "an unknown unit id must bail before any dispatch");
    }
}

// ── test 5: a dispatch error does not stop the crawl ────────────────────

#[test]
#[serial_test::serial]
fn a_dispatch_error_on_one_unit_does_not_stop_the_crawl() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let mut scripts = BTreeMap::new();
    scripts.insert("u-0001".to_string(), ScriptedUnit { err: true, ..Default::default() });
    scripts.insert("u-0002".to_string(), ScriptedUnit::default());
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(scripts, &calls, |_| {});

    let code = run(&params_for(&fx), None, &mut dispatch).unwrap();
    assert_eq!(code, 0);
    assert_eq!(calls.borrow().clone(), vec!["u-0001".to_string(), "u-0002".to_string()]);

    let records = read_all_flow_records();
    let completed = records.iter().find(|r| r["action"] == "crawl.mission.completed").unwrap();
    assert_eq!(completed["payload"]["units_errored"], 1);
    assert_eq!(completed["payload"]["units_completed"], 1);
    assert_eq!(completed["payload"]["stopped_by"], "done");
}

// ── test 6: a stale plan sha bails loudly before any dispatch ──────────

#[test]
#[serial_test::serial]
fn stale_plan_sha_bails_loud_before_any_dispatch() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let (manifest, _) = CorpusManifest::load(&fx.manifest_path).unwrap();
    let (rules_vec, _) = rules::resolve(&manifest.rules, None).unwrap();
    let resolved = sources::resolve(&manifest, true).unwrap();
    let mut stale_plan = plan::plan(&manifest, &rules_vec, &resolved).unwrap();
    stale_plan.sources[0].sha = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string();
    let plan_path = fx.root.path().join("stale-plan.json");
    std::fs::write(&plan_path, serde_json::to_string(&stale_plan).unwrap()).unwrap();

    let mut params = params_for(&fx);
    params.insert("plan".to_string(), Value::String(plan_path.to_string_lossy().to_string()));
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(BTreeMap::new(), &calls, |_| {});
    let err = run(&params, None, &mut dispatch).unwrap_err();
    assert!(err.to_string().contains("moved since"), "{err}");
    assert!(calls.borrow().clone().is_empty());
}

// ── test 7: workspace_read_only reaches DispatchOpts ────────────────────

#[test]
#[serial_test::serial]
fn crawl_launcher_dispatches_with_workspace_read_only() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let seen: RefCell<Vec<bool>> = RefCell::new(Vec::new());
    let calls = RefCell::new(Vec::new());
    let scripts: BTreeMap<String, ScriptedUnit> = BTreeMap::new();
    let mut dispatch = |opts: DispatchOpts| -> Result<DispatchResult> {
        seen.borrow_mut().push(opts.workspace_read_only);
        let session_id = opts.session_id.clone().unwrap_or_default();
        let unit_id = unit_id_from_session(&session_id);
        calls.borrow_mut().push(unit_id.clone());
        emit_scripted_bookend(darkmux_flow::DISPATCH_START, &opts, &session_id, None);
        let script = scripts.get(&unit_id).cloned().unwrap_or_default();
        let result = scripted_ok_result(&unit_id, &script, session_id.clone());
        emit_scripted_bookend(darkmux_flow::DISPATCH_COMPLETE, &opts, &session_id, Some(script.model));
        Ok(result)
    };

    run(&params_for(&fx), None, &mut dispatch).unwrap();
    let seen = seen.into_inner();
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|&b| b), "every unit dispatch must set workspace_read_only: true — {seen:?}");
}

// ── test 8: message builder shapes + no darkmux-internal vocabulary ────

fn test_rule(id: &str, matches: &str) -> Rule {
    Rule {
        id: id.to_string(),
        kind: RuleKind::Site,
        title: Some(format!("Title for {id}")),
        applies_to: vec![],
        exclude: vec![],
        prefilter: vec![],
        window: None,
        chunk_tokens: None,
        edge: None,
        matches: Some(matches.to_string()),
        no_match: Some(format!("NOMATCH for {id}")),
        evidence: Some(format!("EVIDENCE for {id}")),
        why_hint: Some(format!("WHY for {id}")),
        extras: Default::default(),
    }
}

#[test]
fn message_builder_site_shape_has_scope_and_pattern_prose() {
    let rules_by_id: BTreeMap<String, Rule> = [("r1".to_string(), test_rule("r1", "MATCH ONE"))].into_iter().collect();
    let unit = Unit::Site {
        id: "u-0001".to_string(),
        rule: "r1".to_string(),
        source: "app1".to_string(),
        sites: vec![Site { file: "x.ts".to_string(), line: 3, start: 1, end: 6, hits: vec![3] }],
        est_tokens: 10,
    };
    let msg = build_message(&rules_by_id, &unit).unwrap();
    assert!(msg.contains("MATCH ONE"), "{msg}");
    assert!(msg.contains("NOMATCH for r1"), "{msg}");
    assert!(msg.contains("EVIDENCE for r1"), "{msg}");
    assert!(msg.contains("WHY for r1"), "{msg}");
    assert!(msg.contains("/workspace/app1"), "{msg}");
    assert!(msg.contains("- /workspace/app1/x.ts:3 (read lines 1-6)"), "{msg}");
    assert!(!msg.contains("- x.ts:3"), "sites must be full container paths, not source-relative: {msg}");
    assert!(msg.contains("report_finding"), "{msg}");
}

#[test]
fn message_builder_read_shape_lists_files_and_every_bound_rule() {
    let rules_by_id: BTreeMap<String, Rule> =
        [("r1".to_string(), test_rule("r1", "MATCH ONE")), ("r2".to_string(), test_rule("r2", "MATCH TWO"))]
            .into_iter()
            .collect();
    let unit = Unit::Read {
        id: "u-0002".to_string(),
        rules: vec!["r1".to_string(), "r2".to_string()],
        source: "app1".to_string(),
        files: vec![
            ReadFileEntry::Whole("a.ts".to_string()),
            ReadFileEntry::Range { file: "b.ts".to_string(), start: 1, end: 10 },
        ],
        est_tokens: 20,
    };
    let msg = build_message(&rules_by_id, &unit).unwrap();
    assert!(msg.contains("MATCH ONE"), "{msg}");
    assert!(msg.contains("MATCH TWO"), "{msg}");
    assert!(msg.contains("- /workspace/app1/a.ts"), "{msg}");
    assert!(msg.contains("- /workspace/app1/b.ts (lines 1-10)"), "{msg}");
    assert!(msg.contains("/workspace/app1"), "{msg}");
}

#[test]
fn message_builder_edge_shape_includes_library_surface_and_versions() {
    let rules_by_id: BTreeMap<String, Rule> = [("r1".to_string(), test_rule("r1", "MATCH EDGE"))].into_iter().collect();
    let unit = Unit::Edge {
        id: "u-0003".to_string(),
        rule: "r1".to_string(),
        source: "app1".to_string(),
        library: "lib1".to_string(),
        package: "@org/lib".to_string(),
        pinned: "^5.0.0".to_string(),
        library_version: "8.1.1".to_string(),
        range_admits: false,
        sites: vec![Site { file: "uses.ts".to_string(), line: 1, start: 1, end: 1, hits: vec![1] }],
        library_tree: PathBuf::from("/tmp/x"),
        library_surface: vec!["index.js".to_string(), "CHANGELOG.md".to_string()],
        note: None,
        est_tokens: 5,
    };
    let msg = build_message(&rules_by_id, &unit).unwrap();
    assert!(msg.contains("MATCH EDGE"), "{msg}");
    assert!(msg.contains("/workspace/lib1"), "{msg}");
    assert!(msg.contains("@org/lib"), "{msg}");
    assert!(msg.contains("^5.0.0"), "{msg}");
    assert!(msg.contains("8.1.1"), "{msg}");
    assert!(msg.contains("index.js"), "{msg}");
    assert!(msg.contains("CHANGELOG.md"), "{msg}");
    assert!(msg.contains("- /workspace/app1/uses.ts:1"), "{msg}");
    assert!(msg.contains("starting with `/workspace/`"), "{msg}");
}

#[test]
fn message_builder_never_uses_darkmux_internal_vocabulary() {
    let rules_by_id: BTreeMap<String, Rule> = [("r1".to_string(), test_rule("r1", "MATCH X"))].into_iter().collect();
    let site = Unit::Site {
        id: "u-0001".to_string(),
        rule: "r1".to_string(),
        source: "app1".to_string(),
        sites: vec![Site { file: "x.ts".to_string(), line: 1, start: 1, end: 1, hits: vec![1] }],
        est_tokens: 1,
    };
    let read = Unit::Read {
        id: "u-0002".to_string(),
        rules: vec!["r1".to_string()],
        source: "app1".to_string(),
        files: vec![ReadFileEntry::Whole("a.ts".to_string())],
        est_tokens: 1,
    };
    let edge = Unit::Edge {
        id: "u-0003".to_string(),
        rule: "r1".to_string(),
        source: "app1".to_string(),
        library: "lib1".to_string(),
        package: "@org/lib".to_string(),
        pinned: "^5.0.0".to_string(),
        library_version: "8.1.1".to_string(),
        range_admits: false,
        sites: vec![Site { file: "uses.ts".to_string(), line: 1, start: 1, end: 1, hits: vec![1] }],
        library_tree: PathBuf::from("/tmp/x"),
        library_surface: vec!["index.js".to_string()],
        note: None,
        est_tokens: 1,
    };
    for u in [&site, &read, &edge] {
        let msg = build_message(&rules_by_id, u).unwrap();
        let lower = msg.to_lowercase();
        for banned in ["unit", "ledger", "corpus", "packet"] {
            assert!(!lower.contains(banned), "message must not contain '{banned}': {msg}");
        }
    }
}

// ── test 9: tokens_per_hour is a measurement, computed correctly ───────

#[test]
#[serial_test::serial]
fn tokens_per_hour_is_total_tokens_over_wall_hours() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let mut scripts = BTreeMap::new();
    // 1000 prompt + 500 completion tokens over exactly one hour of wall
    // time => 1500 tokens/hour, deterministically (no clock reads).
    scripts.insert(
        "u-0001".to_string(),
        ScriptedUnit { prompt_tokens: 1000, completion_tokens: 500, wall_ms: 3_600_000, ..Default::default() },
    );
    scripts.insert(
        "u-0002".to_string(),
        ScriptedUnit { prompt_tokens: 0, completion_tokens: 0, wall_ms: 0, ..Default::default() },
    );
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(scripts, &calls, |_| {});
    run(&params_for(&fx), None, &mut dispatch).unwrap();

    let records = read_all_flow_records();
    let completed = records.iter().find(|r| r["action"] == "crawl.mission.completed").unwrap();
    assert_eq!(completed["payload"]["tokens_per_hour"], 1500);
    assert_eq!(completed["payload"]["prompt_tokens"], 1000);
    assert_eq!(completed["payload"]["completion_tokens"], 500);
}

// ── test 10: the persisted mission/task/step structure mission status
//    reads from ───────────────────────────────────────────────────────
//
// There is no structured (non-stdout-parsing) `mission status` test
// harness to reuse — `mission_status::run` prints a table and returns
// only an exit code. This asserts directly against the SAME
// `crew::lifecycle` records that verb reads, which is the load-bearing
// claim ("mission status lists the crawl mission with its tasks") without
// coupling the test to stdout formatting.

#[test]
#[serial_test::serial]
fn mission_and_task_structure_is_the_shape_mission_status_reads() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let mut scripts = BTreeMap::new();
    scripts.insert("u-0001".to_string(), ScriptedUnit::default());
    scripts.insert("u-0002".to_string(), ScriptedUnit::default());
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(scripts, &calls, |_| {});
    run(&params_for(&fx), None, &mut dispatch).unwrap();

    let records = read_all_flow_records();
    let mission_id = mission_id_from_records(&records);

    let mission_json = std::fs::read_to_string(crew::lifecycle::mission_path(&mission_id)).unwrap();
    let mission: crew::types::Mission = serde_json::from_str(&mission_json).unwrap();
    assert_eq!(mission.phase_ids.len(), 1);
    assert_eq!(mission.status, crew::types::MissionStatus::Finalized);

    let phase_id = &mission.phase_ids[0];
    let tasks = crew::lifecycle::load_tasks_for_phase(&mission_id, phase_id).unwrap();
    // Two sources, each its own Task — the (source, rule) grouping.
    assert_eq!(tasks.len(), 2, "{tasks:#?}");
    for t in &tasks {
        assert_eq!(t.role_id.as_deref(), Some("crawler"));
        assert_eq!(t.step_ids.len(), 1, "one unit per source in this fixture: {t:#?}");
    }

    let steps = crew::lifecycle::load_steps_for_phase(&mission_id, phase_id).unwrap();
    assert_eq!(steps.len(), 2, "{steps:#?}");
    for s in &steps {
        assert_eq!(s.status, crew::types::NodeStatus::Complete, "{s:#?}");
    }
}

// ── #1959 merge-gate findings ───────────────────────────────────────────

/// A synthetic plan with `n` `Unit::Site` entries, all bound to
/// `swallowed-error` against source `app1` (present in `two_source_
/// fixture`'s manifest) — used where a test needs a plan LARGER than the
/// fixture's own 2-unit corpus (finding 2's "70 in plan, 1 selected"
/// shape) without spinning up 70 real git repos. `sources` is built from
/// the CALLER-supplied `resolved` list (real `sources::resolve` output for
/// the fixture) rather than left empty: an empty `sources` here used to
/// exploit the exact hole finding 3 closes — a unit naming a source absent
/// from `plan.sources` sailed through unvalidated, defaulting to `sha: ""`
/// in every downstream `crawl.*` record. Declaring `app1` properly keeps
/// this fixture honest against that guard.
fn synthetic_plan_with_n_units(corpus_name: &str, n: usize, resolved: &[sources::ResolvedSource]) -> Plan {
    let units: Vec<Unit> = (1..=n)
        .map(|i| Unit::Site {
            id: format!("u-{i:04}"),
            rule: "swallowed-error".to_string(),
            source: "app1".to_string(),
            sites: vec![Site { file: "x.ts".to_string(), line: 5, start: 1, end: 6, hits: vec![5] }],
            est_tokens: 10,
        })
        .collect();
    let plan_sources: Vec<plan::PlanSource> = resolved
        .iter()
        .map(|r| plan::PlanSource {
            id: r.id.clone(),
            sha: r.sha.clone(),
            git_ref: r.git_ref.clone(),
            tree: r.tree.clone(),
            files_walked: 0,
        })
        .collect();
    Plan {
        schema_version: plan::PLAN_SCHEMA_VERSION.to_string(),
        corpus: corpus_name.to_string(),
        planned_at: "2026-01-01T00:00:00Z".to_string(),
        sources: plan_sources,
        units,
        totals: plan::Totals::default(),
    }
}

/// Whole-word (case-insensitive) substring check — `str::contains` alone
/// would false-positive on e.g. "corpuscular", and finding 4's vocabulary
/// guard is specifically about a darkmux-internal TERM leaking into
/// model-facing prose, not any substring collision.
fn contains_word(haystack: &str, word: &str) -> bool {
    let lower = haystack.to_lowercase();
    let word = word.to_lowercase();
    lower.split(|c: char| !c.is_alphanumeric()).any(|tok| tok == word)
}

// ── finding 1a: pre-mint rule validation ────────────────────────────────

#[test]
#[serial_test::serial]
fn plan_naming_a_rule_the_manifest_no_longer_declares_bails_before_any_mint() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let (manifest, _) = CorpusManifest::load(&fx.manifest_path).unwrap();
    let (rules_vec, _) = rules::resolve(&manifest.rules, None).unwrap();
    let resolved = sources::resolve(&manifest, true).unwrap();
    let mut stale_plan = plan::plan(&manifest, &rules_vec, &resolved).unwrap();
    match &mut stale_plan.units[0] {
        Unit::Site { rule, .. } => *rule = "ghost-rule".to_string(),
        other => panic!("fixture expected to produce Unit::Site units, got {other:?}"),
    }
    let plan_path = fx.root.path().join("ghost-rule-plan.json");
    std::fs::write(&plan_path, serde_json::to_string(&stale_plan).unwrap()).unwrap();

    let mut params = params_for(&fx);
    params.insert("plan".to_string(), Value::String(plan_path.to_string_lossy().to_string()));
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(BTreeMap::new(), &calls, |_| {});
    let err = run(&params, None, &mut dispatch).unwrap_err();
    assert!(err.to_string().contains("ghost-rule"), "{err}");
    assert!(calls.borrow().clone().is_empty(), "no dispatch before the pre-mint bail");

    let records = read_all_flow_records();
    assert!(
        records.iter().all(|r| r["action"] != "crawl.mission.started"),
        "a pre-mint bail must never emit crawl.mission.started — no mission was minted: {records:#?}"
    );
}

// ── round-3 must-fix 1: the guarded mint window ─────────────────────────

/// A failure in the mint window (mission mint through the mission-specific
/// runs-dir creation) must never strand an Active mission with an unpaired
/// `crawl.mission.started`. `<corpus root>/runs` already existing as a
/// regular file forces `create_dir_all` to fail at the LAST fallible step
/// of the window — after the mission/phase/task/step records and the
/// `crawl.mission.started` record have already been written — which is
/// exactly the shape that used to strand.
#[test]
#[serial_test::serial]
fn runs_dir_collision_reconciles_the_mint_instead_of_stranding_it() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    // `runs` as a regular FILE, not a directory — `create_dir_all` on any
    // path under it must fail.
    std::fs::write(fx.root.path().join("runs"), "not a directory").unwrap();

    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(BTreeMap::new(), &calls, |_| {});
    let err = run(&params_for(&fx), None, &mut dispatch).unwrap_err();
    assert!(err.to_string().contains("runs"), "{err}");
    assert!(calls.borrow().clone().is_empty(), "no dispatch — the mint window fails before the per-unit loop");

    let records = read_all_flow_records();
    let mission_id = mission_id_from_records(&records);

    // paired-or-both-absent: `crawl.mission.started` DID fire (the window
    // fails after it), so `crawl.mission.completed` must have too.
    let started = records.iter().filter(|r| r["action"] == "crawl.mission.started").count();
    let completed = records.iter().filter(|r| r["action"] == "crawl.mission.completed").count();
    assert_eq!(started, completed, "crawl.mission.started/completed must be paired, never one without the other: {records:#?}");
    assert_eq!(started, 1, "expected exactly the one mint attempt's started record: {records:#?}");
    let completed_record = records.iter().find(|r| r["action"] == "crawl.mission.completed").unwrap();
    assert_eq!(completed_record["payload"]["stopped_by"], "error");
    assert_eq!(completed_record["payload"]["units_completed"], 0);

    // never left stuck Active — reconciled to a terminal by `reconcile_
    // mint_failure`, matching `dispatch_as_crew_of_one`'s own mint-window
    // guard and `mission status`'s ACTIVE bucket definition (both key off
    // exactly this field).
    let mission_json = std::fs::read_to_string(crew::lifecycle::mission_path(&mission_id)).unwrap();
    let mission: crew::types::Mission = serde_json::from_str(&mission_json).unwrap();
    assert_ne!(
        mission.status,
        crew::types::MissionStatus::Active,
        "a mint-window failure must never leave the mission stuck Active — {mission:#?}"
    );
}

// ── round-3 must-fix 3: the `sources: []` plan hole ─────────────────────

/// A hand-crafted (or corrupted) plan whose `units` name a real source but
/// whose `sources` list is empty must bail BEFORE any mint, naming both
/// the unit and the source — not sail through and let the unit's sha
/// silently default to `""` in every downstream `crawl.*` record.
#[test]
#[serial_test::serial]
fn plan_with_empty_sources_naming_a_real_unit_bails_before_any_mint() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let (manifest, _) = CorpusManifest::load(&fx.manifest_path).unwrap();
    let empty_sources_plan = Plan {
        schema_version: plan::PLAN_SCHEMA_VERSION.to_string(),
        corpus: manifest.name.clone(),
        planned_at: "2026-01-01T00:00:00Z".to_string(),
        sources: Vec::new(),
        units: vec![Unit::Site {
            id: "u-0001".to_string(),
            rule: "swallowed-error".to_string(),
            source: "app1".to_string(),
            sites: vec![Site { file: "x.ts".to_string(), line: 5, start: 1, end: 6, hits: vec![5] }],
            est_tokens: 10,
        }],
        totals: plan::Totals::default(),
    };
    let plan_path = fx.root.path().join("empty-sources-plan.json");
    std::fs::write(&plan_path, serde_json::to_string(&empty_sources_plan).unwrap()).unwrap();

    let mut params = params_for(&fx);
    params.insert("plan".to_string(), Value::String(plan_path.to_string_lossy().to_string()));
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(BTreeMap::new(), &calls, |_| {});
    let err = run(&params, None, &mut dispatch).unwrap_err();
    assert!(err.to_string().contains("u-0001"), "{err}");
    assert!(err.to_string().contains("app1"), "{err}");
    assert!(calls.borrow().clone().is_empty(), "no dispatch before the pre-mint bail");

    let records = read_all_flow_records();
    assert!(
        records.iter().all(|r| r["action"] != "crawl.mission.started"),
        "a pre-mint bail must never emit crawl.mission.started — no mission was minted: {records:#?}"
    );
}

// ── finding 1b: the RAII finalize guard ─────────────────────────────────

/// A panic injected through the `dispatch_fn` seam mid-loop (the task's
/// own suggested injection point) must still leave a matching `crawl.
/// mission.completed` record, a written envelope, and a non-`Active`
/// mission behind — the guard's whole reason to exist.
#[test]
#[serial_test::serial]
fn a_panic_mid_loop_still_finalizes_via_the_raii_guard() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();

    let mut dispatch = |opts: DispatchOpts| -> Result<DispatchResult> {
        let session_id = opts.session_id.clone().unwrap_or_default();
        let unit_id = unit_id_from_session(&session_id);
        emit_scripted_bookend(darkmux_flow::DISPATCH_START, &opts, &session_id, None);
        if unit_id == "u-0001" {
            panic!("simulated mid-dispatch panic for {unit_id}");
        }
        let script = ScriptedUnit::default();
        let result = scripted_ok_result(&unit_id, &script, session_id.clone());
        emit_scripted_bookend(darkmux_flow::DISPATCH_COMPLETE, &opts, &session_id, Some(script.model));
        Ok(result)
    };

    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&params_for(&fx), None, &mut dispatch)));
    std::panic::set_hook(prev_hook);
    assert!(outcome.is_err(), "expected the injected panic to propagate out of run()");

    let records = read_all_flow_records();
    let mission_id = mission_id_from_records(&records);
    let completed = records
        .iter()
        .find(|r| r["action"] == "crawl.mission.completed")
        .expect("the RAII guard must still emit crawl.mission.completed on a panic");
    assert_eq!(completed["payload"]["stopped_by"], "error");
    assert_eq!(completed["payload"]["units_completed"], 0);

    let runs_dir = fx.root.path().join("runs").join(&mission_id);
    assert!(runs_dir.join("envelope.json").exists(), "the envelope must still be written on the abort path");
    let envelope: Value = serde_json::from_str(&std::fs::read_to_string(runs_dir.join("envelope.json")).unwrap()).unwrap();
    assert_eq!(envelope["stopped_by"], "error");

    let mission_json = std::fs::read_to_string(crew::lifecycle::mission_path(&mission_id)).unwrap();
    let mission: crew::types::Mission = serde_json::from_str(&mission_json).unwrap();
    assert_ne!(
        mission.status,
        crew::types::MissionStatus::Active,
        "the mission must never be left stuck Active after an abort — {mission:#?}"
    );

    let phase_id = format!("{mission_id}-crawl");
    let phase_json = std::fs::read_to_string(crew::lifecycle::phase_path(&mission_id, &phase_id)).unwrap();
    let phase: crew::types::Phase = serde_json::from_str(&phase_json).unwrap();
    assert_eq!(phase.status, crew::types::PhaseStatus::Abandoned, "an aborted crawl abandons its phase, not completes it");
}

// ── finding 2: plan-size accounting ─────────────────────────────────────

#[test]
#[serial_test::serial]
fn seventy_in_plan_one_selected_reports_units_not_run_everywhere() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let (manifest, _) = CorpusManifest::load(&fx.manifest_path).unwrap();
    let resolved = sources::resolve(&manifest, true).unwrap();
    let synthetic = synthetic_plan_with_n_units(&manifest.name, 70, &resolved);
    let plan_path = fx.root.path().join("big-plan.json");
    std::fs::write(&plan_path, serde_json::to_string(&synthetic).unwrap()).unwrap();

    let mut params = params_for(&fx);
    params.insert("plan".to_string(), Value::String(plan_path.to_string_lossy().to_string()));
    params.insert("limit".to_string(), Value::String("1".to_string()));

    let mut scripts = BTreeMap::new();
    scripts.insert("u-0001".to_string(), ScriptedUnit::default());
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(scripts, &calls, |_| {});

    let code = run(&params, None, &mut dispatch).unwrap();
    assert_eq!(code, 0);
    assert_eq!(calls.borrow().clone(), vec!["u-0001".to_string()], "only the selected unit is ever dispatched");

    let records = read_all_flow_records();
    let started = records.iter().find(|r| r["action"] == "crawl.mission.started").unwrap();
    assert_eq!(started["payload"]["units_in_plan"], 70);
    assert_eq!(started["payload"]["units_selected"], 1);

    let completed = records.iter().find(|r| r["action"] == "crawl.mission.completed").unwrap();
    assert_eq!(completed["payload"]["units_in_plan"], 70);
    assert_eq!(completed["payload"]["units_selected"], 1);
    assert_eq!(completed["payload"]["units_not_run"], 69);

    let mission_id = mission_id_from_records(&records);
    let runs_dir = fx.root.path().join("runs").join(&mission_id);
    let envelope: Value = serde_json::from_str(&std::fs::read_to_string(runs_dir.join("envelope.json")).unwrap()).unwrap();
    assert_eq!(envelope["units_in_plan"], 70);
    assert_eq!(envelope["units_selected"], 1);
    assert_eq!(envelope["units_not_run"], 69);
}

// ── finding 3: a deliberate stop is not a completion ────────────────────

#[test]
#[serial_test::serial]
fn kill_file_stop_abandons_the_phase_not_completes_it() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let root_path = fx.root.path().to_path_buf();
    let mut scripts = BTreeMap::new();
    scripts.insert("u-0001".to_string(), ScriptedUnit::default());
    scripts.insert("u-0002".to_string(), ScriptedUnit::default());
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(scripts, &calls, move |unit_id| {
        if unit_id == "u-0001" {
            std::fs::write(root_path.join("STOP"), "").unwrap();
        }
    });

    let code = run(&params_for(&fx), None, &mut dispatch).unwrap();
    assert_eq!(code, 3);

    let records = read_all_flow_records();
    let mission_id = mission_id_from_records(&records);
    let phase_id = format!("{mission_id}-crawl");
    let phase_json = std::fs::read_to_string(crew::lifecycle::phase_path(&mission_id, &phase_id)).unwrap();
    let phase: crew::types::Phase = serde_json::from_str(&phase_json).unwrap();
    assert_eq!(
        phase.status,
        crew::types::PhaseStatus::Abandoned,
        "a kill-file stop is not a completion (#1959 merge-gate finding 3)"
    );
}

#[test]
#[serial_test::serial]
fn done_stop_leaves_the_phase_complete() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let mut scripts = BTreeMap::new();
    scripts.insert("u-0001".to_string(), ScriptedUnit::default());
    scripts.insert("u-0002".to_string(), ScriptedUnit::default());
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(scripts, &calls, |_| {});

    run(&params_for(&fx), None, &mut dispatch).unwrap();

    let records = read_all_flow_records();
    let mission_id = mission_id_from_records(&records);
    let phase_id = format!("{mission_id}-crawl");
    let phase_json = std::fs::read_to_string(crew::lifecycle::phase_path(&mission_id, &phase_id)).unwrap();
    let phase: crew::types::Phase = serde_json::from_str(&phase_json).unwrap();
    assert_eq!(phase.status, crew::types::PhaseStatus::Complete, "a clean `done` stop IS a completion");
}

// ── finding 4: shipped rule prose vs the vocabulary guard ───────────────

/// Every EMBEDDED rule (not just a synthetic test rule) run through the
/// exact resolution path `run()` itself uses, checked against the full
/// darkmux-internal vocabulary list — this is what caught
/// `stale-consumer.json`'s "this unit" in its `no_match` field, which the
/// pre-existing `message_builder_never_uses_darkmux_internal_vocabulary`
/// test above could not: that test only ever exercises a synthetic
/// `test_rule`, never the shipped rule files.
#[test]
fn every_shipped_rule_avoids_darkmux_internal_vocabulary() {
    let (embedded, load_warnings) = rules::load_all(None);
    assert!(load_warnings.is_empty(), "{load_warnings:?}");
    let all_ids: Vec<String> = embedded.keys().cloned().collect();
    assert!(all_ids.len() >= 3, "expected at least the 3 known built-in rules: {all_ids:?}");

    let (resolved, resolve_warnings) = rules::resolve(&all_ids, None).unwrap();
    assert!(resolve_warnings.is_empty(), "{resolve_warnings:?}");
    assert_eq!(resolved.len(), all_ids.len());

    for rule in &resolved {
        let rendered = pattern_block(rule);
        for banned in ["unit", "ledger", "corpus", "packet", "darkmux"] {
            assert!(
                !contains_word(&rendered, banned),
                "rule `{}` renders the darkmux-internal word `{banned}` into model-facing prose: {rendered}",
                rule.id
            );
        }
    }
}

// ── finding 5: crawl.unit.started carries the UNIT session id ───────────

#[test]
#[serial_test::serial]
fn unit_started_and_completed_share_the_same_session_id() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let mut scripts = BTreeMap::new();
    scripts.insert("u-0001".to_string(), ScriptedUnit::default());
    scripts.insert("u-0002".to_string(), ScriptedUnit::default());
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(scripts, &calls, |_| {});
    run(&params_for(&fx), None, &mut dispatch).unwrap();

    let records = read_all_flow_records();
    for r in records.iter().filter(|r| r["action"] == "crawl.unit.started") {
        let sid = r["session_id"].as_str().expect("crawl.unit.started must carry a session_id");
        assert!(sid.starts_with("crawl-"), "{r:#?}");
        assert_ne!(sid, r["mission_id"].as_str().unwrap(), "must be the UNIT session, not the mission id");
    }

    let started_sessions: Vec<&str> =
        records.iter().filter(|r| r["action"] == "crawl.unit.started").map(|r| r["session_id"].as_str().unwrap()).collect();
    let completed_sessions: Vec<&str> = records
        .iter()
        .filter(|r| r["action"] == "crawl.unit.completed")
        .map(|r| r["session_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        started_sessions, completed_sessions,
        "each unit's started/completed records must carry the SAME session id (#1959 merge-gate finding 5)"
    );
}

// ── finding 6: plan reload validation ───────────────────────────────────

#[test]
#[serial_test::serial]
fn plan_corpus_mismatch_bails_loud() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let (manifest, _) = CorpusManifest::load(&fx.manifest_path).unwrap();
    let (rules_vec, _) = rules::resolve(&manifest.rules, None).unwrap();
    let resolved = sources::resolve(&manifest, true).unwrap();
    let mut plan_wrong_corpus = plan::plan(&manifest, &rules_vec, &resolved).unwrap();
    plan_wrong_corpus.corpus = "some-other-corpus".to_string();
    let plan_path = fx.root.path().join("wrong-corpus-plan.json");
    std::fs::write(&plan_path, serde_json::to_string(&plan_wrong_corpus).unwrap()).unwrap();

    let mut params = params_for(&fx);
    params.insert("plan".to_string(), Value::String(plan_path.to_string_lossy().to_string()));
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(BTreeMap::new(), &calls, |_| {});
    let err = run(&params, None, &mut dispatch).unwrap_err();
    assert!(err.to_string().contains("some-other-corpus"), "{err}");
    assert!(calls.borrow().clone().is_empty());
}

#[test]
#[serial_test::serial]
fn plan_source_tree_mismatch_bails_loud() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let (manifest, _) = CorpusManifest::load(&fx.manifest_path).unwrap();
    let (rules_vec, _) = rules::resolve(&manifest.rules, None).unwrap();
    let resolved = sources::resolve(&manifest, true).unwrap();
    let mut plan_wrong_tree = plan::plan(&manifest, &rules_vec, &resolved).unwrap();
    plan_wrong_tree.sources[0].tree = PathBuf::from("/nonexistent/relocated/tree");
    let plan_path = fx.root.path().join("wrong-tree-plan.json");
    std::fs::write(&plan_path, serde_json::to_string(&plan_wrong_tree).unwrap()).unwrap();

    let mut params = params_for(&fx);
    params.insert("plan".to_string(), Value::String(plan_path.to_string_lossy().to_string()));
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(BTreeMap::new(), &calls, |_| {});
    let err = run(&params, None, &mut dispatch).unwrap_err();
    assert!(err.to_string().contains("different tree"), "{err}");
    assert!(calls.borrow().clone().is_empty());
}

#[test]
fn schema_major_mismatch_warns_only_on_a_real_major_difference() {
    assert!(plan_schema_major_mismatch_warning("2.0", "1.0").is_some());
    assert!(plan_schema_major_mismatch_warning("1.0", "1.0").is_none());
    assert!(plan_schema_major_mismatch_warning("1.9", "1.0").is_none(), "a minor bump is not a major mismatch");
}

// ── finding 7: `--param units=` parsing to zero ids bails loudly ───────

#[test]
#[serial_test::serial]
fn units_param_parsing_to_zero_ids_bails_loud() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let mut params = params_for(&fx);
    params.insert("units".to_string(), Value::String(",, ".to_string()));
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(BTreeMap::new(), &calls, |_| {});
    let err = run(&params, None, &mut dispatch).unwrap_err();
    assert!(err.to_string().contains("named no unit ids"), "{err}");
    assert!(calls.borrow().clone().is_empty());
}

// ── finding 8: self-describing envelope ─────────────────────────────────

#[test]
#[serial_test::serial]
fn envelope_is_self_describing() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let mut params = params_for(&fx);
    params.insert("limit".to_string(), Value::String("1".to_string()));
    let mut scripts = BTreeMap::new();
    scripts.insert("u-0001".to_string(), ScriptedUnit { model: "darkmux:test-model", ..Default::default() });
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(scripts, &calls, |_| {});

    run(&params, Some(120), &mut dispatch).unwrap();

    let records = read_all_flow_records();
    let mission_id = mission_id_from_records(&records);
    let runs_dir = fx.root.path().join("runs").join(&mission_id);
    let envelope: Value = serde_json::from_str(&std::fs::read_to_string(runs_dir.join("envelope.json")).unwrap()).unwrap();

    assert_eq!(envelope["model"], "darkmux:test-model");
    assert_eq!(envelope["timeout_secs"], 120);
    assert_eq!(envelope["limit"], 1);
    assert_eq!(envelope["plan_path"], "planned in-process");
    assert!(envelope["units_filter"].is_null());
    assert_eq!(envelope["units"][0]["model"], "darkmux:test-model");

    let completed = records.iter().find(|r| r["action"] == "crawl.mission.completed").unwrap();
    assert_eq!(completed["model"], "darkmux:test-model", "the mission-level record's own FlowRecord.model field");
}

// ── round-3 CONSIDER 7: interrupted classification at readback ──────────

/// `INTERRUPTED` (`darkmux_types::interrupt`) is a process-wide flag that
/// never resets in production — see that module's own doc. Reset it
/// around this test (both before AND, via `Drop`, after) so a SIGINT
/// simulated here can never leak into any OTHER test sharing this same
/// test binary process.
struct InterruptFlagGuard;

impl InterruptFlagGuard {
    fn new() -> Self {
        darkmux_types::interrupt::reset_for_test();
        Self
    }
}

impl Drop for InterruptFlagGuard {
    fn drop(&mut self) {
        darkmux_types::interrupt::reset_for_test();
    }
}

/// (#1959 merge-gate finding 13) SIGINT arriving WHILE a unit's dispatch
/// is in flight must classify that unit as `"interrupted"` at readback —
/// neither a completion nor a per-unit error — and the mission overall as
/// `stopped_by: "interrupted"`, exit 130. Simulated by setting the flag
/// from inside the scripted dispatch closure itself (via `interrupt::
/// simulate_sigint_for_test`, the `test-support` hook — calls the SAME
/// internal handler a real Ctrl-C would), mimicking a signal that arrives
/// mid-dispatch rather than between units (the loop-top kill-file/
/// interrupt check, already covered by `kill_file_stop_abandons_the_
/// phase_not_completes_it`'s sibling tests, is a different code path).
#[test]
#[serial_test::serial]
fn interrupted_at_readback_reports_interrupted_not_error() {
    let _guard = TestGuard::new();
    let _interrupt_guard = InterruptFlagGuard::new();
    let fx = two_source_fixture();
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(BTreeMap::new(), &calls, |_| {
        darkmux_types::interrupt::simulate_sigint_for_test();
    });

    let code = run(&params_for(&fx), None, &mut dispatch).unwrap();
    assert_eq!(code, 130, "an interrupted run must exit 130");
    assert_eq!(calls.borrow().len(), 1, "the second unit must never be dispatched once interrupted");

    let records = read_all_flow_records();
    let completed =
        records.iter().find(|r| r["action"] == "crawl.unit.completed").expect("expected one completed record");
    assert_eq!(completed["payload"]["result"], "interrupted");

    let mission_completed = records.iter().find(|r| r["action"] == "crawl.mission.completed").unwrap();
    assert_eq!(mission_completed["payload"]["stopped_by"], "interrupted");
    assert_eq!(mission_completed["payload"]["units_errored"], 0, "an interrupted unit must not count as errored");
    assert_eq!(mission_completed["payload"]["units_completed"], 0, "an interrupted unit must not count as completed either");
}

// ── finding 9: watchdog timeout detection ───────────────────────────────

#[test]
fn watchdog_timeout_marker_is_detected_in_stderr() {
    assert!(watchdog_timeout_fired(&format!(
        "{} — no proof-of-work signal in 600s",
        crew::dispatch_internal::INACTIVITY_TIMEOUT_MARKER
    )));
    assert!(!watchdog_timeout_fired("some other stderr, nothing to do with a timeout"));
}

#[test]
fn interpret_dispatch_result_reports_timeout_when_the_watchdog_marker_is_present() {
    let res = DispatchResult {
        exit_code: 137,
        stdout: String::new(),
        stderr: format!("{} — no proof-of-work signal in 600s", crew::dispatch_internal::INACTIVITY_TIMEOUT_MARKER),
        session_id: "sess".to_string(),
        out_dir: None,
    };
    let (result_label, ..) = interpret_dispatch_result("u-0001", &res);
    assert_eq!(result_label, "timeout");
}

#[test]
fn interpret_dispatch_result_handles_non_json_stdout_via_exit_code() {
    let ok = DispatchResult {
        exit_code: 0,
        stdout: "not json, plain prose the model printed instead of the --json envelope".to_string(),
        stderr: String::new(),
        session_id: "s".to_string(),
        out_dir: None,
    };
    let (result_label, ..) = interpret_dispatch_result("u-0001", &ok);
    assert_eq!(result_label, "stop", "a zero exit with unparseable stdout still reads as a clean stop");

    let bad = DispatchResult {
        exit_code: 1,
        stdout: "not json, plain prose".to_string(),
        stderr: String::new(),
        session_id: "s".to_string(),
        out_dir: None,
    };
    let (result_label2, ..) = interpret_dispatch_result("u-0001", &bad);
    assert_eq!(result_label2, "error");
}

// ── finding 12: quiet zero-unit crawl still finalizes correctly ────────

#[test]
#[serial_test::serial]
fn limit_zero_selects_nothing_and_completes_cleanly() {
    let _guard = TestGuard::new();
    let fx = two_source_fixture();
    let mut params = params_for(&fx);
    params.insert("limit".to_string(), Value::String("0".to_string()));
    let calls = RefCell::new(Vec::new());
    let mut dispatch = make_dispatch_fn(BTreeMap::new(), &calls, |_| {});

    let code = run(&params, None, &mut dispatch).unwrap();
    assert_eq!(code, 0);
    assert!(calls.borrow().clone().is_empty(), "zero selected units means zero dispatches");

    let records = read_all_flow_records();
    let completed = records.iter().find(|r| r["action"] == "crawl.mission.completed").unwrap();
    assert_eq!(completed["payload"]["units_selected"], 0);
    assert_eq!(completed["payload"]["units_completed"], 0);
    assert_eq!(completed["payload"]["stopped_by"], "limit");
}

// (#1959, found by the first hooks-enabled live loop) A finding's `rule` must
// be ONE rule id — the pattern the model reported it under — never the unit's
// whole rule list. The receiver keys identity on it and refused every finding
// from a read unit because `rule` arrived as an array.
#[test]
fn finding_rule_is_the_reported_pattern_when_the_unit_carries_several_rules() {
    let ids = vec!["doc-contradicts-code".to_string(), "style-guide".to_string()];
    let (rule, unmatched) = super::finding_rule_for(Some("style-guide"), &ids);
    assert_eq!(rule, "style-guide");
    assert!(unmatched.is_none());
    // Case-insensitive match on what the model typed.
    let (rule, _) = super::finding_rule_for(Some("Doc-Contradicts-Code"), &ids);
    assert_eq!(rule, "doc-contradicts-code");
}

#[test]
fn finding_rule_on_a_single_rule_unit_is_that_rule_whatever_the_pattern_says() {
    let ids = vec!["swallowed-error".to_string()];
    let (rule, unmatched) = super::finding_rule_for(Some("something else"), &ids);
    assert_eq!(rule, "swallowed-error");
    assert!(unmatched.is_none(), "a single-rule unit has nothing to disambiguate");
}

#[test]
fn finding_rule_with_an_unmatched_pattern_keeps_the_first_rule_and_records_the_pattern() {
    let ids = vec!["a".to_string(), "b".to_string()];
    let (rule, unmatched) = super::finding_rule_for(Some("zzz"), &ids);
    assert_eq!(rule, "a");
    assert_eq!(unmatched.as_deref(), Some("zzz"));
}
