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
        run_on: darkmux_crew::types::default_run_on(),
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

    // The findings the crawl stamps land beside the run, rule-namespaced
    // (#2360) — `<rule>.<unit>.findings.jsonl`, not `<unit>.findings.jsonl`.
    let stamped = fs::read_to_string(
        darkmux_crew::loader::missions_dir().join(MISSION).join("unnamed-predicate.u-0001.findings.jsonl"),
    )
    .unwrap();
    let first: Value = serde_json::from_str(stamped.lines().next().unwrap()).unwrap();
    assert_eq!(first["file"], serde_json::json!("src/a.ts"), "the container prefix is stripped");
    assert_eq!(first["file_raw"], serde_json::json!("/workspace/app/src/a.ts"), "the raw value survives");
    assert_eq!(first["sha"], serde_json::json!("a".repeat(40)));
    assert_eq!(first["rule"], serde_json::json!("unnamed-predicate"));
}

/// (#2360) Two DIFFERENT rules whose plans both mint a unit named
/// `u-0001` — exactly the shape a real crawl/review-v2 mission produces,
/// since a per-rule plan numbers its own units starting from 1
/// (`plan::unit_seq`). Live evidence: mission
/// `review-v2-1788566897-9c149e` (2026-09-05), 4 of 5 units errored with
/// "caller-provided out-dir already exists — refusing to reuse it",
/// because both rules' units resolved to the SAME
/// `<mission>/units/u-0001/out` with no rule component.
///
/// The stub dispatch below is NOT a scripted `Ok`/`Err` like the other
/// tests in this file — it reproduces `dispatch_internal::
/// resolve_host_out`'s OWN collision check (`fs::create_dir` on the
/// caller-named host-out dir, refusing outright on `AlreadyExists`
/// rather than reusing it), so this test exercises the real defect
/// mechanically rather than asserting it from memory. Before the fix,
/// `outcome_b` is `Err` (rule B's unit finds rule A's already-populated
/// dir). After the fix, both rules get their own on-disk home and this
/// also serves as the two-rules-one-mission-dir harness scenario the
/// issue asks for, since `crawl.unit` is dispatched (stubbed) for real,
/// not bypassed the way `review_v2_fixture_plans_every_rule_and_delivers_
/// one_comment_per_form` (`plan.rs`) stubs findings directly.
#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn two_rules_growing_unit_u_0001_do_not_collide_on_disk() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);

    let ws_a = TempDir::new().unwrap();
    let ws_b = TempDir::new().unwrap();
    let plan_a = write_plan(ws_a.path(), "unnamed-predicate", "u-0001", &"a".repeat(40));
    let plan_b = write_plan(ws_b.path(), "swallowed-error", "u-0001", &"b".repeat(40));

    let kind = CrawlUnitStepKind::with_dispatch(Arc::new(|opts: DispatchOpts| {
        let dir = opts.host_out.clone().expect("crawl.unit always names its host_out");
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!(
                    "darkmux dispatch: caller-provided out-dir already exists — refusing to \
                     reuse it: {}",
                    dir.display()
                );
            }
            Err(e) => return Err(e.into()),
        }
        let rt = dir.join(".darkmux-runtime");
        fs::create_dir_all(&rt).unwrap();
        let rule = opts
            .record_context
            .as_ref()
            .and_then(|c| c.get("rule"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        fs::write(
            rt.join("findings.jsonl"),
            format!(
                "{{\"file\":\"/workspace/app/src/a.ts\",\"line\":1,\"pattern\":\"{rule}\",\"evidence\":\"e\",\"why\":\"w\"}}\n"
            ),
        )
        .unwrap();
        Ok(DispatchResult {
            exit_code: 0,
            stdout: envelope("stop", 10, 5, 100),
            stderr: String::new(),
            session_id: opts.session_id.unwrap_or_default(),
            out_dir: Some(dir),
        })
    }));

    let step_a = unit_step(serde_json::json!({
        "plan": plan_a.to_string_lossy(), "unit": "u-0001", "rule": "unnamed-predicate"
    }));
    let step_b = unit_step(serde_json::json!({
        "plan": plan_b.to_string_lossy(), "unit": "u-0001", "rule": "swallowed-error"
    }));

    let outcome_a =
        kind.run(&step_a, &unit_task(), &BTreeMap::new()).expect("the first rule's unit is first to the dir");
    // Before the fix this is where #2360 reproduces: rule B's unit
    // resolves to the SAME `units/u-0001/out` rule A's dispatch already
    // populated, and the stub's collision check refuses it exactly the
    // way production does.
    let outcome_b = kind
        .run(&step_b, &unit_task(), &BTreeMap::new())
        .expect("a second rule naming the same unit id must get its OWN on-disk home (#2360)");

    let a = darkmux_crew::step_output::Output::<UnitOutcome>::read(&outcome_a.output, UNIT_OUTCOME_KIND)
        .unwrap()
        .body;
    let b = darkmux_crew::step_output::Output::<UnitOutcome>::read(&outcome_b.output, UNIT_OUTCOME_KIND)
        .unwrap()
        .body;
    assert_eq!(a.result, "stop");
    assert_eq!(b.result, "stop");
    assert_ne!(a.out_dir, b.out_dir, "each rule's unit must own a distinct out dir");
    assert!(a.out_dir.contains("unnamed-predicate"), "{}", a.out_dir);
    assert!(b.out_dir.contains("swallowed-error"), "{}", b.out_dir);

    // Findings recorded beside the run must be per-rule too, not one
    // shared `u-0001.findings.jsonl` the second rule would silently fail
    // to write (or, worse, overwrite).
    let missions_dir = darkmux_crew::loader::missions_dir().join(MISSION);
    let findings_a =
        fs::read_to_string(missions_dir.join("unnamed-predicate.u-0001.findings.jsonl")).expect("rule A's findings");
    let findings_b =
        fs::read_to_string(missions_dir.join("swallowed-error.u-0001.findings.jsonl")).expect("rule B's findings");
    assert!(findings_a.contains("unnamed-predicate"));
    assert!(findings_b.contains("swallowed-error"));
}

/// (#2360 follow-up) An empty rule-dir component (no declared
/// `config.rule` AND a plan unit naming no rule id at all) must be
/// REFUSED, never silently resolved to `""` — an empty component would
/// revert `units/<rule_dir>/<unit_id>` to the pre-fix colliding
/// `units/<unit_id>` and turn the mission-root findings file into a
/// leading-dot dotfile (`.` + `<unit_id>.findings.jsonl`), which is a
/// second, quieter way back to the exact bug this issue closed.
#[test]
fn unit_rule_dir_refuses_when_nothing_names_a_rule() {
    let err = unit_rule_dir(None, &[]).expect_err("no declared rule and no plan rule id must refuse");
    assert!(err.to_string().contains("could not resolve a rule"), "{err:#}");
}

/// (#2360 follow-up) The resolved rule-dir component becomes a raw path
/// segment (`units/<rule_dir>/<unit_id>`) with no guard downstream, so
/// `unit_rule_dir` is the one place that must refuse a traversal-shaped
/// id rather than trust the plan/config to have already checked it.
#[test]
fn unit_rule_dir_refuses_a_path_traversal_shaped_rule_id() {
    let err = unit_rule_dir(Some("../x"), &["../x".to_string()])
        .expect_err("a rule id shaped like a path traversal must be refused, not joined onto a path");
    assert!(err.to_string().contains("not a safe path component"), "{err:#}");
}

/// (#2310 P4c-2b) `config.draws: 2` dispatches the same unit TWICE —
/// proven by a stub dispatcher that counts its own calls and stamps a
/// distinct session id per call — and the resulting `finding_refs` dedup
/// duplicate `(rule, file, line)` keys down to one entry even though the
/// STORE keeps both underlying findings (`findings` stays the raw total).
#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn draws_dispatches_the_unit_n_times_and_dedups_matching_finding_refs() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    let ws = TempDir::new().unwrap();
    let plan = write_plan(ws.path(), "unnamed-predicate", "u-0001", &"a".repeat(40));
    // Two SEPARATE out dirs, each seeded with ONE finding at the SAME
    // (file, line) — the shape two draws independently re-observing the
    // same real issue would produce.
    let draw1_dir = TempDir::new().unwrap();
    let draw2_dir = TempDir::new().unwrap();
    let out1 = seeded_out_dir(draw1_dir.path(), 1, 0);
    let out2 = seeded_out_dir(draw2_dir.path(), 1, 0);

    let calls: Arc<std::sync::Mutex<Vec<Option<String>>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = calls.clone();
    let kind = CrawlUnitStepKind::with_dispatch(Arc::new(move |opts: DispatchOpts| {
        let n = {
            let mut c = captured.lock().unwrap();
            c.push(opts.session_id.clone());
            c.len()
        };
        let out = if n == 1 { out1.clone() } else { out2.clone() };
        ok_result(envelope("stop", 50, 10, 1_000), out)
    }));

    let step = unit_step(serde_json::json!({
        "plan": plan.to_string_lossy(), "unit": "u-0001", "rule": "unnamed-predicate", "draws": 2
    }));
    let outcome = kind.run(&step, &unit_task(), &BTreeMap::new()).unwrap();
    let parsed = darkmux_crew::step_output::Output::<UnitOutcome>::read(&outcome.output, UNIT_OUTCOME_KIND)
        .unwrap()
        .body;

    let seen = calls.lock().unwrap();
    assert_eq!(seen.len(), 2, "draws: 2 must dispatch exactly twice");
    assert_ne!(
        seen[0], seen[1],
        "two draws must never share a session id — the finding store addresses by <session>/<seq>"
    );
    assert_eq!(seen[0].as_deref(), Some(format!("crawl-{MISSION}-u-0001").as_str()), "draw 0 is unchanged");

    assert_eq!(parsed.findings, 2, "the RAW total across both draws — every finding the store holds");
    assert_eq!(
        parsed.finding_refs.len(),
        1,
        "both draws reported the same (rule, file, line) — dedup collapses them to one: {:?}",
        parsed.finding_refs
    );
    assert_eq!(parsed.wall_ms, 2_000, "wall_ms sums across draws");
    assert_eq!(parsed.prompt_tokens, 100);

    // (#2360 follow-up) The draw-1 (`d2`) readback file must be
    // rule-namespaced too — `<rule>.<unit>.findings.jsonl.d2`, never bare
    // `<unit>.findings.jsonl.d2` — because a second rule growing the same
    // unit id and also drawing twice would otherwise `std::fs::write`
    // (truncate) the SAME `.d2` path this draw just wrote. Nothing else in
    // this test reads this exact path, so dropping the rule prefix off
    // ONLY the `draw > 0` branch would otherwise leave every assertion
    // above green.
    let draw2_findings = fs::read_to_string(
        darkmux_crew::loader::missions_dir().join(MISSION).join("unnamed-predicate.u-0001.findings.jsonl.d2"),
    )
    .expect("draw 1's readback file must be rule-namespaced");
    assert!(draw2_findings.contains("unnamed-predicate"), "{draw2_findings}");
}

/// (#2310 P4c-2b) `draws` absent (the default) must dispatch exactly ONCE
/// — a mutation-kill against a `for draw in 0..cfg.draws` off-by-one, and
/// the explicit statement of the byte-identical-to-before-P4c-2b claim
/// this packet's own module doc makes.
#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn draws_defaults_to_exactly_one_dispatch() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    let ws = TempDir::new().unwrap();
    let plan = write_plan(ws.path(), "unnamed-predicate", "u-0001", &"a".repeat(40));
    let out = seeded_out_dir(ws.path(), 1, 0);

    let calls: Arc<std::sync::Mutex<usize>> = Arc::new(std::sync::Mutex::new(0));
    let captured = calls.clone();
    let out_for_dispatch = out.clone();
    let kind = CrawlUnitStepKind::with_dispatch(Arc::new(move |_opts: DispatchOpts| {
        *captured.lock().unwrap() += 1;
        ok_result(envelope("stop", 50, 10, 1_000), out_for_dispatch.clone())
    }));

    let step = unit_step(serde_json::json!({
        "plan": plan.to_string_lossy(), "unit": "u-0001", "rule": "unnamed-predicate"
    }));
    kind.run(&step, &unit_task(), &BTreeMap::new()).unwrap();
    assert_eq!(*calls.lock().unwrap(), 1);
}

/// (#2310 P4c mutation-kill) `CrawlUnitStepKind` used to hardcode
/// `role_id: "crawler".to_string()` regardless of what the owning Task
/// declared — DESIGN.md's "Units. Already generic" claim was true in
/// prose only. A Task whose `role_id` is `"reviewer"` must now dispatch
/// AS `"reviewer"`, and the finding's `context.confirm` (host-stamped,
/// not model-supplied — see `pattern_block`'s doc) must name the rule's
/// own `confirm` form so a downstream reader can tell a search-confirmed
/// finding from a mod-confirmed one without re-resolving the registry.
#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn a_task_naming_a_different_role_dispatches_as_that_role_and_stamps_its_confirm_form() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    let ws = TempDir::new().unwrap();
    let plan = write_plan(ws.path(), "swallowed-error", "u-0001", &"b".repeat(40));
    let out = seeded_out_dir(ws.path(), 1, 0);

    let seen: Arc<std::sync::Mutex<Option<DispatchOpts>>> = Arc::new(std::sync::Mutex::new(None));
    let captured = seen.clone();
    let out_for_dispatch = out.clone();
    let kind = CrawlUnitStepKind::with_dispatch(Arc::new(move |opts: DispatchOpts| {
        *captured.lock().unwrap() = Some(opts);
        ok_result(envelope("stop", 50, 10, 2_000), out_for_dispatch.clone())
    }));

    let step = unit_step(serde_json::json!({
        "plan": plan.to_string_lossy(), "unit": "u-0001", "rule": "swallowed-error"
    }));
    let mut task = unit_task();
    task.role_id = Some("reviewer".to_string());
    kind.run(&step, &task, &BTreeMap::new()).unwrap();

    let opts = seen.lock().unwrap().take().unwrap();
    assert_eq!(opts.role_id, "reviewer", "the Task's own role_id, not the hardcoded default");
    let ctx = opts.record_context.expect("provenance the runtime cannot know");
    assert_eq!(
        ctx["confirm"],
        serde_json::json!("mod"),
        "swallowed-error's built-in confirm form, stamped from the resolved rule: {ctx}"
    );
}

/// (#2310 P4c review round 2, item (e) — proven) `intent_file` was an
/// input `review-v2.json` declares and NOTHING reads — `intent-vs-diff`'s
/// own `match`/`compare` prose talks about "the intent you were given for
/// this change (a PR body or intent file, provided alongside your
/// window)", which was a lie: no intent text ever reached the dispatch,
/// so the rule was structurally inert regardless of what any seat did.
/// `config.intent_file`, when present, must land in the unit's dispatched
/// message.
#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn an_intent_file_in_config_lands_in_the_dispatched_message() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    let ws = TempDir::new().unwrap();
    let plan = write_plan(ws.path(), "intent-vs-diff", "u-0001", &"c".repeat(40));
    let out = seeded_out_dir(ws.path(), 0, 0);
    let intent_file = ws.path().join("intent.txt");
    const INTENT_MARKER: &str = "PR-INTENT-MARKER: fix the off-by-one in the pagination cursor";
    fs::write(&intent_file, INTENT_MARKER).unwrap();

    let seen: Arc<std::sync::Mutex<Option<DispatchOpts>>> = Arc::new(std::sync::Mutex::new(None));
    let captured = seen.clone();
    let out_for_dispatch = out.clone();
    let kind = CrawlUnitStepKind::with_dispatch(Arc::new(move |opts: DispatchOpts| {
        *captured.lock().unwrap() = Some(opts);
        ok_result(envelope("stop", 50, 10, 2_000), out_for_dispatch.clone())
    }));

    let step = unit_step(serde_json::json!({
        "plan": plan.to_string_lossy(), "unit": "u-0001", "rule": "intent-vs-diff",
        "intent_file": intent_file.to_string_lossy()
    }));
    kind.run(&step, &unit_task(), &BTreeMap::new()).unwrap();

    let opts = seen.lock().unwrap().take().unwrap();
    assert!(
        opts.message.contains(INTENT_MARKER),
        "the dispatched message must carry the intent file's text: {}",
        opts.message
    );
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


/// A `ModelHost` handle onto ONE shared [`MockHost`], so a factory that is
/// asked for a fresh host per call still records every op in one place.
struct SharedHost(Arc<std::sync::Mutex<darkmux_gestalt::mock::MockHost>>);
impl darkmux_gestalt::ModelHost for SharedHost {
    fn list_resident(&mut self) -> std::result::Result<Vec<darkmux_gestalt::ResidentFact>, darkmux_gestalt::HostError> {
        self.0.lock().unwrap().list_resident()
    }
    fn list_catalog(&mut self) -> std::result::Result<Vec<darkmux_gestalt::CatalogFact>, darkmux_gestalt::HostError> {
        self.0.lock().unwrap().list_catalog()
    }
    fn load(
        &mut self,
        model_key: &str,
        identifier: &str,
        min_ctx: u32,
        deadline: darkmux_gestalt::Deadline,
    ) -> std::result::Result<darkmux_gestalt::LoadReport, darkmux_gestalt::HostError> {
        self.0.lock().unwrap().load(model_key, identifier, min_ctx, deadline)
    }
    fn unload(
        &mut self,
        target: &darkmux_gestalt::plan::OwnedTarget,
        deadline: darkmux_gestalt::Deadline,
    ) -> std::result::Result<(), darkmux_gestalt::HostError> {
        self.0.lock().unwrap().unload(target, deadline)
    }
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
    assert!(s.plans_errored.is_empty(), "a clean plan step names nothing errored: {:?}", s.plans_errored);
}

/// (#2310 P4c-2b PR #2357 round-2 review item 5) `crawl.json` shares
/// `src/mission_launch.rs::grow_phase` with `review-v2.json` — since that
/// function now grows zero units from an errored plan step's rule rather
/// than aborting the whole launch (MUST FIX C), a crawl run reaches
/// `summarize` with a silently smaller `units_in_plan` unless the summary
/// NAMES the rule whose `crawl.plan` step failed. Proven here directly
/// against `summarize_mission`, independent of the launcher.
#[test]
#[serial_test::serial] // scopes DARKMUX_HOME, a process-global
fn the_summary_names_the_rule_whose_plan_step_errored() {
    let home = TempDir::new().unwrap();
    let _g = HomeGuard::set(home.path());
    save_phase(PHASE, MISSION);
    darkmux_crew::lifecycle::save_step(
        MISSION,
        PHASE,
        &Step {
            id: "plan-swallowed-error-step".into(),
            task_id: "plan-swallowed-error".into(),
            kind: crate::crawl::plan_step::CRAWL_PLAN_KIND.into(),
            gate: None,
            status: NodeStatus::Error,
            config: serde_json::json!({ "rule": "swallowed-error" }),
            started_ts: None,
            completed_ts: None,
            output: Some("workspace_spec::materialize failed: no such source".into()),
        },
    )
    .unwrap();

    let s = summarize_mission(MISSION).unwrap();
    assert_eq!(s.plans_errored, vec!["swallowed-error".to_string()], "{:?}", s.plans_errored);
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
        run_on: darkmux_crew::types::default_run_on(),
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

    // ONE mock host behind the factory (the scheduler asks for a fresh host per
    // call and drops it, so a per-call mock's op log is never inspectable): the
    // wave must SEE the resident model and must NOT load anything.
    let shared_host = Arc::new(std::sync::Mutex::new(
        darkmux_gestalt::mock::MockHost::new().resident("darkmux:m-local", "m-local", 8192, Some(1 << 30)),
    ));
    let facts = shared_host.lock().unwrap().facts(Default::default(), Default::default());
    let host_for_factory = shared_host.clone();
    let host_factory = move || -> Box<dyn darkmux_gestalt::ModelHost> { Box::new(SharedHost(host_for_factory.clone())) };
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
    // (#2321) The wave path ran, and it ran on the resident model: the packer
    // asked the host what is resident and issued no load. Delete the unit
    // kind's `residency()` and this goes red — the units then queue as remote
    // jobs and the host is never consulted at all.
    let ops = shared_host.lock().unwrap().ops.clone();
    assert!(
        ops.iter().any(|op| matches!(op, darkmux_gestalt::mock::HostOp::ListResident)),
        "the scheduler took the wave path (it consulted the host): {ops:?}"
    );
    assert!(
        !ops.iter().any(|op| matches!(op, darkmux_gestalt::mock::HostOp::Load { .. })),
        "the model was already resident, so the wave must not load: {ops:?}"
    );
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
                source: None,
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

/// (#2310 P4c review round 2, SHOULD FIX (c) — proven, premise corrected)
/// `residency()` used to hardcode `"crawler"` regardless of the owning
/// Task's `role_id`, the same bug the dispatch-side fix
/// (`a_task_naming_a_different_role_dispatches_as_that_role_and_stamps_
/// its_confirm_form`, above) proved for `run()`.
///
/// **Why this test does NOT use `config.json`'s `role_profiles` map to
/// distinguish roles by MODEL** (the first version of this test tried
/// exactly that and failed on a false premise, caught by actually running
/// it before trusting the design): `darkmux_types::config_access::
/// config()` is `EMPTY_CONFIG` by construction in every test build of this
/// crate (`test-support` feature, #811 — see that function's own doc,
/// and `resolve_local_placement_inner`'s "test builds see an empty map by
/// construction"). A `role_profiles` mapping written to disk in a test is
/// silently never read, so `resolve_role_profile` always falls through to
/// `default_profile` for EVERY role id — proving nothing about which role
/// string `residency()` actually passed through.
///
/// What DOES observably depend on `role_id`, with no config() involved: the
/// ROLE MANIFEST lookup inside `resolve_local_placement_inner_with`
/// (`crate::loader::load_roles().find(|r| r.id == role_id)`), which errors
/// — and `residency()` then returns `None` — when no role by that name is
/// registered. A Task naming a role the registry has no manifest for must
/// fail to resolve; a hardcoded `"crawler"` (which DOES exist) would
/// instead silently resolve crawler's own placement, masking the bug. That
/// is the actual mutation-kill: hardcode `"crawler"` back in and this test
/// goes from `None` to `Some(..)`.
#[test]
#[serial_test::serial] // scopes DARKMUX_HOME + DARKMUX_PROFILES, process-globals
fn residency_resolves_the_tasks_own_role_not_a_hardcoded_crawler() {
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

    // Sanity leg: the Task's role IS a registered role (unchanged from the
    // hardcoded-crawler test above, just spelled through the Task now) —
    // resolution succeeds.
    let mut task = unit_task();
    task.role_id = Some("crawler".to_string());
    assert!(
        kind.residency(&unit_step(serde_json::json!({})), &task, &BTreeMap::new(), &ctx).is_some(),
        "a Task declaring a real, registered role must resolve"
    );

    // The actual proof: a role no manifest exists for must FAIL to
    // resolve — which is only possible if `residency()` is reading this
    // Task's own `role_id` rather than substituting a hardcoded, always-
    // valid `"crawler"`.
    task.role_id = Some("no-such-role-in-the-registry".to_string());
    let placement = kind.residency(&unit_step(serde_json::json!({})), &task, &BTreeMap::new(), &ctx);
    assert!(
        placement.is_none(),
        "a Task naming a role the registry has no manifest for must not silently resolve as if \
         it were \"crawler\": {placement:?}"
    );
}

/// (#2310 P4c) `pattern_block`'s confirm-form appendix, all three shapes.
fn base_rule(id: &str, confirm: darkmux_crew::rules::ConfirmForm) -> darkmux_crew::rules::Rule {
    darkmux_crew::rules::Rule {
        id: id.to_string(),
        kind: darkmux_crew::rules::RuleKind::Site,
        title: None,
        applies_to: vec![],
        exclude: vec![],
        prefilter: vec![],
        window: None,
        chunk_tokens: None,
        edge: None,
        matches: None,
        no_match: None,
        evidence: None,
        why_hint: None,
        scope: vec![],
        confirm,
        search: None,
        compare: None,
        extras: Default::default(),
    }
}

#[test]
fn pattern_block_mod_confirm_appends_nothing() {
    let rule = base_rule("r", darkmux_crew::rules::ConfirmForm::Mod);
    let block = pattern_block(&rule);
    assert!(!block.contains("search"), "{block}");
    assert!(!block.contains("QUESTION"), "{block}");
}

#[test]
fn pattern_block_search_confirm_with_fixed_patterns_names_them() {
    let mut rule = base_rule("r", darkmux_crew::rules::ConfirmForm::Search);
    rule.search = Some(darkmux_crew::rules::SearchRecipe {
        patterns: vec!["fooBar(".to_string()],
        path: None,
        note: Some("every caller".to_string()),
        extras: Default::default(),
    });
    let block = pattern_block(&rule);
    assert!(block.contains("\"fooBar(\""), "{block}");
    assert!(block.contains("every caller"), "{block}");
    assert!(block.contains("SEARCHING"), "{block}");
}

#[test]
fn pattern_block_search_confirm_with_no_fixed_patterns_still_reads_as_one_instruction() {
    let mut rule = base_rule("r", darkmux_crew::rules::ConfirmForm::Search);
    rule.search = Some(darkmux_crew::rules::SearchRecipe {
        patterns: vec![],
        path: None,
        note: Some("search for the same verbs/nouns as the new routine's name".to_string()),
        extras: Default::default(),
    });
    let block = pattern_block(&rule);
    assert!(
        !block.contains("patterns over the tree (not just the window you were given): ."),
        "an empty pattern list must not render as a dangling, punctuated empty list: {block}"
    );
    assert!(block.contains("same verbs/nouns"), "{block}");
}

#[test]
fn pattern_block_question_confirm_renders_the_compare_question_and_an_optional_search_first() {
    let mut rule = base_rule("r", darkmux_crew::rules::ConfirmForm::Question);
    rule.compare = Some("does an existing helper do this already?".to_string());
    let block = pattern_block(&rule);
    assert!(block.contains("QUESTION"), "{block}");
    assert!(block.contains("does an existing helper do this already?"), "{block}");
    assert!(!block.contains("SEARCHING"), "a question-only rule with no search block gets no search instruction: {block}");

    rule.search = Some(darkmux_crew::rules::SearchRecipe {
        patterns: vec![],
        path: None,
        note: Some("look for a similar helper".to_string()),
        extras: Default::default(),
    });
    let block = pattern_block(&rule);
    assert!(block.contains("look for a similar helper"), "{block}");
    assert!(
        block.find("look for a similar helper").unwrap() < block.find("QUESTION").unwrap(),
        "the search instruction must precede the question instruction: {block}"
    );
}

// ── the FROZEN model-facing text (#2310 swarm F / S2-6) ─────────────────
//
// The dispatched unit message is measured model-facing text, so contract 6
// binds it: "frozen means ONE HASH, not one intention." Before this it was
// pinned by exactly two NEGATIVE substring assertions, which is why a
// mutation injecting a whole paragraph into `pattern_block`'s `Mod` arm
// left 858/858 green — the text could drift arbitrarily as long as it did
// not contain the two forbidden words.
//
// These goldens are GENERATED from the reference implementation and
// committed, so a change to the text is a reviewable diff instead of an
// invisible one. Every rule below is a BUILT-IN loaded through
// `rules::resolve_default`, not a hand-built fixture: a fixture would
// freeze the renderer while leaving the shipped prose (which is what
// actually reaches a model) unpinned.
//
// To regenerate after a deliberate, reviewed change to the text:
//   DARKMUX_UNIT_MESSAGE_GOLDEN_UPDATE=1 cargo test -p darkmux-lab --lib \
//     crawl::unit_step::tests::the_
// then READ the diff — this is the text a model is given.

const UNIT_MESSAGE_GOLDEN_UPDATE: &str = "DARKMUX_UNIT_MESSAGE_GOLDEN_UPDATE";

fn unit_message_golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/unit-message")
}

/// Compare `actual` against the committed golden `name`, or write it when
/// the regenerate env var is set. Compared as BYTES (via `String`) — this
/// is prose, so whitespace and ordering ARE the artifact.
fn assert_text_golden(name: &str, actual: &str) {
    let path = unit_message_golden_dir().join(name);
    if std::env::var(UNIT_MESSAGE_GOLDEN_UPDATE).is_ok() {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, actual).unwrap();
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "read {} ({e}) — run with {UNIT_MESSAGE_GOLDEN_UPDATE}=1 to generate it",
            path.display()
        )
    });
    assert_eq!(
        actual,
        expected,
        "\nthe model-facing text at {} drifted.\n\
         This is measured prose a model is dispatched with, so a diff here is a REVIEW item, \
         not a rebase: read the change, and if it is intended regenerate with\n  \
         {UNIT_MESSAGE_GOLDEN_UPDATE}=1 cargo test -p darkmux-lab --lib crawl::unit_step::tests::the_\n",
        path.display()
    );
}

/// The one built-in rule named, resolved through the real loader.
fn builtin_rule(id: &str) -> Rule {
    let (rules, _warnings) = darkmux_crew::rules::resolve_default(&[id.to_string()])
        .unwrap_or_else(|e| panic!("built-in rule `{id}` must resolve: {e}"));
    rules.into_iter().next().unwrap()
}

/// (#2310 swarm F / S2-6) `pattern_block`, once per `confirm` form, from
/// the shipped built-in that uses it. `swallowed-error` is `mod` (no
/// appended instruction at all — the arm a prose injection sailed
/// through), `shared-symbol-callers` is `search` with FIXED patterns,
/// `intent-vs-diff` is `question`, and `existing-solution` is the
/// `question`-plus-`search` composition whose recipe declares NO patterns
/// (the other half of `search_instruction`).
#[test]
fn the_pattern_block_text_is_frozen_per_confirm_form() {
    for (rule_id, golden) in [
        ("swallowed-error", "pattern-block-confirm-mod.txt"),
        ("shared-symbol-callers", "pattern-block-confirm-search.txt"),
        ("intent-vs-diff", "pattern-block-confirm-question.txt"),
        ("existing-solution", "pattern-block-confirm-question-with-search.txt"),
    ] {
        assert_text_golden(golden, &pattern_block(&builtin_rule(rule_id)));
    }
}

/// The forms must actually DIFFER, or the four goldens above could all be
/// the same block and the per-form freeze would be vacuous.
#[test]
fn the_pattern_block_forms_are_distinguishable() {
    let mod_form = pattern_block(&builtin_rule("swallowed-error"));
    let search_form = pattern_block(&builtin_rule("shared-symbol-callers"));
    let question_form = pattern_block(&builtin_rule("intent-vs-diff"));
    assert!(
        !mod_form.contains("Before you call create_finding"),
        "the `mod` form appends no confirmation instruction: {mod_form}"
    );
    assert!(search_form.contains("Run the `search` tool"), "{search_form}");
    assert!(question_form.contains("answer this question"), "{question_form}");
}

/// (#2310 swarm F / S2-6) The WHOLE dispatched message for one crawl
/// built-in unit, with and without `intent`. The two goldens differ by
/// exactly the intent block, which is the only thing `intent: Some(..)`
/// is allowed to change — an `intent` that silently reworded the rest
/// would be caught here.
#[test]
fn the_unit_dispatch_message_is_frozen_with_and_without_intent() {
    let rule = builtin_rule("swallowed-error");
    let mut rules_by_id = BTreeMap::new();
    rules_by_id.insert(rule.id.clone(), rule.clone());
    let unit = Unit::Site {
        id: "u-golden-1".into(),
        rule: rule.id.clone(),
        source: "app".into(),
        sites: vec![
            Site { file: "src/orders.ts".into(), line: 41, start: 30, end: 55, hits: vec![41] },
            Site { file: "src/util.ts".into(), line: 7, start: 1, end: 20, hits: vec![7] },
        ],
        est_tokens: 120,
    };

    let plain = build_message(&rules_by_id, &unit, None).unwrap();
    assert_text_golden("build-message-site-no-intent.txt", &plain);

    let with_intent =
        build_message(&rules_by_id, &unit, Some("Tighten error handling in the orders path.")).unwrap();
    assert_text_golden("build-message-site-with-intent.txt", &with_intent);

    // The intent block is ADDITIVE and nothing else moves: the no-intent
    // message must be a suffix of the with-intent one.
    assert!(
        with_intent.ends_with(&plain),
        "`intent` must only PREPEND its block — the rest of the message is unchanged",
    );
    assert_ne!(plain, with_intent, "the intent block must actually render");
}

