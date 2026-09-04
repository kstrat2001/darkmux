//! (#2301) `crawl.unit` + `crawl.summary` — no model, no container.
//!
//! Every test injects the dispatch (`CrawlUnitStepKind::with_dispatch`) and
//! scopes `DARKMUX_HOME` to a tempdir, so nothing here reads or writes the
//! operator's real root.

use super::*;
use darkmux_crew::types::{NodeStatus, Phase, PhaseStatus};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Scopes `DARKMUX_HOME` for one test and restores the prior value.
struct HomeGuard(Option<String>);
impl HomeGuard {
    fn set(p: &Path) -> Self {
        let prior = std::env::var("DARKMUX_HOME").ok();
        std::env::set_var("DARKMUX_HOME", p);
        Self(prior)
    }
}
impl Drop for HomeGuard {
    fn drop(&mut self) {
        match &self.0 {
            Some(v) => std::env::set_var("DARKMUX_HOME", v),
            None => std::env::remove_var("DARKMUX_HOME"),
        }
    }
}

const MISSION: &str = "crawl-2301";
const PHASE: &str = "crawl-2301-crawl";

fn save_phase(id: &str, mission: &str) {
    darkmux_crew::lifecycle::save_phase(&Phase {
        id: id.into(),
        mission_id: mission.into(),
        description: String::new(),
        display_name: None,
        status: PhaseStatus::Running,
        created_ts: 1,
        started_ts: None,
        completed_ts: None,
        abandoned_ts: None,
        task_ids: vec!["unit-task".into()],
    })
    .unwrap();
}

/// A one-unit plan naming a real rule, whose source tree lives under
/// `root/tree/app` so the unit's workspace root is `root/tree`.
fn write_plan(root: &Path, rule: &str, unit_id: &str, sha: &str) -> PathBuf {
    let tree = root.join("tree").join("app");
    fs::create_dir_all(&tree).unwrap();
    let plan = serde_json::json!({
        "schema_version": crate::crawl::plan::PLAN_SCHEMA_VERSION,
        "workspace": "fixture-ws",
        "planned_at": "2026-09-04T00:00:00Z",
        "rules": [rule],
        "sources": [{
            "id": "app", "sha": sha, "ref": "main",
            "tree": tree.to_string_lossy(), "files_walked": 1, "out_of_scope": 0
        }],
        "units": [{
            "kind": "site", "id": unit_id, "rule": rule, "source": "app",
            "sites": [{"file": "src/a.ts", "line": 2, "start": 1, "end": 5, "hits": [2]}],
            "est_tokens": 400
        }],
        "totals": {"units": 1, "est_tokens": 400, "by_rule": {}, "skipped": [], "edges": []}
    });
    let p = root.join("plan.json");
    // The plan is WRAPPED on disk (#2301) — the same envelope `crawl.plan`
    // writes, so what these tests read is what production produces.
    let wrapped = serde_json::json!({
        "schema_version": darkmux_crew::step_output::OUTPUT_SCHEMA_VERSION,
        "kind": crate::crawl::plan_step::CRAWL_PLAN_OUTPUT_KIND,
        "producer": {"mission": MISSION, "task": "plan-task", "step": "plan-step", "machine_id": "t"},
        "produced_at": "2026-09-04T00:00:00Z",
        // A real digest, so these fixtures exercise the integrity check
        // rather than skipping past it on an empty hash.
        "hash": darkmux_crew::step_output::body_hash(&plan),
        "body": plan
    });
    fs::write(&p, serde_json::to_string(&wrapped).unwrap()).unwrap();
    p
}

fn unit_step(config: serde_json::Value) -> Step {
    Step {
        id: "unit-step".into(),
        task_id: "unit-task".into(),
        kind: CRAWL_UNIT_KIND.into(),
        gate: None,
        status: NodeStatus::Planned,
        config,
        started_ts: None,
        completed_ts: None,
        output: None,
    }
}

fn unit_task() -> Task {
    Task {
        id: "unit-task".into(),
        phase_id: PHASE.into(),
        description: String::new(),
        display_name: None,
        step_ids: vec!["unit-step".into()],
        depends_on: Vec::new(),
        reads: Vec::new(),
        role_id: Some("crawler".into()),
        profile_name: None,
        workdir: None,
        image: None,
    }
}

/// A dispatch out dir seeded with `findings` accepted findings and,
/// optionally, a trajectory whose turns all made no progress.
fn seeded_out_dir(dir: &Path, findings: usize, idle_turns: usize) -> PathBuf {
    let out = dir.join("out");
    let rt = out.join(".darkmux-runtime");
    fs::create_dir_all(&rt).unwrap();
    let mut body = String::new();
    for i in 0..findings {
        body.push_str(&format!(
            "{{\"file\":\"/workspace/app/src/a.ts\",\"line\":{},\"pattern\":\"unnamed-predicate\",\"evidence\":\"if (a && b)\",\"why\":\"w\"}}\n",
            i + 1
        ));
    }
    if findings > 0 {
        fs::write(rt.join("findings.jsonl"), body).unwrap();
    }
    if idle_turns > 0 {
        let mut traj = String::new();
        for seq in 0..idle_turns {
            traj.push_str(&format!(
                "{{\"type\":\"tool.completed\",\"seq\":{seq},\"tool_name\":\"bash\",\"ok\":true}}\n"
            ));
        }
        fs::write(rt.join("trajectory.jsonl"), traj).unwrap();
    }
    out
}

fn envelope(result: &str, prompt: u64, completion: u64, wall_ms: u64) -> String {
    serde_json::json!({
        "result": result,
        "metrics": {"model": "m-1", "wall_ms": wall_ms, "prompt_tokens": prompt,
                    "completion_tokens": completion, "rest_ms": 0}
    })
    .to_string()
}

fn ok_result(stdout: String, out: PathBuf) -> Result<DispatchResult> {
    Ok(DispatchResult { exit_code: 0, stdout, stderr: String::new(), session_id: "s".into(), out_dir: Some(out) })
}

// ── the kind ─────────────────────────────────────────────────────────────

#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn a_clean_unit_dispatch_produces_a_typed_outcome_and_counts_its_findings() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    let ws = TempDir::new().unwrap();
    let plan = write_plan(ws.path(), "unnamed-predicate", "u-0001", &"a".repeat(40));
    let out = seeded_out_dir(ws.path(), 2, 0);

    let seen: Arc<std::sync::Mutex<Option<DispatchOpts>>> = Arc::new(std::sync::Mutex::new(None));
    let captured = seen.clone();
    let out_for_dispatch = out.clone();
    let kind = CrawlUnitStepKind::with_dispatch(Arc::new(move |opts: DispatchOpts| {
        *captured.lock().unwrap() = Some(opts);
        ok_result(envelope("stop", 100, 20, 5_000), out_for_dispatch.clone())
    }));

    let step = unit_step(serde_json::json!({
        "plan": plan.to_string_lossy(), "unit": "u-0001", "rule": "unnamed-predicate"
    }));
    let outcome = kind.run(&step, &unit_task(), &BTreeMap::new()).unwrap();
    let env = darkmux_crew::step_output::Output::<UnitOutcome>::read(&outcome.output, UNIT_OUTCOME_KIND)
        .expect("the output IS a wrapped UnitOutcome");
    assert_eq!(env.producer.mission, MISSION, "the envelope names who produced it");
    let parsed = env.body;
    assert_eq!(parsed.schema_version, UNIT_OUTCOME_SCHEMA_VERSION);
    assert_eq!(parsed.unit, "u-0001");
    assert_eq!(parsed.result, "stop");
    assert_eq!(parsed.findings, 2, "counted from the seeded findings.jsonl");
    assert_eq!(parsed.findings_rejected, 0);
    assert_eq!(parsed.prompt_tokens, 100);
    assert_eq!(parsed.completion_tokens, 20);
    assert_eq!(parsed.wall_ms, 5_000);
    assert_eq!(parsed.rule.as_deref(), Some("unnamed-predicate"));
    assert_eq!(parsed.model.as_deref(), Some("m-1"));

    let opts = seen.lock().unwrap().take().unwrap();
    assert_eq!(opts.role_id, "crawler");
    assert!(opts.workspace_read_only, "the crawl never writes into the tree it reads");
    assert!(opts.json, "the classification reads a --json envelope");
    assert_eq!(
        opts.workdir.as_deref(),
        Some(ws.path().join("tree").as_path()),
        "the workspace root is the PARENT of the unit's source tree, so /workspace/app/... resolves"
    );
    assert_eq!(opts.max_turns_override, Some(12), "a 1-site unit floors at MIN_UNIT_MAX_TURNS");
    let ctx = opts.record_context.expect("provenance the runtime cannot know");
    assert_eq!(ctx["unit"], serde_json::json!("u-0001"));
    assert_eq!(ctx["source"], serde_json::json!("app"));
    assert_eq!(ctx["sha"], serde_json::json!("a".repeat(40)));
    assert_eq!(ctx["rule"], serde_json::json!("unnamed-predicate"));

    // The findings the crawl stamps land beside the run.
    let stamped =
        fs::read_to_string(darkmux_crew::loader::missions_dir().join(MISSION).join("u-0001.findings.jsonl")).unwrap();
    let first: Value = serde_json::from_str(stamped.lines().next().unwrap()).unwrap();
    assert_eq!(first["file"], serde_json::json!("src/a.ts"), "the container prefix is stripped");
    assert_eq!(first["file_raw"], serde_json::json!("/workspace/app/src/a.ts"), "the raw value survives");
    assert_eq!(first["sha"], serde_json::json!("a".repeat(40)));
    assert_eq!(first["rule"], serde_json::json!("unnamed-predicate"));
}

#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn max_turns_is_a_bound_not_a_failure_and_the_step_still_completes() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    let ws = TempDir::new().unwrap();
    let plan = write_plan(ws.path(), "unnamed-predicate", "u-0001", &"b".repeat(40));
    let out = seeded_out_dir(ws.path(), 0, 0);
    let kind =
        CrawlUnitStepKind::with_dispatch(Arc::new(move |_| ok_result(envelope("max_turns", 10, 5, 900), out.clone())));
    let step = unit_step(serde_json::json!({ "plan": plan.to_string_lossy(), "unit": "u-0001" }));
    let outcome = kind.run(&step, &unit_task(), &BTreeMap::new()).unwrap();
    let parsed = darkmux_crew::step_output::Output::<UnitOutcome>::read(&outcome.output, UNIT_OUTCOME_KIND)
        .unwrap()
        .body;
    assert_eq!(parsed.result, "unit_budget_exhausted", "a BOUND, never `error`");
}

#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn a_no_progress_tail_ends_a_clean_stop_as_budget_exhausted() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    let ws = TempDir::new().unwrap();
    let plan = write_plan(ws.path(), "unnamed-predicate", "u-0001", &"c".repeat(40));
    let out = seeded_out_dir(ws.path(), 0, 4);
    let kind = CrawlUnitStepKind::with_dispatch(Arc::new(move |_| ok_result(envelope("stop", 1, 1, 10), out.clone())));
    // The bound only fires once there IS a full trailing window: 4 idle
    // turns clear a window of 3 and do not clear a window of 5.
    for (n, want) in [(3u64, "unit_budget_exhausted"), (5, "stop")] {
        let step = unit_step(serde_json::json!({
            "plan": plan.to_string_lossy(), "unit": "u-0001", "no_progress_turns": n
        }));
        let outcome = kind.run(&step, &unit_task(), &BTreeMap::new()).unwrap();
        let parsed = darkmux_crew::step_output::Output::<UnitOutcome>::read(&outcome.output, UNIT_OUTCOME_KIND)
            .unwrap()
            .body;
        assert_eq!(parsed.result, want, "no_progress_turns={n}");
    }
}

#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn a_dispatch_error_fails_the_step_naming_the_unit() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    let ws = TempDir::new().unwrap();
    let plan = write_plan(ws.path(), "unnamed-predicate", "u-0001", &"d".repeat(40));
    let kind = CrawlUnitStepKind::with_dispatch(Arc::new(|_| Err(anyhow!("container refused"))));
    let step = unit_step(serde_json::json!({ "plan": plan.to_string_lossy(), "unit": "u-0001" }));
    let err = format!("{:#}", kind.run(&step, &unit_task(), &BTreeMap::new()).unwrap_err());
    assert!(err.contains("u-0001") && err.contains("container refused"), "{err}");
}

#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn a_unit_the_plan_does_not_hold_is_refused_before_any_dispatch() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    let ws = TempDir::new().unwrap();
    let plan = write_plan(ws.path(), "unnamed-predicate", "u-0001", &"e".repeat(40));
    let dispatched = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = dispatched.clone();
    let kind = CrawlUnitStepKind::with_dispatch(Arc::new(move |_| {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        Err(anyhow!("unreachable"))
    }));
    let step = unit_step(serde_json::json!({ "plan": plan.to_string_lossy(), "unit": "u-9999" }));
    let err = format!("{:#}", kind.run(&step, &unit_task(), &BTreeMap::new()).unwrap_err());
    assert!(err.contains("u-9999") && err.contains("no unit"), "{err}");
    assert!(!dispatched.load(std::sync::atomic::Ordering::SeqCst), "nothing dispatched");
}

#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn a_step_whose_rule_disagrees_with_the_plan_is_refused() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    let ws = TempDir::new().unwrap();
    let plan = write_plan(ws.path(), "unnamed-predicate", "u-0001", &"f".repeat(40));
    let kind = CrawlUnitStepKind::with_dispatch(Arc::new(|_| Err(anyhow!("unreachable"))));
    let step = unit_step(serde_json::json!({
        "plan": plan.to_string_lossy(), "unit": "u-0001", "rule": "swallowed-error"
    }));
    let err = format!("{:#}", kind.run(&step, &unit_task(), &BTreeMap::new()).unwrap_err());
    assert!(err.contains("swallowed-error") && err.contains("disagree"), "{err}");
}

#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn an_empty_sha_is_refused_rather_than_stamped_onto_findings() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    let ws = TempDir::new().unwrap();
    let plan = write_plan(ws.path(), "unnamed-predicate", "u-0001", "");
    let kind = CrawlUnitStepKind::with_dispatch(Arc::new(|_| Err(anyhow!("unreachable"))));
    let step = unit_step(serde_json::json!({ "plan": plan.to_string_lossy(), "unit": "u-0001" }));
    let err = format!("{:#}", kind.run(&step, &unit_task(), &BTreeMap::new()).unwrap_err());
    assert!(err.contains("empty sha") && err.contains("unversioned"), "{err}");
}

#[test]
fn a_missing_required_field_is_refused_by_name() {
    let err = UnitStepConfig::from_step(&unit_step(serde_json::json!({ "unit": "u-1" }))).unwrap_err();
    assert!(format!("{err}").contains("config.plan"), "{err}");
    let err = UnitStepConfig::from_step(&unit_step(serde_json::json!({ "plan": "/p.json" }))).unwrap_err();
    assert!(format!("{err}").contains("config.unit"), "{err}");
}

#[test]
fn the_turn_ceiling_floors_clamps_and_scales_with_the_units_own_site_count() {
    let unit = |sites: usize| Unit::Site {
        id: "u".into(),
        rule: "r".into(),
        source: "app".into(),
        sites: (0..sites).map(|i| Site { file: "a.ts".into(), line: i + 1, start: 1, end: 2, hits: vec![i + 1] }).collect(),
        est_tokens: 1,
    };
    assert_eq!(default_unit_max_turns(&unit(1)), MIN_UNIT_MAX_TURNS, "floor");
    assert_eq!(default_unit_max_turns(&unit(6)), 18, "TURNS_PER_SITE per site");
    assert_eq!(default_unit_max_turns(&unit(500)), MAX_UNIT_MAX_TURNS, "ceiling");
}

/// (#2301, for the #2312 validator) A data port's LABEL is the same string
/// as the wrapper `kind` its output carries, and a consumer `requires` the
/// exact label its producer `provides` — so the two can be compared
/// directly, with no rename table in between.
#[test]
fn every_port_label_is_the_wrapper_kind_it_names() {
    use crate::crawl::plan_step::{CrawlPlanStepKind, CRAWL_PLAN_OUTPUT_KIND};
    let labels = |ports: &[darkmux_crew::step_kinds::Port]| -> Vec<&'static str> {
        ports.iter().map(|p| p.name).collect()
    };
    assert_eq!(labels(CrawlPlanStepKind.provides()), vec![CRAWL_PLAN_OUTPUT_KIND]);
    assert!(CrawlPlanStepKind.requires().is_empty(), "the plan reads no step output");

    let unit = CrawlUnitStepKind::with_dispatch(Arc::new(|_| Err(anyhow!("unused"))));
    assert_eq!(labels(unit.requires()), vec![CRAWL_PLAN_OUTPUT_KIND]);
    assert_eq!(labels(unit.provides()), vec![UNIT_OUTCOME_KIND]);

    assert_eq!(labels(CrawlSummaryStepKind.requires()), vec![UNIT_OUTCOME_KIND]);
    assert_eq!(labels(CrawlSummaryStepKind.provides()), vec![CRAWL_SUMMARY_OUTPUT_KIND]);

    // And each producer's own port matches the `kind` it actually writes:
    // the two constants above are the SAME items the wrap calls use, so a
    // rename on one side without the other cannot compile.
    assert_eq!(CRAWL_PLAN_OUTPUT_KIND, "crawl.plan");
    assert_eq!(UNIT_OUTCOME_KIND, "crawl.unit-outcome");
    assert_eq!(CRAWL_SUMMARY_OUTPUT_KIND, "crawl.summary");
}

#[test]
fn the_kinds_register_beside_the_builtins() {
    let registry = StepKindRegistry::with_builtins();
    crate::crawl::plan_step::register_crawl_kinds(&registry).unwrap();
    let ids = registry.ids();
    for want in [CRAWL_UNIT_KIND, CRAWL_SUMMARY_KIND] {
        assert!(ids.iter().any(|id| id == want), "{want} missing from {ids:?}");
    }
}

// ── the summary ──────────────────────────────────────────────────────────

fn save_unit_step(mission: &str, phase: &str, id: &str, status: NodeStatus, output: Option<&str>) {
    darkmux_crew::lifecycle::save_step(
        mission,
        phase,
        &Step {
            id: id.into(),
            task_id: format!("{id}-task"),
            kind: CRAWL_UNIT_KIND.into(),
            gate: None,
            status,
            config: serde_json::json!({}),
            started_ts: None,
            completed_ts: None,
            output: output.map(String::from),
        },
    )
    .unwrap();
}

fn outcome_json(unit: &str, result: &str, findings: u64, tokens: u64, wall_ms: u64) -> String {
    darkmux_crew::step_output::Output::wrap(
        UNIT_OUTCOME_KIND,
        UnitOutcome {
        schema_version: UNIT_OUTCOME_SCHEMA_VERSION.into(),
        unit: unit.into(),
        rule: Some("unnamed-predicate".into()),
        source: "app".into(),
        result: result.into(),
        findings,
        findings_rejected: 0,
        wall_ms,
        prompt_tokens: tokens,
        completion_tokens: tokens,
        model: Some("m-1".into()),
        out_dir: "/out".into(),
        rules: vec!["unnamed-predicate".into()],
        workspace: "fixture-ws".into(),
        sha: "a".repeat(40),
        rest_ms: 0,
            reason: None,
            detections: None,
            host: None,
            finding_refs: Vec::new(),
        },
        darkmux_crew::step_output::Producer::default(),
    )
    .to_output_string()
    .unwrap()
}

#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn the_summary_totals_every_unit_and_keeps_the_retired_launchers_payload_keys() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    save_unit_step(MISSION, PHASE, "u1", NodeStatus::Complete, Some(&outcome_json("u-0001", "stop", 3, 100, 3_600_000)));
    save_unit_step(
        MISSION,
        PHASE,
        "u2",
        NodeStatus::Complete,
        Some(&outcome_json("u-0002", "unit_budget_exhausted", 0, 50, 0)),
    );
    // (#2301 review) The scheduler writes a failing kind's ERROR TEXT into
    // `step.output` — an errored unit has a non-empty, non-`UnitOutcome`
    // output. `output: None` is a shape production never produces.
    save_unit_step(
        MISSION,
        PHASE,
        "u3",
        NodeStatus::Error,
        Some("`crawl.unit`: unit `u-0003` ended `timeout` — dispatch ended `timeout`"),
    );

    let s = summarize_mission(MISSION).unwrap();
    assert_eq!(s.units_completed, 1);
    assert_eq!(s.units_budget_exhausted, 1);
    assert_eq!(s.units_errored, 1, "a step with no output is an honest error row");
    assert_eq!(s.findings, 3);
    assert_eq!(s.prompt_tokens, 150);
    assert_eq!(s.completion_tokens, 150);
    assert_eq!(s.wall_ms, 3_600_000);
    assert_eq!(s.tokens_per_hour, 300, "300 tokens over exactly one hour");
    assert_eq!(s.stopped_by, "error");
    assert_eq!(s.model.as_deref(), Some("m-1"));
    assert_eq!(s.units_skipped, 0);
    assert_eq!(s.mission_id, MISSION);
    assert_eq!(s.units.len(), 3);
    let errored = s.units.iter().find(|u| u.result == "error").expect("the errored unit has a row");
    assert!(
        errored.reason.as_deref().is_some_and(|r| r.contains("timeout")),
        "the scheduler's error text is carried as the row's reason: {errored:?}"
    );
}

#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn an_errored_unit_never_refuses_the_whole_summary() {
    // (#2301 review, MUST FIX) The defect this kills: reading EVERY unit
    // step's output typed meant one failed unit — whose output is the
    // scheduler's error text, not a `UnitOutcome` — made `summarize_mission`
    // return `Err`, so the summary step errored and the mission closed with
    // NO payload at all. A run with one bad unit must still summarize.
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    save_unit_step(MISSION, PHASE, "u1", NodeStatus::Complete, Some(&outcome_json("u-0001", "stop", 2, 10, 10)));
    let mut errored = Step {
        id: "u2".into(),
        task_id: "u2-task".into(),
        kind: CRAWL_UNIT_KIND.into(),
        gate: None,
        status: NodeStatus::Error,
        // What the grow seam stamped, and what the scheduler recorded.
        config: serde_json::json!({"unit": "u-0002", "rule": "swallowed-error"}),
        started_ts: None,
        completed_ts: None,
        output: Some("`crawl.unit`: unit `u-0002` ended `error` — container refused".into()),
    };
    darkmux_crew::lifecycle::save_step(MISSION, PHASE, &errored).unwrap();

    let s = summarize_mission(MISSION).expect("one errored unit must not refuse the run's summary");
    assert_eq!(s.units_completed, 1);
    assert_eq!(s.units_errored, 1);
    assert_eq!(s.findings, 2, "the good unit's numbers still count");
    assert_eq!(s.stopped_by, "error");
    let row = s.units.iter().find(|u| u.result == "error").unwrap();
    assert_eq!(row.unit, "u-0002", "the row names the UNIT (from config), not the step id");
    assert_eq!(row.rule.as_deref(), Some("swallowed-error"));
    assert!(row.reason.as_deref().is_some_and(|r| r.contains("container refused")), "{row:?}");

    // A step still RUNNING at summary time is neither complete nor a
    // failure of the read — it simply never settled.
    errored.id = "u3".into();
    errored.task_id = "u3-task".into();
    errored.status = NodeStatus::Running;
    errored.output = None;
    errored.config = serde_json::json!({"unit": "u-0003"});
    darkmux_crew::lifecycle::save_step(MISSION, PHASE, &errored).unwrap();
    let s = summarize_mission(MISSION).unwrap();
    assert_eq!(s.units.iter().filter(|u| u.result == "not_run").count(), 1);
}

#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn a_malformed_unit_output_is_refused_naming_the_missing_field() {
    // The typed read IS the validation: a producer that drifted must not
    // be summarized as zeros.
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    save_unit_step(
        MISSION,
        PHASE,
        "u1",
        // COMPLETE: the only status whose output is read typed, so this is
        // where real producer drift is caught.
        NodeStatus::Complete,
        // A correctly-wrapped envelope whose BODY is missing `findings`.
        Some(
            &serde_json::json!({
                "schema_version": "1.0", "kind": UNIT_OUTCOME_KIND, "producer": {},
                "produced_at": "", "body": {
                    "schema_version": "1.0", "unit": "u-0001", "rule": "r", "source": "app",
                    "result": "stop", "findings_rejected": 0, "wall_ms": 1, "prompt_tokens": 1,
                    "completion_tokens": 1, "model": null, "out_dir": "/out"
                }
            })
            .to_string(),
        ),
    );
    let err = format!("{:#}", summarize_mission(MISSION).unwrap_err());
    assert!(err.contains("findings"), "the refusal names the missing field: {err}");
    assert!(err.contains("UnitOutcome"), "and the struct it failed to read as: {err}");
}

#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn a_unit_step_holding_someone_elses_output_is_refused_naming_both_kinds() {
    // Mutation-kill for the kind check: swap the content id and the read
    // must stop, not parse on and summarize a plan as a unit outcome.
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    save_unit_step(
        MISSION,
        PHASE,
        "u1",
        NodeStatus::Complete,
        Some(
            &serde_json::json!({
                "schema_version": "1.0", "kind": "crawl.summary", "producer": {},
                "produced_at": "", "body": {}
            })
            .to_string(),
        ),
    );
    let err = format!("{:#}", summarize_mission(MISSION).unwrap_err());
    assert!(err.contains("crawl.summary") && err.contains(UNIT_OUTCOME_KIND), "{err}");
    // (#2301 review) Assert the KIND refusal specifically. Without this the
    // test passes on the body-parse error path too, so deleting the kind
    // check would leave it green — it would be testing that something went
    // wrong, not that the graph was named as mis-wired.
    assert!(
        err.contains("the graph wires this step to the wrong producer"),
        "the refusal must be the kind check, not a body-parse error: {err}"
    );
}

#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn a_unit_step_reading_a_plan_wired_to_the_wrong_producer_is_refused() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    let ws = TempDir::new().unwrap();
    let wrong = ws.path().join("wrong.json");
    fs::write(
        &wrong,
        serde_json::json!({
            "schema_version": "1.0", "kind": UNIT_OUTCOME_KIND, "producer": {},
            "produced_at": "", "body": {}
        })
        .to_string(),
    )
    .unwrap();
    let kind = CrawlUnitStepKind::with_dispatch(Arc::new(|_| Err(anyhow!("unreachable"))));
    let step = unit_step(serde_json::json!({ "plan": wrong.to_string_lossy(), "unit": "u-0001" }));
    let err = format!("{:#}", kind.run(&step, &unit_task(), &BTreeMap::new()).unwrap_err());
    assert!(err.contains(UNIT_OUTCOME_KIND) && err.contains("crawl.plan"), "{err}");
}

#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn the_summary_reads_the_runs_own_plan_files_for_what_was_planned() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    let run_dir = darkmux_crew::loader::missions_dir().join(MISSION);
    fs::create_dir_all(run_dir.join("plan")).unwrap();
    let ws = TempDir::new().unwrap();
    let plan = write_plan(ws.path(), "unnamed-predicate", "u-0001", &"a".repeat(40));
    fs::copy(&plan, run_dir.join("plan").join("unnamed-predicate.json")).unwrap();
    save_unit_step(MISSION, PHASE, "u1", NodeStatus::Complete, Some(&outcome_json("u-0001", "stop", 1, 10, 10)));

    let s = summarize_mission(MISSION).unwrap();
    assert_eq!(s.units_in_plan, 1);
    assert_eq!(s.units_selected, 1);
    assert_eq!(s.units_not_run, 0);
    assert_eq!(s.est_tokens, 400);
    assert_eq!(s.workspace, "fixture-ws");
    assert_eq!(s.sources[0].id, "app");
}

// ── the kinds through the REAL scheduler (#2301 review) ──────────────────
//
// `with_dispatch` had no caller outside the per-kind unit tests, so nothing
// exercised `crawl.unit` and `crawl.summary` the way a run does: through
// `run_step_graph`, where the scheduler — not the test — decides a step's
// status and writes its `output`. That gap is exactly what hid the errored-
// unit defect (the scheduler records a failing kind's ERROR TEXT as the
// step's output; no unit test produced that shape). This runs the real
// graph.


fn graph_task(id: &str, step_id: &str, depends_on: &[&str]) -> Task {
    Task {
        id: id.into(),
        phase_id: PHASE.into(),
        description: String::new(),
        display_name: None,
        step_ids: vec![step_id.into()],
        depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
        reads: Vec::new(),
        role_id: Some("crawler".into()),
        profile_name: None,
        workdir: None,
        image: None,
    }
}

fn graph_step(id: &str, task_id: &str, kind: &str, config: serde_json::Value) -> Step {
    Step {
        id: id.into(),
        task_id: task_id.into(),
        kind: kind.into(),
        gate: None,
        status: NodeStatus::Planned,
        config,
        started_ts: None,
        completed_ts: None,
        output: None,
    }
}

#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn two_units_and_a_summary_run_through_the_real_scheduler() {
    // One unit converges, one fails. The run must still summarize, and the
    // summary must count 1 complete + 1 errored — the shape the pre-review
    // code turned into a refused summary and an empty close payload.
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    // (#2321) The units DECLARE their residency now, so this graph takes the
    // scheduler's real wave path: a fixture registry names the model, and the
    // mock host already has it resident — both units pack into ONE wave with
    // no load, which is the whole point of declaring.
    let registry_path = home.path().join("profiles.json");
    let _p = ProfilesGuard::set(&registry_path);
    fs::write(
        &registry_path,
        serde_json::json!({
            "default_profile": "p",
            "profiles": {"p": {"models": [{"id": "m-local", "n_ctx": 8192, "role": "primary"}]}}
        })
        .to_string(),
    )
    .unwrap();
    save_phase(PHASE, MISSION);
    let ws = TempDir::new().unwrap();
    let plan = write_plan(ws.path(), "unnamed-predicate", "u-0001", &"a".repeat(40));
    let out = seeded_out_dir(ws.path(), 1, 0);

    // The dispatch converges for u-0001 and refuses for the second unit,
    // keyed on what the step actually asked for.
    let dispatched: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = dispatched.clone();
    let kind = CrawlUnitStepKind::with_dispatch(Arc::new(move |opts: DispatchOpts| {
        seen.lock().unwrap().push(opts.session_id.clone().unwrap_or_default());
        ok_result(envelope("stop", 40, 8, 1_234), out.clone())
    }));

    let registry = StepKindRegistry::with_builtins();
    registry.register(Arc::new(kind)).unwrap();
    registry.register(Arc::new(CrawlSummaryStepKind)).unwrap();

    // u-0002 is NOT in the plan, so its step fails inside the kind — a real
    // `Err` the scheduler turns into `status: Error` + the error text as
    // the step's `output`, which is the production shape under test.
    //
    // `summary` declares NO `depends_on`, exactly as `crawl.json` does, and
    // is run in a SECOND `run_step_graph` call — the phase boundary. That
    // is not incidental: a summary that depended on the unit tasks would be
    // SKIPPED (left `Planned`) the moment any one of them errored, since
    // the scheduler drops every task whose dependency chain includes a dead
    // one. Ordering the summary by PHASE instead of by dependency is what
    // makes a run with a failed unit still produce a close payload.
    let tasks: Vec<Task> = vec![
        graph_task("unit-a", "unit-a-step", &[]),
        graph_task("unit-b", "unit-b-step", &[]),
        graph_task("summary", "summary-step", &[]),
    ];
    let mut steps: BTreeMap<String, Step> = [
        graph_step(
            "unit-a-step",
            "unit-a",
            CRAWL_UNIT_KIND,
            serde_json::json!({"plan": plan.to_string_lossy(), "unit": "u-0001", "rule": "unnamed-predicate"}),
        ),
        graph_step(
            "unit-b-step",
            "unit-b",
            CRAWL_UNIT_KIND,
            serde_json::json!({"plan": plan.to_string_lossy(), "unit": "u-0002", "rule": "unnamed-predicate"}),
        ),
        graph_step("summary-step", "summary", CRAWL_SUMMARY_KIND, serde_json::json!({})),
    ]
    .into_iter()
    .map(|s| (s.id.clone(), s))
    .collect();
    let tasks_by_id: BTreeMap<String, Task> = tasks.into_iter().map(|t| (t.id.clone(), t)).collect();

    let resident_host = || darkmux_gestalt::mock::MockHost::new().resident("darkmux:m-local", "m-local", 8192, Some(1 << 30));
    let facts = resident_host().facts(Default::default(), Default::default());
    let host_factory = || -> Box<dyn darkmux_gestalt::ModelHost> { Box::new(resident_host()) };
    let est = darkmux_gestalt::FixedEstimator(Default::default());

    // One `run_step_graph` call per phase, in order — what
    // `mission_launch::launch` does (#2300).
    let run_phase = |steps: &mut BTreeMap<String, Step>| {
        darkmux_crew::scheduler::run_step_graph(
            steps,
            &tasks_by_id,
            &registry,
            &facts,
            &est,
            1,
            &host_factory,
            &mut |_record| {},
            &mut |step| {
                let _ = darkmux_crew::lifecycle::save_step(MISSION, PHASE, step);
            },
            None,
            None,
            &[],
        )
        .expect("the graph run itself completes — a failed STEP is not a failed run");
    };
    let summary_step_only = steps.remove("summary-step").expect("seeded above");
    run_phase(&mut steps);
    steps.insert("summary-step".into(), summary_step_only);
    run_phase(&mut steps);

    // The scheduler's own shape, asserted rather than assumed: the failed
    // unit's output is its error TEXT, not a `UnitOutcome`.
    let unit_b = &steps["unit-b-step"];
    assert_eq!(unit_b.status, NodeStatus::Error);
    let text = unit_b.output.as_deref().expect("the scheduler records the error text as the output");
    assert!(text.contains("u-0002"), "{text}");
    assert!(serde_json::from_str::<serde_json::Value>(text).is_err(), "and it is not JSON: {text}");

    assert_eq!(steps["unit-a-step"].status, NodeStatus::Complete);
    assert_eq!(dispatched.lock().unwrap().len(), 1, "only the unit that resolved ever dispatched");

    // The summary ran, and read both.
    let summary_step = &steps["summary-step"];
    assert_eq!(summary_step.status, NodeStatus::Complete, "output: {:?}", summary_step.output);
    let summary = darkmux_crew::step_output::Output::<CrawlSummary>::read(
        summary_step.output.as_deref().expect("the summary produced output"),
        CRAWL_SUMMARY_OUTPUT_KIND,
    )
    .expect("the summary's own output is a typed CrawlSummary")
    .body;
    assert_eq!(summary.units_completed, 1);
    assert_eq!(summary.units_errored, 1);
    assert_eq!(summary.findings, 1, "the converged unit's finding still counts");
    assert_eq!(summary.stopped_by, "error");
    let bad = summary.units.iter().find(|u| u.result == "error").expect("the failed unit has a row");
    assert_eq!(bad.unit, "u-0002");
    assert!(bad.reason.as_deref().is_some_and(|r| r.contains("u-0002")), "{bad:?}");
}

// ── (#2302) the outcome NAMES its findings ───────────────────────────────

/// A unit's outcome carries one [`FindingRef`] per accepted finding, keyed
/// the way the finding store is keyed, and the summary's roster is the
/// union of those in unit order.
#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn a_units_outcome_names_every_finding_it_recorded_by_store_key() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    let ws = TempDir::new().unwrap();
    let plan = write_plan(ws.path(), "unnamed-predicate", "u-0001", &"a".repeat(40));
    let out = seeded_out_dir(ws.path(), 2, 0);
    let kind =
        CrawlUnitStepKind::with_dispatch(Arc::new(move |_| ok_result(envelope("stop", 1, 1, 10), out.clone())));

    let step = unit_step(serde_json::json!({
        "plan": plan.to_string_lossy(), "unit": "u-0001", "rule": "unnamed-predicate"
    }));
    let outcome = kind.run(&step, &unit_task(), &BTreeMap::new()).unwrap();
    let body = darkmux_crew::step_output::Output::<UnitOutcome>::read(&outcome.output, UNIT_OUTCOME_KIND)
        .unwrap()
        .body;

    assert_eq!(body.findings, 2, "the count and the roster come from ONE read");
    assert_eq!(body.finding_refs.len(), 2, "one ref per accepted finding");
    let session = format!("crawl-{MISSION}-u-0001");
    assert_eq!(
        body.finding_refs.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
        vec![format!("{session}/1"), format!("{session}/2")],
        "`<dispatch>/<emit_seq>`, 1-based over non-empty lines — the runtime's own ordinal"
    );
    assert_eq!(
        body.finding_refs[0].id,
        format!("crawl-{MISSION}-u-0001-1"),
        "`/` swapped for `-`: the id becomes a task id suffix"
    );
    assert_eq!(body.finding_refs[0].file.as_deref(), Some("src/a.ts"), "the container prefix is stripped");
    assert_eq!(body.finding_refs[0].line, Some(1));
    assert_eq!(body.finding_refs[1].line, Some(2), "each line's own number, in record order");
    assert_eq!(body.finding_refs[0].rule, "unnamed-predicate");
    assert_eq!(
        body.finding_refs[0].tree_root,
        ws.path().join("tree").to_string_lossy(),
        "the materialized root a create-mods step names as its workdir"
    );

    // And every key ADDRESSES a record: it parses, and it resolves through
    // the store the dispatch tailer writes.
    let store = TempDir::new().unwrap();
    for r in &body.finding_refs {
        let (dispatch, seq) =
            darkmux_crew::findings::parse_key(&r.key).unwrap_or_else(|| panic!("`{}` must be a finding key", r.key));
        assert_eq!(dispatch, session);
        darkmux_crew::findings::materialize(
            store.path(),
            &darkmux_crew::findings::FindingRecord {
                key: r.key.clone(),
                dispatch: dispatch.clone(),
                seq,
                ts: "2026-09-04T00:00:00Z".into(),
                tool_name: "create_finding".into(),
                proposer: darkmux_crew::findings::Proposer {
                    handle: "crawler".into(),
                    model: "m-1".into(),
                    machine_id: None,
                },
                mission_id: Some(MISSION.into()),
                phase_id: Some(PHASE.into()),
                step_id: Some("unit-step".into()),
                context: serde_json::json!({"unit": "u-0001"}),
                emitted: serde_json::json!({"why": "w"}),
                schema_version: darkmux_crew::findings::FINDING_SCHEMA_VERSION.into(),
                extras: serde_json::Map::new(),
            },
        )
        .expect("the tailer's own write");
        let back = darkmux_crew::findings::load_at(store.path(), &dispatch, seq)
            .unwrap()
            .unwrap_or_else(|| panic!("`{}` must resolve — `brief_refs` refuses a key that does not", r.key));
        assert_eq!(back.key, r.key);
    }
}

/// The summary's roster is the union over units, in unit order — and its
/// `findings` COUNT still means what the retired launcher's close payload
/// meant.
#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn the_summary_unions_the_units_finding_refs_in_unit_order() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);

    let mk = |unit: &str, refs: usize| {
        let body: Vec<serde_json::Value> = (1..=refs)
            .map(|i| {
                serde_json::json!({
                    "key": format!("crawl-{MISSION}-{unit}/{i}"),
                    "id": format!("crawl-{MISSION}-{unit}-{i}"),
                    "file": "src/a.ts", "line": i, "rule": "unnamed-predicate", "tree_root": "/t"
                })
            })
            .collect();
        serde_json::json!({
            "schema_version": UNIT_OUTCOME_SCHEMA_VERSION, "unit": unit, "rule": "unnamed-predicate",
            "source": "app", "result": "stop", "findings": refs, "findings_rejected": 0,
            "wall_ms": 1, "prompt_tokens": 1, "completion_tokens": 1, "model": "m-1",
            "out_dir": "/o", "finding_refs": body
        })
    };
    let wrap = |v: serde_json::Value| {
        darkmux_crew::step_output::Output::wrap(
            UNIT_OUTCOME_KIND,
            v,
            darkmux_crew::step_output::Producer::default(),
        )
        .to_output_string()
        .unwrap()
    };
    save_unit_step(MISSION, PHASE, "s-a", NodeStatus::Complete, Some(&wrap(mk("u-0001", 2))));
    save_unit_step(MISSION, PHASE, "s-b", NodeStatus::Complete, Some(&wrap(mk("u-0002", 1))));

    let s = summarize_mission(MISSION).unwrap();
    assert_eq!(s.findings, 3, "the count is the sum");
    assert_eq!(
        s.finding_refs.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
        vec![
            format!("crawl-{MISSION}-u-0001/1"),
            format!("crawl-{MISSION}-u-0001/2"),
            format!("crawl-{MISSION}-u-0002/1"),
        ],
        "the union, in unit order"
    );
    assert_eq!(s.schema_version, CRAWL_SUMMARY_SCHEMA_VERSION);
}

/// The `emit_seq` a key carries is the ordinal over NON-EMPTY LINES — the
/// runtime's own — not over the lines that happened to parse. A line the
/// host cannot read still consumed an ordinal in the container.
#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn a_key_keeps_the_runtimes_ordinal_even_when_a_line_does_not_parse() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    let ws = TempDir::new().unwrap();
    let plan = write_plan(ws.path(), "unnamed-predicate", "u-0001", &"a".repeat(40));
    let out = ws.path().join("out");
    let rt = out.join(".darkmux-runtime");
    fs::create_dir_all(&rt).unwrap();
    // Line 2 is not JSON: the runtime counted it, this reader cannot use it.
    fs::write(
        rt.join("findings.jsonl"),
        "{\"file\":\"/workspace/app/src/a.ts\",\"line\":1,\"pattern\":\"unnamed-predicate\",\"evidence\":\"e\",\"why\":\"w\"}\n\
         {truncated\n\
         {\"file\":\"/workspace/app/src/c.ts\",\"line\":3,\"pattern\":\"unnamed-predicate\",\"evidence\":\"e\",\"why\":\"w\"}\n",
    )
    .unwrap();

    let kind =
        CrawlUnitStepKind::with_dispatch(Arc::new(move |_| ok_result(envelope("stop", 1, 1, 10), out.clone())));
    let step = unit_step(serde_json::json!({ "plan": plan.to_string_lossy(), "unit": "u-0001" }));
    let outcome = kind.run(&step, &unit_task(), &BTreeMap::new()).unwrap();
    let body = darkmux_crew::step_output::Output::<UnitOutcome>::read(&outcome.output, UNIT_OUTCOME_KIND)
        .unwrap()
        .body;

    assert_eq!(body.findings, 2, "only the readable lines are counted");
    let session = format!("crawl-{MISSION}-u-0001");
    assert_eq!(
        body.finding_refs.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
        vec![format!("{session}/1"), format!("{session}/3")],
        "the second ref keys /3 — the unreadable line took /2 in the container"
    );
}

/// A session id that could not be a path segment under the finding store
/// produces NO refs — a key nothing can resolve is worse than none — while
/// the COUNT stays honest about what the unit observed.
#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn an_unaddressable_session_id_yields_no_refs_but_still_counts_the_findings() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    let ws = TempDir::new().unwrap();
    // The session id is `crawl-<mission>-<unit id>`, so a unit id holding a
    // separator is the one input that can make it unaddressable.
    let plan = write_plan(ws.path(), "unnamed-predicate", "u/0001", &"a".repeat(40));
    let out = seeded_out_dir(ws.path(), 2, 0);
    let kind =
        CrawlUnitStepKind::with_dispatch(Arc::new(move |_| ok_result(envelope("stop", 1, 1, 10), out.clone())));
    let step = unit_step(serde_json::json!({ "plan": plan.to_string_lossy(), "unit": "u/0001" }));
    let outcome = kind.run(&step, &unit_task(), &BTreeMap::new()).unwrap();
    let body = darkmux_crew::step_output::Output::<UnitOutcome>::read(&outcome.output, UNIT_OUTCOME_KIND)
        .unwrap()
        .body;

    assert!(
        !darkmux_crew::findings::is_safe_dispatch_segment(&format!("crawl-{MISSION}-u/0001")),
        "the fixture's premise: this session id cannot be a store segment"
    );
    assert_eq!(body.findings, 2, "the unit observed two findings, addressable or not");
    assert!(body.finding_refs.is_empty(), "and named none of them: {:?}", body.finding_refs);
}

/// Points `DARKMUX_PROFILES` at a registry written for one test and restores
/// the prior value, so the placement resolves from the fixture, never from
/// the operator's registry (`load_registry(None)` reads the documented
/// override first, and the real `~/.darkmux/profiles.json` otherwise).
struct ProfilesGuard(Option<String>);
impl ProfilesGuard {
    fn set(p: &Path) -> Self {
        let prior = std::env::var("DARKMUX_PROFILES").ok();
        std::env::set_var("DARKMUX_PROFILES", p);
        Self(prior)
    }
}
impl Drop for ProfilesGuard {
    fn drop(&mut self) {
        match &self.0 {
            Some(v) => std::env::set_var("DARKMUX_PROFILES", v),
            None => std::env::remove_var("DARKMUX_PROFILES"),
        }
    }
}

/// (#2321) The scheduler wave-packs only the steps that DECLARE a residency;
/// a kind that stays silent is queued as a remote job under `remote_cap`
/// (1 on the launch path), so sibling units ran strictly one at a time —
/// measured 3× on a three-unit crawl whose model was already resident. The
/// unit dispatches the `crawler` role locally; its residency must say so,
/// in the same terms `dispatch.internal` uses (`step:<id>` seat).
#[test]
#[serial_test::serial] // scopes DARKMUX_HOME + DARKMUX_PROFILES, process-globals
fn a_unit_declares_the_crawler_seats_residency_so_siblings_wave_pack() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    let registry = home.path().join("profiles.json");
    let _p = ProfilesGuard::set(&registry);
    fs::write(
        &registry,
        serde_json::json!({
            "default_profile": "p",
            "profiles": {"p": {"models": [{"id": "m-local", "n_ctx": 8192, "role": "primary"}]}}
        })
        .to_string(),
    )
    .unwrap();
    let kind = CrawlUnitStepKind::with_dispatch(Arc::new(|_| Err(anyhow!("residency never dispatches"))));
    let ctx = darkmux_crew::step_kinds::StepRunCtx::new(None, None, None, Arc::new(darkmux_crew::step_kinds::ArtifactBus::new()));
    let placement = kind
        .residency(&unit_step(serde_json::json!({})), &unit_task(), &BTreeMap::new(), &ctx)
        .expect("a local crawler dispatch declares where it will run");
    assert_eq!(placement.model_key, "m-local");
    assert_eq!(placement.min_ctx, 8192);
    assert_eq!(placement.seat, "step:unit-step");
}
