//! Dependency-graph scheduler (#1230 Packet 2, revised #1341).
//!
//! **All real dependency/concurrency/data-flow lives at the Task level**
//! (`Task::depends_on`) — see `types::Task`'s doc. Phase is strictly
//! linear (ordered by `Mission::phase_ids` position, no dependency
//! semantics of its own); Step is strictly linear within its Task
//! (ordered by `Task::step_ids` position, no dependency semantics of its
//! own either). `run_step_graph` is the actual DAG executor: compute every
//! currently-ready Step (Task-aware — see `step_is_ready`), fan them out
//! through Packet 1's `run_bounded` (one `run_bounded` call = one "wave"
//! of concurrently-runnable work), flush results, recompute readiness,
//! repeat until nothing is ready and nothing is left `Planned`.
//!
//! # Residency (resolved for real in Packet 3)
//!
//! `run_bounded` wants to know, per job, whether it needs a local model
//! resident (`Residency::Local(Placement)`, gestalt-wave-planned) or is
//! remote/unbound (`Residency::Remote`, cap-bounded only). Packet 2 shipped
//! this hardcoded to `Residency::Remote` for every step (storage + scheduler
//! only, no production caller wiring a real dispatch chain through the
//! graph yet) — Packet 3 resolves it for real via `StepKind::residency`
//! (`step_kinds::types`): each ready step's registered kind is asked which
//! local model (if any) it needs, best-effort (see that trait method's
//! doc — a resolution miss fails OPEN to `Remote`, never a hard error).
//! `DispatchInternalStepKind` implements it via `step_kinds::
//! resolve_local_placement` (role→profile→`select_model`, mirroring the
//! dispatch preflight's own resolution); `mission_run`'s own
//! `MissionCoderStepKind`/`MissionVerifyStepKind` do the same for the
//! `mission.coder`/`mission.verify` kinds. `dispatch.single_shot`'s
//! residency (the review's probe/judge seats) is left at the default
//! (`None` → `Remote`) — Packet 4's job, once real concurrent local
//! seats exist to benefit from it; today's linear graphs (mission_run's
//! 3-step chain) never have more than one step ready per wave, so the
//! classification is correctness/observability, not a measured speedup.

use crate::step_kinds::StepKindRegistry;
use crate::types::{NodeStatus, Step, Task};
use anyhow::{anyhow, Result};
use darkmux_flow::{Category, FlowRecord, Level, Stage, Tier};
use darkmux_gestalt::{Facts, FootprintEstimator};
use std::collections::{BTreeMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

// ─── Task dependency cycle detection (graph-load-time) (#1341) ────────
//
// (#1341 — reaches back into Packet 2) The generic `DependencyNode`/
// `is_ready`/`reachable`/`PhaseNode` machinery this module used to define
// is GONE: Phase is now strictly linear (ordered purely by
// `Mission::phase_ids` position, no `depends_on` of its own — see
// `types::Phase`'s doc) and Step has no `depends_on` either (ordered by
// `Task::step_ids` position). The only real graph left is Task-level
// (`Task::depends_on`), handled by the direct functions below — Phase's
// "is this the next runnable one"/"is this unreachable" questions
// (`mission_run::select_phase`, `mission_status::unreachable_phase_drifts`)
// are now simple linear scans over `Mission::phase_ids`, needing no
// graph-walking trait at all.

/// Rejects a `Task` graph containing a `Task.depends_on` cycle with a
/// clear error naming the cycle, rather than letting `run_step_graph` hang
/// forever waiting for a Task that can never become ready. Task-level now
/// (#1341 moved ALL cross-Step dependency declaration up to
/// `Task.depends_on` — Steps within one Task are ordered purely by
/// `step_ids` position, which is structurally acyclic by construction).
pub fn detect_cycles(tasks: &BTreeMap<String, Task>) -> Result<()> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Color {
        White,
        Gray,
        Black,
    }

    fn visit(
        id: &str,
        tasks: &BTreeMap<String, Task>,
        colors: &mut BTreeMap<String, Color>,
        path: &mut Vec<String>,
    ) -> Result<()> {
        match colors.get(id).copied() {
            Some(Color::Black) | None => return Ok(()),
            Some(Color::Gray) => {
                path.push(id.to_string());
                let cycle_start = path.iter().position(|p| p == id).unwrap_or(0);
                let cycle = path[cycle_start..].join(" -> ");
                anyhow::bail!("cycle detected in task graph: {cycle}");
            }
            Some(Color::White) => {}
        }
        colors.insert(id.to_string(), Color::Gray);
        path.push(id.to_string());
        if let Some(task) = tasks.get(id) {
            for dep in &task.depends_on {
                visit(dep, tasks, colors, path)?;
            }
        }
        path.pop();
        colors.insert(id.to_string(), Color::Black);
        Ok(())
    }

    let mut colors: BTreeMap<String, Color> =
        tasks.keys().map(|k| (k.clone(), Color::White)).collect();
    for id in tasks.keys() {
        let mut path = Vec::new();
        visit(id, tasks, &mut colors, &mut path)?;
    }
    Ok(())
}

// ─── Shared-workdir concurrency warning (#1341) ────────────────────────

/// Warn (never reject — "concurrency with responsibility": the system
/// informs, it doesn't block) when two Tasks share a non-empty `workdir`
/// and are NOT dependency-related to each other (directly or transitively,
/// in EITHER direction) — meaning they could run CONCURRENTLY against the
/// same workspace (e.g. two coder dispatches both pointed at the same git
/// worktree). Loud, named, surfaced in `SchedulerReport.warnings`; never
/// blocks the run.
pub fn shared_workdir_warnings(tasks: &BTreeMap<String, Task>) -> Vec<String> {
    let mut warnings = Vec::new();
    let ids: Vec<&String> = tasks.keys().collect();
    for i in 0..ids.len() {
        for other in &ids[i + 1..] {
            let a = &tasks[ids[i]];
            let b = &tasks[*other];
            let (Some(wa), Some(wb)) = (&a.workdir, &b.workdir) else { continue };
            if wa != wb {
                continue;
            }
            if task_depends_transitively(a, &b.id, tasks) || task_depends_transitively(b, &a.id, tasks) {
                continue; // one depends on the other — ordered, never concurrent
            }
            warnings.push(format!(
                "task `{}` and task `{}` share workdir `{}` and have no dependency relationship \
                 between them — they could run concurrently against the same workspace",
                a.id,
                b.id,
                wa.display()
            ));
        }
    }
    warnings
}

/// `true` iff `from` depends on `target`, directly or transitively, via
/// `Task.depends_on` edges.
fn task_depends_transitively(from: &Task, target: &str, tasks: &BTreeMap<String, Task>) -> bool {
    let mut stack: Vec<&str> = from.depends_on.iter().map(String::as_str).collect();
    let mut seen: HashSet<&str> = HashSet::new();
    while let Some(id) = stack.pop() {
        if id == target {
            return true;
        }
        if !seen.insert(id) {
            continue;
        }
        if let Some(t) = tasks.get(id) {
            stack.extend(t.depends_on.iter().map(String::as_str));
        }
    }
    false
}

// ─── Task-derived status + Step readiness (#1341) ──────────────────────

/// A Task's status is DERIVED from its steps, never a stored field:
/// `Error` if any step errored, `Abandoned` if any step is abandoned (and
/// none errored), `Complete` iff every step is `Complete` (and it has at
/// least one), `Running` if any step is running, else `Planned`.
/// Recomputed fresh every readiness pass — a Task never goes stale.
fn task_status(task: &Task, steps: &BTreeMap<String, Step>) -> NodeStatus {
    let statuses: Vec<NodeStatus> =
        task.step_ids.iter().filter_map(|id| steps.get(id)).map(|s| s.status).collect();
    if statuses.contains(&NodeStatus::Error) {
        NodeStatus::Error
    } else if statuses.contains(&NodeStatus::Abandoned) {
        NodeStatus::Abandoned
    } else if !statuses.is_empty() && statuses.iter().all(|s| *s == NodeStatus::Complete) {
        NodeStatus::Complete
    } else if statuses.contains(&NodeStatus::Running) {
        NodeStatus::Running
    } else {
        NodeStatus::Planned
    }
}

/// `true` iff `step` is ready to run: itself `Planned`, AND —
/// - if it's the FIRST step of `task` (or `task.step_ids` doesn't list it
///   at all — defensive): every Task named in `task.depends_on` has
///   `task_status(..) == Complete`;
/// - otherwise (a later step in a multi-step Task): the step immediately
///   before it in `task.step_ids` is `Complete`.
///
/// A Task whose dependency chain includes a dead (`Error`/`Abandoned`)
/// ancestor never satisfies the first branch — its steps simply never
/// become ready, the same "stays `Planned` forever" terminal shape the
/// pre-#1341 `reachable`-gated design had, now emerging naturally from
/// this fixed-point check rather than a separate reachability pre-pass.
fn step_is_ready(
    step: &Step,
    task: &Task,
    tasks: &BTreeMap<String, Task>,
    steps: &BTreeMap<String, Step>,
) -> bool {
    if step.status != NodeStatus::Planned {
        return false;
    }
    match task.step_ids.iter().position(|id| id == &step.id) {
        Some(i) if i > 0 => steps
            .get(&task.step_ids[i - 1])
            .map(|s| s.status == NodeStatus::Complete)
            .unwrap_or(false),
        _ => task
            .depends_on
            .iter()
            .all(|dep_id| tasks.get(dep_id).map(|t| task_status(t, steps) == NodeStatus::Complete).unwrap_or(false)),
    }
}

// ─── Input gathering (#1341 — Task-aware) ──────────────────────────────

/// The `input` map `step`'s job should receive:
/// - If `step` is the FIRST step of `task`: one entry per `task.depends_on`
///   Task id whose LAST step is `Complete` and has recorded `output`,
///   keyed by that dependency TASK's id (#1341 — Task is the
///   dependency-declaring unit now; see `Task::depends_on`'s doc for the
///   "only the first step receives upstream Task output" design choice).
/// - Otherwise (a later step in a multi-step Task): exactly one entry —
///   the immediately-previous SAME-TASK step's `output` (if `Complete`),
///   keyed by that step's id.
///
/// A dependency that's `Complete` but has no recorded `output` (a step
/// kind that legitimately produces none) is omitted, not stubbed with an
/// empty string.
pub fn gather_inputs(
    step: &Step,
    task: &Task,
    tasks: &BTreeMap<String, Task>,
    steps: &BTreeMap<String, Step>,
) -> BTreeMap<String, String> {
    match task.step_ids.iter().position(|id| id == &step.id) {
        Some(i) if i > 0 => {
            let prev_id = &task.step_ids[i - 1];
            steps
                .get(prev_id)
                .filter(|s| s.status == NodeStatus::Complete)
                .and_then(|s| s.output.clone())
                .map(|output| [(prev_id.clone(), output)].into_iter().collect())
                .unwrap_or_default()
        }
        _ => task
            .depends_on
            .iter()
            .filter_map(|dep_task_id| {
                let dep_task = tasks.get(dep_task_id)?;
                let last_step_id = dep_task.step_ids.last()?;
                let last_step = steps.get(last_step_id)?;
                if last_step.status != NodeStatus::Complete {
                    return None;
                }
                last_step.output.clone().map(|output| (dep_task_id.clone(), output))
            })
            .collect(),
    }
}

// ─── The scheduler loop ─────────────────────────────────────────────────

/// Summary of one `run_step_graph` call: which steps completed, which
/// errored, and how many wave iterations it took. Steps left `Planned`
/// at the end (possible only if their owning Task's dependency chain
/// includes a dead — `Error`/`Abandoned` — Task) are NOT listed in either
/// `completed` or `errored`; the caller can find them by scanning `steps`
/// for lingering `NodeStatus::Planned` after the call returns. `warnings`
/// carries non-fatal graph-shape findings computed up front (today: only
/// `shared_workdir_warnings` — #1341) — "concurrency with responsibility":
/// surfaced loud, never blocking.
#[derive(Debug, Default, Clone)]
pub struct SchedulerReport {
    pub completed: Vec<String>,
    pub errored: Vec<String>,
    pub iterations: usize,
    pub warnings: Vec<String>,
}

/// Walk `steps` to completion: each iteration computes every currently-
/// ready node, marks them `Running`, fans them out through Packet 1's
/// `run_bounded` (one call = one wave — see the module doc's Residency
/// section for why every job here is `Residency::Remote`), flushes each
/// job's `StepOutcome` onto its Step (status + `output` + timestamps),
/// emits step-lifecycle bookend records through `emit`, and recomputes
/// readiness. Stops when nothing is ready (either the graph finished, or
/// every remaining `Planned` step's owning Task depends, directly or
/// transitively, on a dead Task — see `step_is_ready`).
///
/// `tasks` is the FULL Task map (#1341 — dependency/concurrency/data-flow
/// all live at Task level now; see `Task`'s doc): readiness, cycle
/// detection, and `input` gathering all resolve through it. A Step whose
/// `task_id` has no entry in `tasks` (a caller that never registered one —
/// e.g. a scheduler-level test exercising pure Step scheduling with no
/// Task-assignment concerns) falls back to a synthetic single-step Task
/// with no dependencies (always immediately ready) rather than erroring —
/// a SCHEDULING CONVENIENCE for Task-agnostic callers, not license to skip
/// building real Tasks in production; every production caller in this
/// codebase always builds a real, persisted Task per Step.
///
/// Rejects a cyclic Task graph up front via `detect_cycles` rather than
/// looping forever on a Task that can never become ready.
pub fn run_step_graph(
    steps: &mut BTreeMap<String, Step>,
    tasks: &BTreeMap<String, Task>,
    kinds: &StepKindRegistry,
    facts: &Facts,
    est: &dyn FootprintEstimator,
    remote_cap: usize,
    emit: &mut dyn FnMut(FlowRecord),
) -> Result<SchedulerReport> {
    detect_cycles(tasks)?;

    let mut report = SchedulerReport {
        warnings: shared_workdir_warnings(tasks),
        ..Default::default()
    };

    loop {
        let ready_ids: Vec<String> = steps
            .values()
            .filter(|s| {
                let task = tasks.get(&s.task_id).cloned().unwrap_or_else(|| synthetic_task(s));
                step_is_ready(s, &task, tasks, steps)
            })
            .map(|s| s.id.clone())
            .collect();

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
        // recorded — `gather_inputs` needs `&steps`/`&tasks` (completed
        // sibling/upstream outputs), and the job closures below need owned
        // snapshots ('static, per `run_bounded`'s `Send + 'static` job
        // contract).
        for (idx, id) in ready_ids.iter().enumerate() {
            let step_snapshot = steps.get(id).expect("just set to Running above").clone();
            let task_snapshot = tasks
                .get(&step_snapshot.task_id)
                .cloned()
                .unwrap_or_else(|| synthetic_task(&step_snapshot));
            let input = gather_inputs(&step_snapshot, &task_snapshot, tasks, steps);
            let kind = kinds
                .get(&step_snapshot.kind)
                .with_context_step(&step_snapshot)?;
            // (#1230 Packet 3) Per-step residency classification — see the
            // trait doc on `StepKind::residency` and the module doc above.
            // Best-effort: `None` (every kind's behavior before this hook
            // existed, and every non-dispatch kind today) schedules Remote.
            let residency = match kind.residency(&step_snapshot, &task_snapshot) {
                Some(placement) => crate::concurrent_dispatch::Residency::Local(placement),
                None => crate::concurrent_dispatch::Residency::Remote,
            };
            let job: crate::concurrent_dispatch::DispatchJob<StepJobResult> =
                Box::new(move || {
                    let outcome = kind.run(&step_snapshot, &task_snapshot, &input)?;
                    Ok((
                        StepJobResult {
                            output: outcome.output,
                        },
                        outcome.flow_records,
                    ))
                });
            jobs.push(crate::concurrent_dispatch::QueuedJob {
                index: idx,
                residency,
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

/// The synthetic empty `Task` a Step resolves to when the caller's `tasks`
/// map has no entry for `step.task_id` — see `run_step_graph`'s doc for why
/// this falls back rather than erroring (a scheduling convenience for
/// Task-assignment-agnostic callers, not a production shape).
fn synthetic_task(step: &Step) -> Task {
    Task {
        id: step.task_id.clone(),
        phase_id: String::new(),
        description: String::new(),
        step_ids: vec![step.id.clone()],
        depends_on: Vec::new(),
        role_id: None,
        profile_name: None,
        workdir: None,
        image: None,
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
/// `emit_phase_transition_record` shape (`Category::Work`,
/// `Tier::Local` since these are scheduler-driven, not operator-explicit
/// like a Phase transition; `Stage::Dispatch` since a Step is
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
        phase_id: None,
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

    // ─── fixtures (#1341 Task-level model) ─────────────────────────────

    /// A single-step Task: Task id `<id>`, its one Step id `<id>-step`,
    /// `Task.depends_on` set from `deps` (other TASK ids). The overwhelming
    /// majority of this codebase's real Tasks are exactly this shape.
    fn task_and_step(id: &str, deps: &[&str]) -> (Task, Step) {
        let step_id = format!("{id}-step");
        let task = Task {
            id: id.to_string(),
            phase_id: "p1".to_string(),
            description: format!("task {id}"),
            step_ids: vec![step_id.clone()],
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let step = Step {
            id: step_id,
            task_id: id.to_string(),
            kind: "procedural.noop".to_string(),
            status: NodeStatus::Planned,
            config: json!(null),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        (task, step)
    }

    fn step_with_status(id: &str, deps: &[&str], status: NodeStatus) -> (Task, Step) {
        let (task, mut step) = task_and_step(id, deps);
        step.status = status;
        (task, step)
    }

    fn graph(pairs: Vec<(Task, Step)>) -> (BTreeMap<String, Task>, BTreeMap<String, Step>) {
        let mut tasks = BTreeMap::new();
        let mut steps = BTreeMap::new();
        for (t, s) in pairs {
            steps.insert(s.id.clone(), s);
            tasks.insert(t.id.clone(), t);
        }
        (tasks, steps)
    }

    // ─── task_status ────────────────────────────────────────────────

    #[test]
    fn task_status_planned_with_no_steps_run() {
        let (task, step) = task_and_step("a", &[]);
        let steps: BTreeMap<String, Step> = [(step.id.clone(), step)].into_iter().collect();
        assert_eq!(task_status(&task, &steps), NodeStatus::Planned);
    }

    #[test]
    fn task_status_complete_when_every_step_complete() {
        let (task, mut step) = task_and_step("a", &[]);
        step.status = NodeStatus::Complete;
        let steps: BTreeMap<String, Step> = [(step.id.clone(), step)].into_iter().collect();
        assert_eq!(task_status(&task, &steps), NodeStatus::Complete);
    }

    #[test]
    fn task_status_error_if_any_step_errored() {
        let (task, mut step) = task_and_step("a", &[]);
        step.status = NodeStatus::Error;
        let steps: BTreeMap<String, Step> = [(step.id.clone(), step)].into_iter().collect();
        assert_eq!(task_status(&task, &steps), NodeStatus::Error);
    }

    #[test]
    fn task_status_running_if_any_step_running_and_none_dead() {
        let (task, mut step) = task_and_step("a", &[]);
        step.status = NodeStatus::Running;
        let steps: BTreeMap<String, Step> = [(step.id.clone(), step)].into_iter().collect();
        assert_eq!(task_status(&task, &steps), NodeStatus::Running);
    }

    // ─── step_is_ready ──────────────────────────────────────────────

    #[test]
    fn step_is_ready_true_for_first_step_with_no_task_deps() {
        let (task, step) = task_and_step("a", &[]);
        let tasks: BTreeMap<String, Task> = [(task.id.clone(), task.clone())].into_iter().collect();
        let steps: BTreeMap<String, Step> = [(step.id.clone(), step.clone())].into_iter().collect();
        assert!(step_is_ready(&step, &task, &tasks, &steps));
    }

    #[test]
    fn step_is_ready_false_when_not_planned() {
        let (task, mut step) = task_and_step("a", &[]);
        step.status = NodeStatus::Running;
        let tasks: BTreeMap<String, Task> = [(task.id.clone(), task.clone())].into_iter().collect();
        let steps: BTreeMap<String, Step> = [(step.id.clone(), step.clone())].into_iter().collect();
        assert!(!step_is_ready(&step, &task, &tasks, &steps));
    }

    #[test]
    fn step_is_ready_false_when_task_dependency_incomplete() {
        let (task_a, step_a) = task_and_step("a", &[]);
        let (task_b, step_b) = task_and_step("b", &["a"]);
        let (tasks, steps) = graph(vec![(task_a, step_a), (task_b.clone(), step_b.clone())]);
        assert!(!step_is_ready(&step_b, &task_b, &tasks, &steps));
    }

    #[test]
    fn step_is_ready_true_when_task_dependency_complete() {
        let (task_a, step_a) = step_with_status("a", &[], NodeStatus::Complete);
        let (task_b, step_b) = task_and_step("b", &["a"]);
        let (tasks, steps) = graph(vec![(task_a, step_a), (task_b.clone(), step_b.clone())]);
        assert!(step_is_ready(&step_b, &task_b, &tasks, &steps));
    }

    #[test]
    fn step_is_ready_false_on_dangling_task_dependency() {
        let (task_b, step_b) = task_and_step("b", &["ghost"]);
        let tasks: BTreeMap<String, Task> = [(task_b.id.clone(), task_b.clone())].into_iter().collect();
        let steps: BTreeMap<String, Step> = [(step_b.id.clone(), step_b.clone())].into_iter().collect();
        assert!(!step_is_ready(&step_b, &task_b, &tasks, &steps), "a dangling task dep must fail closed");
    }

    #[test]
    fn step_is_ready_later_step_needs_only_immediately_previous_same_task_step() {
        // A two-step Task: step-0 -> step-1, positional order, no
        // `Task.depends_on` involved at all for the intra-task edge.
        let task = Task {
            id: "multi".to_string(),
            phase_id: "p1".to_string(),
            description: "multi-step task".to_string(),
            step_ids: vec!["multi-0".to_string(), "multi-1".to_string()],
            depends_on: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let step0 = Step {
            id: "multi-0".to_string(),
            task_id: "multi".to_string(),
            kind: "procedural.noop".to_string(),
            status: NodeStatus::Complete,
            config: json!(null),
            started_ts: None,
            completed_ts: None,
            output: Some("step0 out".to_string()),
        };
        let step1 = Step {
            id: "multi-1".to_string(),
            task_id: "multi".to_string(),
            kind: "procedural.noop".to_string(),
            status: NodeStatus::Planned,
            config: json!(null),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let tasks: BTreeMap<String, Task> = [("multi".to_string(), task.clone())].into_iter().collect();
        let steps: BTreeMap<String, Step> =
            [("multi-0".to_string(), step0), ("multi-1".to_string(), step1.clone())].into_iter().collect();
        assert!(step_is_ready(&step1, &task, &tasks, &steps));
    }

    // ─── gather_inputs ──────────────────────────────────────────────

    #[test]
    fn gather_inputs_first_step_keys_by_dependency_task_id() {
        let (task_a, mut step_a) = task_and_step("a", &[]);
        step_a.status = NodeStatus::Complete;
        step_a.output = Some("a's output".to_string());
        let (task_b, step_b) = task_and_step("b", &["a"]);
        let (tasks, steps) = graph(vec![(task_a, step_a), (task_b.clone(), step_b.clone())]);
        let input = gather_inputs(&step_b, &task_b, &tasks, &steps);
        assert_eq!(input.get("a").map(String::as_str), Some("a's output"));
    }

    #[test]
    fn gather_inputs_omits_incomplete_or_outputless_dependency() {
        let (task_a, step_a) = task_and_step("a", &[]); // still Planned
        let (task_b, step_b) = task_and_step("b", &["a"]);
        let (tasks, steps) = graph(vec![(task_a, step_a), (task_b.clone(), step_b.clone())]);
        let input = gather_inputs(&step_b, &task_b, &tasks, &steps);
        assert!(input.is_empty());
    }

    #[test]
    fn gather_inputs_later_step_keys_by_previous_same_task_step_id() {
        let task = Task {
            id: "multi".to_string(),
            phase_id: "p1".to_string(),
            description: "d".to_string(),
            step_ids: vec!["multi-0".to_string(), "multi-1".to_string()],
            depends_on: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let step0 = Step {
            id: "multi-0".to_string(),
            task_id: "multi".to_string(),
            kind: "procedural.noop".to_string(),
            status: NodeStatus::Complete,
            config: json!(null),
            started_ts: None,
            completed_ts: None,
            output: Some("step0 out".to_string()),
        };
        let step1 = Step {
            id: "multi-1".to_string(),
            task_id: "multi".to_string(),
            kind: "procedural.noop".to_string(),
            status: NodeStatus::Planned,
            config: json!(null),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let tasks: BTreeMap<String, Task> = [("multi".to_string(), task.clone())].into_iter().collect();
        let steps: BTreeMap<String, Step> =
            [("multi-0".to_string(), step0), ("multi-1".to_string(), step1.clone())].into_iter().collect();
        let input = gather_inputs(&step1, &task, &tasks, &steps);
        assert_eq!(input.get("multi-0").map(String::as_str), Some("step0 out"));
    }

    // ─── detect_cycles (Task-level, #1341) ──────────────────────────

    #[test]
    fn detect_cycles_ok_on_acyclic_task_graph() {
        let (task_a, _) = task_and_step("a", &[]);
        let (task_b, _) = task_and_step("b", &["a"]);
        let tasks: BTreeMap<String, Task> =
            [(task_a.id.clone(), task_a), (task_b.id.clone(), task_b)].into_iter().collect();
        assert!(detect_cycles(&tasks).is_ok());
    }

    #[test]
    fn detect_cycles_rejects_direct_task_cycle() {
        let (task_a, _) = task_and_step("a", &["b"]);
        let (task_b, _) = task_and_step("b", &["a"]);
        let tasks: BTreeMap<String, Task> =
            [(task_a.id.clone(), task_a), (task_b.id.clone(), task_b)].into_iter().collect();
        let err = detect_cycles(&tasks).unwrap_err();
        assert!(err.to_string().contains("cycle detected"), "{err}");
    }

    #[test]
    fn detect_cycles_rejects_transitive_task_cycle() {
        let (task_a, _) = task_and_step("a", &["c"]);
        let (task_b, _) = task_and_step("b", &["a"]);
        let (task_c, _) = task_and_step("c", &["b"]);
        let tasks: BTreeMap<String, Task> = [
            (task_a.id.clone(), task_a),
            (task_b.id.clone(), task_b),
            (task_c.id.clone(), task_c),
        ]
        .into_iter()
        .collect();
        let err = detect_cycles(&tasks).unwrap_err();
        assert!(err.to_string().contains("cycle detected"), "{err}");
    }

    #[test]
    fn detect_cycles_self_dependency_is_a_cycle() {
        let (task_a, _) = task_and_step("a", &["a"]);
        let tasks: BTreeMap<String, Task> = [(task_a.id.clone(), task_a)].into_iter().collect();
        assert!(detect_cycles(&tasks).is_err());
    }

    // ─── shared_workdir_warnings (#1341) ────────────────────────────

    #[test]
    fn shared_workdir_warnings_flags_unrelated_tasks_sharing_a_workdir() {
        let (mut task_a, step_a) = task_and_step("a", &[]);
        let (mut task_b, step_b) = task_and_step("b", &[]); // no dependency edge
        task_a.workdir = Some(std::path::PathBuf::from("/tmp/wt"));
        task_b.workdir = Some(std::path::PathBuf::from("/tmp/wt"));
        let (tasks, _steps) = graph(vec![(task_a, step_a), (task_b, step_b)]);
        let warnings = shared_workdir_warnings(&tasks);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("task `a`") && warnings[0].contains("task `b`"), "{warnings:?}");
    }

    #[test]
    fn shared_workdir_warnings_silent_when_tasks_are_dependency_related() {
        let (mut task_a, step_a) = task_and_step("a", &[]);
        let (mut task_b, step_b) = task_and_step("b", &["a"]); // b depends on a — ordered
        task_a.workdir = Some(std::path::PathBuf::from("/tmp/wt"));
        task_b.workdir = Some(std::path::PathBuf::from("/tmp/wt"));
        let (tasks, _steps) = graph(vec![(task_a, step_a), (task_b, step_b)]);
        assert!(shared_workdir_warnings(&tasks).is_empty());
    }

    #[test]
    fn shared_workdir_warnings_silent_when_workdirs_differ() {
        let (mut task_a, step_a) = task_and_step("a", &[]);
        let (mut task_b, step_b) = task_and_step("b", &[]);
        task_a.workdir = Some(std::path::PathBuf::from("/tmp/wt-a"));
        task_b.workdir = Some(std::path::PathBuf::from("/tmp/wt-b"));
        let (tasks, _steps) = graph(vec![(task_a, step_a), (task_b, step_b)]);
        assert!(shared_workdir_warnings(&tasks).is_empty());
    }

    // ─── run_step_graph (integration, via procedural.noop) ────────────

    fn run_test_graph(
        tasks: &BTreeMap<String, Task>,
        steps: &mut BTreeMap<String, Step>,
    ) -> SchedulerReport {
        let kinds = StepKindRegistry::with_builtins();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let mut emitted = Vec::new();
        run_step_graph(steps, tasks, &kinds, &facts, &est, 8, &mut |r| emitted.push(r)).unwrap()
    }

    #[test]
    fn run_step_graph_respects_topological_ordering_linear_task_chain() {
        let (task_a, step_a) = task_and_step("a", &[]);
        let (task_b, step_b) = task_and_step("b", &["a"]);
        let (task_c, step_c) = task_and_step("c", &["b"]);
        let (tasks, mut steps) = graph(vec![(task_a, step_a), (task_b, step_b), (task_c, step_c)]);

        let report = run_test_graph(&tasks, &mut steps);

        assert_eq!(report.completed.len(), 3);
        assert_eq!(report.errored.len(), 0);
        for id in ["a-step", "b-step", "c-step"] {
            assert_eq!(steps[id].status, NodeStatus::Complete, "{id} should be Complete");
        }
        let a_done = steps["a-step"].completed_ts.unwrap();
        let b_start = steps["b-step"].started_ts.unwrap();
        let b_done = steps["b-step"].completed_ts.unwrap();
        let c_start = steps["c-step"].started_ts.unwrap();
        assert!(a_done <= b_start, "b must not start before a completes");
        assert!(b_done <= c_start, "c must not start before b completes");
    }

    /// (#1230 Packet 2 acceptance, revised #1341 for Task-level deps)
    /// Diamond shape: A→B, A→C, B and C both →D — now expressed as
    /// `Task.depends_on` edges. B and C must both complete before D
    /// becomes ready — and, since they're scheduled in the SAME wave
    /// (both ready at once after A completes), they run concurrently via
    /// Packet 1's `run_bounded`.
    #[test]
    fn run_step_graph_diamond_runs_b_and_c_concurrently_then_d() {
        let (task_a, step_a) = task_and_step("a", &[]);
        let (task_b, step_b) = task_and_step("b", &["a"]);
        let (task_c, step_c) = task_and_step("c", &["a"]);
        let (task_d, step_d) = task_and_step("d", &["b", "c"]);
        let (tasks, mut steps) =
            graph(vec![(task_a, step_a), (task_b, step_b), (task_c, step_c), (task_d, step_d)]);

        let report = run_test_graph(&tasks, &mut steps);

        assert_eq!(report.completed.len(), 4);
        for id in ["a-step", "b-step", "c-step", "d-step"] {
            assert_eq!(steps[id].status, NodeStatus::Complete, "{id} should be Complete");
        }
        let b_done = steps["b-step"].completed_ts.unwrap();
        let c_done = steps["c-step"].completed_ts.unwrap();
        let d_start = steps["d-step"].started_ts.unwrap();
        assert!(b_done <= d_start && c_done <= d_start);
        assert_eq!(report.iterations, 3, "A, then B+C together, then D");
    }

    #[test]
    fn run_step_graph_reports_errored_step_and_still_completes_independent_task() {
        let (task_fails, mut step_fails) = task_and_step("fails", &[]);
        step_fails.kind = "procedural.shell".to_string();
        step_fails.config = json!({"command": "exit 1"});
        let (task_ind, step_ind) = task_and_step("independent", &[]);
        let (tasks, mut steps) = graph(vec![(task_fails, step_fails), (task_ind, step_ind)]);

        let report = run_test_graph(&tasks, &mut steps);

        assert_eq!(steps["fails-step"].status, NodeStatus::Error);
        assert_eq!(steps["independent-step"].status, NodeStatus::Complete);
        assert_eq!(report.errored, vec!["fails-step".to_string()]);
        assert!(report.completed.contains(&"independent-step".to_string()));
    }

    #[test]
    fn run_step_graph_downstream_task_of_errored_task_never_runs_and_stays_planned() {
        let (task_fails, mut step_fails) = task_and_step("fails", &[]);
        step_fails.kind = "procedural.shell".to_string();
        step_fails.config = json!({"command": "exit 1"});
        let (task_down, step_down) = task_and_step("downstream", &["fails"]);
        let (tasks, mut steps) = graph(vec![(task_fails, step_fails), (task_down, step_down)]);

        let report = run_test_graph(&tasks, &mut steps);

        assert_eq!(steps["fails-step"].status, NodeStatus::Error);
        assert_eq!(
            steps["downstream-step"].status,
            NodeStatus::Planned,
            "downstream of an errored task dependency never becomes ready"
        );
        assert!(!report.completed.contains(&"downstream-step".to_string()));
        assert!(!report.errored.contains(&"downstream-step".to_string()));
    }

    #[test]
    fn run_step_graph_rejects_cyclic_task_graph_before_running_anything() {
        let (task_a, step_a) = task_and_step("a", &["b"]);
        let (task_b, step_b) = task_and_step("b", &["a"]);
        let (tasks, mut steps) = graph(vec![(task_a, step_a), (task_b, step_b)]);

        let kinds = StepKindRegistry::with_builtins();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let mut emitted = Vec::new();
        let err = run_step_graph(&mut steps, &tasks, &kinds, &facts, &est, 8, &mut |r| emitted.push(r))
            .unwrap_err();
        assert!(err.to_string().contains("cycle detected"));
        assert!(emitted.is_empty(), "no step-lifecycle records before the cycle check fires");
        for step in steps.values() {
            assert_eq!(step.status, NodeStatus::Planned, "nothing should have run");
        }
    }

    #[test]
    fn run_step_graph_emits_step_start_and_step_complete_records() {
        let (task_a, step_a) = task_and_step("a", &[]);
        let (tasks, mut steps) = graph(vec![(task_a, step_a)]);
        let kinds = StepKindRegistry::with_builtins();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let mut emitted: Vec<FlowRecord> = Vec::new();
        run_step_graph(&mut steps, &tasks, &kinds, &facts, &est, 8, &mut |r| emitted.push(r)).unwrap();

        let actions: Vec<&str> = emitted.iter().map(|r| r.action.as_str()).collect();
        assert!(actions.contains(&"step start"));
        assert!(actions.contains(&"step complete"));
    }

    #[test]
    fn run_step_graph_surfaces_shared_workdir_warning_without_blocking() {
        let (mut task_a, step_a) = task_and_step("a", &[]);
        let (mut task_b, step_b) = task_and_step("b", &[]);
        task_a.workdir = Some(std::path::PathBuf::from("/tmp/wt"));
        task_b.workdir = Some(std::path::PathBuf::from("/tmp/wt"));
        let (tasks, mut steps) = graph(vec![(task_a, step_a), (task_b, step_b)]);

        let report = run_test_graph(&tasks, &mut steps);

        assert_eq!(report.completed.len(), 2, "the warning never blocks the run");
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    }
}
