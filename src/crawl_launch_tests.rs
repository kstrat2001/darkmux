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
    }
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
    let (rules_vec, _) = rules::resolve_default(&manifest.rules).unwrap();
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
    assert!(msg.contains("- x.ts:3 (read lines 1-6)"), "{msg}");
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
    assert!(msg.contains("- a.ts"), "{msg}");
    assert!(msg.contains("- b.ts (lines 1-10)"), "{msg}");
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
    assert!(msg.contains("- uses.ts:1"), "{msg}");
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
