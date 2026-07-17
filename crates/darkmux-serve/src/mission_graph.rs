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
//! snapshot, and a row within that node's `steps` array (see
//! `assets/mission-graph.html`'s `applyFlowRecord`). No flow-record read
//! happens here.
//!
//! **Steps render as ROWS inside their owning Task node, not separate
//! nodes (#1401).** post-#1341, `Step` carries no `depends_on` of its own —
//! ALL real dependency/concurrency semantics live at `Task::depends_on`
//! (see that field's doc in `darkmux_crew::types`) — and the overwhelming
//! majority of production Tasks carry exactly one Step, so a separate Step
//! node doubled the node count without adding graph-shape information. A
//! Task node now carries its full `steps: Vec<StepRow>` (kind display
//! label + status, in `Task.step_ids` order); the derived step-to-step
//! edge synthesis this module previously built (an upstream task's LAST
//! step → a downstream task's FIRST step) retires along with the step
//! nodes it connected — `depends_on` edges connect TASK nodes directly on
//! the real `Task::depends_on`, one edge per dependency, no step-level
//! detour needed.
//!
//! **Kind fallback chain for a step's label (#1402).** A step that hasn't
//! dispatched yet may have no persisted Step file — only STATUS is
//! legitimately unknown mid-run; the KIND is fixed at mint time and frozen
//! into `config-snapshot.json` (see [`kind_from_config_snapshot`]). Once a
//! kind (persisted or recovered) is known, [`resolve_step_label`] resolves
//! its human label: registered `StepKind::display_name()` → the raw kind
//! id itself → the step id → `"unknown"` (a genuinely unconstructible/
//! foreign kind with no snapshot either — the deep fallback, not the
//! common case).

use darkmux_crew::types::{Mission, MissionStatus, NodeStatus, Phase, PhaseStatus, Step, Task};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// One node in the rendered graph — a Phase or a Task (steps render as
/// rows inside a Task node, see the module doc — #1401).
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
    /// `Mission::phase_ids`; task nodes use the task-dependency
    /// longest-path depth computed by [`layer_tasks_by_depth`].
    /// Diagram-only, never scheduler-authoritative — see that function's
    /// doc for the cycle/dangling-reference fallback.
    pub depth: usize,
    /// (#1398) The FULL phase/task description, for a tooltip/detail
    /// affordance — `label` above is the short operator-facing name
    /// (`display_name` → `id`, never `description`), so the long text
    /// (which for `coder-phase` doubles as the coder's dispatch brief)
    /// stays one hover away rather than truncating the node itself. `None`
    /// when the phase/task has no description text at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// (#1401) One row per Step in `Task.step_ids` order — empty for a
    /// phase node. The page renders each row as `label` + a status dot (a
    /// `"running"` row pulses); `id` is what the page's SSE handler
    /// matches an incoming step-lifecycle record's `handle` against to
    /// flip a row's status in place, without needing a separate node to
    /// look up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<StepRow>,
}

/// One row inside a Task node's card (#1401). Same `camelCase` wire
/// contract as [`GraphNode`].
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StepRow {
    pub id: String,
    /// Resolved via [`resolve_step_label`] — StepKind display name → kind
    /// id → step id → `"unknown"` (#1402).
    pub label: String,
    /// (#1403) The RAW step kind id (`"dispatch.internal"`, `"review.probe"`,
    /// `"mission.coder"`, `"procedural.shell"`, …) — distinct from the
    /// human `label`. The page gates the live token/turn meter on this: an
    /// AI-DISPATCHING kind (`dispatch.*`, `review.probe/judge/verify`,
    /// `mission.coder/verify`) shows a meter; a procedural kind
    /// (`procedural.*`, `review.bundle/dedup`, `mission.worktree`) shows
    /// none. Empty string when the kind is genuinely unknown (a synthesized
    /// step with no config-snapshot recovery — see `build_mission_graph`);
    /// the page then falls back to showing a meter only if metrics arrive.
    pub kind: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_ts: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_ts: Option<u64>,
    /// (#1432 item 4) FINALIZED token/turn totals folded from this
    /// mission's flow records at page-load time, so a completed step whose
    /// dispatch ran BEFORE the page opened shows its real total immediately
    /// (the page's SSE channel is tail-from-now, so without this a page
    /// opened after a step finished renders its meter blank). Absent (None)
    /// for a step that never dispatched or whose records aren't on disk (a
    /// mixed-era mission) — the page then shows the idle placeholder, an
    /// honest "no data" rather than a wrong zero. The SSE stream stays the
    /// LIVE-increment channel; these are only the terminal totals. Additive
    /// camelCase (`tokensFinal`/`turnsFinal`/`cloud`); pre-#1432 consumers
    /// ignore them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_final: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns_final: Option<u64>,
    /// `Some(true)` when any folded record for this step carried a hosted
    /// endpoint (a remote/cloud call) — the page colors the backfilled total
    /// the same "cloud" hue the live meter uses. Absent when local-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud: Option<bool>,
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
    /// `"contains"` (phase→task) or `"depends_on"` (a real `Task::depends_on`
    /// dependency — #1401 retired the derived step-granularity edges this
    /// used to carry; every dependency edge connects two TASK nodes now).
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

/// Non-empty description text, or `None` — shared by phase/task node
/// construction so an empty-string `description` (the zero value every
/// `Phase`/`Task` literal defaults to) doesn't round-trip as a present but
/// useless tooltip (#1398).
fn description_or_none(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        None
    } else {
        Some(text.to_string())
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

// ─── Step kind display-name fallback chain (#1402) ─────────────────────

/// (#1402) Static kind-id → display-name table for `src/coder_phase.rs`'s
/// three Tier 3 `mission.*` kinds — `darkmux-serve` structurally cannot
/// depend on the root `darkmux` binary crate (that crate depends on
/// `darkmux-serve` to embed the daemon; the reverse edge would be
/// circular), so unlike `review.*` (visible via the already-present
/// `darkmux-lab` dependency, see [`darkmux_lab::lab::review::review_step_kind_display_name`]),
/// this table can't call into the real `StepKind::display_name()` impls
/// directly. It's a literal duplication, guarded per #1352's "a
/// conformance test in a crate that sees both is acceptable and preferred
/// over duplication without a guard": the root crate (which depends on
/// BOTH this crate and owns `coder_phase.rs`) pins this table against the
/// live impls in its own test suite
/// (`coder_phase::tests::mission_step_kind_display_names_match_this_crates_static_table`).
pub fn mission_step_kind_display_name(kind: &str) -> Option<&'static str> {
    match kind {
        "mission.worktree" => Some("Worktree"),
        "mission.coder" => Some("Coder"),
        "mission.verify" => Some("Verify (QA)"),
        _ => None,
    }
}

/// The full display-name resolution, trying every kind family this crate
/// can see: Tier 1 builtins (via the real registry — `darkmux-serve`
/// already depends on `darkmux-crew`), Tier 3 `review.*` (via
/// `darkmux-lab`, already a dependency), then Tier 3 `mission.*` (the
/// static table above, this crate's own literal). `None` when `kind`
/// isn't recognized by any of them — the caller falls back further (see
/// [`resolve_step_label`]).
fn step_kind_display_name(kind: &str) -> Option<&'static str> {
    if let Ok(k) = darkmux_crew::step_kinds::StepKindRegistry::with_builtins().get(kind) {
        return Some(k.display_name());
    }
    if let Some(n) = darkmux_lab::lab::review::review_step_kind_display_name(kind) {
        return Some(n);
    }
    mission_step_kind_display_name(kind)
}

/// (#1402) The full label fallback chain a step row renders:
/// StepKind display name → the raw kind id itself → the step id →
/// `"unknown"` (a deep fallback reserved for a genuinely unconstructible/
/// foreign kind with an empty id AND no step id — which never happens in
/// practice, since every node always has an id; kept for defensive
/// completeness).
pub fn resolve_step_label(kind: &str, step_id: &str) -> String {
    if kind.is_empty() {
        return if step_id.is_empty() { "unknown".to_string() } else { step_id.to_string() };
    }
    step_kind_display_name(kind).map(str::to_string).unwrap_or_else(|| kind.to_string())
}

// ─── config-snapshot kind recovery for synthesized steps (#1402) ───────

/// (#1402 point 1) Recover a step's real `kind` from the mission's frozen
/// `config-snapshot.json` when no Step file has been persisted for it yet
/// (mid-run — see `build_mission_graph`'s doc). Only STATUS is legitimately
/// unknown mid-run (`Planned` is the correct default); the KIND was fixed
/// at mint time and the snapshot is its authority.
///
/// Returns the doc's raw (unrendered) `StepConfig.kind` — for an EXPANDING
/// template task this is the BASE kind (e.g. `"review.probe"`, not the
/// per-seat-rendered `"review.probe:alpha"`), since recovering the exact
/// per-copy name would require re-deriving the launcher's dynamic
/// expansion inputs (e.g. staffed seat names), which this read-only path
/// doesn't have. The base kind is enough for [`resolve_step_label`]'s
/// fallback chain (`review_step_kind_display_name` prefix-matches
/// `"review.probe"` to the same "Probe" label its per-seat form resolves
/// to) — see [`pattern_matches`]'s doc for the matching mechanics.
///
/// `None` when the mission has no config-snapshot at all (a hand-authored
/// mission, or one that predates #1284 Packet 4a) or the given ids don't
/// match anything in it — the caller's own `"unknown"`-deep-fallback path
/// still applies in that case, unchanged from before this feature.
fn kind_from_config_snapshot(mission_id: &str, real_task_id: &str, real_step_id: &str) -> Option<String> {
    let config = darkmux_crew::lifecycle::load_config_snapshot(mission_id).ok().flatten()?;
    for phase in &config.phases {
        // Mirrors `mission_launch::ensure_mission_and_phases_with_provenance`'s
        // deterministic composition rule (`format!("{mission_id}-{}", p.id)`)
        // — every config-launched instance's real phase id follows this
        // convention, so it's derivable here with no launch-time state.
        let real_phase_id = format!("{mission_id}-{}", phase.id);
        for task_cfg in &phase.tasks {
            match &task_cfg.expand {
                Some(spec) => {
                    if !pattern_matches(&spec.task_id_pattern, real_task_id) {
                        continue;
                    }
                    if let Some(step_cfg) = task_cfg.steps.first() {
                        if pattern_matches(&spec.step_id_pattern, real_step_id) {
                            return Some(step_cfg.kind.clone());
                        }
                    }
                }
                None => {
                    let candidate_task_id = substitute_id_placeholder_prefix(&task_cfg.id, &phase.id, &real_phase_id);
                    if candidate_task_id != real_task_id {
                        continue;
                    }
                    for step_cfg in &task_cfg.steps {
                        let candidate_step_id =
                            substitute_id_placeholder_prefix(&step_cfg.id, &phase.id, &real_phase_id);
                        if candidate_step_id == real_step_id {
                            return Some(step_cfg.kind.clone());
                        }
                    }
                }
            }
        }
    }
    None
}

/// The placeholder-prefix rule (`darkmux_crew::mission_config::TaskConfig`'s
/// doc — this is a local, read-only mirror of `mission_config::interpret`'s
/// private `substitute_id`, duplicated rather than exported since it's 8
/// lines and this crate has no other reason to depend on `interpret`'s
/// internals): if `id` is literally prefixed by `"<doc_phase_id>-"`,
/// replace that PREFIX with `"<real_phase_id>-"`, keeping everything after
/// it unchanged. An id with no such prefix (the FIXED-id convention, e.g.
/// review.json's `"review-bundle-task"`) passes through verbatim.
fn substitute_id_placeholder_prefix(id: &str, doc_phase_id: &str, real_phase_id: &str) -> String {
    if real_phase_id == doc_phase_id {
        return id.to_string();
    }
    let prefix = format!("{doc_phase_id}-");
    match id.strip_prefix(prefix.as_str()) {
        Some(rest) => format!("{real_phase_id}-{rest}"),
        None => id.to_string(),
    }
}

/// Structural match of an expansion pattern (e.g. `"review-probe-{index}-task"`)
/// against a candidate real id, treating `{index}`/`{name}` as wildcards —
/// this read-only path doesn't have the launcher's actual index/name
/// values (those are dynamic, resolved from crew staffing at launch time),
/// so it can't RENDER the pattern and compare strings; instead it checks
/// that `candidate` starts with the pattern's literal text before the
/// placeholder and ends with the literal text after it (and, for a pattern
/// with more than one placeholder, that every literal segment between them
/// appears in order) — sufficient to recognize "this real id plausibly
/// came from this template" without knowing which specific item it was.
fn pattern_matches(pattern: &str, candidate: &str) -> bool {
    let segments = split_on_placeholders(pattern);
    let mut cur = candidate;
    let last = segments.len().saturating_sub(1);
    for (i, seg) in segments.iter().enumerate() {
        if seg.is_empty() {
            continue;
        }
        let is_first = i == 0;
        let is_last = i == last;
        if is_first && is_last {
            // No placeholder in the pattern at all — a single literal
            // segment requires an EXACT match, not just a prefix (a
            // `starts_with`-only check here would let "fixed-task-2"
            // falsely match the fixed id "fixed-task").
            if cur != seg.as_str() {
                return false;
            }
            cur = "";
        } else if is_first {
            let Some(rest) = cur.strip_prefix(seg.as_str()) else {
                return false;
            };
            cur = rest;
        } else if is_last {
            if !cur.ends_with(seg.as_str()) {
                return false;
            }
        } else {
            match cur.find(seg.as_str()) {
                Some(pos) => cur = &cur[pos + seg.len()..],
                None => return false,
            }
        }
    }
    true
}

/// Split `pattern` into literal segments around every `{index}`/`{name}`
/// placeholder occurrence, e.g. `"review-probe-{index}-task"` →
/// `["review-probe-", "-task"]`. A pattern with no placeholder at all
/// yields one segment (the whole literal string) — [`pattern_matches`]
/// then requires an exact `starts_with`+`ends_with` on that single literal,
/// which for a one-segment list means an exact substring match.
fn split_on_placeholders(pattern: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut rest = pattern;
    loop {
        let next_index = rest.find("{index}");
        let next_name = rest.find("{name}");
        let next = match (next_index, next_name) {
            (Some(i), Some(n)) => Some(i.min(n)),
            (Some(i), None) => Some(i),
            (None, Some(n)) => Some(n),
            (None, None) => None,
        };
        match next {
            None => {
                segments.push(rest.to_string());
                break;
            }
            Some(pos) => {
                segments.push(rest[..pos].to_string());
                let tok_len = if rest[pos..].starts_with("{index}") { 7 } else { 6 };
                rest = &rest[pos + tok_len..];
            }
        }
    }
    segments
}

/// (#1432 item 4) The finalized token/turn totals folded for one step from
/// this mission's flow records. Mirrors the page's own
/// `applyRecordToMetrics` FINAL branch (`assets/mission-graph.html`) so the
/// server backfill and the live SSE channel agree on what "finalized" means.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct StepFinals {
    pub tokens: Option<u64>,
    pub turns: Option<u64>,
    pub cloud: bool,
}

/// Which step id (if any) a flow record attributes to, using the SAME three
/// correlation keys, in the SAME order, the page's `stepForRecord` uses:
/// (1) `payload.step_id`; (2) a `session_id` of the `step-<id>` shape a
/// `dispatch.internal` step defaults to; (3) `handle == <step id>`. Returns
/// the id only when it is one of THIS mission's steps (`step_ids`), so a
/// scan over a shared per-day flow file attributes nothing foreign.
fn step_for_record<'a>(rec: &serde_json::Value, step_ids: &'a BTreeSet<String>) -> Option<&'a str> {
    if let Some(sid) = rec.get("payload").and_then(|p| p.get("step_id")).and_then(|s| s.as_str()) {
        if let Some(found) = step_ids.get(sid) {
            return Some(found.as_str());
        }
    }
    if let Some(session) = rec.get("session_id").and_then(|s| s.as_str()) {
        if let Some(rest) = session.strip_prefix("step-") {
            if let Some(found) = step_ids.get(rest) {
                return Some(found.as_str());
            }
        }
    }
    if let Some(handle) = rec.get("handle").and_then(|s| s.as_str()) {
        if let Some(found) = step_ids.get(handle) {
            return Some(found.as_str());
        }
    }
    None
}

/// Pure: fold a stream of flow records into per-step FINALIZED totals.
/// Only TERMINAL totals are folded — `dispatch complete`/`dispatch.complete`
/// (`total_tokens` + `total_turns`) and `step result` (`total_tokens`) —
/// taking the max across records (a re-run's later complete wins). The
/// per-turn RUNNING increments (`telemetry.tokens`, `dispatch.turn`) are
/// deliberately NOT folded here: those stay the page's live SSE channel, so
/// the backfill can never race ahead of or double-count the live meter. A
/// record naming a hosted `payload.endpoint` marks its step `cloud`. Pure +
/// iterator-driven so the correlation/fold logic is unit-testable without
/// touching the filesystem.
pub(crate) fn fold_step_finals<I>(records: I, step_ids: &BTreeSet<String>) -> BTreeMap<String, StepFinals>
where
    I: IntoIterator<Item = serde_json::Value>,
{
    let mut out: BTreeMap<String, StepFinals> = BTreeMap::new();
    for rec in records {
        let Some(sid) = step_for_record(&rec, step_ids) else {
            continue;
        };
        let action = rec.get("action").and_then(|a| a.as_str()).unwrap_or("");
        let payload = rec.get("payload");
        let total_tokens = payload.and_then(|p| p.get("total_tokens")).and_then(|v| v.as_u64());
        let total_turns = payload.and_then(|p| p.get("total_turns")).and_then(|v| v.as_u64());
        let endpoint = payload.and_then(|p| p.get("endpoint")).map(|v| !v.is_null()).unwrap_or(false);
        let is_complete = action == "dispatch complete" || action == "dispatch.complete";
        let is_step_result = action == "step result";
        if !is_complete && !is_step_result {
            // Still honor a cloud marker riding an otherwise-ignored record
            // for this step, but only if we already fold something for it.
            continue;
        }
        let entry = out.entry(sid.to_string()).or_default();
        if let Some(t) = total_tokens {
            entry.tokens = Some(entry.tokens.map_or(t, |cur| cur.max(t)));
        }
        if is_complete {
            if let Some(n) = total_turns {
                entry.turns = Some(entry.turns.map_or(n, |cur| cur.max(n)));
            }
        }
        if endpoint {
            entry.cloud = true;
        }
    }
    out
}

/// Read every `.jsonl` day file in `flows_dir` once and fold this mission's
/// steps' finalized totals out of them (#1432 item 4). Best-effort: a
/// missing/unreadable directory or file yields an empty map (the graph then
/// carries no backfill, honest "no data"). Scans ALL day files rather than
/// the last two (`resolve_session`'s window) because a multi-day mission's
/// earlier steps would otherwise lose their totals; the per-mission
/// `step_ids` filter keeps each line's cost to a parse plus a few lookups.
fn backfill_step_finals(flows_dir: &std::path::Path, step_ids: &BTreeSet<String>) -> BTreeMap<String, StepFinals> {
    use std::io::BufRead;
    if step_ids.is_empty() {
        return BTreeMap::new();
    }
    let Ok(dir) = std::fs::read_dir(flows_dir) else {
        return BTreeMap::new();
    };
    let mut day_files: Vec<std::path::PathBuf> = dir
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    day_files.sort();
    let records = day_files.into_iter().flat_map(|path| {
        let mut recs: Vec<serde_json::Value> = Vec::new();
        if let Ok(file) = std::fs::File::open(&path) {
            for line in std::io::BufReader::new(file).lines() {
                let Ok(line) = line else { continue };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    recs.push(v);
                }
            }
        }
        recs
    });
    fold_step_finals(records, step_ids)
}

/// Build the full node-link graph for one mission. `Ok(None)` when no
/// mission with this id exists (the route answers 404).
///
/// `flows_dir` is scanned once (#1432 item 4) to backfill completed steps'
/// finalized token/turn totals into their `StepRow`s — see
/// [`backfill_step_finals`]. An empty/nonexistent `flows_dir` (e.g. a test
/// router built with `PathBuf::new()`) simply yields no backfill.
///
/// **Degradation granularity is per-DIRECTORY, not per-file.** Every
/// filesystem read here is `unwrap_or_default`, but the underlying
/// `lifecycle::load_tasks_for_phase`/`load_steps_for_phase` readers
/// (`load_json_dir`) `?`-propagate on the FIRST unreadable/corrupt file —
/// so one corrupt step JSON drops that phase's ENTIRE step set (that
/// phase's steps then render as synthesized `planned` rows, see below), it
/// does not skip just the one bad file. Per-file leniency would need a
/// change in `darkmux-crew`'s `load_json_dir`, deliberately not made from
/// this read-only viewer feature — a possible future improvement if
/// corrupt single files show up in practice.
///
/// **Mid-run step synthesis:** the three production graph runners persist
/// Step JSONs only AFTER `run_step_graph` returns (`coder_phase.rs`,
/// `mission_launch.rs`, `review.rs`) — so a page opened DURING a run sees
/// tasks whose `step_ids` name steps with no file on disk yet. Rather than
/// omitting those rows (which would leave the SSE layer with no row to
/// animate — the scheduler's `step start`/`step complete` records key on
/// step id), every `step_ids` entry with no persisted file gets a
/// synthesized `planned` row whose KIND is recovered from
/// `config-snapshot.json` when available (#1402 — see
/// [`kind_from_config_snapshot`]), falling back to the deep `"unknown"`
/// chain only when no snapshot exists either.
pub fn build_mission_graph(
    mission_id: &str,
    flows_dir: &std::path::Path,
) -> anyhow::Result<Option<MissionGraph>> {
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

    // (#1432 item 4) Every step id this mission declares, across all phases —
    // the correlation filter for the flow-record backfill below. Read once.
    let all_step_ids: BTreeSet<String> =
        all_tasks.iter().flat_map(|t| t.step_ids.iter().cloned()).collect();
    let step_finals = backfill_step_finals(flows_dir, &all_step_ids);

    for (phase_index, phase_id) in mission.phase_ids.iter().enumerate() {
        let Some(phase) = phase_by_id.remove(phase_id) else {
            continue;
        };
        nodes.push(GraphNode {
            id: phase.id.clone(),
            label: phase.display_name.clone().unwrap_or_else(|| phase.id.clone()),
            kind: "phase",
            status: phase_status_str(phase.status),
            parent_id: None,
            started_ts: phase.started_ts,
            completed_ts: phase.completed_ts,
            depth: phase_index,
            description: description_or_none(&phase.description),
            steps: Vec::new(),
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

            // (#1401) Steps render as ROWS on the task node, in
            // `Task.step_ids` order — a synthesized (not-yet-persisted)
            // step's kind is recovered from config-snapshot.json when
            // possible (#1402), never hardcoded "unknown".
            let step_rows: Vec<StepRow> = task
                .step_ids
                .iter()
                .map(|step_id| {
                    // (#1432 item 4) Attach the backfilled finalized totals
                    // (None everywhere the mission has no folded records for
                    // this step — a never-dispatched or mixed-era step).
                    let fin = step_finals.get(step_id);
                    let tokens_final = fin.and_then(|f| f.tokens);
                    let turns_final = fin.and_then(|f| f.turns);
                    let cloud = match fin {
                        Some(f) if f.cloud => Some(true),
                        _ => None,
                    };
                    match steps.get(step_id) {
                        Some(step) => StepRow {
                            id: step.id.clone(),
                            label: resolve_step_label(&step.kind, &step.id),
                            kind: step.kind.clone(),
                            status: node_status_str(step.status),
                            started_ts: step.started_ts,
                            completed_ts: step.completed_ts,
                            tokens_final,
                            turns_final,
                            cloud,
                        },
                        None => {
                            let kind = kind_from_config_snapshot(mission_id, &task.id, step_id)
                                .unwrap_or_default();
                            StepRow {
                                id: step_id.clone(),
                                label: resolve_step_label(&kind, step_id),
                                kind,
                                status: node_status_str(NodeStatus::Planned),
                                started_ts: None,
                                completed_ts: None,
                                tokens_final,
                                turns_final,
                                cloud,
                            }
                        }
                    }
                })
                .collect();

            nodes.push(GraphNode {
                id: task.id.clone(),
                label: task.display_name.clone().unwrap_or_else(|| task.id.clone()),
                kind: "task",
                status: node_status_str(status),
                parent_id: Some(phase.id.clone()),
                started_ts,
                completed_ts,
                depth: *task_depth.get(&task.id).unwrap_or(&0),
                description: description_or_none(&task.description),
                steps: step_rows,
            });

            // (#1401) Cross-task dependency edges connect TASK nodes
            // directly on the real `Task::depends_on` — the derived
            // step-granularity fan-in edges this module used to synthesize
            // (upstream task's last step → downstream task's first step)
            // retired along with the step nodes they connected; a real
            // Task dependency is now exactly one edge, no detour needed.
            for dep_task_id in &task.depends_on {
                let edge_id = format!("depends_on:{dep_task_id}:{}", task.id);
                if !seen_dep_edges.insert(edge_id.clone()) {
                    continue;
                }
                edges.push(GraphEdge {
                    id: edge_id,
                    source: dep_task_id.clone(),
                    target: task.id.clone(),
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
            display_name: None,
            step_ids: step_ids.iter().map(|s| s.to_string()).collect(),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        }
    }

    // ─── (#1432 item 4) flow-record backfill ───────────────────────────

    fn ids(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn fold_finals_step_result_record_folds_total_tokens() {
        let step_ids = ids(&["review-judge-step"]);
        let rec = serde_json::json!({
            "action": "step result",
            "payload": { "step_id": "review-judge-step", "total_tokens": 4200 }
        });
        let out = fold_step_finals(vec![rec], &step_ids);
        assert_eq!(out["review-judge-step"].tokens, Some(4200));
        assert_eq!(out["review-judge-step"].turns, None);
        assert!(!out["review-judge-step"].cloud);
    }

    #[test]
    fn fold_finals_dispatch_complete_folds_tokens_turns_via_session_id() {
        // Correlation key 2: session_id `step-<id>` (the dispatch.internal default).
        let step_ids = ids(&["s1"]);
        let rec = serde_json::json!({
            "action": "dispatch.complete",
            "session_id": "step-s1",
            "payload": { "total_tokens": 15200, "total_turns": 9 }
        });
        let out = fold_step_finals(vec![rec], &step_ids);
        assert_eq!(out["s1"].tokens, Some(15200));
        assert_eq!(out["s1"].turns, Some(9));
    }

    #[test]
    fn fold_finals_handle_key_and_max_across_records() {
        // Correlation key 3 (handle == step id) + max wins across a re-run.
        let step_ids = ids(&["s1"]);
        let recs = vec![
            serde_json::json!({ "action": "dispatch complete", "handle": "s1",
                                "payload": { "total_tokens": 100, "total_turns": 2 } }),
            serde_json::json!({ "action": "dispatch complete", "handle": "s1",
                                "payload": { "total_tokens": 900, "total_turns": 5 } }),
        ];
        let out = fold_step_finals(recs, &step_ids);
        assert_eq!(out["s1"].tokens, Some(900), "later/higher complete wins");
        assert_eq!(out["s1"].turns, Some(5));
    }

    #[test]
    fn fold_finals_endpoint_marks_cloud() {
        let step_ids = ids(&["s1"]);
        let rec = serde_json::json!({
            "action": "step result",
            "payload": { "step_id": "s1", "total_tokens": 50, "endpoint": "https://api.example/v1" }
        });
        let out = fold_step_finals(vec![rec], &step_ids);
        assert!(out["s1"].cloud);
        assert_eq!(out["s1"].tokens, Some(50));
    }

    #[test]
    fn fold_finals_ignores_foreign_steps_and_running_increments() {
        let step_ids = ids(&["mine"]);
        let recs = vec![
            // Foreign step — not in this mission's set.
            serde_json::json!({ "action": "step result",
                                "payload": { "step_id": "someone-else", "total_tokens": 999 } }),
            // A RUNNING per-turn increment for my step — NOT a finalized total,
            // stays the SSE channel's job, must not fold here.
            serde_json::json!({ "action": "telemetry.tokens", "handle": "mine",
                                "payload": { "total_tokens": 7 } }),
            serde_json::json!({ "action": "dispatch.turn", "handle": "mine" }),
        ];
        let out = fold_step_finals(recs, &step_ids);
        assert!(!out.contains_key("mine"), "no finalized record -> no entry (honest absent)");
        assert!(!out.contains_key("someone-else"));
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

    // ─── resolve_step_label (#1402) ─────────────────────────────────────

    #[test]
    fn resolve_step_label_tier1_kind_resolves_via_the_registry() {
        assert_eq!(resolve_step_label("dispatch.internal", "s1"), "Dispatch");
        assert_eq!(resolve_step_label("procedural.noop", "s1"), "No-op");
    }

    #[test]
    fn resolve_step_label_review_kind_resolves_via_darkmux_lab() {
        assert_eq!(resolve_step_label("review.bundle", "s1"), "Bundle");
        assert_eq!(resolve_step_label("review.probe:alpha", "s1"), "Probe");
    }

    #[test]
    fn resolve_step_label_mission_kind_resolves_via_the_static_table() {
        assert_eq!(resolve_step_label("mission.coder", "s1"), "Coder");
        assert_eq!(resolve_step_label("mission.verify", "s1"), "Verify (QA)");
    }

    #[test]
    fn resolve_step_label_falls_back_to_the_raw_kind_id_when_unrecognized() {
        assert_eq!(resolve_step_label("some.custom.kind", "s1"), "some.custom.kind");
    }

    #[test]
    fn resolve_step_label_falls_back_to_the_step_id_when_kind_is_empty() {
        assert_eq!(resolve_step_label("", "s1"), "s1");
    }

    #[test]
    fn resolve_step_label_deep_fallback_is_unknown_only_when_both_are_empty() {
        assert_eq!(resolve_step_label("", ""), "unknown");
    }

    // ─── pattern_matches / split_on_placeholders (#1402) ────────────────

    #[test]
    fn pattern_matches_single_index_placeholder() {
        assert!(pattern_matches("review-probe-{index}-task", "review-probe-0-task"));
        assert!(pattern_matches("review-probe-{index}-task", "review-probe-17-task"));
        assert!(!pattern_matches("review-probe-{index}-task", "review-dedup-task"));
    }

    #[test]
    fn pattern_matches_name_placeholder() {
        assert!(pattern_matches("review-probe-{name}-task", "review-probe-alpha-task"));
    }

    #[test]
    fn pattern_matches_no_placeholder_requires_exact_containment() {
        assert!(pattern_matches("fixed-task", "fixed-task"));
        assert!(!pattern_matches("fixed-task", "fixed-task-2"));
    }

    #[test]
    fn pattern_matches_rejects_a_candidate_missing_the_suffix() {
        assert!(!pattern_matches("review-probe-{index}-task", "review-probe-0"));
    }

    // ─── kind_from_config_snapshot (#1402) ──────────────────────────────

    #[test]
    fn kind_from_config_snapshot_none_when_no_snapshot_exists() {
        // No mission dir at all under this id in the test's isolated
        // DARKMUX_CREW_DIR — the function must return None, not error.
        assert_eq!(kind_from_config_snapshot("no-such-mission-xyz", "t1", "s1"), None);
    }
}
