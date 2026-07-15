//! Mission graph builder for `GET /mission/:id/graph.json` (#1284 Packet 5).
//!
//! Reads the persisted Phase/Task/Step graph for one mission (the JSON
//! source-of-truth under `~/.darkmux/missions/<id>/`, via
//! `darkmux_crew::loader`/`lifecycle`) and shapes it into a node-link
//! diagram the mission-graph page's vendored React Flow renders.
//!
//! **Live status is NOT this module's job.** This builds the INITIAL
//! snapshot only; the page's own SSE subscription (`/flow/:date/stream`)
//! layers status deltas on top client-side by matching a step-lifecycle
//! record's `handle` field against a node id already present in this
//! snapshot (see `assets/mission-graph.html`'s `applyFlowRecord`). No
//! flow-record read happens here.
//!
//! **Schema note (deviation from the original #1284 design comment):**
//! post-#1341, `Step` carries no `depends_on` of its own — ALL real
//! dependency/concurrency semantics live at `Task::depends_on` (see that
//! field's doc in `darkmux_crew::types`). This module derives step-level
//! `depends_on` EDGES for the diagram (so a fan-in like N probe steps
//! feeding one dedup step stays visually legible) from the OWNING tasks'
//! `depends_on`: an edge runs from an upstream task's LAST step to a
//! downstream task's FIRST step, mirroring `Step::output`'s own doc
//! ("an upstream Task's output reaches ONLY the downstream Task's FIRST
//! step"). The edges are diagram-only — never fed back into the
//! scheduler.

use darkmux_crew::types::{Mission, MissionStatus, NodeStatus, Phase, PhaseStatus, Step, Task};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// One node in the rendered graph — a Phase, a Task, or a Step.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GraphNode {
    pub id: String,
    pub label: String,
    pub kind: &'static str,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_ts: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_ts: Option<u64>,
    /// Layering depth for layout (0 = a root with no known upstream
    /// dependency). Phase nodes use their position in
    /// `Mission::phase_ids`; task/step nodes use the task-dependency
    /// longest-path depth computed by [`layer_tasks_by_depth`].
    /// Diagram-only, never scheduler-authoritative — see that function's
    /// doc for the cycle/dangling-reference fallback.
    pub depth: usize,
}

/// One edge in the rendered graph.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GraphEdge {
    pub id: String,
    pub source: String,
    pub target: String,
    /// `"contains"` (phase→task, task→step) or `"depends_on"` (a real
    /// scheduler dependency, derived to step granularity — see module doc).
    pub kind: &'static str,
}

/// The full graph payload for one mission.
#[derive(Debug, Clone, Serialize)]
pub struct MissionGraph {
    pub mission_id: String,
    pub mission_status: &'static str,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// `true` when the mission has phase data but NO task/step graph
    /// underneath any phase — a legacy pre-registry instance (#1284
    /// Packet 4a's `mission migrate` target) or a freeform hand-authored
    /// mission with step-less phases. The page renders a phases-only
    /// graph with a note instead of an error.
    pub legacy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub generated_at_ms: u64,
}

fn mission_status_str(s: MissionStatus) -> &'static str {
    match s {
        MissionStatus::Active => "active",
        MissionStatus::Closed => "closed",
        MissionStatus::Paused => "paused",
    }
}

fn phase_status_str(s: PhaseStatus) -> &'static str {
    match s {
        PhaseStatus::Planned => "planned",
        PhaseStatus::Running => "running",
        PhaseStatus::Complete => "complete",
        PhaseStatus::Abandoned => "abandoned",
    }
}

fn node_status_str(s: NodeStatus) -> &'static str {
    match s {
        NodeStatus::Planned => "planned",
        NodeStatus::Running => "running",
        NodeStatus::Complete => "complete",
        NodeStatus::Abandoned => "abandoned",
        NodeStatus::Error => "error",
    }
}

/// Derive a Task's status from its Steps — `Task` carries no `status`
/// field of its own (#1230/#1341: only `Step` is scheduler-driven).
/// Priority: any `Error` wins (a task with one failed step is a failed
/// task); else any `Running` wins; else all-`Complete` (and non-empty)
/// is `Complete`; else any `Abandoned` is `Abandoned`; else `Planned`
/// (covers "no steps yet" and "all still Planned").
fn derive_task_status(steps: &[&Step]) -> NodeStatus {
    if steps.iter().any(|s| s.status == NodeStatus::Error) {
        return NodeStatus::Error;
    }
    if steps.iter().any(|s| s.status == NodeStatus::Running) {
        return NodeStatus::Running;
    }
    if !steps.is_empty() && steps.iter().all(|s| s.status == NodeStatus::Complete) {
        return NodeStatus::Complete;
    }
    if steps.iter().any(|s| s.status == NodeStatus::Abandoned) {
        return NodeStatus::Abandoned;
    }
    NodeStatus::Planned
}

/// Topological longest-path depth over a set of Tasks connected by
/// `Task::depends_on`, for DIAGRAM LAYERING ONLY — never scheduler-
/// authoritative (that's `scheduler::detect_cycles` + the real readiness
/// walk). A task's depth is `1 + max(depth of its dependencies present in
/// `tasks`)`, or `0` if it has none. Two defensive fallbacks so a
/// malformed or partial graph never panics or hangs the diagram:
///
/// - a dependency id not present in `tasks` (cross-mission reference, or
///   a task from a phase this call wasn't given) contributes nothing —
///   treated as if the edge didn't exist for layering purposes;
/// - a task caught in a dependency cycle (which `scheduler::detect_cycles`
///   would reject before ever running the graph, but this function makes
///   no such assumption about its input) gets depth `0` rather than
///   infinite recursion, via a visiting-set guard.
pub fn layer_tasks_by_depth(tasks: &[Task]) -> BTreeMap<String, usize> {
    let by_id: BTreeMap<&str, &Task> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();
    let cyclic = cyclic_task_ids(tasks, &by_id);
    let mut memo: BTreeMap<String, usize> = BTreeMap::new();

    fn visit(
        id: &str,
        by_id: &BTreeMap<&str, &Task>,
        cyclic: &BTreeSet<String>,
        memo: &mut BTreeMap<String, usize>,
    ) -> usize {
        if let Some(d) = memo.get(id) {
            return *d;
        }
        let Some(task) = by_id.get(id) else {
            return 0;
        };
        // Every edge into a cyclic node is pre-filtered out (see
        // `cyclic_task_ids`), so this recursion can never re-enter a node
        // it's still computing — no separate "currently visiting" guard is
        // needed; the pre-pass already broke every cycle.
        let depth = task
            .depends_on
            .iter()
            .filter(|dep| by_id.contains_key(dep.as_str()) && !cyclic.contains(dep.as_str()))
            .map(|dep| 1 + visit(dep, by_id, cyclic, memo))
            .max()
            .unwrap_or(0);
        memo.insert(id.to_string(), depth);
        depth
    }

    for task in tasks {
        visit(&task.id, &by_id, &cyclic, &mut memo);
    }
    memo
}

/// Every task id that participates in at least one `Task::depends_on`
/// cycle, via a coloring DFS (mirrors `scheduler::detect_cycles`'s
/// approach, but COLLECTS the cycle membership instead of erroring — this
/// function is diagram-layout support, not the scheduler's own load-time
/// guard). [`layer_tasks_by_depth`] treats every edge INTO a cyclic node as
/// nonexistent, the same treatment a dangling reference already gets,
/// which keeps the depth DFS itself simple (no separate recursion guard)
/// and gives every node in a cycle a deterministic depth of `0` rather than
/// an order-dependent value that would differ depending on which node in
/// the cycle happened to be visited first.
fn cyclic_task_ids(tasks: &[Task], by_id: &BTreeMap<&str, &Task>) -> BTreeSet<String> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Color {
        Gray,
        Black,
    }

    fn visit(
        id: &str,
        by_id: &BTreeMap<&str, &Task>,
        colors: &mut BTreeMap<String, Color>,
        path: &mut Vec<String>,
        cyclic: &mut BTreeSet<String>,
    ) {
        match colors.get(id).copied() {
            Some(Color::Black) => return,
            Some(Color::Gray) => {
                // Found a back-edge to `id` — every node on the path from
                // `id`'s position to here (inclusive) is part of this cycle.
                if let Some(pos) = path.iter().position(|p| p == id) {
                    for n in &path[pos..] {
                        cyclic.insert(n.clone());
                    }
                }
                return;
            }
            // Absence from the map IS "white" (unvisited) — no separate
            // variant needed for a state that's only ever represented by a
            // missing map entry.
            None => {}
        }
        colors.insert(id.to_string(), Color::Gray);
        path.push(id.to_string());
        if let Some(task) = by_id.get(id) {
            for dep in &task.depends_on {
                if by_id.contains_key(dep.as_str()) {
                    visit(dep, by_id, colors, path, cyclic);
                }
            }
        }
        path.pop();
        colors.insert(id.to_string(), Color::Black);
    }

    let mut colors = BTreeMap::new();
    let mut path = Vec::new();
    let mut cyclic = BTreeSet::new();
    for task in tasks {
        visit(&task.id, by_id, &mut colors, &mut path, &mut cyclic);
    }
    cyclic
}

fn current_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Build the full node-link graph for one mission. `Ok(None)` when no
/// mission with this id exists (the route answers 404). Every filesystem
/// read is best-effort (`unwrap_or_default`) — a partially-written or
/// mid-migration mission degrades to whatever phases/tasks/steps DO parse
/// rather than 500ing the whole page.
pub fn build_mission_graph(mission_id: &str) -> anyhow::Result<Option<MissionGraph>> {
    let missions = darkmux_crew::loader::load_missions().unwrap_or_default();
    let Some(mission) = missions.into_iter().find(|m: &Mission| m.id == mission_id) else {
        return Ok(None);
    };

    let all_phases = darkmux_crew::loader::load_phases().unwrap_or_default();
    let mut phase_by_id: BTreeMap<String, Phase> = all_phases
        .into_iter()
        .filter(|p| p.mission_id == mission_id)
        .map(|p| (p.id.clone(), p))
        .collect();

    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    let mut any_tasks = false;

    // First pass: collect every Task across every phase (needed up front so
    // `layer_tasks_by_depth` sees the WHOLE mission's dependency graph —
    // a Task in a later phase may depend on one in an earlier phase, see
    // `Phase`'s own doc).
    let mut tasks_by_phase: BTreeMap<String, Vec<Task>> = BTreeMap::new();
    let mut steps_by_phase: BTreeMap<String, BTreeMap<String, Step>> = BTreeMap::new();
    let mut all_tasks: Vec<Task> = Vec::new();
    for phase_id in &mission.phase_ids {
        let tasks =
            darkmux_crew::lifecycle::load_tasks_for_phase(mission_id, phase_id).unwrap_or_default();
        let steps: BTreeMap<String, Step> =
            darkmux_crew::lifecycle::load_steps_for_phase(mission_id, phase_id)
                .unwrap_or_default()
                .into_iter()
                .map(|s| (s.id.clone(), s))
                .collect();
        if !tasks.is_empty() {
            any_tasks = true;
        }
        all_tasks.extend(tasks.iter().cloned());
        tasks_by_phase.insert(phase_id.clone(), tasks);
        steps_by_phase.insert(phase_id.clone(), steps);
    }
    let task_depth = layer_tasks_by_depth(&all_tasks);

    for (phase_index, phase_id) in mission.phase_ids.iter().enumerate() {
        let Some(phase) = phase_by_id.remove(phase_id) else {
            continue;
        };
        nodes.push(GraphNode {
            id: phase.id.clone(),
            label: phase.description.clone(),
            kind: "phase",
            status: phase_status_str(phase.status),
            parent_id: None,
            started_ts: phase.started_ts,
            completed_ts: phase.completed_ts,
            depth: phase_index,
        });

        let tasks = tasks_by_phase.remove(phase_id).unwrap_or_default();
        let steps = steps_by_phase.remove(phase_id).unwrap_or_default();

        for task in &tasks {
            edges.push(GraphEdge {
                id: format!("contains:{}:{}", phase.id, task.id),
                source: phase.id.clone(),
                target: task.id.clone(),
                kind: "contains",
            });

            let task_steps: Vec<&Step> = task
                .step_ids
                .iter()
                .filter_map(|sid| steps.get(sid))
                .collect();
            let status = derive_task_status(&task_steps);
            let started_ts = task_steps.iter().filter_map(|s| s.started_ts).min();
            let completed_ts = if status == NodeStatus::Complete {
                task_steps.iter().filter_map(|s| s.completed_ts).max()
            } else {
                None
            };
            nodes.push(GraphNode {
                id: task.id.clone(),
                label: task.description.clone(),
                kind: "task",
                status: node_status_str(status),
                parent_id: Some(phase.id.clone()),
                started_ts,
                completed_ts,
                depth: *task_depth.get(&task.id).unwrap_or(&0),
            });

            // Intra-task step containment + sequence edges (step at index i
            // depends on step at index i-1 — see `Step`'s doc; no
            // `Step::depends_on` field exists post-#1341).
            for (i, step_id) in task.step_ids.iter().enumerate() {
                let Some(step) = steps.get(step_id) else { continue };
                nodes.push(GraphNode {
                    id: step.id.clone(),
                    label: step.kind.clone(),
                    kind: "step",
                    status: node_status_str(step.status),
                    parent_id: Some(task.id.clone()),
                    started_ts: step.started_ts,
                    completed_ts: step.completed_ts,
                    depth: *task_depth.get(&task.id).unwrap_or(&0),
                });
                edges.push(GraphEdge {
                    id: format!("contains:{}:{}", task.id, step.id),
                    source: task.id.clone(),
                    target: step.id.clone(),
                    kind: "contains",
                });
                if i > 0 {
                    let prev = &task.step_ids[i - 1];
                    edges.push(GraphEdge {
                        id: format!("depends_on:{}:{}", prev, step.id),
                        source: prev.clone(),
                        target: step.id.clone(),
                        kind: "depends_on",
                    });
                }
            }

            // Cross-task dependency edges, at STEP granularity: an upstream
            // task's LAST step feeds a downstream task's FIRST step — this
            // is what keeps a fan-in (N probe steps -> one dedup step)
            // visually legible instead of collapsing to a single task-level
            // edge. A dependency naming a task with no steps (or not found
            // yet in this pass — cross-phase deps resolve fine since
            // `all_tasks`/`task_depth` cover the whole mission, but the
            // per-phase `steps` map above is scoped to THIS phase) is
            // skipped rather than guessed at.
            for dep_task_id in &task.depends_on {
                let Some(dep_last_step) = all_tasks
                    .iter()
                    .find(|t| &t.id == dep_task_id)
                    .and_then(|t| t.step_ids.last())
                else {
                    continue;
                };
                let Some(first_step) = task.step_ids.first() else {
                    continue;
                };
                edges.push(GraphEdge {
                    id: format!("depends_on:{}:{}", dep_last_step, first_step),
                    source: dep_last_step.clone(),
                    target: first_step.clone(),
                    kind: "depends_on",
                });
            }
        }
    }

    let legacy = !any_tasks;
    let note = if legacy {
        Some(
            "This mission predates the Task/Step registry (or has no dispatch-bearing steps) — \
             showing phases only."
                .to_string(),
        )
    } else if nodes.is_empty() {
        Some("No phase data recorded for this mission yet.".to_string())
    } else {
        None
    };

    Ok(Some(MissionGraph {
        mission_id: mission.id.clone(),
        mission_status: mission_status_str(mission.status),
        nodes,
        edges,
        legacy,
        note,
        generated_at_ms: current_millis(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, deps: &[&str], step_ids: &[&str]) -> Task {
        Task {
            id: id.to_string(),
            phase_id: "p1".to_string(),
            description: format!("task {id}"),
            step_ids: step_ids.iter().map(|s| s.to_string()).collect(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        }
    }

    fn step(id: &str, task_id: &str, status: NodeStatus) -> Step {
        Step {
            id: id.to_string(),
            task_id: task_id.to_string(),
            kind: "procedural.noop".to_string(),
            status,
            config: serde_json::Value::Null,
            started_ts: None,
            completed_ts: None,
            output: None,
        }
    }

    // ─── layer_tasks_by_depth ──────────────────────────────────────────

    #[test]
    fn layer_tasks_linear_chain_increases_depth() {
        let tasks = vec![task("a", &[], &[]), task("b", &["a"], &[]), task("c", &["b"], &[])];
        let depths = layer_tasks_by_depth(&tasks);
        assert_eq!(depths["a"], 0);
        assert_eq!(depths["b"], 1);
        assert_eq!(depths["c"], 2);
    }

    #[test]
    fn layer_tasks_diamond_converges_at_max_plus_one() {
        // a -> b, a -> c, {b, c} -> d
        let tasks = vec![
            task("a", &[], &[]),
            task("b", &["a"], &[]),
            task("c", &["a"], &[]),
            task("d", &["b", "c"], &[]),
        ];
        let depths = layer_tasks_by_depth(&tasks);
        assert_eq!(depths["a"], 0);
        assert_eq!(depths["b"], 1);
        assert_eq!(depths["c"], 1);
        assert_eq!(depths["d"], 2);
    }

    #[test]
    fn layer_tasks_disconnected_node_is_depth_zero() {
        let tasks = vec![task("a", &[], &[]), task("lonely", &[], &[])];
        let depths = layer_tasks_by_depth(&tasks);
        assert_eq!(depths["lonely"], 0);
    }

    #[test]
    fn layer_tasks_dangling_dependency_does_not_panic() {
        let tasks = vec![task("a", &["ghost"], &[])];
        let depths = layer_tasks_by_depth(&tasks);
        assert_eq!(depths["a"], 0);
    }

    #[test]
    fn layer_tasks_cycle_falls_back_to_zero_without_hanging() {
        // a -> b -> a (a real scheduler would reject this at load time via
        // `scheduler::detect_cycles`; this function must still terminate).
        let tasks = vec![task("a", &["b"], &[]), task("b", &["a"], &[])];
        let depths = layer_tasks_by_depth(&tasks);
        assert_eq!(depths["a"], 0);
        assert_eq!(depths["b"], 0);
    }

    // ─── derive_task_status ────────────────────────────────────────────

    #[test]
    fn derive_task_status_error_wins_over_everything() {
        let s1 = step("s1", "t", NodeStatus::Complete);
        let s2 = step("s2", "t", NodeStatus::Error);
        assert_eq!(derive_task_status(&[&s1, &s2]), NodeStatus::Error);
    }

    #[test]
    fn derive_task_status_running_when_any_running() {
        let s1 = step("s1", "t", NodeStatus::Complete);
        let s2 = step("s2", "t", NodeStatus::Running);
        assert_eq!(derive_task_status(&[&s1, &s2]), NodeStatus::Running);
    }

    #[test]
    fn derive_task_status_complete_when_all_complete() {
        let s1 = step("s1", "t", NodeStatus::Complete);
        let s2 = step("s2", "t", NodeStatus::Complete);
        assert_eq!(derive_task_status(&[&s1, &s2]), NodeStatus::Complete);
    }

    #[test]
    fn derive_task_status_planned_with_no_steps() {
        assert_eq!(derive_task_status(&[]), NodeStatus::Planned);
    }

    #[test]
    fn derive_task_status_planned_when_mixed_planned_and_complete() {
        let s1 = step("s1", "t", NodeStatus::Complete);
        let s2 = step("s2", "t", NodeStatus::Planned);
        assert_eq!(derive_task_status(&[&s1, &s2]), NodeStatus::Planned);
    }
}
