//! Dependency-graph scheduler (#1230 Packet 2).
//!
//! Generic over a `DependencyNode` trait so ONE readiness function
//! (`is_ready`) and one reachability function (`reachable`) serve both
//! Steps within a Task/Sprint AND Sprints within a Mission — the latter
//! via the `SprintNode` adapter at the bottom of this file, which is what
//! finally makes `Sprint.depends_on` dependency-aware instead of the
//! historical flat `depends_on.is_empty()` "is this a root" filter (see
//! `src/mission_run.rs::select_sprint` and `src/main.rs`'s `mission
//! dispatch` fan-out, both migrated to `is_ready` via this adapter).
//!
//! `run_step_graph` is the actual DAG executor: compute every currently-
//! ready Step, fan them out through Packet 1's `run_bounded` (one
//! `run_bounded` call = one "wave" of concurrently-runnable work), flush
//! results, recompute readiness, repeat until nothing is ready and
//! nothing is left `Planned`.
//!
//! # Residency (a deliberate Packet 2 scope cut)
//!
//! `run_bounded` wants to know, per job, whether it needs a local model
//! resident (`Residency::Local(Placement)`, gestalt-wave-planned) or is
//! remote/unbound (`Residency::Remote`, cap-bounded only). Resolving
//! *which* local model (if any) a `dispatch.internal`/`dispatch.
//! single_shot` step's config ultimately targets is entangled with
//! role/profile resolution that Packets 3 and 4 own (mission_run's
//! migration onto this engine, and the funnel's migration, respectively)
//! — Packet 2 ships storage + scheduler only, no CLI verb, no production
//! caller wiring a real dispatch chain through this graph yet. So every
//! Step here runs through `run_bounded`'s `Residency::Remote` track,
//! which still gives real bounded concurrency (the diamond-shape
//! acceptance case) without gestalt RAM-safety reasoning. Local-model
//! contention protection for `dispatch.*` steps is Packet 3/4's job once
//! real dispatch data flows through this graph — tracked forward here
//! rather than silently assumed solved.

use crate::step_kinds::StepKindRegistry;
use crate::types::{NodeStatus, Sprint, SprintStatus, Step};
use anyhow::{anyhow, Result};
use darkmux_flow::{Category, FlowRecord, Level, Stage, Tier};
use darkmux_gestalt::{Facts, FootprintEstimator};
use std::collections::{BTreeMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

// ─── DependencyNode + readiness/reachability ───────────────────────────

/// A node in a dependency graph — implemented by `Step` directly and, via
/// `SprintNode`, by `Sprint` too (see module doc).
pub trait DependencyNode {
    fn node_id(&self) -> &str;
    fn node_depends_on(&self) -> &[String];
    fn node_status(&self) -> NodeStatus;
}

impl DependencyNode for Step {
    fn node_id(&self) -> &str {
        &self.id
    }
    fn node_depends_on(&self) -> &[String] {
        &self.depends_on
    }
    fn node_status(&self) -> NodeStatus {
        self.status
    }
}

/// `Sprint → DependencyNode` adapter. Maps `SprintStatus` onto
/// `NodeStatus` (`Complete → Complete`, `Abandoned → Abandoned`,
/// `Planned`/`Running` passthrough — `SprintStatus` has no `Error`
/// variant, so that arm is unreachable from this mapping direction).
/// `Copy` (just wraps a `&Sprint`) so building the `by_id` map the
/// generic functions need is cheap.
#[derive(Clone, Copy)]
pub struct SprintNode<'a>(pub &'a Sprint);

impl<'a> DependencyNode for SprintNode<'a> {
    fn node_id(&self) -> &str {
        &self.0.id
    }
    fn node_depends_on(&self) -> &[String] {
        &self.0.depends_on
    }
    fn node_status(&self) -> NodeStatus {
        match self.0.status {
            SprintStatus::Planned => NodeStatus::Planned,
            SprintStatus::Running => NodeStatus::Running,
            SprintStatus::Complete => NodeStatus::Complete,
            SprintStatus::Abandoned => NodeStatus::Abandoned,
        }
    }
}

/// `true` iff `node` is itself `Planned` AND every dependency it names
/// resolves (in `by_id`) to a node whose status is `Complete`. A
/// dependency id that doesn't resolve in `by_id` (a dangling reference)
/// is treated as unsatisfied — `is_ready` fails closed, never open, on
/// a graph-integrity problem.
pub fn is_ready<N: DependencyNode>(node: &N, by_id: &BTreeMap<String, &N>) -> bool {
    if node.node_status() != NodeStatus::Planned {
        return false;
    }
    node.node_depends_on().iter().all(|dep_id| {
        by_id
            .get(dep_id)
            .map(|dep| dep.node_status() == NodeStatus::Complete)
            .unwrap_or(false)
    })
}

/// `true` iff `target_id` and every node in its FULL ancestor chain
/// (transitive `depends_on`, not just one hop) is neither `Abandoned`
/// nor `Error`. This is the doom-loop signal: `validate-cure` depending
/// on `runtime-capture` (planned), `file-match` (abandoned), and
/// `sovereignty-verbs` (planned) is permanently unreachable the moment
/// `file-match` is abandoned, even though `validate-cure` itself is
/// still `Planned` — see the `doom_loop_m4_fixture` regression test.
///
/// An unresolvable dependency id (dangling reference) does NOT itself
/// make the target unreachable — a missing node can't be judged
/// abandoned/errored, only a *known* bad node can. A cycle in the graph
/// (which `detect_cycles` should already have rejected before this runs
/// in `run_step_graph`'s path) is defused defensively via a `seen` set
/// so this function terminates rather than looping forever.
pub fn reachable<N: DependencyNode>(target_id: &str, by_id: &BTreeMap<String, &N>) -> bool {
    let mut seen: HashSet<String> = HashSet::new();
    reachable_inner(target_id, by_id, &mut seen)
}

fn reachable_inner<N: DependencyNode>(
    id: &str,
    by_id: &BTreeMap<String, &N>,
    seen: &mut HashSet<String>,
) -> bool {
    if !seen.insert(id.to_string()) {
        // Already visited this id on the current walk without finding a
        // bad ancestor — treat as fine rather than looping (cycles are
        // supposed to be rejected upstream by `detect_cycles`).
        return true;
    }
    let Some(node) = by_id.get(id) else {
        // Dangling reference — can't judge; not this function's job to
        // flag graph integrity (that's `detect_cycles`/loader validation).
        return true;
    };
    if matches!(node.node_status(), NodeStatus::Abandoned | NodeStatus::Error) {
        return false;
    }
    node.node_depends_on()
        .iter()
        .all(|dep| reachable_inner(dep, by_id, seen))
}

// ─── Cycle detection (graph-load-time, before scheduling starts) ──────

/// Rejects a `Step` graph containing a dependency cycle with a clear
/// error naming the cycle, rather than letting `run_step_graph` hang
/// forever waiting for a ready node that can never become ready.
pub fn detect_cycles(steps: &BTreeMap<String, Step>) -> Result<()> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Color {
        White,
        Gray,
        Black,
    }

    fn visit(
        id: &str,
        steps: &BTreeMap<String, Step>,
        colors: &mut BTreeMap<String, Color>,
        path: &mut Vec<String>,
    ) -> Result<()> {
        match colors.get(id).copied() {
            Some(Color::Black) | None => return Ok(()),
            Some(Color::Gray) => {
                path.push(id.to_string());
                let cycle_start = path.iter().position(|p| p == id).unwrap_or(0);
                let cycle = path[cycle_start..].join(" -> ");
                anyhow::bail!("cycle detected in step graph: {cycle}");
            }
            Some(Color::White) => {}
        }
        colors.insert(id.to_string(), Color::Gray);
        path.push(id.to_string());
        if let Some(step) = steps.get(id) {
            for dep in &step.depends_on {
                visit(dep, steps, colors, path)?;
            }
        }
        path.pop();
        colors.insert(id.to_string(), Color::Black);
        Ok(())
    }

    let mut colors: BTreeMap<String, Color> =
        steps.keys().map(|k| (k.clone(), Color::White)).collect();
    for id in steps.keys() {
        let mut path = Vec::new();
        visit(id, steps, &mut colors, &mut path)?;
    }
    Ok(())
}

// ─── Input gathering ────────────────────────────────────────────────────

/// The `output` text of every already-`Complete` dependency of `step`,
/// keyed by dependency Step id. A dependency that's `Complete` but has
/// no recorded `output` (a step kind that legitimately produces none) is
/// omitted, not stubbed with an empty string.
pub fn gather_inputs(step: &Step, steps: &BTreeMap<String, Step>) -> BTreeMap<String, String> {
    step.depends_on
        .iter()
        .filter_map(|dep_id| {
            let dep = steps.get(dep_id)?;
            if dep.status != NodeStatus::Complete {
                return None;
            }
            dep.output.clone().map(|output| (dep_id.clone(), output))
        })
        .collect()
}

// ─── The scheduler loop ─────────────────────────────────────────────────

/// Summary of one `run_step_graph` call: which steps completed, which
/// errored, and how many wave iterations it took. Steps left `Planned`
/// at the end (possible only if their dependency chain includes an
/// `Error`/`Abandoned` node — see `reachable`) are NOT listed in either
/// `completed` or `errored`; the caller can find them by scanning
/// `steps` for lingering `NodeStatus::Planned` after the call returns.
#[derive(Debug, Default, Clone)]
pub struct SchedulerReport {
    pub completed: Vec<String>,
    pub errored: Vec<String>,
    pub iterations: usize,
}

/// Walk `steps` to completion: each iteration computes every currently-
/// ready node, marks them `Running`, fans them out through Packet 1's
/// `run_bounded` (one call = one wave — see the module doc's Residency
/// section for why every job here is `Residency::Remote`), flushes each
/// job's `StepOutcome` onto its Step (status + `output` + timestamps),
/// emits step-lifecycle bookend records through `emit`, and recomputes
/// readiness. Stops when nothing is ready (either the graph finished, or
/// every remaining `Planned` step's dependency chain includes an
/// `Error`/`Abandoned` node — i.e. is `!reachable`).
///
/// Rejects a cyclic graph up front via `detect_cycles` rather than
/// looping forever on a Step that can never become ready.
pub fn run_step_graph(
    steps: &mut BTreeMap<String, Step>,
    kinds: &StepKindRegistry,
    facts: &Facts,
    est: &dyn FootprintEstimator,
    remote_cap: usize,
    emit: &mut dyn FnMut(FlowRecord),
) -> Result<SchedulerReport> {
    detect_cycles(steps)?;

    let mut report = SchedulerReport::default();

    loop {
        // Scoped so `by_id`'s borrows into `steps` end before the
        // mutable `steps.get_mut` calls below — `ready_ids` itself is
        // fully owned (`Vec<String>`) and carries no borrow forward.
        let ready_ids: Vec<String> = {
            let by_id: BTreeMap<String, &Step> =
                steps.iter().map(|(k, v)| (k.clone(), v)).collect();
            steps
                .values()
                .filter(|s| is_ready(*s, &by_id))
                .map(|s| s.id.clone())
                .collect()
        };

        if ready_ids.is_empty() {
            break;
        }

        let now = now_unix();
        let mut jobs = Vec::with_capacity(ready_ids.len());
        for id in &ready_ids {
            let step = steps.get_mut(id).expect("id came from `steps` itself");
            step.status = NodeStatus::Running;
            step.started_ts = Some(now);
            emit(step_lifecycle_record(step, "step start"));
        }
        // Re-borrow immutably now that every ready step's status flip is
        // recorded — `gather_inputs` needs `&steps` (completed sibling
        // outputs), and the job closures below need owned snapshots
        // ('static, per `run_bounded`'s `Send + 'static` job contract).
        for (idx, id) in ready_ids.iter().enumerate() {
            let step_snapshot = steps.get(id).expect("just set to Running above").clone();
            let input = gather_inputs(&step_snapshot, steps);
            let kind = kinds
                .get(&step_snapshot.kind)
                .with_context_step(&step_snapshot)?;
            let job: crate::concurrent_dispatch::DispatchJob<StepJobResult> =
                Box::new(move || {
                    let outcome = kind.run(&step_snapshot, &input)?;
                    Ok((
                        StepJobResult {
                            output: outcome.output,
                        },
                        outcome.flow_records,
                    ))
                });
            jobs.push(crate::concurrent_dispatch::QueuedJob {
                index: idx,
                residency: crate::concurrent_dispatch::Residency::Remote,
                job,
            });
        }

        let results = crate::concurrent_dispatch::run_bounded(jobs, facts, est, remote_cap)?;

        let finished_at = now_unix();
        for (idx, outcome) in results {
            let id = &ready_ids[idx];
            let step = steps.get_mut(id).expect("id came from ready_ids itself");
            match outcome {
                Ok((job_result, flow_records)) => {
                    for record in flow_records {
                        emit(record);
                    }
                    step.status = NodeStatus::Complete;
                    step.completed_ts = Some(finished_at);
                    step.output = Some(job_result.output);
                    emit(step_lifecycle_record(step, "step complete"));
                    report.completed.push(id.clone());
                }
                Err(e) => {
                    step.status = NodeStatus::Error;
                    step.completed_ts = Some(finished_at);
                    step.output = Some(format!("{e:#}"));
                    emit(step_lifecycle_record(step, "step error"));
                    report.errored.push(id.clone());
                }
            }
        }

        report.iterations += 1;
    }

    Ok(report)
}

/// The value a `dispatch.internal`/etc. job closure returns through
/// `run_bounded` — the scheduler only needs the output text back
/// (status/timestamps are updated on the main thread by `run_step_graph`
/// itself from the `Ok`/`Err` outcome, not from this payload).
struct StepJobResult {
    output: String,
}

/// Small `anyhow::Context`-shaped helper so the step-kind-lookup failure
/// names the step id, mirroring every other `with_context` call in this
/// module.
trait WithContextStep<T> {
    fn with_context_step(self, step: &Step) -> Result<T>;
}

impl<T> WithContextStep<T> for Result<T> {
    fn with_context_step(self, step: &Step) -> Result<T> {
        self.map_err(|e| anyhow!("step `{}` (kind `{}`): {e}", step.id, step.kind))
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One `FlowRecord` for a step-lifecycle transition (`"step start"` /
/// `"step complete"` / `"step error"`). Mirrors `lifecycle.rs`'s
/// `emit_sprint_transition_record` shape (`Category::Work`,
/// `Tier::Local` since these are scheduler-driven, not operator-explicit
/// like a Sprint transition; `Stage::Dispatch` since a Step is
/// dispatch-shaped work).
fn step_lifecycle_record(step: &Step, action: &str) -> FlowRecord {
    FlowRecord {
        ts: darkmux_flow::ts_utc_now(),
        level: if action == "step error" { Level::Warn } else { Level::Info },
        category: Category::Work,
        tier: Tier::Local,
        stage: Stage::Dispatch,
        action: action.to_string(),
        handle: step.id.clone(),
        sprint_id: None,
        session_id: Some(format!("task:{}", step.task_id)),
        source: Some("scheduler".to_string()),
        model: None,
        reasoning: None,
        mission_id: None,
        machine_id: None,
        machine_uid: None,
        orchestrator: None,
        prev_hash: None,
        hash: None,
        payload: None,
        work_id: None,
        attempt: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step_kinds::StepKindRegistry;
    use darkmux_gestalt::FixedEstimator;
    use serde_json::json;

    fn noop_step(id: &str, depends_on: &[&str]) -> Step {
        Step {
            id: id.to_string(),
            task_id: "t1".to_string(),
            kind: "procedural.noop".to_string(),
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            status: NodeStatus::Planned,
            config: json!(null),
            started_ts: None,
            completed_ts: None,
            output: None,
        }
    }

    fn step_with_status(id: &str, depends_on: &[&str], status: NodeStatus) -> Step {
        let mut s = noop_step(id, depends_on);
        s.status = status;
        s
    }

    // ─── is_ready ────────────────────────────────────────────────────

    #[test]
    fn is_ready_true_for_planned_no_deps() {
        let a = step_with_status("a", &[], NodeStatus::Planned);
        let by_id: BTreeMap<String, &Step> = [("a".to_string(), &a)].into_iter().collect();
        assert!(is_ready(&a, &by_id));
    }

    #[test]
    fn is_ready_false_when_not_planned() {
        let a = step_with_status("a", &[], NodeStatus::Running);
        let by_id: BTreeMap<String, &Step> = [("a".to_string(), &a)].into_iter().collect();
        assert!(!is_ready(&a, &by_id));
    }

    #[test]
    fn is_ready_false_when_dependency_incomplete() {
        let a = step_with_status("a", &[], NodeStatus::Planned);
        let b = step_with_status("b", &["a"], NodeStatus::Planned);
        let by_id: BTreeMap<String, &Step> =
            [("a".to_string(), &a), ("b".to_string(), &b)].into_iter().collect();
        assert!(!is_ready(&b, &by_id));
    }

    #[test]
    fn is_ready_true_when_dependency_complete() {
        let a = step_with_status("a", &[], NodeStatus::Complete);
        let b = step_with_status("b", &["a"], NodeStatus::Planned);
        let by_id: BTreeMap<String, &Step> =
            [("a".to_string(), &a), ("b".to_string(), &b)].into_iter().collect();
        assert!(is_ready(&b, &by_id));
    }

    #[test]
    fn is_ready_false_on_dangling_dependency() {
        let b = step_with_status("b", &["ghost"], NodeStatus::Planned);
        let by_id: BTreeMap<String, &Step> = [("b".to_string(), &b)].into_iter().collect();
        assert!(!is_ready(&b, &by_id), "a dangling dep must fail closed");
    }

    // ─── reachable ───────────────────────────────────────────────────

    #[test]
    fn reachable_true_for_clean_chain() {
        let a = step_with_status("a", &[], NodeStatus::Complete);
        let b = step_with_status("b", &["a"], NodeStatus::Planned);
        let by_id: BTreeMap<String, &Step> =
            [("a".to_string(), &a), ("b".to_string(), &b)].into_iter().collect();
        assert!(reachable("b", &by_id));
    }

    #[test]
    fn reachable_false_when_direct_ancestor_abandoned() {
        let a = step_with_status("a", &[], NodeStatus::Abandoned);
        let b = step_with_status("b", &["a"], NodeStatus::Planned);
        let by_id: BTreeMap<String, &Step> =
            [("a".to_string(), &a), ("b".to_string(), &b)].into_iter().collect();
        assert!(!reachable("b", &by_id));
    }

    #[test]
    fn reachable_false_when_transitive_ancestor_errored() {
        let a = step_with_status("a", &[], NodeStatus::Error);
        let b = step_with_status("b", &["a"], NodeStatus::Complete);
        let c = step_with_status("c", &["b"], NodeStatus::Planned);
        let by_id: BTreeMap<String, &Step> = [
            ("a".to_string(), &a),
            ("b".to_string(), &b),
            ("c".to_string(), &c),
        ]
        .into_iter()
        .collect();
        assert!(
            !reachable("c", &by_id),
            "c depends transitively on errored `a` via `b` — must be unreachable"
        );
    }

    #[test]
    fn reachable_true_when_target_itself_is_abandoned_sibling_branch_unaffected() {
        // Sibling branches don't cross-contaminate reachability.
        let a = step_with_status("a", &[], NodeStatus::Complete);
        let b = step_with_status("b", &[], NodeStatus::Abandoned);
        let d = step_with_status("d", &["a"], NodeStatus::Planned);
        let by_id: BTreeMap<String, &Step> = [
            ("a".to_string(), &a),
            ("b".to_string(), &b),
            ("d".to_string(), &d),
        ]
        .into_iter()
        .collect();
        assert!(reachable("d", &by_id), "d depends only on healthy `a`, not on abandoned `b`");
    }

    /// (#1230 Packet 2 acceptance) Reproduces the REAL `doom-loop-m4`
    /// mission's shape read from `~/.darkmux/missions/doom-loop-m4/`:
    /// `validate-cure` depends on `runtime-capture` (planned),
    /// `file-match` (abandoned), `sovereignty-verbs` (planned).
    /// `file-match` being abandoned must make `validate-cure`
    /// permanently unreachable even though `validate-cure` itself is
    /// still `Planned`.
    #[test]
    fn doom_loop_m4_fixture_validate_cure_is_unreachable() {
        let runtime_capture = crate::types::Sprint {
            id: "runtime-capture".to_string(),
            mission_id: "doom-loop-m4".to_string(),
            description: "runtime-side firing-time capture".to_string(),
            status: SprintStatus::Planned,
            depends_on: vec![],
            created_ts: 1_782_141_824,
            started_ts: None,
            completed_ts: None,
            abandoned_ts: None,
            task_ids: Vec::new(),
        };
        let file_match = crate::types::Sprint {
            id: "file-match".to_string(),
            mission_id: "doom-loop-m4".to_string(),
            description: "file-match precision".to_string(),
            status: SprintStatus::Abandoned,
            depends_on: vec![],
            created_ts: 1_782_141_824,
            started_ts: Some(1_782_141_937),
            completed_ts: None,
            abandoned_ts: Some(1_782_147_136),
            task_ids: Vec::new(),
        };
        let sovereignty_verbs = crate::types::Sprint {
            id: "sovereignty-verbs".to_string(),
            mission_id: "doom-loop-m4".to_string(),
            description: "lessons sovereignty verbs".to_string(),
            status: SprintStatus::Planned,
            depends_on: vec![],
            created_ts: 1_782_141_824,
            started_ts: None,
            completed_ts: None,
            abandoned_ts: None,
            task_ids: Vec::new(),
        };
        let validate_cure = crate::types::Sprint {
            id: "validate-cure".to_string(),
            mission_id: "doom-loop-m4".to_string(),
            description: "validate the cure".to_string(),
            status: SprintStatus::Planned,
            depends_on: vec![
                "runtime-capture".to_string(),
                "file-match".to_string(),
                "sovereignty-verbs".to_string(),
            ],
            created_ts: 1_782_141_824,
            started_ts: None,
            completed_ts: None,
            abandoned_ts: None,
            task_ids: Vec::new(),
        };

        let nodes: Vec<SprintNode> = vec![
            SprintNode(&runtime_capture),
            SprintNode(&file_match),
            SprintNode(&sovereignty_verbs),
            SprintNode(&validate_cure),
        ];
        let by_id: BTreeMap<String, &SprintNode> = nodes
            .iter()
            .map(|n| (n.node_id().to_string(), n))
            .collect();

        assert!(
            !reachable("validate-cure", &by_id),
            "validate-cure must be unreachable — file-match (a dependency) is abandoned"
        );
        assert!(
            !is_ready(&SprintNode(&validate_cure), &by_id),
            "validate-cure must not be ready either — file-match never completes"
        );
        // The healthy sprints (no abandoned ancestors) remain reachable —
        // this fixture isn't accidentally flagging the whole mission.
        assert!(reachable("runtime-capture", &by_id));
        assert!(reachable("sovereignty-verbs", &by_id));
    }

    // ─── detect_cycles ───────────────────────────────────────────────

    #[test]
    fn detect_cycles_ok_on_acyclic_graph() {
        let a = noop_step("a", &[]);
        let b = noop_step("b", &["a"]);
        let steps: BTreeMap<String, Step> =
            [("a".to_string(), a), ("b".to_string(), b)].into_iter().collect();
        assert!(detect_cycles(&steps).is_ok());
    }

    #[test]
    fn detect_cycles_rejects_direct_cycle() {
        let a = noop_step("a", &["b"]);
        let b = noop_step("b", &["a"]);
        let steps: BTreeMap<String, Step> =
            [("a".to_string(), a), ("b".to_string(), b)].into_iter().collect();
        let err = detect_cycles(&steps).unwrap_err();
        assert!(err.to_string().contains("cycle detected"), "{err}");
    }

    #[test]
    fn detect_cycles_rejects_transitive_cycle() {
        let a = noop_step("a", &["c"]);
        let b = noop_step("b", &["a"]);
        let c = noop_step("c", &["b"]);
        let steps: BTreeMap<String, Step> = [
            ("a".to_string(), a),
            ("b".to_string(), b),
            ("c".to_string(), c),
        ]
        .into_iter()
        .collect();
        let err = detect_cycles(&steps).unwrap_err();
        assert!(err.to_string().contains("cycle detected"), "{err}");
    }

    #[test]
    fn detect_cycles_self_dependency_is_a_cycle() {
        let a = noop_step("a", &["a"]);
        let steps: BTreeMap<String, Step> = [("a".to_string(), a)].into_iter().collect();
        assert!(detect_cycles(&steps).is_err());
    }

    // ─── run_step_graph (integration, via procedural.noop) ────────────

    fn run_test_graph(steps: &mut BTreeMap<String, Step>) -> SchedulerReport {
        let kinds = StepKindRegistry::with_builtins();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let mut emitted = Vec::new();
        run_step_graph(steps, &kinds, &facts, &est, 8, &mut |r| emitted.push(r)).unwrap()
    }

    #[test]
    fn run_step_graph_respects_topological_ordering_linear_chain() {
        let mut steps: BTreeMap<String, Step> = [
            ("a".to_string(), noop_step("a", &[])),
            ("b".to_string(), noop_step("b", &["a"])),
            ("c".to_string(), noop_step("c", &["b"])),
        ]
        .into_iter()
        .collect();

        let report = run_test_graph(&mut steps);

        assert_eq!(report.completed.len(), 3);
        assert_eq!(report.errored.len(), 0);
        for id in ["a", "b", "c"] {
            assert_eq!(steps[id].status, NodeStatus::Complete, "{id} should be Complete");
        }
        let a_done = steps["a"].completed_ts.unwrap();
        let b_start = steps["b"].started_ts.unwrap();
        let b_done = steps["b"].completed_ts.unwrap();
        let c_start = steps["c"].started_ts.unwrap();
        assert!(a_done <= b_start, "b must not start before a completes");
        assert!(b_done <= c_start, "c must not start before b completes");
    }

    /// (#1230 Packet 2 acceptance) Diamond shape: A→B, A→C, B and C both
    /// →D. B and C must both complete before D becomes ready — and,
    /// since they're scheduled in the SAME wave (both ready at once
    /// after A completes), they run concurrently via Packet 1's
    /// `run_bounded`.
    #[test]
    fn run_step_graph_diamond_runs_b_and_c_concurrently_then_d() {
        let mut steps: BTreeMap<String, Step> = [
            ("a".to_string(), noop_step("a", &[])),
            ("b".to_string(), noop_step("b", &["a"])),
            ("c".to_string(), noop_step("c", &["a"])),
            ("d".to_string(), noop_step("d", &["b", "c"])),
        ]
        .into_iter()
        .collect();

        let report = run_test_graph(&mut steps);

        assert_eq!(report.completed.len(), 4);
        for id in ["a", "b", "c", "d"] {
            assert_eq!(steps[id].status, NodeStatus::Complete, "{id} should be Complete");
        }
        // D must start only after BOTH B and C have completed.
        let b_done = steps["b"].completed_ts.unwrap();
        let c_done = steps["c"].completed_ts.unwrap();
        let d_start = steps["d"].started_ts.unwrap();
        assert!(b_done <= d_start && c_done <= d_start);
        // B and C were scheduled in the same wave (both became ready in
        // the same readiness computation, right after A completed) —
        // the iteration count proves the wave shape: A alone (1), then
        // B+C together (2), then D alone (3) = 3 iterations, not 4.
        assert_eq!(report.iterations, 3, "A, then B+C together, then D");
    }

    #[test]
    fn run_step_graph_reports_errored_step_and_still_completes_independent_branch() {
        let failing = {
            let mut s = noop_step("fails", &[]);
            s.kind = "procedural.shell".to_string();
            s.config = json!({"command": "exit 1"});
            s
        };
        let mut steps: BTreeMap<String, Step> = [
            ("fails".to_string(), failing),
            ("independent".to_string(), noop_step("independent", &[])),
        ]
        .into_iter()
        .collect();

        let report = run_test_graph(&mut steps);

        assert_eq!(steps["fails"].status, NodeStatus::Error);
        assert_eq!(steps["independent"].status, NodeStatus::Complete);
        assert_eq!(report.errored, vec!["fails".to_string()]);
        assert!(report.completed.contains(&"independent".to_string()));
    }

    #[test]
    fn run_step_graph_downstream_of_errored_step_never_runs_and_stays_planned() {
        let failing = {
            let mut s = noop_step("fails", &[]);
            s.kind = "procedural.shell".to_string();
            s.config = json!({"command": "exit 1"});
            s
        };
        let mut steps: BTreeMap<String, Step> = [
            ("fails".to_string(), failing),
            ("downstream".to_string(), noop_step("downstream", &["fails"])),
        ]
        .into_iter()
        .collect();

        let report = run_test_graph(&mut steps);

        assert_eq!(steps["fails"].status, NodeStatus::Error);
        assert_eq!(
            steps["downstream"].status,
            NodeStatus::Planned,
            "downstream of an errored dependency never becomes ready"
        );
        assert!(!report.completed.contains(&"downstream".to_string()));
        assert!(!report.errored.contains(&"downstream".to_string()));

        // Cross-check against `reachable`: the scheduler's own behavior
        // (never running `downstream`) matches what `reachable` predicts.
        let by_id: BTreeMap<String, &Step> =
            steps.iter().map(|(k, v)| (k.clone(), v)).collect();
        assert!(!reachable("downstream", &by_id));
    }

    #[test]
    fn run_step_graph_rejects_cyclic_graph_before_running_anything() {
        let a = noop_step("a", &["b"]);
        let b = noop_step("b", &["a"]);
        let mut steps: BTreeMap<String, Step> =
            [("a".to_string(), a), ("b".to_string(), b)].into_iter().collect();

        let kinds = StepKindRegistry::with_builtins();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let mut emitted = Vec::new();
        let err = run_step_graph(&mut steps, &kinds, &facts, &est, 8, &mut |r| emitted.push(r))
            .unwrap_err();
        assert!(err.to_string().contains("cycle detected"));
        assert!(emitted.is_empty(), "no step-lifecycle records before the cycle check fires");
        for step in steps.values() {
            assert_eq!(step.status, NodeStatus::Planned, "nothing should have run");
        }
    }

    #[test]
    fn run_step_graph_emits_step_start_and_step_complete_records() {
        let mut steps: BTreeMap<String, Step> =
            [("a".to_string(), noop_step("a", &[]))].into_iter().collect();
        let kinds = StepKindRegistry::with_builtins();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let mut emitted: Vec<FlowRecord> = Vec::new();
        run_step_graph(&mut steps, &kinds, &facts, &est, 8, &mut |r| emitted.push(r)).unwrap();

        let actions: Vec<&str> = emitted.iter().map(|r| r.action.as_str()).collect();
        assert!(actions.contains(&"step start"));
        assert!(actions.contains(&"step complete"));
    }
}
