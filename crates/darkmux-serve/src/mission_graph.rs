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
///
/// **Wire casing contract:** serialized `camelCase` (`parentId`,
/// `startedTs`, `completedTs`) because the CONSUMER is JS
/// (`assets/mission-graph.html` reads `n.parentId` in `computeLayout`).
/// The review gate on the first cut of this feature caught the mismatch
/// (Rust emitted `parent_id`, JS read `parentId` — every task grouped
/// under a missing parent and the whole layout collapsed to the origin);
/// the `mission_graph_json_fan_in_shape` route test now pins the exact
/// key the JS reads so a rename on either side fails a test instead of
/// silently flattening the layout.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
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

/// One edge in the rendered graph. Same `camelCase` wire contract as
/// [`GraphNode`] (a no-op for the current single-word field names, but the
/// attribute keeps a future two-word field from re-introducing the
/// casing trap).
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GraphEdge {
    pub id: String,
    pub source: String,
    pub target: String,
    /// `"contains"` (phase→task, task→step) or `"depends_on"` (a real
    /// scheduler dependency, derived to step granularity — see module doc).
    pub kind: &'static str,
}

/// The full graph payload for one mission.
///
/// Deliberately snake_case on the wire (NO `rename_all` — `mission_id`,
/// `mission_status`, `generated_at_ms`), matching every other daemon
/// endpoint's envelope convention (`/missions`, `/phases`, `/lab/*`), and
/// the page's JS reads it that way (`g.mission_id`, `g.mission_status`).
/// Only the node/edge OBJECTS are camelCase — see [`GraphNode`]'s casing
/// contract. The `mission_graph_json_fan_in_shape` test pins both casings.
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

/// Derive a Task's status from its Steps' statuses — `Task` carries no
/// `status` field of its own (#1230/#1341: only `Step` is
/// scheduler-driven). The caller passes ONE status per `Task.step_ids`
/// entry, substituting `Planned` for a step whose file doesn't exist yet
/// (a mid-run graph — see `build_mission_graph`'s step synthesis), so a
/// task whose only PERSISTED step is complete but whose later steps
/// haven't materialized yet reads `Planned`-mixed (not `Complete`).
/// Priority: any `Error` wins (a task with one failed step is a failed
/// task); else any `Running` wins; else all-`Complete` (and non-empty)
/// is `Complete`; else any `Abandoned` is `Abandoned`; else `Planned`
/// (covers "no steps yet" and "all still Planned").
fn derive_task_status(step_statuses: &[NodeStatus]) -> NodeStatus {
    if step_statuses.contains(&NodeStatus::Error) {
        return NodeStatus::Error;
    }
    if step_statuses.contains(&NodeStatus::Running) {
        return NodeStatus::Running;
    }
    if !step_statuses.is_empty() && step_statuses.iter().all(|s| *s == NodeStatus::Complete) {
        return NodeStatus::Complete;
    }
    if step_statuses.contains(&NodeStatus::Abandoned) {
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
/// mission with this id exists (the route answers 404).
///
/// **Degradation granularity is per-DIRECTORY, not per-file.** Every
/// filesystem read here is `unwrap_or_default`, but the underlying
/// `lifecycle::load_tasks_for_phase`/`load_steps_for_phase` readers
/// (`load_json_dir`) `?`-propagate on the FIRST unreadable/corrupt file —
/// so one corrupt step JSON drops that phase's ENTIRE step set (the page
/// then shows that phase's steps as synthesized `planned` placeholders,
/// see below), it does not skip just the one bad file. Per-file leniency
/// would need a change in `darkmux-crew`'s `load_json_dir`, deliberately
/// not made from this read-only viewer feature — a possible future
/// improvement if corrupt single files show up in practice.
///
/// **Mid-run step synthesis:** the three production graph runners persist
/// Step JSONs only AFTER `run_step_graph` returns (`mission_run.rs`,
/// `mission_launch.rs`, `review.rs`) — so a page opened DURING a run sees
/// tasks whose `step_ids` name steps with no file on disk yet. Rather
/// than omitting those nodes (which would leave the SSE layer with no
/// node to animate — the scheduler's `step start`/`step complete` records
/// key on step id — and would leave cross-task dependency edges pointing
/// at nonexistent nodes), every `step_ids` entry with no persisted file
/// gets a synthesized `planned` step node (kind label `"unknown"` until
/// the file exists). Self-contained fix on the read side; the writers are
/// deliberately untouched.
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
    // Dedup guard for cross-task depends_on edges (duplicate `depends_on`
    // entries on one Task would otherwise emit duplicate edge ids — React
    // keys must be unique).
    let mut seen_dep_edges: BTreeSet<String> = BTreeSet::new();

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

            // One status per step_ids entry — a step whose file doesn't
            // exist yet (mid-run; see the fn doc's synthesis note) counts
            // as Planned, so a task can't read Complete while later steps
            // haven't materialized.
            let step_statuses: Vec<NodeStatus> = task
                .step_ids
                .iter()
                .map(|sid| steps.get(sid).map(|s| s.status).unwrap_or(NodeStatus::Planned))
                .collect();
            let status = derive_task_status(&step_statuses);
            let persisted: Vec<&Step> =
                task.step_ids.iter().filter_map(|sid| steps.get(sid)).collect();
            let started_ts = persisted.iter().filter_map(|s| s.started_ts).min();
            let completed_ts = if status == NodeStatus::Complete {
                persisted.iter().filter_map(|s| s.completed_ts).max()
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
            // `Step::depends_on` field exists post-#1341). Every step_ids
            // entry produces a node — synthesized `planned` when the file
            // isn't on disk yet (see the fn doc) — so the SSE layer always
            // has a node to flip and the dependency edges below always have
            // both endpoints.
            for (i, step_id) in task.step_ids.iter().enumerate() {
                let node = match steps.get(step_id) {
                    Some(step) => GraphNode {
                        id: step.id.clone(),
                        label: step.kind.clone(),
                        kind: "step",
                        status: node_status_str(step.status),
                        parent_id: Some(task.id.clone()),
                        started_ts: step.started_ts,
                        completed_ts: step.completed_ts,
                        depth: *task_depth.get(&task.id).unwrap_or(&0),
                    },
                    None => GraphNode {
                        id: step_id.clone(),
                        label: "unknown".to_string(),
                        kind: "step",
                        status: node_status_str(NodeStatus::Planned),
                        parent_id: Some(task.id.clone()),
                        started_ts: None,
                        completed_ts: None,
                        depth: *task_depth.get(&task.id).unwrap_or(&0),
                    },
                };
                nodes.push(node);
                edges.push(GraphEdge {
                    id: format!("contains:{}:{}", task.id, step_id),
                    source: task.id.clone(),
                    target: step_id.clone(),
                    kind: "contains",
                });
                if i > 0 {
                    let prev = &task.step_ids[i - 1];
                    edges.push(GraphEdge {
                        id: format!("depends_on:{}:{}", prev, step_id),
                        source: prev.clone(),
                        target: step_id.clone(),
                        kind: "depends_on",
                    });
                }
            }

            // Cross-task dependency edges, at STEP granularity: an upstream
            // task's LAST step feeds a downstream task's FIRST step — this
            // is what keeps a fan-in (N probe steps -> one dedup step)
            // visually legible instead of collapsing to a single task-level
            // edge. Both endpoints are guaranteed to exist as nodes (real or
            // synthesized — see above). A dependency naming a task with no
            // steps at all, or an unknown task id, is skipped rather than
            // guessed at. Duplicate `depends_on` entries on a Task are
            // deduped here (`seen_dep_edges`) so React never receives two
            // edges with the same key.
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
                let edge_id = format!("depends_on:{}:{}", dep_last_step, first_step);
                if !seen_dep_edges.insert(edge_id.clone()) {
                    continue;
                }
                edges.push(GraphEdge {
                    id: edge_id,
                    source: dep_last_step.clone(),
                    target: first_step.clone(),
                    kind: "depends_on",
                });
            }
        }
    }

    let legacy = !any_tasks;
    // Empty-graph check FIRST: a mission with zero phase nodes is also
    // `legacy` (no tasks anywhere), and the pre-registry wording would be
    // nonsense over an empty canvas — "nothing to draw" is the honest note.
    let note = if nodes.is_empty() {
        Some("No phase data recorded for this mission yet.".to_string())
    } else if legacy {
        Some(
            "This mission predates the Task/Step registry (or has no dispatch-bearing steps) — \
             showing phases only."
                .to_string(),
        )
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
        assert_eq!(
            derive_task_status(&[NodeStatus::Complete, NodeStatus::Error]),
            NodeStatus::Error
        );
    }

    #[test]
    fn derive_task_status_running_when_any_running() {
        assert_eq!(
            derive_task_status(&[NodeStatus::Complete, NodeStatus::Running]),
            NodeStatus::Running
        );
    }

    #[test]
    fn derive_task_status_complete_when_all_complete() {
        assert_eq!(
            derive_task_status(&[NodeStatus::Complete, NodeStatus::Complete]),
            NodeStatus::Complete
        );
    }

    #[test]
    fn derive_task_status_planned_with_no_steps() {
        assert_eq!(derive_task_status(&[]), NodeStatus::Planned);
    }

    #[test]
    fn derive_task_status_planned_when_mixed_planned_and_complete() {
        // Also the mid-run shape: a persisted-complete first step + a
        // not-yet-materialized (synthesized Planned) second step must NOT
        // read as a complete task.
        assert_eq!(
            derive_task_status(&[NodeStatus::Complete, NodeStatus::Planned]),
            NodeStatus::Planned
        );
    }
}
