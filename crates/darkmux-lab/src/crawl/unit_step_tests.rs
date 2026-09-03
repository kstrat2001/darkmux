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
            detections: None,
            host: None,
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
    save_unit_step(MISSION, PHASE, "u3", NodeStatus::Error, None);

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
