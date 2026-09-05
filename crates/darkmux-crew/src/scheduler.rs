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
//! dispatch preflight's own resolution); `coder_phase`'s own
//! `MissionCoderStepKind`/`MissionVerifyStepKind` do the same for the
//! `mission.coder`/`mission.verify` kinds. `dispatch.single_shot`'s
//! residency (the review's probe/judge seats) is left at the default
//! (`None` → `Remote`) — Packet 4's job, once real concurrent local
//! seats exist to benefit from it; today's linear graphs (coder_phase's
//! 3-step chain) never have more than one step ready per wave, so the
//! classification is correctness/observability, not a measured speedup.

use crate::run_record::StepRecord;
use crate::step_kinds::StepKindRegistry;
use crate::types::{NodeStatus, Step, Task};
use anyhow::{anyhow, Result};
use darkmux_flow::{Category, FlowRecord, Level, Stage, Tier};
use darkmux_gestalt::{Facts, FootprintEstimator, ModelHost};
use std::collections::{BTreeMap, HashSet};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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
// (`coder_phase::select_phase`, `mission_status::unreachable_phase_drifts`)
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
            // (#1619) `reads` is an ordering relation too — a reads loop is
            // exactly as unschedulable as a depends_on loop, so the cycle
            // walk covers the union.
            for dep in task.depends_on.iter().chain(task.reads.iter()) {
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
    let mut stack: Vec<&str> =
        from.depends_on.iter().chain(from.reads.iter()).map(String::as_str).collect();
    let mut seen: HashSet<&str> = HashSet::new();
    while let Some(id) = stack.pop() {
        if id == target {
            return true;
        }
        if !seen.insert(id) {
            continue;
        }
        if let Some(t) = tasks.get(id) {
            // (#1619) reads orders like depends_on — same union as the
            // cycle walk above.
            stack.extend(t.depends_on.iter().chain(t.reads.iter()).map(String::as_str));
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
///   at all — defensive): every Task named in `task.depends_on` OR
///   `task.reads` (#1619 — the ledger relation orders identically)
///   satisfies `task.run_on` (#2310 P4, see [`dependency_satisfies_run_on`]
///   — defaults to requiring `task_status(..) == Complete`, same as
///   pre-#2310);
/// - otherwise (a later step in a multi-step Task): the step immediately
///   before it in `task.step_ids` is `Complete`.
///
/// A Task whose dependency chain includes a dead ancestor status its own
/// `run_on` doesn't accept never satisfies the first branch — its steps
/// simply never become ready, the same "stays `Planned` forever" terminal
/// shape the pre-#1341 `reachable`-gated design had, now emerging
/// naturally from this fixed-point check rather than a separate
/// reachability pre-pass.
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
        // (#1619) `reads` joins `depends_on` in readiness: a task can no
        // more start before an output it READS exists than before an
        // explicit dependency completes. This is what lets the ledger
        // relation carry data without a rendered edge and stay correct —
        // ordering is enforced HERE, not by the graph drawing.
        _ => task
            .depends_on
            .iter()
            .chain(task.reads.iter())
            .all(|dep_id| {
                tasks
                    .get(dep_id)
                    .map(|t| dependency_satisfies_run_on(task_status(t, steps), &task.run_on))
                    .unwrap_or(false)
            }),
    }
}

/// (#2310 P4/P4a) Does a dependency's DERIVED `task_status` satisfy the
/// DEPENDENT task's declared `run_on`? `run_on` names which of the
/// dependency's TERMINAL statuses count as "done enough to proceed" —
/// `"complete"` accepts `NodeStatus::Complete`; `"error"` additionally
/// accepts EITHER `NodeStatus::Error` OR `NodeStatus::Abandoned`.
/// `Abandoned` is folded into the `"error"` literal deliberately, never a
/// separate `"abandoned"` literal a document could name on its own
/// (`MissionConfig::validate` only recognizes `"complete"`/`"error"`):
/// under `cascade_abandon`'s design (#2310 P4a), a task only ever reaches
/// `Abandoned` as the TRANSITIVE consequence of some ancestor's `Error` —
/// there is no scenario where an operator would want to accept the
/// cascade's own shadow without also accepting the `Error` that caused
/// it. `Planned`/`Running` (non-terminal) never satisfy any `run_on`
/// value — neither is ever produced by this match, so the exhaustive
/// `match` below has no catch-all default to silently paper over one.
fn dependency_satisfies_run_on(dep_status: NodeStatus, run_on: &[String]) -> bool {
    match dep_status {
        NodeStatus::Complete => run_on.iter().any(|s| s == "complete"),
        NodeStatus::Error | NodeStatus::Abandoned => run_on.iter().any(|s| s == "error"),
        NodeStatus::Planned | NodeStatus::Running => false,
    }
}

/// (#2310 fix-loop C3 / S4-3) The step of `task` whose own outcome MADE
/// the task's derived status what it is — the step whose `output` is the
/// honest thing to forward to a downstream dependent:
/// - `Complete` → the LAST step (the task's final product; unchanged from
///   the pre-fix `step_ids.last()` rule, which is why every default-
///   `run_on` consumer sees byte-identical inputs).
/// - `Error` → the FIRST errored step (`task_status` reads `Error` off
///   ANY errored step, so that step IS the cause; the task's later steps
///   are `Planned` forever by `step_is_ready`'s intra-task rule).
/// - `Abandoned` → the first abandoned step carrying output, which
///   `cascade_abandon` wrote as the ORIGIN's `"<step-id>: <reason>"` — so
///   the true root cause travels one more hop without re-wrapping.
/// - `Planned`/`Running` → `None` (nothing terminal to forward).
fn terminal_source_step<'a>(
    task: &Task,
    status: NodeStatus,
    steps: &'a BTreeMap<String, Step>,
) -> Option<&'a Step> {
    let of = |id: &String| steps.get(id);
    match status {
        NodeStatus::Complete => task.step_ids.last().and_then(of),
        NodeStatus::Error => {
            task.step_ids.iter().filter_map(of).find(|s| s.status == NodeStatus::Error)
        }
        NodeStatus::Abandoned => task
            .step_ids
            .iter()
            .filter_map(of)
            .find(|s| s.status == NodeStatus::Abandoned && s.output.is_some())
            .or_else(|| {
                task.step_ids.iter().filter_map(of).find(|s| s.status == NodeStatus::Abandoned)
            }),
        NodeStatus::Planned | NodeStatus::Running => None,
    }
}

// ─── Input gathering (#1341 — Task-aware) ──────────────────────────────

/// The `input` map `step`'s job should receive:
/// - One entry per `task.depends_on` OR `task.reads` (#1619 — deduped when
///   both name the same task) Task id whose LAST step is `Complete` and has
///   recorded `output`, keyed by that dependency TASK's id (#1341 — Task is
///   the dependency-declaring unit; see `Task::depends_on`'s doc). **Every**
///   step of `task` receives these entries, not just the first — a Task's
///   declared `depends_on`/`reads` describe what the Task as a whole reads,
///   and that's visible to whichever step is running (#2310 P2a; see this
///   change's note below).
/// - If `step` is NOT the first step of `task`: one MORE entry — the
///   immediately-previous SAME-TASK step's `output` (if `Complete`), keyed
///   by that step's id. This is ADDITIVE to the `depends_on`/`reads` entries
///   above, never a replacement for them.
///
/// A dependency that's `Complete` but has no recorded `output` (a step
/// kind that legitimately produces none) is omitted, not stubbed with an
/// empty string.
///
/// (#2310 P2a) Before this change, a later step in a multi-step Task saw
/// ONLY its same-task predecessor's output — the Task's own
/// `depends_on`/`reads` reached exclusively the Task's FIRST step, so a
/// second (or third) step had no way to see what the Task declares it
/// reads. That forced awkward workarounds (carrying an upstream value
/// forward through every same-task step's own `output` just so a later
/// step could see it again). The rule now: a later step gets the SAME
/// `depends_on`/`reads` entries the first step would, PLUS its own
/// predecessor's output. First steps are unchanged (they had no
/// predecessor to chain from). A consumer keyed on `input.len() == 1` to
/// infer "there is exactly one predecessor" (e.g. `dispatch.map`'s
/// implicit single-dependency collection fallback) must instead prefer a
/// known predecessor-step key when present — see
/// `step_kinds::builtins::resolve_map_collection`.
/// The map is a `BTreeMap`, so consumers that iterate every input (the
/// `dispatch.internal` prompt blocks, `procedural.shell` env vars) see them in
/// KEY order — a later step's predecessor entry sorts among the task-id
/// entries by its step id, not last.
pub fn gather_inputs(
    step: &Step,
    task: &Task,
    tasks: &BTreeMap<String, Task>,
    steps: &BTreeMap<String, Step>,
) -> BTreeMap<String, String> {
    // (#1619) `reads` entries deliver exactly like `depends_on` entries —
    // the dependency task's LAST step output, keyed by that task's id. The
    // BTreeMap collect dedups a task named in both relations (legal during
    // config migration) to one entry. (#2310 P2a) This now feeds EVERY step
    // of `task`, not just the first — see the fn doc.
    // (#2310 P4a review fix M1) A dependency's output forwards when its
    // TERMINAL status satisfies THIS task's own `run_on` — not only when
    // it is literally `Complete`. Default `run_on` (`["complete"]`) keeps
    // the pre-#2310 behavior byte-for-byte (only `Complete` ever
    // satisfied it). A task declaring `"error"` also receives an `Error`
    // or cascade-`Abandoned` dependency's `output` — which is exactly how
    // the ORIGINATING failure's reason text (see `cascade_abandon`'s own
    // doc) reaches a downstream report/summary task: without this, a
    // task whose readiness `run_on` cascade-abandonment satisfies could
    // still run with an EMPTY view of why, forced back to guessing from
    // its own config wiring instead of the graph's own recorded reason.
    let mut inputs: BTreeMap<String, String> = task
        .depends_on
        .iter()
        .chain(task.reads.iter())
        .filter_map(|dep_task_id| {
            let dep_task = tasks.get(dep_task_id)?;
            // (#2310 fix-loop C3 / S4-3 / #2352 item 2) Keyed on the
            // dependency TASK's derived status and on the step that MADE it
            // terminal — not on `step_ids.last()`. For a multi-step task
            // whose FIRST step errored, the last step is still `Planned`
            // (`step_is_ready`'s intra-task rule guarantees it can never
            // run), so the old `last()` lookup forwarded nothing at all and
            // a `run_on: ["error"]` dependent ran with an EMPTY view of why
            // — the one thing the reason-forwarding contract above exists
            // to prevent.
            let dep_status = task_status(dep_task, steps);
            if !dependency_satisfies_run_on(dep_status, &task.run_on) {
                return None;
            }
            let src = terminal_source_step(dep_task, dep_status, steps)?;
            src.output.clone().map(|output| (dep_task_id.clone(), output))
        })
        .collect();

    // (#2310 P2a) A later step in a multi-step Task ALSO gets its
    // immediately-previous same-task step's output, chained onto (never
    // replacing) the `depends_on`/`reads` entries above.
    if let Some(i) = task.step_ids.iter().position(|id| id == &step.id) {
        if i > 0 {
            let prev_id = &task.step_ids[i - 1];
            if let Some(output) = steps
                .get(prev_id)
                .filter(|s| s.status == NodeStatus::Complete)
                .and_then(|s| s.output.clone())
            {
                inputs.insert(prev_id.clone(), output);
            }
        }
    }

    inputs
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
    /// (#1877 item 3) One [`StepRecord`] per step that reached a terminal
    /// transition through the LIVE `WaveSignal::StepTerminal` path, timed
    /// with an `Instant` pair taken strictly around that step's own
    /// `kind.run_streaming(...)` call on its own worker thread (see the
    /// job closure below), so a sibling's duration in here is never
    /// inflated by queueing or by a slower wave member. `items_in`/
    /// `items_out` are always `None` — the scheduler observes that a step
    /// ran and how long it took, never how many items it processed (that
    /// is per-kind business semantics; see `run_record`'s module doc for
    /// the full reconciliation-with-review argument).
    ///
    /// This field's `wall_ms` strictly CONTAINS a review step kind's own
    /// reported `wall_ms` when both exist for the same step: the kind's
    /// own record is computed and emitted BEFORE `run_streaming` returns;
    /// this one is timed around that SAME call, from outside it, so it
    /// can never be smaller. See `darkmux_lab::lab::review`'s "Timing: two
    /// scopes, not one duplicated" module doc (#1877) for why that
    /// containment is correct design, not duplication, and this file's own
    /// `#1877` invariant test for where the `>=` relationship is pinned.
    ///
    /// This is NOT simply "every completed or errored step" — a step can
    /// land in `errored` through THREE distinct no-live-terminal paths,
    /// and only one of them streamed a real duration:
    /// - A step whose `StepRunCtx` job PANICKED never streamed a
    ///   terminal (it still lands in `errored`).
    /// - A step whose LOCAL wave never even got to dispatch — either
    ///   `plan_waves` refused its placement outright, or the wave's own
    ///   `ensure_wave_loaded` failed to make it resident (see
    ///   `concurrent_dispatch::run_local_waves`) — is pushed straight into
    ///   the executor's results as an `Err` with no job closure ever
    ///   having run, so it likewise never streamed a terminal.
    ///
    /// Both of the above reconcile through the SAME post-scope
    /// `apply_step_terminal(..., None, ...)` call as the panic path (see
    /// that function's own doc) — there is no honest per-step duration to
    /// report for a step that never actually dispatched or never finished
    /// its own `run_streaming` call, and this module never substitutes a
    /// wave-clock number or a fabricated `0` in its place. So `errored`
    /// can be, and in practice often is, larger than the set of steps
    /// that have an entry here: a run where every dispatch step fails to
    /// load its model (the machine cannot fit it) produces N `errored`
    /// entries and an EMPTY `step_records` — nothing to render a timeline
    /// from, and nothing in `errored` alone says why.
    ///
    /// Two more properties worth knowing before treating this as a
    /// complete run log: (1) `run_step_graph` returns its `Result` via
    /// `?` from a couple of points mid-loop (an unregistered step kind;
    /// the wave-drain thread scope itself erroring) — either one discards
    /// the WHOLE `SchedulerReport`, including the records of every step
    /// that completed in EARLIER waves of the same call, and there is no
    /// partial-report recovery on that path (see
    /// `dispatch_as_crew_of_one::reconcile_on_error`, which cannot recover
    /// them either). (2) The `Vec` is in COMPLETION order (arrival at the
    /// live drain), not step-graph or wave order — concurrent siblings
    /// finish in a genuinely nondeterministic sequence, so a consumer
    /// rendering a timeline from this field should sort by whatever field
    /// it actually needs ordered, not rely on `Vec` order.
    ///
    /// This is a NEW, additive field: no existing caller of
    /// `run_step_graph` is required to read it, and none does yet (a
    /// mission gets these BY CONSTRUCTION but is free to ignore the field
    /// entirely, subject to the caveats above).
    ///
    /// **(#1877, final wiring step) This in-memory summary is no longer the
    /// only surface.** `apply_step_terminal` also streams a `STEP_TIMING_
    /// ACTION` ("step timing") flow record for each entry, at the SAME
    /// moment it is pushed here. See `step_timing_record`'s own doc for
    /// the shape and `run_record.rs`'s module doc for why the flow stream,
    /// not `MissionEnvelope`, is the destination. This closes the gap
    /// point (1) above describes: a scheduler-level `Err` mid-run discards
    /// the WHOLE in-memory `SchedulerReport` (including earlier waves'
    /// completed records), but by then every one of those waves' records
    /// has ALREADY reached the durable flow stream. The live emission
    /// survives exactly the failure the in-memory summary cannot recover
    /// from.
    pub step_records: Vec<StepRecord>,
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
///
/// Argument count exceeds clippy's default threshold (9 vs 7) since #1360's
/// `host_factory` addition and #1397's `persist` addition — mirrors the
/// same accepted trade-off as `WorkloadProvider::run()`'s own
/// `#[allow(clippy::too_many_arguments)]`: each parameter is inherent to
/// the call (graph state, planning inputs, the emission sink, the
/// injectable host, now the durable-persistence hook), not incidental
/// bloat.
///
/// `persist` (#1397 — "persist step transitions at transition time") is
/// called with the step's OWN post-flip state at each of the three status
/// transitions this loop performs (`Planned` -> `Running` at dispatch,
/// `Running` -> `Complete`/`Error` at completion) — immediately after the
/// matching `emit(step_lifecycle_record(...))` call, so a durable step
/// file and its flow-record announcement land together. This closes the
/// mid-run blind window a page opened between transitions used to see: the
/// pre-#1397 pattern (every caller bulk-`save_step`ing only AFTER
/// `run_step_graph` returns) left `graph.json` truthfully `planned` for
/// the ENTIRE run and only jumped to the final state all at once when the
/// caller's post-run loop ran. A no-op closure (`&mut |_| {}`) is a valid
/// `persist` for callers with no durable Step storage (most scheduler unit
/// tests) — the bulk end-of-run save loops every production caller already
/// runs stay in place as a cheap idempotent reconcile, not the only write.
#[allow(clippy::too_many_arguments)]
pub fn run_step_graph(
    steps: &mut BTreeMap<String, Step>,
    tasks: &BTreeMap<String, Task>,
    kinds: &StepKindRegistry,
    facts: &Facts,
    est: &(dyn FootprintEstimator + Sync),
    remote_cap: usize,
    host_factory: &(dyn Fn() -> Box<dyn ModelHost> + Sync),
    emit: &mut dyn FnMut(FlowRecord),
    persist: &mut dyn FnMut(&Step),
    // (#1684 Packet 2) The operator sign-off gate handler — mirrors
    // `persist`'s caller-supplied-seam shape immediately above. Invoked
    // (via `gate::resolve_gate`) for every READY step whose `gate` field
    // is `Some("operator")`, BEFORE that step ever flips `Planned` ->
    // `Running` — the check happens synchronously on this (the main)
    // thread, once per gated ready step, before this wave's jobs are
    // built, so a blocking handler (a tty prompt, an ACP `session/
    // request_permission` round-trip) can never race a sibling step's
    // dispatch. A step whose gate DECLINES never runs at all: it goes
    // straight from `Planned` to `Error` (see the wave loop below), and
    // every downstream dependent skips it exactly like any other failed
    // step. `None` is a valid, fail-CLOSED default (see `gate::
    // resolve_gate`'s own doc) — production callers with no gated steps
    // in their own graphs (the review driver, `dispatch_as_crew_of_one`,
    // most scheduler unit tests) pass `None`.
    //
    // (#1684 QA CONSIDER) The gate pass runs SERIALLY over every ready
    // step BEFORE any of this wave's jobs are built — a gated step ready
    // alongside N ungated siblings in the SAME wave holds all N of them
    // waiting behind the operator's decision, not just the gated one. This
    // is a deliberate simplicity trade-off (no per-step concurrent gate
    // resolution in Packet 2), not an oversight: answering the dialog is
    // on the critical path of every sibling in that wave.
    mut gate: Option<&mut crate::gate::GateHandler<'_>>,
    // (#1442 ship-2b) Optional dispatch interceptor threaded into every
    // step's `StepRunCtx` — `None` on every production path; a test
    // harness passes `Some` so `dispatch.map` items dispatch through the
    // caller's mock ON THE WORKER THREAD (see `MapDispatchOverride`'s doc
    // for why a thread-local seam structurally cannot serve here).
    dispatch_override: Option<crate::step_kinds::MapDispatchOverride>,
    // (#1530 Packet 1) The CALLER-SEED path onto the run-scoped
    // `ArtifactBus`: named values the caller already owns (a pre-stamped
    // envelope, a run-level context) that no `Port::artifact` factory can
    // produce, because a factory is a plain context-free `fn` (see
    // `Port`'s doc). Applied AFTER the `provides()` pre-scan below
    // materializes every declared `Artifact` port's default — a caller
    // seed OVERWRITES that default for its name (`ArtifactBus::seed`),
    // never the reverse, so a migrated `StepKind` still gets a valid
    // (if unstamped) artifact even when a caller seeds nothing. Every
    // pre-#1530-Packet-1 caller passes `&[]` here — zero behavior change.
    seed_artifacts: &[(&'static str, std::sync::Arc<dyn std::any::Any + Send + Sync>)],
) -> Result<SchedulerReport> {
    detect_cycles(tasks)?;

    let mut report = SchedulerReport {
        warnings: shared_workdir_warnings(tasks),
        ..Default::default()
    };

    // (#1442) Scheduler-owned `bucket_group -> shared remote bucket` map,
    // living for the WHOLE graph run so sibling steps naming the same group
    // meter ONE per-execution allowance between them regardless of which
    // wave each lands in. This is the allowance-multiplication fix: without
    // it, `seats x k` sibling `dispatch.map` probe steps would each mint a
    // fresh full allowance, multiplying the effective stage ceiling by the
    // step count. A step that names no group gets a step-scoped bucket
    // inside its own kind, so ungrouped behavior is unchanged.
    //
    // (#1530 Packet 0) Deliberately NOT unified with the `ArtifactBus`
    // materialized below, even though both are "scheduler-owned shared
    // state keyed by a name". `bucket_group` is CONFIG-DRIVEN: its name
    // comes from a Step's own `config.bucket_group` (resolved per-step,
    // inline in the wave loop below, because a group can first appear in
    // ANY wave) and its budget is a runtime value read from that same
    // config or `config_access::remote_max_tokens_per_execution()`. An
    // `ArtifactBus` entry's factory is a plain `fn() -> Arc<dyn Any + Send
    // + Sync>` chosen specifically for `Port` to stay `const`-constructible
    // (see `Port`'s doc) — it cannot capture a runtime budget value, so
    // retrofitting `RemoteBudget` (`crate::remote_budget`, #1877's shared
    // home) onto it would need either a captured-closure factory
    // (abandoning the const-array ergonomics `Port` is built around) or
    // resolving the bucket_group's budget BEFORE the artifact-bus pre-scan
    // below (which does not know per-step config, only the STATIC ports a
    // `StepKind` impl declares). Either path is a real design change for
    // zero behavior gain in a zero-behavior-change packet, so
    // `bucket_groups` keeps its own dedicated map — the BINDING requirement
    // is that `dispatch.map`'s allowance-sharing stays byte-identical, and
    // leaving its proven mechanism untouched is how this packet guarantees
    // that.
    let mut bucket_groups: std::collections::BTreeMap<
        String,
        std::sync::Arc<std::sync::Mutex<crate::remote_budget::RemoteBudget>>,
    > = std::collections::BTreeMap::new();

    // (#1530 Packet 0) Materialize the run-scoped `ArtifactBus` ONCE, on
    // this main thread, BEFORE the wave loop below ever spawns a worker —
    // the same "build fully, then treat as read-only across the thread
    // boundary" discipline `bucket_groups` above uses per-step, applied
    // here up front since a `Port::Artifact` declaration is STATIC (a
    // property of the `StepKind` impl, not of any one step's config), so
    // every artifact this graph could ever need is knowable before the
    // first step runs. Scans only the KINDS ACTUALLY USED by steps in
    // THIS graph (not every kind in the registry) — a registry can hold
    // kinds this particular graph never references, and those should
    // never pay to materialize an artifact nothing here will read.
    let mut bus = crate::step_kinds::ArtifactBus::new();
    {
        let mut seen_kind_ids: HashSet<&str> = HashSet::new();
        for step in steps.values() {
            if !seen_kind_ids.insert(step.kind.as_str()) {
                continue;
            }
            // A step whose `kind` isn't registered is a real error, but
            // this pre-scan isn't the place to raise it — the ordinary
            // `kinds.get(...)` lookup inside the wave loop below already
            // does that with full step-id context (`with_context_step`).
            // Here, an unresolvable kind simply contributes no ports.
            if let Ok(kind) = kinds.get(&step.kind) {
                for port in kind.provides() {
                    if let crate::step_kinds::PortKind::Artifact(factory) = port.kind {
                        bus.materialize(port.name, factory);
                    }
                }
            }
        }
    }
    // (#1530 Packet 1) Merge the caller's seeds OVER the `provides()`
    // defaults just materialized — see `seed_artifacts`'s own doc on this
    // parameter for why a caller-seed always wins.
    for (name, value) in seed_artifacts {
        bus.seed(name, value.clone());
    }

    // (#1530) COMPOSITION CHECK: every declared `requires()` Artifact port
    // must actually be on the bus by now.
    //
    // Until this existed, `requires()` was declared by nine kinds and read
    // by nothing — decorative. That is what made the kinds' own
    // `.expect("… seeds this artifact before the graph runs")` calls
    // load-bearing: a graph that composed a kind whose artifact nobody
    // provides or seeds reached `run_streaming` and panicked mid-run, after
    // the mission had minted and (for coder-phase) a worktree existed.
    //
    // Generalizing the graph is what made that reachable: an operator can
    // now mix kinds across configs freely, so "this kind needs an artifact
    // this graph never supplies" is an ordinary authoring mistake rather
    // than an impossible one. Catch it HERE — before any step runs, naming
    // the artifact, the kinds that need it, and how it gets supplied.
    {
        let mut unmet: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
        let mut unregistered: Vec<String> = Vec::new();
        let mut seen_kind_ids: HashSet<&str> = HashSet::new();
        for step in steps.values() {
            if !seen_kind_ids.insert(step.kind.as_str()) {
                continue;
            }
            match kinds.get(&step.kind) {
                Ok(kind) => {
                    for port in kind.requires() {
                        if matches!(port.kind, crate::step_kinds::PortKind::Artifact(_))
                            && !bus.has(port.name)
                        {
                            unmet.entry(port.name).or_default().push(step.kind.clone());
                        }
                    }
                }
                // (#1530) An UNREGISTERED kind contributes no `provides()` to
                // the pre-scan above, so it can make a sibling's requirement
                // look unmet — e.g. a stale user-tier config still naming
                // `review.probe` (retired in #1442) loses that kind's ports and
                // the operator gets "nothing provides review.probe-selection"
                // instead of "unknown step kind". Track them so the message can
                // name the real cause rather than misdirecting — which is the
                // whole point of this change.
                Err(_) => unregistered.push(step.kind.clone()),
            }
        }
        if !unmet.is_empty() {
            let detail = unmet
                .iter()
                .map(|(artifact, kinds)| format!("`{artifact}` (required by {})", kinds.join(", ")))
                .collect::<Vec<_>>()
                .join("; ");
            anyhow::bail!(
                "darkmux: this graph declares step kind(s) that require run-scoped artifact(s) \
                 nothing in the graph provides or seeds: {detail}. An artifact reaches the bus \
                 either from a step kind that declares it in `provides()` — so adding that kind's \
                 step to the graph supplies it — or from the launcher's `seed_artifacts`. Add the \
                 providing step, or launch this config through the launcher that seeds it.{}",
                if unregistered.is_empty() {
                    String::new()
                } else {
                    format!(
                        " NOTE: this graph also names {} unregistered step kind(s) ({}) — an \
                         unregistered kind contributes no `provides()`, so fixing those may \
                         resolve the requirement(s) above.",
                        unregistered.len(),
                        unregistered.join(", ")
                    )
                }
            );
        }
    }

    let bus = std::sync::Arc::new(bus);

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

        // (#1684 Packet 2) Evaluate every ready step's operator sign-off
        // gate BEFORE it ever flips to `Running` — see this parameter's
        // own doc above and `gate::resolve_gate`. A step whose gate
        // Declines is removed from `ready_ids` here and never reaches the
        // `Running` flip below, never dispatches, and never invokes
        // `kinds.get(...)`/`run_streaming` — it fails exactly like a step
        // that was invalid before it ever ran. `apply_step_terminal` is the
        // SAME function the wave-drain path below uses for a real
        // dispatch's `Err` outcome, so a declined step is byte-for-byte
        // indistinguishable, downstream, from any other failed step: same
        // `NodeStatus::Error`, same `report.errored` membership, same
        // "step error" flow record, same durable `persist` call, same
        // "downstream dependent never becomes ready" consequence via
        // `step_is_ready`/`task_status`.
        let ready_ids: Vec<String> = {
            let mut approved: Vec<String> = Vec::with_capacity(ready_ids.len());
            for id in ready_ids {
                let step_snapshot = steps.get(&id).expect("id came from `steps` itself").clone();
                let task_snapshot = tasks
                    .get(&step_snapshot.task_id)
                    .cloned()
                    .unwrap_or_else(|| synthetic_task(&step_snapshot));
                let facts = gather_inputs(&step_snapshot, &task_snapshot, tasks, steps);
                // (#1684 QA CONSIDER) `resolve_gate` can block for an
                // arbitrarily long time (a tty prompt, an ACP round trip)
                // — the completion timestamp below is sampled AFTER it
                // returns, never before the loop starts, so a step
                // declined after a five-minute dialog doesn't carry a
                // `completed_ts` from five minutes in the past.
                match crate::gate::resolve_gate(&step_snapshot, &facts, gate.as_deref_mut()) {
                    None | Some(crate::gate::GateDecision::Approved) => approved.push(id),
                    Some(crate::gate::GateDecision::Declined { reason }) => {
                        apply_step_terminal(
                            steps,
                            tasks,
                            &mut report,
                            &mut *emit,
                            &mut *persist,
                            &id,
                            now_unix(),
                            // A declined step never dispatched at all —
                            // nothing to time, so no `StepRecord`.
                            None,
                            Err(reason),
                            Vec::new(),
                        );
                    }
                }
            }
            approved
        };

        if ready_ids.is_empty() {
            // Every step ready this wave was gate-declined — nothing left
            // to run THIS wave, but a later wave may still have work (a
            // sibling task the decline didn't touch). Loop back to
            // `step_is_ready` rather than falling through the empty-wave
            // machinery below for no reason.
            report.iterations += 1;
            continue;
        }

        let now = now_unix();
        let mut jobs = Vec::with_capacity(ready_ids.len());
        for id in &ready_ids {
            let step = steps.get_mut(id).expect("id came from `steps` itself");
            step.status = NodeStatus::Running;
            step.started_ts = Some(now);
            emit(step_lifecycle_record(step, "step start"));
            persist(step);
        }
        // (#1442 gate C3, streaming) One channel per wave: every job's
        // `StepRunCtx` holds a `Sender` clone, and the main thread drains the
        // `Receiver` LIVE while `run_bounded` executes on a sibling scoped
        // thread — so a step's per-item records reach `emit` (the scheduler's
        // own sink, lab/fleet boundary already chosen by the caller) as they
        // are produced, not batched at wave-drain. The step never touches the
        // global flow sink directly; it emits THROUGH this seam.
        //
        // (#1451 gate) The channel is UNBOUNDED on purpose. The main thread
        // drains it continuously (`rx.iter()` below) for the entire time the
        // wave runs, so records do not accumulate without bound in practice.
        // A BOUNDED channel would instead risk a worker thread BLOCKING on a
        // full queue — a deadlock hazard here, because the SAME outer
        // `thread::scope` both drains the receiver and joins the worker, so a
        // producer stalled on backpressure could wedge the drain that would
        // relieve it. `emit` is best-effort observability, never load-bearing
        // control flow, so unbounded-with-continuous-drain is the right trade.
        // (#1483 Bug 3) One channel per wave carrying BOTH live per-item flow
        // records AND each step's OWN terminal transition (see `WaveSignal`).
        // The main thread drains it and applies each `StepTerminal` the moment
        // that step's job finishes — so a fast seat's node freezes without
        // waiting for the wave's slowest sibling.
        let (tx, rx) = std::sync::mpsc::channel::<crate::step_kinds::WaveSignal>();
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
            // (#1442) Resolve the step's `bucket_group` to the scheduler-owned
            // shared bucket (get-or-create), so grouped siblings share ONE
            // allowance. Ungrouped steps carry `None` and fall back to a
            // step-scoped bucket inside the kind.
            let remote_bucket = step_snapshot
                .config
                .get("bucket_group")
                .and_then(|v| v.as_str())
                .map(|group| {
                    // (#1442 ship-2b) A launcher may stamp the group's
                    // already-resolved per-execution allowance into the
                    // step's own config (`bucket_budget`, u64) — the same
                    // self-describing-config key `dispatch.map`'s
                    // step-scoped fallback honors. Sibling steps of one
                    // group are expected to declare the SAME value; the
                    // first step to create the group's bucket wins (the
                    // bucket lives for the whole graph run). Absent, the
                    // `config_access` resolution applies as before.
                    let budget = step_snapshot
                        .config
                        .get("bucket_budget")
                        .and_then(|v| v.as_u64())
                        .unwrap_or_else(
                            darkmux_types::config_access::remote_max_tokens_per_execution,
                        );
                    bucket_groups
                        .entry(group.to_string())
                        .or_insert_with(|| {
                            std::sync::Arc::new(std::sync::Mutex::new(
                                crate::remote_budget::RemoteBudget::new(
                                    budget,
                                    crate::step_kinds::MIN_VIABLE_MAP_GRANT,
                                ),
                            ))
                        })
                        .clone()
                });
            let ctx = crate::step_kinds::StepRunCtx::new(
                Some(tx.clone()),
                remote_bucket,
                dispatch_override.clone(),
                bus.clone(),
            );
            // (#1230 Packet 3) Per-step residency classification — see the
            // trait doc on `StepKind::residency` and the module doc above.
            // Best-effort: `None` (every kind's behavior before this hook
            // existed, and every non-dispatch kind today) schedules Remote.
            // (#1530 Packet 3a) `ctx` is built ABOVE this call (moved up from
            // its original place after residency) so `residency()` can read
            // the same run-scoped `ArtifactBus` `run_streaming` uses below —
            // see `StepKind::residency`'s doc. The borrow here ends before
            // `ctx` moves into the job closure past this point.
            let residency = match kind.residency(&step_snapshot, &task_snapshot, &input, &ctx) {
                Some(placement) => crate::concurrent_dispatch::Residency::Local(placement),
                None => crate::concurrent_dispatch::Residency::Remote,
            };
            // (#1483 Bug 3) The job wrapper's OWN handle on the wave channel,
            // used to stream this step's terminal transition the instant its
            // dispatch finishes. Distinct from the `ctx` clone (which carries
            // the step's per-item records); both drop when this closure ends,
            // so the channel still closes at wave-end exactly as before.
            let term_tx = tx.clone();
            let job: crate::concurrent_dispatch::DispatchJob<StepJobResult> =
                Box::new(move || {
                    // (#1877 item 3) This step's OWN dispatch duration —
                    // timed strictly around its `run_streaming` call, on
                    // THIS job's own worker thread. Never derived from
                    // `step.started_ts` (stamped on the main thread before
                    // this wave's jobs are even built, so it would include
                    // any time this job spent queued behind `remote_cap`)
                    // and never a wall-clock around the whole wave — each
                    // sibling gets its own `Instant` pair, so a fast step
                    // sharing a wave with a slow one reports its own real
                    // duration, not the wave's.
                    let step_t0 = Instant::now();
                    let result =
                        kind.run_streaming(&step_snapshot, &task_snapshot, &input, &ctx);
                    let wall_ms = step_t0.elapsed().as_millis() as u64;
                    // That step's OWN finish time — not the wave's flush time.
                    let at = now_unix();
                    match result {
                        Ok(outcome) => {
                            let output = outcome.output;
                            // Stream the terminal transition LIVE. The main
                            // thread applies it (status + `completed_ts` +
                            // lifecycle record + persist) on receipt, freezing
                            // this seat's node while slower siblings run on.
                            let _ = term_tx.send(crate::step_kinds::WaveSignal::StepTerminal {
                                index: idx,
                                at,
                                wall_ms,
                                result: Ok(output.clone()),
                                flow_records: outcome.flow_records,
                            });
                            // The returned value is consumed by `run_bounded`
                            // for index accounting + panic reconciliation only;
                            // the main thread already applied the transition
                            // from the streamed signal, so return empty records
                            // (they went live above) to avoid a double emit.
                            Ok((StepJobResult { output }, Vec::new()))
                        }
                        Err(e) => {
                            let _ = term_tx.send(crate::step_kinds::WaveSignal::StepTerminal {
                                index: idx,
                                at,
                                wall_ms,
                                result: Err(format!("{e:#}")),
                                flow_records: Vec::new(),
                            });
                            Err(e)
                        }
                    }
                });
            jobs.push(crate::concurrent_dispatch::QueuedJob {
                index: idx,
                residency,
                job,
            });
        }

        // (#1442 gate C3) The scheduler holds NO sender past job-building —
        // only the jobs' `StepRunCtx`s and their job wrappers do. When
        // `run_bounded` finishes every job (each ctx + `term_tx` clone
        // dropped), the channel closes and the drain loop below ends naturally.
        drop(tx);
        // Run the wave on a sibling scoped thread and drain its live signals on
        // this (main) thread. `emit`/`persist`/`steps` stay main-thread-only
        // (no `Send` bound needed on the caller's closures); the `Sender`s
        // crossing into worker threads carry only owned `WaveSignal`s.
        //
        // (#1483 Bug 3) `applied` tracks which steps were already flushed live
        // from a `StepTerminal` signal, so the post-scope reconcile only has
        // to handle a step that produced NO terminal signal — never a normal
        // completion. That is not only a job that PANICKED mid-wave
        // (`run_bounded` synthesizes a terminal `Err` for it, #1452): a step
        // whose local wave was refused by `plan_waves` or failed
        // `ensure_wave_loaded` (see `concurrent_dispatch::run_local_waves`)
        // is pushed straight into `run_bounded`'s results with no job
        // closure ever having run, so it lands here too (#1877 item 3).
        let mut applied: HashSet<usize> = HashSet::new();
        let results = std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                crate::concurrent_dispatch::run_bounded(jobs, facts, est, remote_cap, host_factory)
            });
            for sig in rx.iter() {
                match sig {
                    crate::step_kinds::WaveSignal::Record(rec) => emit(rec),
                    crate::step_kinds::WaveSignal::StepTerminal { index, at, wall_ms, result, flow_records } => {
                        apply_step_terminal(
                            steps,
                            tasks,
                            &mut report,
                            &mut *emit,
                            &mut *persist,
                            &ready_ids[index],
                            at,
                            Some(wall_ms),
                            result,
                            flow_records,
                        );
                        applied.insert(index);
                    }
                }
            }
            worker.join().expect("run_bounded worker thread panicked")
        })?;

        // (#1483 Bug 3) Reconcile only the indices that never streamed a
        // terminal. Every normally-completed job already flipped its step
        // live above, so its index is in `applied` and is skipped here. A
        // panicked job left its step `Running`; `run_bounded` handed back a
        // terminal `Err` for it (#1452), applied here as an error so the run
        // fails loud rather than stranding the step. A step whose local wave
        // never dispatched at all (refused by `plan_waves`, or the wave's
        // `ensure_wave_loaded` failed) arrives here the same way, also as an
        // `Err` with no job ever having run (#1877 item 3) — see this
        // function's own errored-vs-step_records distinction above and
        // `apply_step_terminal`'s doc.
        let finished_at = now_unix();
        for (idx, outcome) in results {
            if applied.contains(&idx) {
                continue;
            }
            let id = ready_ids[idx].clone();
            let (result, flow_records) = match outcome {
                Ok((job_result, records)) => (Ok(job_result.output), records),
                Err(e) => (Err(format!("{e:#}")), Vec::new()),
            };
            apply_step_terminal(
                steps,
                tasks,
                &mut report,
                &mut *emit,
                &mut *persist,
                &id,
                finished_at,
                // Neither a panicked job's closure nor a step whose local
                // wave never dispatched (refused / load-failed, #1877 item
                // 3) reached the point where a real `wall_ms` exists — no
                // honest duration to report, so no `StepRecord` (see
                // `apply_step_terminal`'s own doc). It still lands in
                // `report.errored` above.
                None,
                result,
                flow_records,
            );
        }

        report.iterations += 1;
    }

    Ok(report)
}

/// (#1483 Bug 3) Apply one step's terminal transition — status flip,
/// `completed_ts`, batched flow records, the `step complete`/`step error`
/// lifecycle record, and a durable `persist` — to `steps`/`report`. Shared by
/// three callers: the gate-declined path (a step that never ran at all), the
/// live per-seat drain (a `WaveSignal::StepTerminal` the moment a job
/// finishes), and the post-scope reconcile — so all three flip a step
/// identically. `at` is the step's own completion epoch (seconds); `result`
/// is `Ok(output)` / `Err(message)`.
///
/// (#1877 item 3) `wall_ms` is `Some(duration)` ONLY from the live drain,
/// where a real per-step `Instant` pair exists (see the job closure above).
/// It is `None` from the other two callers, honestly:
/// - A gate-declined step never dispatched at all — nothing to time.
/// - The post-scope reconcile covers every index that never streamed a
///   live terminal, which is NOT only a panicked job's synthesized `Err`
///   (#1452): it also catches a step whose local wave was refused by
///   `plan_waves` or failed `ensure_wave_loaded` (see
///   `concurrent_dispatch::run_local_waves`), pushed straight into the
///   executor's results with no job closure ever having run. None of
///   these three sub-cases has a real duration recoverable — this
///   function does not substitute a wave-clock number or a `0` in its
///   place for any of them.
///
/// A [`crate::run_record::StepRecord`] is pushed onto `report.step_records`
/// only when `wall_ms` is `Some` — so every entry there carries a genuine,
/// per-step-measured duration, never a fabricated one.
#[allow(clippy::too_many_arguments)]
fn apply_step_terminal(
    steps: &mut BTreeMap<String, Step>,
    // (#2310 P4a) Needed ONLY for the cascade-abandon check at the bottom
    // of this function — every existing transition above it is unchanged
    // and untouched by this parameter.
    tasks: &BTreeMap<String, Task>,
    report: &mut SchedulerReport,
    emit: &mut dyn FnMut(FlowRecord),
    persist: &mut dyn FnMut(&Step),
    id: &str,
    at: u64,
    wall_ms: Option<u64>,
    result: std::result::Result<String, String>,
    flow_records: Vec<FlowRecord>,
) {
    let step = steps.get_mut(id).expect("id came from ready_ids itself");
    for record in flow_records {
        emit(record);
    }
    if let Some(wall_ms) = wall_ms {
        let rec = StepRecord {
            step_id: step.id.clone(),
            kind: step.kind.clone(),
            // (#1877 item 3) The scheduler genuinely does not know how many
            // items this step consumed/produced — that is per-kind business
            // semantics. `None`, never a lying `Some(0)`.
            items_in: None,
            items_out: None,
            wall_ms,
        };
        // (#1877, final wiring step) Stream the companion "step timing"
        // record NOW, alongside the in-memory push below, never batched to
        // the end of `run_step_graph`. See `step_timing_record`'s own doc.
        emit(step_timing_record(step, &rec));
        report.step_records.push(rec);
    }
    // (#2310 P4a review fix M1) Captured alongside `errored_task_id` so
    // `cascade_abandon` can propagate the ORIGINATING step's own id and
    // reason text — not just "some ancestor errored" — down through
    // every task it makes unreachable. `id` here is a STEP id (this
    // function's own parameter), matching the `"{step_id}: {reason}"`
    // shape `errored_steps_degenerate_reason` (darkmux-lab) already uses
    // when a caller with full graph visibility builds the same kind of
    // reason from `report.errored` — a task with only its own gathered
    // `input` (no `report`/`steps` access) can still recover an
    // IDENTICALLY SHAPED reason once `gather_inputs` forwards this text.
    let mut errored: Option<(String, String)> = None;
    match result {
        Ok(output) => {
            step.status = NodeStatus::Complete;
            step.completed_ts = Some(at);
            step.output = Some(output);
            emit(step_lifecycle_record(step, "step complete"));
            persist(step);
            report.completed.push(id.to_string());
        }
        Err(message) => {
            step.status = NodeStatus::Error;
            step.completed_ts = Some(at);
            step.output = Some(message.clone());
            emit(step_lifecycle_record(step, "step error"));
            persist(step);
            report.errored.push(id.to_string());
            errored = Some((id.to_string(), message));
        }
    }
    // (#2310 P4a) "At the moment the error lands, in the same scheduler
    // pass" — this IS that moment: every caller of `apply_step_terminal`
    // (the gate-declined path, the live per-seat drain, the post-scope
    // reconcile) routes a step's `Error` transition through here, so
    // hooking the cascade at this single chokepoint covers all three
    // uniformly rather than needing a per-caller reminder. Only fires
    // when this step's error also flips its OWNING TASK's derived status
    // to `Error` (a task with other still-`Planned`/`Running` steps
    // hasn't reached a terminal task status yet — nothing to cascade
    // from until it does).
    if let Some((origin_step_id, origin_reason)) = errored {
        let task_id = steps.get(&origin_step_id).expect("just written above").task_id.clone();
        if let Some(task) = tasks.get(&task_id) {
            if task_status(task, steps) == NodeStatus::Error {
                cascade_abandon(&task_id, &origin_step_id, &origin_reason, tasks, steps, persist);
            }
        }
    }
}

/// (#2310 P4a) `errored_task_id` just reached `NodeStatus::Error`. Roll
/// every TRANSITIVE dependent Task that does NOT accept `"error"` in its
/// own `run_on` to `NodeStatus::Abandoned`, eagerly, right now — so a
/// dependent that DOES accept `"error"` (and is therefore left alone,
/// see below) sees a resolved, terminal dependency status THIS pass
/// instead of waiting on an ancestor that will never reach `Complete`.
///
/// Mirrors `lifecycle::reconcile_phase_steps_terminal`'s existing
/// convention for "superseded by something terminal closing around it":
/// same `NodeStatus::Abandoned`, same discipline of naming why in
/// `Step.output` (plain English, not a code), same choice to persist the
/// new state without inventing a new LIVE flow-record action —
/// `STEP_LIFECYCLE_ACTIONS`'s three-string contract (`"step start"` /
/// `"step complete"` / `"step error"`, the vocabulary the mission-graph
/// lens's SSE matcher is keyed on) stays closed; the abandonment is
/// visible through persisted step state and `task_status`, not a fourth
/// live action, exactly like `reconcile_phase_steps_terminal` never
/// emits one either.
///
/// Walk shape: breadth-first over the `depends_on`/`reads` edges,
/// STARTING from `errored_task_id`'s direct dependents. A dependent
/// whose `run_on` accepts `"error"` is left untouched AND the walk does
/// not recurse past it — that task still gets a real chance to become
/// ready and run (its OTHER dependencies may already be `Complete`, or
/// it may run and itself reach `Complete`); only if it too eventually
/// reaches `Error` does the cascade continue from there, via this same
/// function, from that task's own `apply_step_terminal` call. A
/// dependent already at a terminal status (`Complete`/`Error`/
/// `Abandoned`) is left untouched — it cannot still be `Planned` this
/// far into the run — and a dependent still `Running` is left to finish
/// on its own (it could not have started before `errored_task_id`
/// reached a terminal status if it named `errored_task_id` as a
/// dependency at all under the default `run_on`, so this branch is
/// defensive, not load-bearing).
fn cascade_abandon(
    errored_task_id: &str,
    origin_step_id: &str,
    origin_reason: &str,
    tasks: &BTreeMap<String, Task>,
    steps: &mut BTreeMap<String, Step>,
    persist: &mut dyn FnMut(&Step),
) {
    cascade_abandon_with_reason(
        errored_task_id,
        &format!("{origin_step_id}: {origin_reason}"),
        tasks,
        steps,
        persist,
    );
}

/// [`cascade_abandon`]'s body, taking the ALREADY-COMPOSED reason text
/// rather than an `(origin step id, message)` pair — so a second entry
/// point ([`cascade_dead_dependents`], #2310 fix-loop C1) can propagate a
/// reason it recovered from an already-abandoned task's own `Step.output`
/// without re-wrapping it into a second `"<id>: <id>: <text>"` prefix.
/// One composition site, one shape.
fn cascade_abandon_with_reason(
    errored_task_id: &str,
    reason: &str,
    tasks: &BTreeMap<String, Task>,
    steps: &mut BTreeMap<String, Step>,
    persist: &mut dyn FnMut(&Step),
) {
    let at = now_unix();
    // (#2310 P4a review fix M1) Every step this cascade abandons — no
    // matter how many hops from `errored_task_id` — gets the SAME output
    // text: `"{origin_step_id}: {origin_reason}"`, the ORIGINATING
    // step's own id and failure message, verbatim. This is what "the
    // reason travels down the graph" means concretely: a task several
    // hops downstream that only ever sees ONE of its dependencies
    // (`gather_inputs`, now `run_on`-aware) still reads the TRUE root
    // cause, not a chain of "depends on X which depends on Y" links that
    // lose the actual failure text after the first hop. The shape
    // matches `errored_steps_degenerate_reason`'s own per-entry format
    // (`darkmux-lab`, `run_review_graph`'s errored branch) on purpose —
    // a caller with only a gathered `input` map (no `report`/`steps`
    // access) can recover an IDENTICALLY SHAPED reason from this text
    // alone.
    let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    queue.push_back(errored_task_id.to_string());
    while let Some(tid) = queue.pop_front() {
        let dependents: Vec<String> = tasks
            .iter()
            .filter(|(_, t)| {
                t.depends_on.iter().any(|d| d == &tid) || t.reads.iter().any(|d| d == &tid)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for dep_id in dependents {
            let Some(dep_task) = tasks.get(&dep_id) else { continue };
            if task_status(dep_task, steps) != NodeStatus::Planned {
                // Already terminal, or (defensively) already Running —
                // nothing this cascade should touch.
                continue;
            }
            if dep_task.run_on.iter().any(|s| s == "error") {
                // Accepts error/abandonment from its dependencies — gets a
                // real chance to run; do not abandon it, do not recurse
                // past it (its own fate cascades separately if/when IT
                // reaches Error).
                continue;
            }
            for step_id in &dep_task.step_ids {
                if let Some(s) = steps.get_mut(step_id) {
                    if s.status == NodeStatus::Planned {
                        s.status = NodeStatus::Abandoned;
                        s.completed_ts = Some(at);
                        s.output = Some(reason.to_string());
                        persist(s);
                    }
                }
            }
            queue.push_back(dep_id);
        }
    }
}

/// (#2310 fix-loop C1 / S4-2) Run [`cascade_abandon`]'s rule over the
/// WHOLE minted graph handed in — every task that has already reached
/// `Error` or `Abandoned` starts a walk over its dependents, right now.
///
/// **Why this exists as a separate entry point.** `cascade_abandon` fires
/// from `apply_step_terminal`, i.e. INSIDE a `run_step_graph` call, over
/// exactly the `tasks` map that call was given. Since #2300 the generic
/// launcher calls `run_step_graph` ONCE PER PHASE with a cumulative map
/// holding only the phases entered SO FAR — so a dependent living in a
/// LATER phase is not in the map when its dependency dies, the walk cannot
/// reach it, and it is left `Planned`. Two things then go wrong at once:
/// the leftover step never reaches a terminal status (a Finalized mission
/// persisting `Planned` steps — S4-1), and a task whose own `run_on`
/// accepts `"error"` never becomes ready either, because
/// [`dependency_satisfies_run_on`] correctly refuses a still-`Planned`
/// dependency — so the ONE declaration a config author writes to keep a
/// delivery/report task alive across a failure was inert for every edge
/// that crosses a phase boundary, which in `review-v2.json` and
/// `crawl.json` is every edge there is.
///
/// The caller runs this at PHASE ENTRY, once the phase's tasks and steps
/// have been merged into the cumulative maps and before that phase's
/// `run_step_graph` — the moment the newly-visible dependents first
/// exist. Idempotent: a task already terminal is skipped by the walk, so
/// calling it every phase re-walks nothing.
///
/// The RULE itself stays here, in `scheduler.rs`, rather than being
/// re-derived by the launcher: same `run_on` acceptance, same
/// `"<origin-step-id>: <origin reason>"` output text, same persist hook,
/// same no-new-flow-record contract (`STEP_LIFECYCLE_ACTIONS` stays three
/// strings — see `cascade_abandon`'s doc).
pub fn cascade_dead_dependents(
    tasks: &BTreeMap<String, Task>,
    steps: &mut BTreeMap<String, Step>,
    persist: &mut dyn FnMut(&Step),
) {
    // Snapshot the dead set BEFORE mutating, so a task this pass abandons
    // is not itself re-walked with a reason it only just acquired — the
    // walk already reaches everything downstream of it.
    let dead: Vec<(String, String)> = tasks
        .values()
        .filter_map(|task| {
            let status = task_status(task, steps);
            if !matches!(status, NodeStatus::Error | NodeStatus::Abandoned) {
                return None;
            }
            let src = terminal_source_step(task, status, steps)?;
            let reason = match status {
                // An already-abandoned task's own output IS the origin
                // text (`cascade_abandon` wrote it) — forward it verbatim.
                NodeStatus::Abandoned => src.output.clone()?,
                _ => format!("{}: {}", src.id, src.output.clone().unwrap_or_default()),
            };
            Some((task.id.clone(), reason))
        })
        .collect();
    for (task_id, reason) in dead {
        cascade_abandon_with_reason(&task_id, &reason, tasks, steps, persist);
    }
}

/// (#2310 fix-loop C2 / S4-1) Roll every still-`Planned` step of the named
/// tasks to `Abandoned` — the in-memory twin of
/// `lifecycle::reconcile_phase_steps_terminal`, run by the launcher the
/// moment a phase's `run_step_graph` returns.
///
/// A step of a phase whose scheduler pass has ENDED and is still `Planned`
/// can never run: its owning task either errored (its own later steps are
/// stranded by `step_is_ready`'s intra-task rule — deliberately NOT the
/// cascade's domain, see `cascade_abandon`'s doc) or its dependency chain
/// holds a status its `run_on` refuses. Leaving it `Planned` is what let a
/// Finalized mission persist non-terminal steps: the launcher's end-of-run
/// bulk save wrote the stale `Planned` back over a phase record that had
/// already closed around them.
///
/// Reason text follows the SAME vocabulary the cascade uses: when the
/// owning task has an errored step, the stranded step names it
/// (`"<step-id>: <reason>"`) so the operator reads the real cause off the
/// step itself; otherwise it gets the plain "never started" line
/// `reconcile_phase_steps_terminal` uses for the same situation. Returns
/// the ids it abandoned.
pub fn abandon_stranded_steps(
    task_ids: impl IntoIterator<Item = String>,
    tasks: &BTreeMap<String, Task>,
    steps: &mut BTreeMap<String, Step>,
    persist: &mut dyn FnMut(&Step),
) -> Vec<String> {
    let at = now_unix();
    let mut abandoned = Vec::new();
    for task_id in task_ids {
        let Some(task) = tasks.get(&task_id) else { continue };
        let reason = match terminal_source_step(task, NodeStatus::Error, steps) {
            Some(src) => {
                format!("{}: {}", src.id, src.output.clone().unwrap_or_default())
            }
            None => "not started: the phase's scheduler pass ended before this step ran"
                .to_string(),
        };
        for step_id in task.step_ids.clone() {
            if let Some(s) = steps.get_mut(&step_id) {
                if s.status == NodeStatus::Planned {
                    s.status = NodeStatus::Abandoned;
                    s.completed_ts = Some(at);
                    s.output.get_or_insert_with(|| reason.clone());
                    persist(s);
                    abandoned.push(step_id);
                }
            }
        }
    }
    abandoned
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
        run_on: crate::types::default_run_on(),
        id: step.task_id.clone(),
        phase_id: String::new(),
        description: String::new(),
        display_name: None,
        step_ids: vec![step.id.clone()],
        depends_on: Vec::new(),
        reads: Vec::new(),
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

/// The complete, canonical step-lifecycle `FlowRecord.action` vocabulary
/// (#1399 — contract 2 territory, "dispatch/lifecycle liveness must be
/// uniform across producers"). `run_step_graph` is ONE producer; the graph
/// lens's SSE matcher (`ui/src/lenses/mission/graph.ts`'s `STATUS_ACTIONS`,
/// folded into the React port #1868 — the standalone
/// `crates/darkmux-serve/assets/mission-graph.html` page this doc used to
/// cite is retired) is the consumer every producer must stay aligned
/// with. ANY execution path that runs a `Task`/`Step` graph — the generic
/// scheduler here, or a Tier-3 driver with its own runner (e.g.
/// `darkmux-lab`'s review pipeline) — emits ONLY these three strings for
/// step transitions, never a competing vocabulary, so the graph lens
/// animates identically regardless of which driver produced the run.
/// Referenced directly by this module's own conformance test AND by
/// `darkmux-lab::lab::review`'s cross-path conformance test, so the two
/// test suites assert against the SAME source of truth and cannot drift
/// apart silently.
pub const STEP_LIFECYCLE_ACTIONS: [&str; 3] = ["step start", "step complete", "step error"];

/// One `FlowRecord` for a step-lifecycle transition (`"step start"` /
/// `"step complete"` / `"step error"`). Mirrors `lifecycle.rs`'s
/// `emit_phase_transition_record` shape (`Category::Work`,
/// `Tier::Local` since these are scheduler-driven, not operator-explicit
/// like a Phase transition; `Stage::Dispatch` since a Step is
/// dispatch-shaped work).
///
/// `mission_id: None` here is deliberate, not a gap (#1641): this function
/// (and every `StepKind`'s own records that reach `run_step_graph`'s
/// `emit` closure — e.g. `dispatch.map`'s per-item "step result") is
/// scheduler-generic and structurally has no `Mission` concept of its own
/// (`darkmux-crew` doesn't own instance minting). The LAUNCHER backfills it
/// instead: every production caller wraps `emit` so a record with no
/// `mission_id` gets THIS run's id stamped on before it's written
/// (`get_or_insert`-style — never overwrites a record that already carries
/// one) — see `src/mission_launch.rs`'s and `src/mission_launch_review.rs`'s
/// (`FleetFlowEmitter`) `run_step_graph`/`run_review_graph` call sites.
/// Without that wrap, `session_id` here is CONFIG-scoped
/// (`session_id::task` hashes only `step.task_id`, a string straight out of
/// the mission config, e.g. `task-review-probe-mid-task`) — identical
/// across every mission launched from the same config, so two concurrent
/// runs collide in the viewer with no `mission_id` to tell them apart.
fn step_lifecycle_record(step: &Step, action: &str) -> FlowRecord {
    step_lifecycle_record_with_payload(step, action, None)
}

/// (#1959) Payload-carrying variant, exported so a Tier-3 bespoke driver
/// that mints its own `Step`s outside `run_step_graph` (the crawl
/// launcher — see `CLAUDE.md`'s StepKind tiering doc for why it's Tier 3)
/// can still emit the SAME canonical `"step start"`/`"step complete"`/
/// `"step error"` vocabulary (`STEP_LIFECYCLE_ACTIONS`) with its own
/// numbers in the payload, rather than inventing a competing action
/// family. Every in-crate call site routes through the 2-arg wrapper
/// above with `payload: None` — behavior unchanged.
///
/// `mission_id` is `None` here for the SAME reason the module doc on the
/// 2-arg wrapper names: this function has no `Mission` concept of its
/// own. A caller outside `run_step_graph`'s own backfill wrap (like the
/// crawl launcher) sets `.mission_id` on the returned record directly
/// before emitting it.
pub fn step_lifecycle_record_with_payload(step: &Step, action: &str, payload: Option<serde_json::Value>) -> FlowRecord {
    FlowRecord {
        ts: darkmux_flow::ts_utc_now(),
        level: if action == "step error" { Level::Warn } else { Level::Info },
        category: Category::Work,
        tier: Tier::Local,
        stage: Stage::Dispatch,
        action: action.to_string(),
        handle: step.id.clone(),
        phase_id: None,
        session_id: Some(darkmux_types::session_id::task(&step.task_id)),
        source: Some("scheduler".to_string()),
        model: None,
        reasoning: None,
        mission_id: None,
        machine_id: None,
        machine_uid: None,
        prev_hash: None,
        hash: None,
        payload,
        work_id: None,
        attempt: None,
    }
}

/// (#1877, final wiring step) The action a scheduler-produced
/// [`StepRecord`]'s companion flow record carries. See
/// [`step_timing_record`] and `SchedulerReport::step_records`'s own doc for
/// what this measures.
///
/// **Deliberately its own action, never `"step result"`.** A `StepKind`'s
/// own business-result record (`dispatch.map`'s per-item/aggregate records,
/// `darkmux_lab::lab::review`'s `review.bundle`/`review.judge`/etc.) already
/// emits under `action: "step result"`, `source: "scheduler"` or
/// `source: "review"`, for the steps that cooperate, and that record
/// carries real `items_in`/`items_out` this module cannot observe (see
/// `run_record.rs`'s module doc). Stamping the SAME action here would put
/// two records for the very same step under the same action string with
/// different, non-overlapping payload shapes, genuinely ambiguous to any
/// consumer that counts or folds by `action == "step result"`
/// (`darkmux-serve`'s `mission_graph::fold_step_finals` is exactly such a
/// consumer). A distinct action is "carry the discriminator" applied at the
/// cheapest possible layer: the action string itself, so a reader never has
/// to inspect `source`/`payload.step_id` to tell the two apart. See
/// `run_record.rs`'s module doc for the full resolution of this arc's
/// vocabulary question.
pub const STEP_TIMING_ACTION: &str = "step timing";

/// One `FlowRecord` per scheduler-produced [`StepRecord`]: the durable,
/// live-streamed counterpart of the in-memory summary
/// `SchedulerReport::step_records` collects. #1877's final wiring step:
/// before this, `StepRecord` existed and every step got one, but nothing
/// outside a synchronous caller of `run_step_graph` could ever see it. A
/// coder-phase run's own timing data was produced and immediately dropped
/// (`src/mission_launch.rs` never read `SchedulerReport::step_records`).
///
/// Emitted from [`apply_step_terminal`] at the SAME moment the `StepRecord`
/// is pushed onto the report: live, per step, never batched to the end of
/// the run (CLAUDE.md's "No blind runs": "per-event records stream to
/// durable... files as they happen, never end-of-run-only writes"). Every
/// mission that runs through [`run_step_graph`] gets this BY CONSTRUCTION,
/// with no opt-in and no cooperation required from the `StepKind` that ran.
/// This is the same "free observability" property `step_lifecycle_record`
/// already gives every step, extended to cover duration.
///
/// `payload` is `rec`'s own `serde_json::to_value`: the WIRE shape a
/// consumer reads here is byte-identical to what a synchronous caller of
/// `run_step_graph` gets off `SchedulerReport::step_records` directly.
/// There is exactly one `StepRecord` JSON shape in the tree, never two.
///
/// `mission_id: None` for the same reason [`step_lifecycle_record`]
/// leaves it `None`. See that function's own doc. `session_id` uses the
/// SAME `session_id::task(&step.task_id)` convention as the lifecycle
/// records for this step, so a consumer can join "step start"/"step
/// complete"/"step timing" for one step by `session_id` + `handle`.
fn step_timing_record(step: &Step, rec: &StepRecord) -> FlowRecord {
    FlowRecord {
        ts: darkmux_flow::ts_utc_now(),
        level: Level::Info,
        category: Category::Work,
        tier: Tier::Local,
        stage: Stage::Dispatch,
        action: STEP_TIMING_ACTION.to_string(),
        handle: step.id.clone(),
        phase_id: None,
        session_id: Some(darkmux_types::session_id::task(&step.task_id)),
        source: Some("scheduler".to_string()),
        model: None,
        reasoning: None,
        mission_id: None,
        machine_id: None,
        machine_uid: None,
        prev_hash: None,
        hash: None,
        payload: Some(serde_json::to_value(rec).expect("StepRecord always serializes")),
        work_id: None,
        attempt: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step_kinds::StepKindRegistry;
    use darkmux_gestalt::{mock::MockHost, FixedEstimator};
    use serde_json::json;

    /// Hermetic `host_factory` for `run_step_graph`'s own scheduling tests
    /// — these fixtures never intend to touch a real LMStudio, matching
    /// `concurrent_dispatch.rs`'s own test discipline (#1360 follow-up).
    fn mock_host_factory() -> Box<dyn ModelHost> {
        Box::new(MockHost::new())
    }

    // ─── (#1959) step_lifecycle_record_with_payload ────────────────────

    fn bare_step(id: &str) -> Step {
        Step {
            id: id.to_string(),
            task_id: "t-1".to_string(),
            gate: None,
            kind: "crawl.unit".to_string(),
            status: NodeStatus::Planned,
            config: json!(null),
            started_ts: None,
            completed_ts: None,
            output: None,
        }
    }

    /// A Tier-3 bespoke driver (the crawl launcher) that mints its own
    /// `Step`s outside `run_step_graph` can still emit the SAME canonical
    /// `"step start"`/`"step complete"`/`"step error"` vocabulary with its
    /// own payload — this is the export `step_lifecycle_record` (the
    /// 2-arg, in-crate wrapper) can't offer since it always passes `None`.
    #[test]
    fn step_lifecycle_record_with_payload_carries_the_payload_and_the_canonical_action() {
        let step = bare_step("s-0001");
        let rec = step_lifecycle_record_with_payload(
            &step,
            "step start",
            Some(json!({"workspace": "acme", "unit": "u-0001", "source": "app", "sha": "abc123"})),
        );
        assert_eq!(rec.action, "step start");
        assert!(STEP_LIFECYCLE_ACTIONS.contains(&rec.action.as_str()));
        let payload = rec.payload.expect("payload set");
        assert_eq!(payload["workspace"], "acme");
        assert_eq!(payload["unit"], "u-0001");
        // mission_id is deliberately None here — the caller stamps it
        // (see the function's own doc); this test pins that it does NOT
        // silently get set.
        assert!(rec.mission_id.is_none());
    }

    /// The 2-arg in-crate wrapper (every `run_step_graph` call site) must
    /// still emit a record with no `payload` at all — no behavior change
    /// from this packet.
    #[test]
    fn step_lifecycle_record_two_arg_wrapper_emits_no_payload() {
        let step = bare_step("s-0002");
        let rec = step_lifecycle_record(&step, "step complete");
        assert!(rec.payload.is_none());
    }

    // ─── fixtures (#1341 Task-level model) ─────────────────────────────

    /// A single-step Task: Task id `<id>`, its one Step id `<id>-step`,
    /// `Task.depends_on` set from `deps` (other TASK ids). The overwhelming
    /// majority of this codebase's real Tasks are exactly this shape.
    fn task_and_step(id: &str, deps: &[&str]) -> (Task, Step) {
        let step_id = format!("{id}-step");
        let task = Task {
            run_on: crate::types::default_run_on(),
            id: id.to_string(),
            phase_id: "p1".to_string(),
            description: format!("task {id}"),
            display_name: None,
            step_ids: vec![step_id.clone()],
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let step = Step {
            id: step_id,
            task_id: id.to_string(),
            gate: None,
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


    // ─── cross-phase reconcile (#2310 fix-loop C1/C2, S4-1/S4-2) ────────

    /// A two-step Task whose FIRST step errored — the `t-fail` shape:
    /// `s-fail` Error, `s-after` Planned forever.
    fn errored_two_step_task(id: &str, deps: &[&str]) -> (Task, Vec<Step>) {
        let (mut task, first) = task_and_step(id, deps);
        let mut first = first;
        first.status = NodeStatus::Error;
        first.output = Some("command exited with Some(3)".to_string());
        let mut second = first.clone();
        second.id = format!("{id}-step-2");
        second.status = NodeStatus::Planned;
        second.output = None;
        second.completed_ts = None;
        task.step_ids.push(second.id.clone());
        (task, vec![first, second])
    }

    fn graph_with(
        pairs: Vec<(Task, Vec<Step>)>,
    ) -> (BTreeMap<String, Task>, BTreeMap<String, Step>) {
        let mut tasks = BTreeMap::new();
        let mut steps = BTreeMap::new();
        for (t, ss) in pairs {
            for s in ss {
                steps.insert(s.id.clone(), s);
            }
            tasks.insert(t.id.clone(), t);
        }
        (tasks, steps)
    }

    fn run_on_error(mut task: Task) -> Task {
        task.run_on = vec!["complete".to_string(), "error".to_string()];
        task
    }

    /// (#2310 fix-loop C1 / S4-2) The rule `run_step_graph`'s own in-pass
    /// cascade cannot apply across a phase boundary: a dependent that only
    /// became visible in a LATER phase is abandoned now, with the
    /// ORIGINATING step's id and reason, and a dependent that accepts
    /// `"error"` is left alone so it can run.
    #[test]
    fn cascade_dead_dependents_reaches_a_dependent_minted_after_the_failure() {
        let (dead, dead_steps) = errored_two_step_task("t-fail", &[]);
        let (dep, dep_step) = task_and_step("t-dep", &["t-fail"]);
        let (chain, chain_step) = task_and_step("t-chain", &["t-dep"]);
        let (survivor, survivor_step) = task_and_step("t-dep2", &["t-fail"]);
        let (tasks, mut steps) = graph_with(vec![
            (dead, dead_steps),
            (dep, vec![dep_step]),
            (chain, vec![chain_step]),
            (run_on_error(survivor), vec![survivor_step]),
        ]);

        let mut persisted: Vec<String> = Vec::new();
        cascade_dead_dependents(&tasks, &mut steps, &mut |s| persisted.push(s.id.clone()));

        for id in ["t-dep-step", "t-chain-step"] {
            assert_eq!(steps[id].status, NodeStatus::Abandoned, "{id}");
            assert_eq!(
                steps[id].output.as_deref(),
                Some("t-fail-step: command exited with Some(3)"),
                "{id} must carry the ORIGINATING step's id and reason verbatim"
            );
        }
        // Accepts `"error"` — untouched, and the walk stops there.
        assert_eq!(steps["t-dep2-step"].status, NodeStatus::Planned);
        // The errored task's OWN stranded step is never the cascade's
        // domain (see `cascade_abandon`'s doc) — the phase-exit sweep owns it.
        assert_eq!(steps["t-fail-step-2"].status, NodeStatus::Planned);
        assert!(persisted.contains(&"t-dep-step".to_string()), "{persisted:?}");
    }

    /// Idempotent: a second pass over an already-cascaded graph re-walks a
    /// now-`Abandoned` task without re-wrapping its reason into
    /// `"<id>: <id>: <text>"` — the launcher calls this at EVERY phase entry.
    #[test]
    fn cascade_dead_dependents_is_idempotent_and_never_double_wraps_the_reason() {
        let (dead, dead_steps) = errored_two_step_task("t-fail", &[]);
        let (dep, dep_step) = task_and_step("t-dep", &["t-fail"]);
        let (chain, chain_step) = task_and_step("t-chain", &["t-dep"]);
        let (tasks, mut steps) =
            graph_with(vec![(dead, dead_steps), (dep, vec![dep_step]), (chain, vec![chain_step])]);

        cascade_dead_dependents(&tasks, &mut steps, &mut |_| {});
        let after_first = steps["t-chain-step"].output.clone();
        cascade_dead_dependents(&tasks, &mut steps, &mut |_| {});
        assert_eq!(steps["t-chain-step"].output, after_first);
        assert_eq!(
            after_first.as_deref(),
            Some("t-fail-step: command exited with Some(3)"),
            "an already-Abandoned task forwards its origin text as-is"
        );
    }

    /// (#2310 fix-loop C2 / S4-1) The phase-exit sweep terminalizes a
    /// stranded step and names the task's own failure as the reason.
    #[test]
    fn abandon_stranded_steps_terminalizes_the_errored_tasks_leftover_step() {
        let (dead, dead_steps) = errored_two_step_task("t-fail", &[]);
        let (ok, mut ok_step) = task_and_step("t-ok", &[]);
        ok_step.status = NodeStatus::Complete;
        let (tasks, mut steps) = graph_with(vec![(dead, dead_steps), (ok, vec![ok_step])]);

        let mut persisted: Vec<String> = Vec::new();
        let abandoned = abandon_stranded_steps(
            ["t-fail".to_string(), "t-ok".to_string()],
            &tasks,
            &mut steps,
            &mut |s| persisted.push(s.id.clone()),
        );

        assert_eq!(abandoned, vec!["t-fail-step-2".to_string()]);
        assert_eq!(steps["t-fail-step-2"].status, NodeStatus::Abandoned);
        assert_eq!(
            steps["t-fail-step-2"].output.as_deref(),
            Some("t-fail-step: command exited with Some(3)")
        );
        // A completed step is untouched, and nothing else is persisted.
        assert_eq!(steps["t-ok-step"].status, NodeStatus::Complete);
        assert_eq!(persisted, vec!["t-fail-step-2".to_string()]);
    }

    /// (#2310 fix-loop C3 / S4-3 / #2352 item 2) The reason reaches a
    /// `run_on: ["error"]` dependent even though the errored task's LAST
    /// step is the one that never ran.
    #[test]
    fn gather_inputs_forwards_the_errored_step_not_the_never_run_last_step() {
        let (dead, dead_steps) = errored_two_step_task("t-fail", &[]);
        let (dep, dep_step) = task_and_step("t-dep2", &["t-fail"]);
        let dep = run_on_error(dep);
        let (tasks, steps) = graph_with(vec![(dead, dead_steps), (dep, vec![dep_step])]);

        let inputs = gather_inputs(&steps["t-dep2-step"], &tasks["t-dep2"], &tasks, &steps);
        assert_eq!(
            inputs.get("t-fail").map(String::as_str),
            Some("command exited with Some(3)"),
            "got {inputs:?}"
        );
    }

    /// The default-`run_on` case is byte-identical to pre-fix behavior: a
    /// dead dependency forwards nothing at all.
    #[test]
    fn gather_inputs_default_run_on_still_forwards_nothing_from_a_dead_dependency() {
        let (dead, dead_steps) = errored_two_step_task("t-fail", &[]);
        let (dep, dep_step) = task_and_step("t-dep", &["t-fail"]);
        let (tasks, steps) = graph_with(vec![(dead, dead_steps), (dep, vec![dep_step])]);

        let inputs = gather_inputs(&steps["t-dep-step"], &tasks["t-dep"], &tasks, &steps);
        assert!(inputs.is_empty(), "got {inputs:?}");
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

    // ─── step_is_ready — `Task.run_on` (#2310 P4) ──────────────────────

    #[test]
    fn step_is_ready_false_when_dependency_errored_by_default() {
        // Default `run_on` (`["complete"]`, unset by `task_and_step`'s
        // fixture) — an errored dependency must NOT satisfy readiness,
        // same as pre-#2310 behavior. This is the "does NOT run it by
        // default" half of the brief's two-task test.
        let (task_a, step_a) = step_with_status("a", &[], NodeStatus::Error);
        let (task_b, step_b) = task_and_step("b", &["a"]);
        let (tasks, steps) = graph(vec![(task_a, step_a), (task_b.clone(), step_b.clone())]);
        assert!(
            !step_is_ready(&step_b, &task_b, &tasks, &steps),
            "an errored dependency must not satisfy a task whose run_on is the default [\"complete\"]"
        );
    }

    #[test]
    fn step_is_ready_true_when_dependency_errored_and_run_on_declares_error() {
        // A task that declares `run_on: ["complete", "error"]` becomes
        // ready once its dependency reaches ANY terminal status —
        // `Error` included — not only `Complete`. This is the "still runs
        // the second when it declares run_on error" half of the brief's
        // two-task test (`review-report-task`'s real-world shape).
        let (task_a, step_a) = step_with_status("a", &[], NodeStatus::Error);
        let (mut task_b, step_b) = task_and_step("b", &["a"]);
        task_b.run_on = vec!["complete".to_string(), "error".to_string()];
        let (tasks, steps) = graph(vec![(task_a, step_a), (task_b.clone(), step_b.clone())]);
        assert!(
            step_is_ready(&step_b, &task_b, &tasks, &steps),
            "a task declaring run_on: [\"complete\", \"error\"] must run once its dependency is TERMINAL, even if that terminal status is Error"
        );
    }

    #[test]
    fn step_is_ready_true_when_dependency_completes_and_run_on_declares_error() {
        // The `error` opt-in is additive, never a narrowing: a task
        // declaring `run_on: ["complete", "error"]` still runs on the
        // ordinary happy path where the dependency actually completes.
        let (task_a, step_a) = step_with_status("a", &[], NodeStatus::Complete);
        let (mut task_b, step_b) = task_and_step("b", &["a"]);
        task_b.run_on = vec!["complete".to_string(), "error".to_string()];
        let (tasks, steps) = graph(vec![(task_a, step_a), (task_b.clone(), step_b.clone())]);
        assert!(step_is_ready(&step_b, &task_b, &tasks, &steps));
    }

    #[test]
    fn step_is_ready_true_when_dependency_abandoned_and_run_on_declares_error_only() {
        // (#2310 P4a) `Abandoned` is the TRANSITIVE form of `Error` — a
        // task only ever reaches `Abandoned` because `cascade_abandon`
        // rolled it there when SOME ancestor of it errored. So a
        // dependent task's `run_on: ["error"]` (no "complete" — legal but
        // narrow) accepts an `Abandoned` dependency exactly like it
        // accepts an `Error` one; there is no scenario where an operator
        // wants "error" without also wanting the cascade's own shadow.
        let (task_a, step_a) = step_with_status("a", &[], NodeStatus::Abandoned);
        let (mut task_b, step_b) = task_and_step("b", &["a"]);
        task_b.run_on = vec!["error".to_string()];
        let (tasks, steps) = graph(vec![(task_a, step_a), (task_b.clone(), step_b.clone())]);
        assert!(step_is_ready(&step_b, &task_b, &tasks, &steps));
    }

    #[test]
    fn step_is_ready_later_step_needs_only_immediately_previous_same_task_step() {
        // A two-step Task: step-0 -> step-1, positional order, no
        // `Task.depends_on` involved at all for the intra-task edge.
        let task = Task {
            run_on: crate::types::default_run_on(),
            id: "multi".to_string(),
            phase_id: "p1".to_string(),
            description: "multi-step task".to_string(),
            display_name: None,
            step_ids: vec!["multi-0".to_string(), "multi-1".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let step0 = Step {
            id: "multi-0".to_string(),
            task_id: "multi".to_string(),
            gate: None,
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
            gate: None,
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

    // ─── the output ledger (#1619 — `Task.reads`) ───────────────────

    /// `task_and_step` with `reads` instead of `depends_on`.
    fn task_and_step_reading(id: &str, reads: &[&str]) -> (Task, Step) {
        let (mut task, step) = task_and_step(id, &[]);
        task.reads = reads.iter().map(|s| s.to_string()).collect();
        (task, step)
    }

    #[test]
    fn a_task_never_starts_before_the_output_it_reads_exists() {
        // (#1619) The ledger relation carries data WITHOUT a rendered edge,
        // which is only safe because ordering is enforced HERE: `reads`
        // joins `depends_on` in readiness. If this chain link is dropped,
        // the review config's judge — which now names dedup via `reads`
        // alone — dispatches against an empty docket.
        let (task_a, step_a) = task_and_step("a", &[]); // still Planned
        let (task_b, step_b) = task_and_step_reading("b", &["a"]);
        let (tasks, steps) = graph(vec![(task_a, step_a), (task_b.clone(), step_b.clone())]);
        assert!(
            !step_is_ready(&steps["b-step"], &task_b, &tasks, &steps),
            "b reads a's output — it must not be ready while a is incomplete"
        );

        let (task_a2, mut step_a2) = task_and_step("a", &[]);
        step_a2.status = NodeStatus::Complete;
        let (tasks, steps) = graph(vec![(task_a2, step_a2), (task_b.clone(), step_b)]);
        assert!(
            step_is_ready(&steps["b-step"], &task_b, &tasks, &steps),
            "once a completes, b becomes ready"
        );
    }

    #[test]
    fn gather_inputs_delivers_read_outputs_keyed_by_task_id() {
        // (#1619) A `reads` entry delivers exactly like a `depends_on`
        // entry — the ledger is the same steps map, only the access rule
        // widened. Asserted on the delivered VALUE, keyed by the read
        // task's id.
        let (task_a, mut step_a) = task_and_step("a", &[]);
        step_a.status = NodeStatus::Complete;
        step_a.output = Some("dedup docket".to_string());
        let (task_b, step_b) = task_and_step_reading("b", &["a"]);
        let (tasks, steps) = graph(vec![(task_a, step_a), (task_b.clone(), step_b.clone())]);
        let input = gather_inputs(&step_b, &task_b, &tasks, &steps);
        assert_eq!(
            input.get("a").map(String::as_str),
            Some("dedup docket"),
            "the read task's output must arrive in the input map"
        );
    }

    #[test]
    fn gather_inputs_dedups_a_task_named_in_both_relations() {
        // Legal during config migration: the same upstream in `depends_on`
        // AND `reads` must deliver ONE entry, not two copies.
        let (task_a, mut step_a) = task_and_step("a", &[]);
        step_a.status = NodeStatus::Complete;
        step_a.output = Some("once".to_string());
        let (mut task_b, step_b) = task_and_step("b", &["a"]);
        task_b.reads = vec!["a".to_string()];
        let (tasks, steps) = graph(vec![(task_a, step_a), (task_b.clone(), step_b.clone())]);
        let input = gather_inputs(&step_b, &task_b, &tasks, &steps);
        assert_eq!(input.len(), 1);
        assert_eq!(input.get("a").map(String::as_str), Some("once"));
    }

    #[test]
    fn a_reads_cycle_is_rejected_like_a_depends_on_cycle() {
        // (#1619) `reads` orders execution, so a reads loop deadlocks the
        // graph identically — it must fail at load, not hang at run.
        let (mut task_a, step_a) = task_and_step("a", &[]);
        task_a.reads = vec!["b".to_string()];
        let (task_b, step_b) = task_and_step_reading("b", &["a"]);
        let (tasks, _steps) = graph(vec![(task_a, step_a), (task_b, step_b)]);
        let err = detect_cycles(&tasks).expect_err("a↔b reads loop must be detected");
        assert!(
            err.to_string().contains("cycle"),
            "the error names the cycle: {err}"
        );
    }

    #[test]
    fn a_mixed_depends_reads_cycle_is_rejected() {
        // A cycle that crosses relations (a depends_on b, b reads a) is the
        // subtle variant — each relation alone is acyclic.
        let (task_a, step_a) = task_and_step("a", &["b"]);
        let (task_b, step_b) = task_and_step_reading("b", &["a"]);
        let (tasks, _steps) = graph(vec![(task_a, step_a), (task_b, step_b)]);
        detect_cycles(&tasks).expect_err("a mixed depends_on/reads loop must be detected");
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
            run_on: crate::types::default_run_on(),
            id: "multi".to_string(),
            phase_id: "p1".to_string(),
            description: "d".to_string(),
            display_name: None,
            step_ids: vec!["multi-0".to_string(), "multi-1".to_string()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let step0 = Step {
            id: "multi-0".to_string(),
            task_id: "multi".to_string(),
            gate: None,
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
            gate: None,
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

    #[test]
    fn gather_inputs_later_step_also_sees_its_task_s_reads_output() {
        // (#2310 P2a) Before this change, a later step in a multi-step Task
        // received ONLY its same-task predecessor's output — the Task's own
        // `reads` (or `depends_on`) reached exclusively the Task's FIRST
        // step. This asserts the chain: `multi-1` (the SECOND step of a
        // two-step task) must see BOTH its predecessor `multi-0`'s output
        // AND the task-level `reads` target's output, keyed the same way
        // each already is for a first step / a same-task successor.
        let (task_a, mut step_a) = task_and_step("a", &[]);
        step_a.status = NodeStatus::Complete;
        step_a.output = Some("a's output".to_string());

        let task = Task {
            run_on: crate::types::default_run_on(),
            id: "multi".to_string(),
            phase_id: "p1".to_string(),
            description: "d".to_string(),
            display_name: None,
            step_ids: vec!["multi-0".to_string(), "multi-1".to_string()],
            depends_on: Vec::new(),
            reads: vec!["a".to_string()],
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        };
        let step0 = Step {
            id: "multi-0".to_string(),
            task_id: "multi".to_string(),
            gate: None,
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
            gate: None,
            kind: "procedural.noop".to_string(),
            status: NodeStatus::Planned,
            config: json!(null),
            started_ts: None,
            completed_ts: None,
            output: None,
        };
        let tasks: BTreeMap<String, Task> =
            [("a".to_string(), task_a), ("multi".to_string(), task.clone())].into_iter().collect();
        let steps: BTreeMap<String, Step> = [
            ("a-step".to_string(), step_a),
            ("multi-0".to_string(), step0),
            ("multi-1".to_string(), step1.clone()),
        ]
        .into_iter()
        .collect();

        let input = gather_inputs(&step1, &task, &tasks, &steps);
        assert_eq!(
            input.get("multi-0").map(String::as_str),
            Some("step0 out"),
            "the same-task predecessor entry must still arrive (unchanged mechanism)"
        );
        assert_eq!(
            input.get("a").map(String::as_str),
            Some("a's output"),
            "the task's own `reads` output must ALSO arrive — chained onto, not \
             replaced by, the predecessor entry"
        );
        assert_eq!(input.len(), 2, "exactly the predecessor entry plus the reads entry, nothing dropped");
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
        run_step_graph(
            steps,
            tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |r| emitted.push(r),
            &mut |_step| {},
            None,
            None,
            &[],
        )
        .unwrap()
    }

    // ─── run_step_graph gate wiring (#1684 Packet 2) ───────────────────
    //
    // `gate::resolve_gate` itself is unit-tested in `crate::gate`'s own
    // test module (the handler CONTRACT: ungated never invokes, recognized
    // invokes, unrecognized fails closed without invoking, the facts map
    // passes through verbatim). These tests are the SCHEDULER-level
    // integration proof: that `run_step_graph`'s wave loop actually wires
    // the ready-step gate check correctly — a declined step never flips to
    // `Running`, its downstream dependent is cascade-abandoned (#2310 P4a)
    // exactly like any other failed-dependency case (see
    // `cascade_abandon`), and a gate with no handler supplied still fails
    // closed end to end (not just at the `gate` module's own unit-test
    // level).

    #[test]
    fn run_step_graph_never_invokes_the_gate_handler_for_an_ungated_step() {
        let (task_a, step_a) = task_and_step("a", &[]); // gate: None (task_and_step's default)
        let (tasks, mut steps) = graph(vec![(task_a, step_a)]);
        let kinds = StepKindRegistry::with_builtins();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let mut calls = 0;
        let mut handler = |_s: &Step, _f: &BTreeMap<String, String>| {
            calls += 1;
            crate::gate::GateDecision::Approved
        };
        let report = run_step_graph(
            &mut steps,
            &tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |_r| {},
            &mut |_s| {},
            Some(&mut handler),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(calls, 0, "an ungated step must never reach the gate handler");
        assert_eq!(steps["a-step"].status, NodeStatus::Complete);
        assert_eq!(report.completed, vec!["a-step".to_string()]);
    }

    #[test]
    fn run_step_graph_runs_the_step_when_the_gate_approves() {
        let (mut task_a, mut step_a) = task_and_step("a", &[]);
        step_a.gate = Some(crate::gate::GATE_KIND_OPERATOR.to_string());
        task_a.description = "gated task".to_string();
        let (tasks, mut steps) = graph(vec![(task_a, step_a)]);
        let kinds = StepKindRegistry::with_builtins();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let mut handler = |_s: &Step, _f: &BTreeMap<String, String>| crate::gate::GateDecision::Approved;
        let report = run_step_graph(
            &mut steps,
            &tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |_r| {},
            &mut |_s| {},
            Some(&mut handler),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(steps["a-step"].status, NodeStatus::Complete, "an approved gated step must still run");
        assert_eq!(report.completed, vec!["a-step".to_string()]);
        assert!(steps["a-step"].started_ts.is_some(), "an approved step actually ran (started_ts set)");
    }

    #[test]
    fn run_step_graph_declined_gate_fails_the_step_and_downstream_never_becomes_ready() {
        let (mut task_a, mut step_a) = task_and_step("a", &[]);
        step_a.gate = Some(crate::gate::GATE_KIND_OPERATOR.to_string());
        task_a.description = "gated task".to_string();
        let (task_b, step_b) = task_and_step("b", &["a"]);
        let (tasks, mut steps) = graph(vec![(task_a, step_a), (task_b, step_b)]);
        let kinds = StepKindRegistry::with_builtins();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let mut emitted: Vec<FlowRecord> = Vec::new();
        let mut handler = |_s: &Step, _f: &BTreeMap<String, String>| crate::gate::GateDecision::Declined {
            reason: "operator declined".to_string(),
        };
        let report = run_step_graph(
            &mut steps,
            &tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |r| emitted.push(r),
            &mut |_s| {},
            Some(&mut handler),
            None,
            &[],
        )
        .unwrap();

        // The declined step: terminal Error, byte-for-byte the same shape
        // any other failed step gets — never `Running` first (started_ts
        // stays unset — it never actually dispatched).
        assert_eq!(steps["a-step"].status, NodeStatus::Error);
        assert_eq!(steps["a-step"].output.as_deref(), Some("operator declined"));
        assert!(steps["a-step"].started_ts.is_none(), "a declined step never flips to Running");
        assert!(report.errored.contains(&"a-step".to_string()));
        assert!(
            emitted.iter().any(|r| r.action == "step error" && r.handle == "a-step"),
            "a declined step still emits the ordinary \"step error\" lifecycle record: {emitted:?}"
        );
        assert!(
            !emitted.iter().any(|r| r.action == "step start" && r.handle == "a-step"),
            "a declined step never emits \"step start\" — it never started: {emitted:?}"
        );

        // The downstream dependent: (#2310 P4a) cascade-abandoned in the
        // SAME pass — exactly the "downstream of an errored task never
        // runs" behavior any other failed step gets, now made an eager,
        // observable terminal status instead of a silent permanent
        // `Planned` wedge. `b`'s `run_on` is the default (`["complete"]`,
        // `task_and_step`'s fixture) — it does not accept "error", so it
        // is abandoned rather than left to run.
        assert_eq!(steps["b-step"].status, NodeStatus::Abandoned);
        assert!(!report.completed.contains(&"b-step".to_string()));
        assert!(!report.errored.contains(&"b-step".to_string()));
    }

    #[test]
    fn run_step_graph_gate_with_no_handler_supplied_fails_closed() {
        let (mut task_a, mut step_a) = task_and_step("a", &[]);
        step_a.gate = Some(crate::gate::GATE_KIND_OPERATOR.to_string());
        task_a.description = "gated task".to_string();
        let (tasks, mut steps) = graph(vec![(task_a, step_a)]);
        let report = run_test_graph(&tasks, &mut steps); // passes `None` for gate

        assert_eq!(
            steps["a-step"].status,
            NodeStatus::Error,
            "a gated step with no handler supplied must fail closed, never silently run"
        );
        assert!(steps["a-step"].output.as_deref().unwrap_or("").contains("operator sign-off"));
        assert!(report.errored.contains(&"a-step".to_string()));
    }

    #[test]
    fn run_step_graph_unrecognized_gate_kind_fails_closed_without_invoking_the_handler() {
        let (mut task_a, mut step_a) = task_and_step("a", &[]);
        step_a.gate = Some("some-future-kind".to_string());
        task_a.description = "gated task".to_string();
        let (tasks, mut steps) = graph(vec![(task_a, step_a)]);
        let kinds = StepKindRegistry::with_builtins();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let mut calls = 0;
        let mut handler = |_s: &Step, _f: &BTreeMap<String, String>| {
            calls += 1;
            crate::gate::GateDecision::Approved
        };
        let report = run_step_graph(
            &mut steps,
            &tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |_r| {},
            &mut |_s| {},
            Some(&mut handler),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(calls, 0, "an unrecognized gate kind must never reach the handler");
        assert_eq!(steps["a-step"].status, NodeStatus::Error);
        assert!(report.errored.contains(&"a-step".to_string()));
    }

    #[test]
    fn run_step_graph_gate_handler_receives_the_composed_upstream_facts() {
        let (task_a, mut step_a) = task_and_step("a", &[]);
        step_a.status = NodeStatus::Complete;
        step_a.output = Some("gathered facts here".to_string());
        let (mut task_b, mut step_b) = task_and_step("b", &["a"]);
        step_b.gate = Some(crate::gate::GATE_KIND_OPERATOR.to_string());
        task_b.description = "gated task".to_string();
        let (tasks, mut steps) = graph(vec![(task_a, step_a), (task_b, step_b)]);
        let kinds = StepKindRegistry::with_builtins();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let mut received: Option<BTreeMap<String, String>> = None;
        let mut handler = |_s: &Step, f: &BTreeMap<String, String>| {
            received = Some(f.clone());
            crate::gate::GateDecision::Approved
        };
        run_step_graph(
            &mut steps,
            &tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |_r| {},
            &mut |_s| {},
            Some(&mut handler),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(
            received.as_ref().and_then(|f| f.get("a")).map(String::as_str),
            Some("gathered facts here"),
            "the gate handler's facts map must be the SAME composed upstream input the step \
             would run with (the dialog-body contract) — got {received:?}"
        );
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

    /// (#1452) A step kind that PANICS in `run` must leave its Step terminal
    /// (`Error`) and error the run — never stranded `Running` in a run the
    /// scheduler reports as success. This is the scheduler-facing half of the
    /// `run_bounded` panic-reconcile fix: `run_bounded` now returns the
    /// panicked job as a terminal `Err`, which this loop flips to `Error` and
    /// persists, so the panic surfaces as an errored step + an errored run.
    /// A sibling step in the same wave still completes.
    #[test]
    fn run_step_graph_panicking_step_persists_terminal_error_never_running() {
        use crate::step_kinds::{StepKind, StepOutcome};

        struct PanicKind;
        impl StepKind for PanicKind {
            fn id(&self) -> &'static str {
                "test.panic"
            }
            fn run(
                &self,
                _step: &Step,
                _task: &Task,
                _input: &BTreeMap<String, String>,
            ) -> Result<StepOutcome> {
                panic!("test.panic: intentional panic in run");
            }
        }

        let (task_p, mut step_p) = task_and_step("panics", &[]);
        step_p.kind = "test.panic".to_string();
        let (task_ind, step_ind) = task_and_step("independent", &[]);
        let (tasks, mut steps) = graph(vec![(task_p, step_p), (task_ind, step_ind)]);

        let kinds = StepKindRegistry::with_builtins();
        kinds.register(std::sync::Arc::new(PanicKind)).expect("register test.panic");
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let report = run_step_graph(
            &mut steps,
            &tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |_r| {},
            &mut |_s| {},
            None,
            None,
            &[],
        )
        .expect("the scheduler returns Ok even when a step panics — the panic is a per-step error");

        assert_eq!(
            steps["panics-step"].status,
            NodeStatus::Error,
            "a panicking step persists terminal Error, never a stranded Running"
        );
        assert!(
            report.errored.contains(&"panics-step".to_string()),
            "the run reports the panicked step as errored: {report:?}"
        );
        assert_eq!(
            steps["independent-step"].status,
            NodeStatus::Complete,
            "an independent step still completes despite the sibling panic"
        );
    }

    #[test]
    fn run_step_graph_downstream_task_of_errored_task_is_cascade_abandoned() {
        // (#2310 P4a) Pre-cascade, this task wedged permanently `Planned`
        // (see this test's old name) — nobody watching the graph could
        // distinguish "still running" from "will never run" without
        // walking the whole dependency chain by hand. `run_on` defaults
        // to `["complete"]` (`task_and_step`'s fixture), which does not
        // accept "error", so `cascade_abandon` rolls it to `Abandoned`
        // eagerly, in the SAME scheduler pass the upstream error lands in.
        let (task_fails, mut step_fails) = task_and_step("fails", &[]);
        step_fails.kind = "procedural.shell".to_string();
        step_fails.config = json!({"command": "exit 1"});
        let (task_down, step_down) = task_and_step("downstream", &["fails"]);
        let (tasks, mut steps) = graph(vec![(task_fails, step_fails), (task_down, step_down)]);

        let report = run_test_graph(&tasks, &mut steps);

        assert_eq!(steps["fails-step"].status, NodeStatus::Error);
        assert_eq!(
            steps["downstream-step"].status,
            NodeStatus::Abandoned,
            "a default-run_on downstream of an errored task dependency is cascade-abandoned, not left wedged Planned"
        );
        // (#2310 P4a review fix M1) The abandoned step's output carries
        // the ORIGINATING step's id + its own failure text VERBATIM —
        // "the reason travels down the graph" — not a generic "depends
        // on X" placeholder that would lose the actual reason a single
        // hop downstream.
        let origin_reason = steps["fails-step"].output.clone().expect("fails-step recorded its own error");
        assert_eq!(steps["downstream-step"].output.as_deref(), Some(format!("fails-step: {origin_reason}").as_str()));
        assert!(!report.completed.contains(&"downstream-step".to_string()));
        assert!(!report.errored.contains(&"downstream-step".to_string()));
    }
    // ─── cascade_abandon (#2310 P4a) ────────────────────────────────────

    /// A→B→C→D straight-line chain (B depends on A, C depends on B, D
    /// depends on C). `run_on` is set on B/C/D independently by the
    /// caller so the same builder serves tests (a) and (b) below.
    fn chain_a_fails(d_run_on: Option<Vec<&str>>) -> (BTreeMap<String, Task>, BTreeMap<String, Step>) {
        let (task_a, mut step_a) = task_and_step("a", &[]);
        step_a.kind = "procedural.shell".to_string();
        step_a.config = json!({"command": "exit 1"});
        let (task_b, step_b) = task_and_step("b", &["a"]);
        let (task_c, step_c) = task_and_step("c", &["b"]);
        let (mut task_d, step_d) = task_and_step("d", &["c"]);
        if let Some(values) = d_run_on {
            task_d.run_on = values.into_iter().map(String::from).collect();
        }
        graph(vec![(task_a, step_a), (task_b, step_b), (task_c, step_c), (task_d, step_d)])
    }

    #[test]
    fn cascade_abandon_test_a_chain_with_accepting_leaf_runs_the_leaf() {
        // (a) A errors ⇒ B and C (both default run_on) are cascade-
        // abandoned; D (run_on: ["complete", "error"]) sees its one
        // dependency (C) reach a terminal status THIS pass and runs.
        let (tasks, mut steps) = chain_a_fails(Some(vec!["complete", "error"]));
        let report = run_test_graph(&tasks, &mut steps);

        assert_eq!(steps["a-step"].status, NodeStatus::Error);
        assert_eq!(steps["b-step"].status, NodeStatus::Abandoned, "B does not accept error — cascade-abandoned");
        assert_eq!(steps["c-step"].status, NodeStatus::Abandoned, "C does not accept error — cascade-abandoned");
        assert_eq!(
            steps["d-step"].status,
            NodeStatus::Complete,
            "D declares run_on: [\"complete\",\"error\"] — its abandoned dependency C satisfies \
             readiness and D actually runs to completion"
        );
        assert!(report.completed.contains(&"d-step".to_string()));
    }

    #[test]
    fn cascade_abandon_test_b_chain_without_accepting_leaf_abandons_everything() {
        // (b) Same chain, D left at the default run_on (no "error") — the
        // cascade does not stop at D either; nothing downstream of A ever
        // runs.
        let (tasks, mut steps) = chain_a_fails(None);
        let report = run_test_graph(&tasks, &mut steps);

        assert_eq!(steps["a-step"].status, NodeStatus::Error);
        assert_eq!(steps["b-step"].status, NodeStatus::Abandoned);
        assert_eq!(steps["c-step"].status, NodeStatus::Abandoned);
        assert_eq!(
            steps["d-step"].status,
            NodeStatus::Abandoned,
            "D's default run_on does not accept error — cascade continues through it too"
        );
        assert!(report.completed.is_empty(), "nothing downstream of the errored root ever runs: {report:?}");
    }

    #[test]
    fn cascade_abandon_test_c_two_deps_one_complete_one_abandoned_run_on_error_is_ready() {
        // (c) A task with TWO dependencies — one Complete, one Abandoned
        // (itself the transitive shadow of some earlier error) — and
        // run_on: ["complete", "error"] is ready: EVERY dependency
        // individually satisfies the declared run_on.
        let (task_p, step_p) = step_with_status("p", &[], NodeStatus::Complete);
        let (task_q, step_q) = step_with_status("q", &[], NodeStatus::Abandoned);
        let (mut task_x, step_x) = task_and_step("x", &["p", "q"]);
        task_x.run_on = vec!["complete".to_string(), "error".to_string()];
        let (tasks, steps) =
            graph(vec![(task_p, step_p), (task_q, step_q), (task_x.clone(), step_x.clone())]);
        assert!(step_is_ready(&step_x, &task_x, &tasks, &steps));
    }

    #[test]
    fn cascade_abandon_test_d_does_not_touch_tasks_outside_the_errored_dependents() {
        // (d) A sibling task with NO relation to the errored task (not a
        // dependent, direct or transitive) must run to completion exactly
        // as if the unrelated failure never happened — the cascade walk
        // never reaches it.
        let (tasks_map, mut steps_map) = chain_a_fails(None);
        let mut tasks = tasks_map;
        let mut steps = steps_map.clone();
        let (task_unrelated, step_unrelated) = task_and_step("unrelated", &[]);
        tasks.insert(task_unrelated.id.clone(), task_unrelated);
        steps.insert(step_unrelated.id.clone(), step_unrelated);
        steps_map = steps;

        let report = run_test_graph(&tasks, &mut steps_map);

        assert_eq!(steps_map["a-step"].status, NodeStatus::Error);
        assert_eq!(steps_map["b-step"].status, NodeStatus::Abandoned);
        assert_eq!(
            steps_map["unrelated-step"].status,
            NodeStatus::Complete,
            "a task with no dependency relation to the errored task must be untouched by the cascade"
        );
        assert!(report.completed.contains(&"unrelated-step".to_string()));
    }

    #[test]
    fn cascade_abandon_test_e_accepting_intermediate_shields_its_default_dependents() {
        // A errors. B (run_on: ["complete", "error"]) depends on A — B is
        // NOT abandoned; the cascade stops there and B gets a real chance
        // to run. C (default run_on) depends on B — since B actually
        // RUNS and COMPLETES (procedural.noop, unconditional success),
        // C's dependency is genuinely Complete, so C runs too — no
        // cascade ever reaches C at all, it was never in danger. This is
        // the branch no shipped config exercises today (review.json's
        // report-task is a leaf, not an intermediate an ordinary task
        // depends on).
        let (task_a, mut step_a) = task_and_step("a", &[]);
        step_a.kind = "procedural.shell".to_string();
        step_a.config = json!({"command": "exit 1"});
        let (mut task_b, step_b) = task_and_step("b", &["a"]);
        task_b.run_on = vec!["complete".to_string(), "error".to_string()];
        let (task_c, step_c) = task_and_step("c", &["b"]);
        let (tasks, mut steps) = graph(vec![(task_a, step_a), (task_b, step_b), (task_c, step_c)]);

        let report = run_test_graph(&tasks, &mut steps);

        assert_eq!(steps["a-step"].status, NodeStatus::Error);
        assert_eq!(
            steps["b-step"].status,
            NodeStatus::Complete,
            "B accepts error — the cascade leaves it alone and it runs to completion"
        );
        assert_eq!(
            steps["c-step"].status,
            NodeStatus::Complete,
            "C depends on B, which genuinely completed — C was never a cascade target at all"
        );
        assert!(report.completed.contains(&"b-step".to_string()));
        assert!(report.completed.contains(&"c-step".to_string()));
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
        let err = run_step_graph(
            &mut steps,
            &tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |r| emitted.push(r),
            &mut |_step| {},
            None,
            None,
            &[],
        )
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
        run_step_graph(
            &mut steps,
            &tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |r| emitted.push(r),
            &mut |_step| {},
            None,
            None,
            &[],
        )
        .unwrap();

        let actions: Vec<&str> = emitted.iter().map(|r| r.action.as_str()).collect();
        assert!(actions.contains(&"step start"));
        assert!(actions.contains(&"step complete"));
        // (#1877) The companion timing record fires for every step that
        // streamed a real terminal, "step complete" here and "step error"
        // on a step that ran and failed (see
        // `errored_step_that_actually_ran_still_gets_a_record_with_real_duration`
        // below for that case). See `step_timing_record`'s own doc.
        assert_eq!(
            actions.iter().filter(|a| **a == STEP_TIMING_ACTION).count(),
            1,
            "expected exactly one \"step timing\" record for the one step that ran: {actions:?}"
        );
        // (#1399/#1877) Every emitted step-scoped action must be drawn from
        // the canonical lifecycle vocabulary OR the documented `step
        // timing` companion, the SAME constant `darkmux-lab`'s review-path
        // conformance test asserts against, so the producers cannot drift
        // onto a competing vocabulary.
        for action in &actions {
            assert!(
                STEP_LIFECYCLE_ACTIONS.contains(action) || *action == STEP_TIMING_ACTION,
                "scheduler emitted an action outside the canonical step-lifecycle vocabulary \
                 or the documented `step timing` companion: {action}"
            );
        }
    }

    /// (#1397) The scheduler persists each step's OWN post-flip state at
    /// transition time — `Running` at dispatch, `Complete`/`Error` at
    /// completion — not just at the end of the whole run. A `persist`
    /// closure that snapshots the step it's handed (cloned, since the
    /// real step keeps mutating after each call) proves this: by the time
    /// `run_step_graph` returns, the FIRST recorded snapshot for this
    /// step must already show `Running` (not `Planned`), matching what a
    /// mid-run page-open would have read from disk before the run
    /// finished.
    #[test]
    fn run_step_graph_persists_running_before_the_step_completes() {
        let (task_a, step_a) = task_and_step("a", &[]);
        let (tasks, mut steps) = graph(vec![(task_a, step_a)]);
        let kinds = StepKindRegistry::with_builtins();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let mut emitted: Vec<FlowRecord> = Vec::new();
        let mut persisted: Vec<Step> = Vec::new();
        run_step_graph(
            &mut steps,
            &tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |r| emitted.push(r),
            &mut |step| persisted.push(step.clone()),
            None,
            None,
            &[],
        )
        .unwrap();

        assert_eq!(persisted.len(), 2, "one persist call at Running, one at Complete: {persisted:?}");
        assert_eq!(persisted[0].status, NodeStatus::Running, "first persisted snapshot must be Running, not Planned");
        assert!(persisted[0].started_ts.is_some());
        assert_eq!(persisted[1].status, NodeStatus::Complete);
        assert!(persisted[1].completed_ts.is_some());
    }

    /// (#1397) An errored step is ALSO persisted at its transition — the
    /// hook fires on every terminal status, not just the happy path.
    #[test]
    fn run_step_graph_persists_error_status_at_transition() {
        let (task_fails, mut step_fails) = task_and_step("fails", &[]);
        step_fails.kind = "procedural.shell".to_string();
        step_fails.config = json!({"command": "exit 1"});
        let (tasks, mut steps) = graph(vec![(task_fails, step_fails)]);
        let kinds = StepKindRegistry::with_builtins();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let mut emitted: Vec<FlowRecord> = Vec::new();
        let mut persisted: Vec<Step> = Vec::new();
        run_step_graph(
            &mut steps,
            &tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |r| emitted.push(r),
            &mut |step| persisted.push(step.clone()),
            None,
            None,
            &[],
        )
        .unwrap();

        assert_eq!(persisted.last().unwrap().status, NodeStatus::Error);
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

    // ─── (#1442) StepRunCtx seam: bucket groups + live streaming ─────────

    use crate::step_kinds::{StepKind, StepOutcome, StepRunCtx};
    use std::sync::{Arc, Mutex};

    /// A step with a specific kind id + config, chained after `deps`.
    fn kinded_step(id: &str, kind: &str, config: serde_json::Value, deps: &[&str]) -> (Task, Step) {
        let (task, mut step) = task_and_step(id, deps);
        step.kind = kind.to_string();
        step.config = config;
        (task, step)
    }

    /// (#1442) Draws from whatever bucket the scheduler handed it: admits
    /// once, then spends the WHOLE remaining allowance (so a shared bucket is
    /// exhausted for the next grouped sibling). Records `(step_id,
    /// had_shared_bucket, admitted)` so a test can prove one allowance was
    /// shared across siblings — vs an ungrouped step getting `None`.
    struct BucketProbeKind {
        log: Arc<Mutex<Vec<(String, bool, bool)>>>,
    }
    impl StepKind for BucketProbeKind {
        fn id(&self) -> &'static str {
            "test.bucket-probe"
        }
        fn run(&self, _s: &Step, _t: &Task, _i: &BTreeMap<String, String>) -> Result<StepOutcome> {
            Ok(StepOutcome { output: "ctx-free".to_string(), flow_records: vec![] })
        }
        fn run_streaming(
            &self,
            step: &Step,
            _t: &Task,
            _i: &BTreeMap<String, String>,
            ctx: &StepRunCtx,
        ) -> Result<StepOutcome> {
            let entry = match ctx.remote_bucket() {
                Some(b) => {
                    let mut g = b.lock().expect("bucket poisoned");
                    // Reserve the WHOLE remaining allowance (u32::MAX
                    // requested clamps to what's left) — the reservation is
                    // never settled down, so one admitted step exhausts the
                    // shared bucket for its siblings, the same shape the old
                    // admit-then-spend(remaining) pair produced.
                    let admitted = g.admit_reserve(u32::MAX).is_some();
                    (step.id.clone(), true, admitted)
                }
                None => (step.id.clone(), false, false),
            };
            self.log.lock().unwrap().push(entry);
            Ok(StepOutcome { output: "ok".to_string(), flow_records: vec![] })
        }
    }

    fn run_graph_with_kind(
        kind: Arc<dyn StepKind>,
        tasks: &BTreeMap<String, Task>,
        steps: &mut BTreeMap<String, Step>,
    ) -> Vec<FlowRecord> {
        let kinds = StepKindRegistry::new();
        kinds.register(kind).unwrap();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let mut emitted = Vec::new();
        run_step_graph(
            steps,
            tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |r| emitted.push(r),
            &mut |_step| {},
            None,
            None,
            &[],
        )
        .unwrap();
        emitted
    }

    #[test]
    #[serial_test::serial]
    fn bucket_group_siblings_share_one_allowance_between_them() {
        // Two CHAINED grouped steps (deterministic order, distinct waves)
        // both name bucket_group "probe". The budget only funds ONE full
        // draw, so step A admits + exhausts and step B is refused — proving
        // the scheduler handed both the SAME shared bucket, not a fresh
        // per-step allowance each.
        let k = "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION";
        let prev = std::env::var(k).ok();
        unsafe {
            std::env::set_var(k, "100");
        }
        let log = Arc::new(Mutex::new(Vec::new()));
        let kind = Arc::new(BucketProbeKind { log: log.clone() });
        let cfg = json!({ "bucket_group": "probe" });
        let (ta, sa) = kinded_step("a", "test.bucket-probe", cfg.clone(), &[]);
        let (tb, sb) = kinded_step("b", "test.bucket-probe", cfg, &["a"]);
        let (tasks, mut steps) = graph(vec![(ta, sa), (tb, sb)]);

        run_graph_with_kind(kind, &tasks, &mut steps);
        unsafe {
            match prev {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }

        let entries = log.lock().unwrap().clone();
        let a = entries.iter().find(|e| e.0 == "a-step").expect("a ran");
        let b = entries.iter().find(|e| e.0 == "b-step").expect("b ran");
        assert!(a.1 && b.1, "both grouped steps got a scheduler-supplied shared bucket");
        assert!(a.2, "step A admitted (fresh shared allowance)");
        assert!(!b.2, "step B refused — the SAME bucket A exhausted, one allowance shared between them");
    }

    #[test]
    #[serial_test::serial]
    fn ungrouped_step_gets_no_shared_bucket() {
        // A step naming NO bucket_group is handed `None` — it falls back to a
        // step-scoped bucket inside its own kind, never joining a group.
        let log = Arc::new(Mutex::new(Vec::new()));
        let kind = Arc::new(BucketProbeKind { log: log.clone() });
        let (ta, sa) = kinded_step("solo", "test.bucket-probe", json!({}), &[]);
        let (tasks, mut steps) = graph(vec![(ta, sa)]);

        run_graph_with_kind(kind, &tasks, &mut steps);

        let entries = log.lock().unwrap().clone();
        let solo = entries.iter().find(|e| e.0 == "solo-step").expect("ran");
        assert!(!solo.1, "ungrouped step receives no scheduler-shared bucket (step-scoped instead)");
    }

    // ─── #1530 Packet 0: the run-scoped `ArtifactBus` ──────────────────

    /// (#1530 Packet 0) Declares ONE `Artifact` port (`"test.shared-log"`,
    /// a `Mutex<Vec<String>>`) and appends its own step id to it when run.
    /// Proves the scheduler materializes the artifact from `provides()`
    /// before the wave loop starts.
    struct ArtifactWriterKind;
    impl StepKind for ArtifactWriterKind {
        fn id(&self) -> &'static str {
            "test.artifact-writer"
        }
        fn provides(&self) -> &'static [crate::step_kinds::Port] {
            const PORTS: [crate::step_kinds::Port; 1] = [crate::step_kinds::Port::artifact(
                "test.shared-log",
                || Arc::new(Mutex::new(Vec::<String>::new())),
            )];
            &PORTS
        }
        fn run(&self, _s: &Step, _t: &Task, _i: &BTreeMap<String, String>) -> Result<StepOutcome> {
            panic!("ArtifactWriterKind is only ever exercised through run_streaming in this test");
        }
        fn run_streaming(
            &self,
            step: &Step,
            _t: &Task,
            _i: &BTreeMap<String, String>,
            ctx: &StepRunCtx,
        ) -> Result<StepOutcome> {
            let log = ctx
                .artifact::<Mutex<Vec<String>>>("test.shared-log")
                .expect("the scheduler materialized this port from `provides()` before the wave ran");
            log.lock().unwrap().push(step.id.clone());
            Ok(StepOutcome { output: "wrote".to_string(), flow_records: vec![] })
        }
    }

    /// (#1530 Packet 0) Reads the SAME `"test.shared-log"` artifact (it
    /// declares no `provides` of its own — only the writer's declaration
    /// materializes it) and returns its current contents as `StepOutcome
    /// ::output`, so the test can assert on what step B saw via the
    /// completed step's persisted output.
    struct ArtifactReaderKind;
    impl StepKind for ArtifactReaderKind {
        fn id(&self) -> &'static str {
            "test.artifact-reader"
        }
        fn run(&self, _s: &Step, _t: &Task, _i: &BTreeMap<String, String>) -> Result<StepOutcome> {
            panic!("ArtifactReaderKind is only ever exercised through run_streaming in this test");
        }
        fn run_streaming(
            &self,
            _s: &Step,
            _t: &Task,
            _i: &BTreeMap<String, String>,
            ctx: &StepRunCtx,
        ) -> Result<StepOutcome> {
            let log = ctx
                .artifact::<Mutex<Vec<String>>>("test.shared-log")
                .expect("the writer step's `provides()` materialized this port for the whole run");
            let seen = log.lock().unwrap().join(",");
            Ok(StepOutcome { output: seen, flow_records: vec![] })
        }
    }

    #[test]
    fn artifact_bus_shares_one_instance_across_steps_in_a_run() {
        // Two CHAINED steps (B depends on A, so they land in distinct
        // waves, exactly like the bucket_group test above) of DIFFERENT
        // kinds: A writes its id into the shared artifact, B reads the
        // artifact back. B seeing A's write proves the scheduler
        // materialized ONE artifact instance (from A's `provides()`) and
        // shared the SAME `Arc` into both steps' `StepRunCtx` — not a
        // fresh instance per step.
        let kinds = StepKindRegistry::new();
        kinds.register(Arc::new(ArtifactWriterKind)).unwrap();
        kinds.register(Arc::new(ArtifactReaderKind)).unwrap();
        let (ta, sa) = kinded_step("a", "test.artifact-writer", json!({}), &[]);
        let (tb, sb) = kinded_step("b", "test.artifact-reader", json!({}), &["a"]);
        let (tasks, mut steps) = graph(vec![(ta, sa), (tb, sb)]);

        let facts = Facts::default();
        let est = FixedEstimator::default();
        run_step_graph(
            &mut steps,
            &tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |_r| {},
            &mut |_step| {},
            None,
            None,
            &[],
        )
        .unwrap();

        let b_output = steps.get("b-step").expect("b ran").output.clone();
        assert_eq!(
            b_output,
            Some("a-step".to_string()),
            "step B's artifact read should see step A's write through the ONE shared bus instance"
        );
    }

    /// (#1483 Bug 3) A step kind that sleeps `config.sleep_ms` before
    /// returning — a deterministic way to make one sibling in a wave finish
    /// well after another so the ORDER terminal transitions land is
    /// observable.
    struct SleepKind;
    impl StepKind for SleepKind {
        fn id(&self) -> &'static str {
            "test.sleep"
        }
        fn run(&self, step: &Step, _t: &Task, _i: &BTreeMap<String, String>) -> Result<StepOutcome> {
            let ms = step.config.get("sleep_ms").and_then(|v| v.as_u64()).unwrap_or(0);
            std::thread::sleep(std::time::Duration::from_millis(ms));
            Ok(StepOutcome { output: step.id.clone(), flow_records: vec![] })
        }
    }

    /// (#1483 Bug 3) Per-seat live completion: within ONE wave, a fast seat's
    /// terminal transition must be emitted the moment ITS OWN job finishes,
    /// not batched at wave-drain behind the slowest sibling. The slow step
    /// sorts FIRST in `ready_ids` (BTreeMap key order), so the OLD wave-drain
    /// code — which flipped steps in `ready_ids` order after `run_bounded`
    /// returned — emitted the slow step's `step complete` FIRST regardless of
    /// actual finish time. With per-seat streaming the FAST sibling's terminal
    /// lands first. This is exactly the live bug (a done seat's node stayed
    /// `running` until the slow gpt-oss seat finished and the whole wave
    /// flushed).
    #[test]
    fn per_seat_terminal_streams_at_each_job_finish_not_wave_drain() {
        let kind = Arc::new(SleepKind);
        let (ta, sa) = kinded_step("a-slow", "test.sleep", json!({ "sleep_ms": 250 }), &[]);
        let (tb, sb) = kinded_step("b-fast", "test.sleep", json!({ "sleep_ms": 0 }), &[]);
        let (tasks, mut steps) = graph(vec![(ta, sa), (tb, sb)]);

        let emitted = run_graph_with_kind(kind, &tasks, &mut steps);

        let complete_pos = |handle: &str| {
            emitted
                .iter()
                .position(|r| r.action == "step complete" && r.handle == handle)
                .unwrap_or_else(|| panic!("no `step complete` emitted for {handle}"))
        };
        let fast = complete_pos("b-fast-step");
        let slow = complete_pos("a-slow-step");
        assert!(
            fast < slow,
            "the fast seat's terminal must stream before the slow sibling's \
             (per-seat completion, not wave-drain) — got fast={fast} slow={slow}"
        );
        // Both still reach a terminal Complete — per-seat streaming changes
        // WHEN each flips, never WHETHER the wave completes.
        assert_eq!(steps["b-fast-step"].status, NodeStatus::Complete);
        assert_eq!(steps["a-slow-step"].status, NodeStatus::Complete);
    }

    /// (#1483 Bug 3) Per-seat early completion must NOT relax the wave
    /// scheduling barrier: a step that DEPENDS on a whole wave still waits for
    /// EVERY sibling in that wave to finish. The fast probe flipping to
    /// Complete live cannot let the dependent (dedup-shaped) step start before
    /// the slow probe is also done. Proven by ordering: the dependent's `step
    /// start` must come AFTER both probes' `step complete`.
    #[test]
    fn dependent_step_still_waits_for_the_whole_wave() {
        let kind = Arc::new(SleepKind);
        // Two independent probe tasks (one slow, one fast) in wave 1; a third
        // task depends on BOTH, so it can only run in wave 2 once the whole
        // wave-1 barrier clears.
        let (ta, sa) = kinded_step("a-slow", "test.sleep", json!({ "sleep_ms": 200 }), &[]);
        let (tb, sb) = kinded_step("b-fast", "test.sleep", json!({ "sleep_ms": 0 }), &[]);
        let (tc, sc) = kinded_step("c-dep", "test.sleep", json!({ "sleep_ms": 0 }), &["a-slow", "b-fast"]);
        let (tasks, mut steps) = graph(vec![(ta, sa), (tb, sb), (tc, sc)]);

        let emitted = run_graph_with_kind(kind, &tasks, &mut steps);

        let pos = |action: &str, handle: &str| {
            emitted
                .iter()
                .position(|r| r.action == action && r.handle == handle)
                .unwrap_or_else(|| panic!("no `{action}` for {handle}"))
        };
        let dep_start = pos("step start", "c-dep-step");
        let slow_done = pos("step complete", "a-slow-step");
        let fast_done = pos("step complete", "b-fast-step");
        assert!(
            dep_start > slow_done && dep_start > fast_done,
            "the dependent step must not start until BOTH wave-1 siblings finish \
             (barrier preserved) — dep_start={dep_start} slow_done={slow_done} fast_done={fast_done}"
        );
        assert_eq!(steps["c-dep-step"].status, NodeStatus::Complete);
    }

    // ─── #1877 item 3: the scheduler times and emits StepRecords itself ─

    /// (#1877 item 3) `run_step_graph` produces a real `StepRecord` for a
    /// step with NO cooperation from its `StepKind` — `procedural.noop` (a
    /// builtin that knows nothing about `run_record`) reaches this through
    /// the wave loop alone. **Proved failing first**: before this PR,
    /// `SchedulerReport` had no `step_records` field at all, so
    /// `report.step_records` failed to compile
    /// (`no field \`step_records\` on type \`SchedulerReport\``); this test
    /// then failed for real once the field existed but nothing populated it
    /// (`report.step_records` was empty). Both observed directly while
    /// writing this test, before `apply_step_terminal` grew its push.
    #[test]
    fn every_step_kind_gets_a_record_without_opting_in() {
        let (task_a, step_a) = task_and_step("a", &[]); // default kind: "procedural.noop"
        let (tasks, mut steps) = graph(vec![(task_a, step_a)]);
        let report = run_test_graph(&tasks, &mut steps);

        assert_eq!(report.step_records.len(), 1, "one record for the one step that ran");
        let rec = &report.step_records[0];
        assert_eq!(rec.step_id, "a-step");
        assert_eq!(rec.kind, "procedural.noop", "the scheduler names the step's own registry kind id");
        assert_eq!(rec.items_in, None, "the scheduler cannot know per-kind item counts");
        assert_eq!(rec.items_out, None, "same honesty contract on the output side");
    }

    /// (#1877, final wiring step) `SchedulerReport::step_records` used to
    /// be produced and immediately dropped by every real caller: nothing
    /// outside a synchronous `run_step_graph` call could ever see it (the
    /// exact gap #1877's own issue names against `src/mission_launch.rs`).
    /// This asserts the fix at its root: the record reaches the durable,
    /// operator-visible flow stream LIVE, as the step completes, not just
    /// the in-memory summary a caller might or might not read.
    ///
    /// **Proved failing first**: before `apply_step_terminal` called
    /// `emit(step_timing_record(...))`, `emitted` here carried only "step
    /// start"/"step complete"; filtering for `STEP_TIMING_ACTION` found
    /// nothing, and this test failed on the `expect` below. Observed
    /// directly while writing this test.
    ///
    /// Also pins the vocabulary decision itself: the flow record's action
    /// is `STEP_TIMING_ACTION` ("step timing"), never `"step result"`. See
    /// `step_timing_record`'s own doc for why reusing that action would be
    /// genuinely ambiguous. Its payload is `StepRecord`'s own
    /// `serde_json::to_value`, so the wire shape a flow-stream consumer
    /// reads is byte-identical to what `SchedulerReport::step_records`
    /// gives a synchronous caller directly. One shape, two surfaces, never
    /// a lossy second translation.
    #[test]
    fn step_records_reach_the_flow_stream_live_under_their_own_vocabulary() {
        let (task_a, step_a) = task_and_step("a", &[]); // default kind: "procedural.noop"
        let (tasks, mut steps) = graph(vec![(task_a, step_a)]);
        let kinds = StepKindRegistry::with_builtins();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let mut emitted: Vec<FlowRecord> = Vec::new();
        let report = run_step_graph(
            &mut steps,
            &tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |r| emitted.push(r),
            &mut |_step| {},
            None,
            None,
            &[],
        )
        .unwrap();

        assert_eq!(report.step_records.len(), 1, "one summary record for the one step that ran");

        let timing: Vec<&FlowRecord> = emitted.iter().filter(|r| r.action == STEP_TIMING_ACTION).collect();
        assert_eq!(
            timing.len(),
            1,
            "expected exactly one \"step timing\" flow record for the one step that ran, got: {:?}",
            emitted.iter().map(|r| r.action.as_str()).collect::<Vec<_>>()
        );
        let rec = timing[0];
        assert_eq!(rec.source.as_deref(), Some("scheduler"));
        assert_eq!(rec.handle, "a-step");
        assert_eq!(
            rec.payload.as_ref(),
            Some(&serde_json::to_value(&report.step_records[0]).unwrap()),
            "the flow record's payload must be the exact same StepRecord shape the summary carries"
        );
        // Never the business-result vocabulary. See `step_timing_record`'s
        // own doc on why the two must stay distinct actions.
        assert!(
            !emitted.iter().any(|r| r.action == "step result"),
            "a procedural.noop step never emits its own \"step result\"; this pins that this \
             test's \"step timing\" record is not accidentally the OTHER vocabulary"
        );
    }

    /// (#1877, final wiring step) The vocabulary decision itself, pinned
    /// directly: `STEP_TIMING_ACTION` must never collapse onto EITHER of
    /// the two vocabularies it has to coexist with: the lifecycle
    /// transitions (`STEP_LIFECYCLE_ACTIONS`) or the business-result
    /// companion (`"step result"`). A future edit that renamed the
    /// constant to reuse one of those strings (accidentally "merging the
    /// two keyings" the issue named as the hazard to avoid) would compile
    /// fine; this is the test that catches it instead.
    #[test]
    fn step_timing_action_is_pinned_distinct_from_every_other_step_vocabulary() {
        assert_eq!(STEP_TIMING_ACTION, "step timing");
        assert_ne!(STEP_TIMING_ACTION, "step result");
        assert!(
            !STEP_LIFECYCLE_ACTIONS.contains(&STEP_TIMING_ACTION),
            "STEP_TIMING_ACTION must never collide with a lifecycle transition action"
        );
    }

    /// (#1877 item 3) A step's own `StepRecord.wall_ms` is a real,
    /// non-zero, measured duration for that step's own dispatch — not a
    /// placeholder. **Proved failing first**: same compile/empty-field
    /// failures as the test above, observed the same way.
    #[test]
    fn step_record_carries_a_real_duration_and_the_right_kind() {
        let kind = Arc::new(SleepKind);
        let (ta, sa) = kinded_step("slow", "test.sleep", json!({ "sleep_ms": 60 }), &[]);
        let (tasks, mut steps) = graph(vec![(ta, sa)]);

        let kinds = StepKindRegistry::new();
        kinds.register(kind).unwrap();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let report = run_step_graph(
            &mut steps,
            &tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |_r| {},
            &mut |_s| {},
            None,
            None,
            &[],
        )
        .unwrap();

        assert_eq!(report.step_records.len(), 1);
        let rec = &report.step_records[0];
        assert_eq!(rec.step_id, "slow-step");
        assert_eq!(rec.kind, "test.sleep");
        assert!(
            rec.wall_ms >= 50,
            "a step that slept ~60ms must carry a real, non-zero, roughly-matching \
             duration, not a placeholder — got {}ms",
            rec.wall_ms
        );
    }

    /// (#1877 item 3) Concurrency correctness, the QUEUEING form — the
    /// actual reason this has to be timed strictly around
    /// `kind.run_streaming(...)` inside each job's own closure rather than
    /// from `step.started_ts` (stamped on the main thread, for every ready
    /// step, BEFORE that wave's jobs are even built — see the wave loop's
    /// own comment on why `remote_cap` can force a ready step to wait
    /// behind a sibling before its closure ever starts). Two independent
    /// siblings land in the SAME wave with `remote_cap: 1` — only ONE can
    /// run at a time. `a-slow` sorts first (`BTreeMap` key order) and
    /// occupies the sole slot for ~250ms; `b-fast` cannot even START until
    /// `a-slow` finishes, then completes near-instantly itself. A `wall_ms`
    /// measured from `step.started_ts` (set for BOTH steps before either
    /// job ran) would show `b-fast` at ~250ms too — its own near-zero
    /// dispatch plus the ~250ms it spent queued behind its sibling.
    ///
    /// **Proved failing first**: temporarily hoisted `step_t0` out of the
    /// job closure to a single `Instant::now()` shared by every job in the
    /// wave (captured once, right before the `for (idx, id) in ready_ids...`
    /// loop that builds them) instead of each closure taking its own,
    /// leaving the rest of the timing logic untouched, and reran this exact
    /// test. It failed:
    /// ```text
    /// the fast sibling slept 0ms and had to queue behind its sibling under
    /// remote_cap=1 — its record must reflect ITS OWN near-zero dispatch
    /// duration, never the ~250ms it spent waiting for the shared slot —
    /// got 256ms
    /// ```
    /// — `b-fast`'s `wall_ms` picked up `a-slow`'s ~250ms queue wait
    /// exactly as the assertion warns against. Reverted to the real
    /// per-closure `Instant` (this file's actual code) before committing.
    #[test]
    fn concurrent_sibling_steps_each_get_their_own_duration_not_the_waves() {
        let kind = Arc::new(SleepKind);
        let (ta, sa) = kinded_step("a-slow", "test.sleep", json!({ "sleep_ms": 250 }), &[]);
        let (tb, sb) = kinded_step("b-fast", "test.sleep", json!({ "sleep_ms": 0 }), &[]);
        let (tasks, mut steps) = graph(vec![(ta, sa), (tb, sb)]);

        let kinds = StepKindRegistry::new();
        kinds.register(kind).unwrap();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let report = run_step_graph(
            &mut steps,
            &tasks,
            &kinds,
            &facts,
            &est,
            // (deliberately 1, not 8) — forces `b-fast` to queue behind
            // `a-slow` instead of running truly in parallel, so a timing
            // bug that leaks queue wait into `wall_ms` has something real
            // to leak.
            1,
            &mock_host_factory,
            &mut |_r| {},
            &mut |_s| {},
            None,
            None,
            &[],
        )
        .unwrap();

        assert_eq!(report.step_records.len(), 2, "both siblings of the one wave get their own record");
        let slow = report
            .step_records
            .iter()
            .find(|r| r.step_id == "a-slow-step")
            .expect("a-slow's record");
        let fast = report
            .step_records
            .iter()
            .find(|r| r.step_id == "b-fast-step")
            .expect("b-fast's record");
        assert!(
            slow.wall_ms >= 200,
            "the slow sibling's own ~250ms sleep must show up in ITS record — got {}ms",
            slow.wall_ms
        );
        assert!(
            fast.wall_ms < 150,
            "the fast sibling slept 0ms and had to queue behind its sibling under \
             remote_cap=1 — its record must reflect ITS OWN near-zero dispatch \
             duration, never the ~250ms it spent waiting for the shared slot — got {}ms",
            fast.wall_ms
        );
    }

    /// (#1877 item 3) A step that never dispatched at all (its operator-sign-
    /// off gate declined it before `run_streaming` ever ran) gets NO
    /// `StepRecord` — there is nothing real to time, and this module never
    /// substitutes a fabricated duration. It still lands in
    /// `report.errored`, exactly like any other failed step.
    #[test]
    fn gate_declined_step_gets_no_fabricated_record() {
        let (mut task_a, mut step_a) = task_and_step("a", &[]);
        step_a.gate = Some(crate::gate::GATE_KIND_OPERATOR.to_string());
        task_a.description = "gated task".to_string();
        let (tasks, mut steps) = graph(vec![(task_a, step_a)]);
        let kinds = StepKindRegistry::with_builtins();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let mut handler = |_s: &Step, _f: &BTreeMap<String, String>| {
            crate::gate::GateDecision::Declined { reason: "operator said no".to_string() }
        };
        let report = run_step_graph(
            &mut steps,
            &tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |_r| {},
            &mut |_s| {},
            Some(&mut handler),
            None,
            &[],
        )
        .unwrap();

        assert_eq!(report.errored, vec!["a-step".to_string()]);
        assert!(
            report.step_records.is_empty(),
            "a declined step never ran — it must not get a StepRecord: {:?}",
            report.step_records
        );
    }

    /// (#1877 item 3 QA) A step that genuinely RAN and then failed (its own
    /// `run_streaming` call returned `Err`) still gets a real, non-zero
    /// `StepRecord` — the interesting half of "a step that ran and failed
    /// is still timed." This is the live per-seat drain path (the job
    /// closure sends `WaveSignal::StepTerminal` with a real `wall_ms` on
    /// BOTH the `Ok` and `Err` arms, see the job closure above), not the
    /// gate-declined or local-wave-load-failure paths, which correctly get
    /// no record because nothing real ran for them.
    ///
    /// **Proved failing first**: temporarily changed `apply_step_terminal`
    /// to push a `StepRecord` only `if result.is_ok()` (leaving the error
    /// path's `wall_ms` un-recorded) and reran this exact test. It failed:
    /// ```text
    /// assertion `left == right` failed: an errored step that actually
    /// dispatched must still get a StepRecord
    ///   left: 0
    ///  right: 1
    /// ```
    /// — confirming the assertion actually distinguishes "ran and failed"
    /// from "never ran." Reverted the mutation before committing.
    #[test]
    fn errored_step_that_actually_ran_still_gets_a_record_with_real_duration() {
        struct FailingSleepKind;
        impl StepKind for FailingSleepKind {
            fn id(&self) -> &'static str {
                "test.fail-sleep"
            }
            fn run(&self, step: &Step, _t: &Task, _i: &BTreeMap<String, String>) -> Result<StepOutcome> {
                let ms = step.config.get("sleep_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                std::thread::sleep(std::time::Duration::from_millis(ms));
                Err(anyhow!("boom: {}", step.id))
            }
        }

        let kind = Arc::new(FailingSleepKind);
        let (ta, sa) = kinded_step("boom", "test.fail-sleep", json!({ "sleep_ms": 60 }), &[]);
        let (tasks, mut steps) = graph(vec![(ta, sa)]);

        let kinds = StepKindRegistry::new();
        kinds.register(kind).unwrap();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let mut emitted: Vec<FlowRecord> = Vec::new();
        let report = run_step_graph(
            &mut steps,
            &tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |r| emitted.push(r),
            &mut |_s| {},
            None,
            None,
            &[],
        )
        .unwrap();

        assert_eq!(report.errored, vec!["boom-step".to_string()]);
        assert_eq!(
            report.step_records.len(),
            1,
            "an errored step that actually dispatched must still get a StepRecord"
        );
        let rec = &report.step_records[0];
        assert_eq!(rec.step_id, "boom-step");
        assert_eq!(rec.kind, "test.fail-sleep");
        assert!(
            rec.wall_ms >= 50,
            "the failed step's own ~60ms sleep must show up in its record — got {}ms",
            rec.wall_ms
        );

        // (#1877) The flow-stream companion fires on this path too, not
        // just the in-memory summary above. This is the "step error" half
        // of the pairing the sibling test
        // `step_records_reach_the_flow_stream_live_under_their_own_vocabulary`
        // (Ok/"step complete" case) exercises above.
        let timing: Vec<&FlowRecord> = emitted.iter().filter(|r| r.action == STEP_TIMING_ACTION).collect();
        assert_eq!(
            timing.len(),
            1,
            "expected exactly one \"step timing\" flow record for the failed-but-ran step: {:?}",
            emitted.iter().map(|r| r.action.as_str()).collect::<Vec<_>>()
        );
        assert_eq!(timing[0].handle, "boom-step");
        assert_eq!(
            timing[0].payload.as_ref().and_then(|p| p.get("wall_ms")).and_then(|v| v.as_u64()),
            Some(rec.wall_ms),
            "the flow record's wall_ms must match the in-memory StepRecord's exactly"
        );
    }

    /// (#1877 item 3 QA) The other half of the trichotomy: a step whose
    /// LOCAL wave never even dispatched — `ensure_wave_loaded` fails to
    /// make its placement resident — gets NO `StepRecord`, exactly like the
    /// gate-declined and panic cases, even though it reaches `errored`
    /// through a completely different code path (`run_local_waves`'s
    /// error-push in `concurrent_dispatch.rs`, reconciled through the SAME
    /// post-scope `apply_step_terminal(..., None, ...)` call the panic path
    /// uses — see the scheduler's own field doc). Without this pinned, a
    /// future refactor that moves the `Instant` outside the `Err` arm (or
    /// substitutes a wave-clock duration for the reconcile path) could
    /// silently fabricate a duration for a step that never actually ran,
    /// and nothing would notice.
    ///
    /// **Proved failing first**: temporarily changed the post-scope
    /// reconcile call's `wall_ms` argument from `None` to `Some(0)`
    /// (exactly the fabrication this module's docs say never happens) and
    /// reran this exact test. It failed:
    /// ```text
    /// a load-failed step never ran — it must not get a StepRecord:
    /// [StepRecord { step_id: "needs-model-step", kind:
    /// "test.local-model", items_in: None, items_out: None, wall_ms: 0 }]
    /// ```
    /// — a fabricated `wall_ms: 0` on a step whose `run` never printed.
    /// Reverted the mutation before committing.
    #[test]
    #[serial_test::serial]
    fn local_wave_load_failure_gets_no_fabricated_record() {
        /// A kind whose `residency()` always resolves a Local placement for
        /// a model no test host will ever successfully load — reaches
        /// `run_local_waves`'s `ensure_wave_loaded` failure path without
        /// depending on `dispatch.map`'s own collection-shaped residency.
        struct LocalModelKind;
        impl StepKind for LocalModelKind {
            fn id(&self) -> &'static str {
                "test.local-model"
            }
            fn residency(
                &self,
                _step: &Step,
                _task: &Task,
                _input: &BTreeMap<String, String>,
                _ctx: &StepRunCtx,
            ) -> Option<darkmux_gestalt::Placement> {
                Some(darkmux_gestalt::Placement {
                    model_key: "unfittable-model".into(),
                    identifier: "darkmux:unfittable-model".into(),
                    min_ctx: 8_000,
                    seat: "step:needs-model".into(),
                })
            }
            fn run(&self, step: &Step, _t: &Task, _i: &BTreeMap<String, String>) -> Result<StepOutcome> {
                // Never reached — the wave loader must fail before this runs.
                panic!("run() must not be called for {}: the wave load should have failed first", step.id);
            }
        }

        /// A host whose `load` ALWAYS fails with an error `ensure_wave_loaded`
        /// treats as non-transient (not `InsufficientResources`, so the
        /// pinned-holder retry branch never applies) — fails the wave on the
        /// first attempt, deterministically, with no real dispatch ever
        /// starting.
        struct FailingLoadHost;
        impl ModelHost for FailingLoadHost {
            fn list_resident(&mut self) -> std::result::Result<Vec<darkmux_gestalt::ResidentFact>, darkmux_gestalt::HostError> {
                Ok(vec![])
            }
            fn list_catalog(&mut self) -> std::result::Result<Vec<darkmux_gestalt::CatalogFact>, darkmux_gestalt::HostError> {
                Ok(vec![])
            }
            fn load(
                &mut self,
                model_key: &str,
                _identifier: &str,
                _min_ctx: u32,
                _deadline: darkmux_gestalt::Deadline,
            ) -> std::result::Result<darkmux_gestalt::LoadReport, darkmux_gestalt::HostError> {
                Err(darkmux_gestalt::HostError::UnknownModel { model_key: model_key.to_string() })
            }
            fn unload(
                &mut self,
                _target: &darkmux_gestalt::plan::OwnedTarget,
                _deadline: darkmux_gestalt::Deadline,
            ) -> std::result::Result<(), darkmux_gestalt::HostError> {
                Ok(())
            }
        }

        let (t, s) = kinded_step("needs-model", "test.local-model", json!({}), &[]);
        let (tasks, mut steps) = graph(vec![(t, s)]);

        let kinds = StepKindRegistry::new();
        kinds.register(Arc::new(LocalModelKind)).unwrap();
        let facts = Facts {
            budget: darkmux_gestalt::Budget { max_darkmux_bytes: Some(20_000_000_000) },
            ..Default::default()
        };
        let est = FixedEstimator(BTreeMap::from([("unfittable-model".to_string(), 5_000_000_000)]));
        let factory = || -> Box<dyn ModelHost> { Box::new(FailingLoadHost) };

        let report = run_step_graph(
            &mut steps, &tasks, &kinds, &facts, &est, 8, &factory,
            &mut |_r| {}, &mut |_s| {}, None, None, &[],
        )
        .unwrap();

        assert_eq!(report.errored, vec!["needs-model-step".to_string()]);
        assert!(
            report.step_records.is_empty(),
            "a load-failed step never ran — it must not get a StepRecord: {:?}",
            report.step_records
        );
        assert_eq!(steps["needs-model-step"].status, NodeStatus::Error);
    }

    /// (#1877 item 3) A mission that never reads `SchedulerReport::
    /// step_records` is unaffected — the field is additive. This is really
    /// two claims: (a) it compiles and passes unmodified for every one of
    /// the scheduler's own many pre-existing `run_step_graph` callers in
    /// this test module, none of which reference `step_records` (that's
    /// the whole test suite around this test, not just this one function);
    /// and (b) explicitly, a caller that discards the report entirely
    /// (`run_graph_with_kind`, used throughout this file) still gets a
    /// correct run — the records are computed regardless of whether
    /// anyone looks.
    #[test]
    fn a_mission_that_never_reads_step_records_is_unaffected() {
        let kind = Arc::new(SleepKind);
        let (ta, sa) = kinded_step("a", "test.sleep", json!({ "sleep_ms": 0 }), &[]);
        let (tasks, mut steps) = graph(vec![(ta, sa)]);
        // `run_graph_with_kind` returns only the emitted flow records,
        // discarding the `SchedulerReport` (and its `step_records`)
        // entirely — exactly the "mission ignores the field" shape.
        let emitted = run_graph_with_kind(kind, &tasks, &mut steps);
        assert!(emitted.iter().any(|r| r.action == "step complete"));
        assert_eq!(steps["a-step"].status, NodeStatus::Complete);
    }

    // ─── #1877 (this issue): the outer/inner timing relationship ───────
    //
    // See `darkmux_lab::lab::review`'s "Timing: two scopes, not one
    // duplicated" module doc for the full argument. In short: a graph step
    // kind (review's `ReviewJudgeStepKind` etc.) times its OWN inner work
    // and reports it before `run_streaming` returns; the scheduler wraps
    // that same `run_streaming` call in its own `Instant` pair, strictly
    // outside it, and that is what lands in `StepRecord::wall_ms` here.
    // One strictly contains the other in wall-clock, so the scheduler's
    // number can never be smaller than an honestly-reported inner number —
    // that containment, not any specific duration, is what this test pins.

    /// A synthetic `StepKind` shaped like review's own step kinds: it takes
    /// its own `Instant` at the top of `run_streaming`, does a small amount
    /// of real work, and reports its OWN measured wall time back — via
    /// `StepOutcome.output` (parsed as a plain `u64` millisecond count),
    /// standing in for the `wall_ms` field review's kinds put in their
    /// `emit_review_step_result` payload. `report_ms_override`, when set,
    /// makes the kind report a DISHONEST wall (bigger than what it actually
    /// did) — this is the knob used to red-prove the invariant test below
    /// before trusting it (see that test's doc for what was observed).
    struct SelfTimedKind {
        report_ms_override: Option<u64>,
    }
    impl StepKind for SelfTimedKind {
        fn id(&self) -> &'static str {
            "test.self-timed"
        }
        fn run(&self, _s: &Step, _t: &Task, _i: &BTreeMap<String, String>) -> Result<StepOutcome> {
            panic!("SelfTimedKind only runs through run_streaming")
        }
        fn run_streaming(
            &self,
            _step: &Step,
            _task: &Task,
            _input: &BTreeMap<String, String>,
            _ctx: &StepRunCtx,
        ) -> Result<StepOutcome> {
            let t0 = Instant::now();
            std::thread::sleep(std::time::Duration::from_millis(15));
            let real_wall_ms = t0.elapsed().as_millis() as u64;
            let reported_ms = self.report_ms_override.unwrap_or(real_wall_ms);
            Ok(StepOutcome { output: reported_ms.to_string(), flow_records: vec![] })
        }
    }

    /// (#1877, correcting the earlier "kinds should stop re-measuring"
    /// plan) The invariant the two timing scopes actually owe each other:
    /// for the same step, the scheduler's `StepRecord::wall_ms` (measured
    /// strictly around the kind's `run_streaming` call) must be `>=` the
    /// kind's own honestly-reported inner wall (here, `SelfTimedKind`'s
    /// `output`, standing in for review's `wall_ms` payload field). Never
    /// assert a specific duration on either side — only the relationship,
    /// which is what stays deterministic under CI load.
    ///
    /// **Proved failing first**, by construction rather than by deleting
    /// scheduler code: run once with `report_ms_override: Some(60_000)` (a
    /// kind that lies and claims an inner wall of one full minute for 15ms
    /// of real work) — the assertion below fails exactly as expected,
    /// `scheduler wall_ms (…) must be >= the kind's own reported wall
    /// (60000)`, proving the assertion is capable of catching a real
    /// violation and is not vacuously true. Then run with
    /// `report_ms_override: None` (the version committed here) — the kind
    /// reports its own honest elapsed time and the assertion passes. Both
    /// runs were observed directly while writing this test.
    #[test]
    fn scheduler_outer_wall_is_never_less_than_the_kinds_own_inner_wall() {
        let kind = Arc::new(SelfTimedKind { report_ms_override: None });
        let (ta, sa) = kinded_step("timed", "test.self-timed", json!({}), &[]);
        let (tasks, mut steps) = graph(vec![(ta, sa)]);

        let kinds = StepKindRegistry::new();
        kinds.register(kind).unwrap();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let report = run_step_graph(
            &mut steps,
            &tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |_r| {},
            &mut |_s| {},
            None,
            None,
            &[],
        )
        .unwrap();

        assert_eq!(report.step_records.len(), 1, "one record for the one step that ran");
        let scheduler_wall_ms = report.step_records[0].wall_ms;
        let kind_reported_wall_ms: u64 = steps["timed-step"]
            .output
            .as_ref()
            .expect("step completed and stored its self-reported wall")
            .parse()
            .expect("SelfTimedKind's output is a plain millisecond count");

        assert!(
            scheduler_wall_ms >= kind_reported_wall_ms,
            "scheduler wall_ms ({scheduler_wall_ms}) must be >= the kind's own reported wall \
             ({kind_reported_wall_ms}) — the scheduler measures strictly around the kind's \
             run_streaming call, so it can never be shorter than an honest inner measurement"
        );
    }

    /// (#1442 ship-2b, the operator-recorded seam decision on PR #1455) The
    /// `MapDispatchOverride` conformance test: an override handed to
    /// `run_step_graph` reaches a REAL `dispatch.map` step's item loop on the
    /// `run_bounded` WORKER THREAD and replaces the transport there. The
    /// endpoint deliberately points at an unroutable address (port 1) — if
    /// the override did NOT intercept, the item would come back `ok: false`
    /// with a connection error instead of the canned reply asserted below.
    /// This is exactly the coverage a thread-local seam cannot provide (the
    /// worker thread never sees the test thread's thread-local), which is
    /// why the seam rides `StepRunCtx`.
    #[test]
    #[serial_test::serial]
    fn dispatch_override_intercepts_dispatch_map_items_on_the_worker_thread() {
        let kinds = StepKindRegistry::with_builtins();
        let (ta, sa) = kinded_step(
            "mapped",
            "dispatch.map",
            json!({
                "model": "override-model",
                "user_template": "check {item}",
                "collection": ["alpha", "beta"],
                "endpoint": { "url": "http://127.0.0.1:1" },
                "timeout_seconds": 1,
            }),
            &[],
        );
        let (tasks, mut steps) = graph(vec![(ta, sa)]);

        let seen: Arc<Mutex<Vec<(String, String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_write = seen.clone();
        let override_fn: crate::step_kinds::MapDispatchOverride =
            Arc::new(move |call: &crate::step_kinds::OverrideDispatchCall| {
                seen_write.lock().unwrap().push((
                    call.model.to_string(),
                    call.user.to_string(),
                    call.endpoint.is_some(),
                ));
                Ok(crate::single_shot::SingleShotReply {
                    content: format!("mocked reply for {}", call.user),
                    total_tokens: Some(7),
                    prompt_tokens: None,
                    completion_tokens: None,
                    model: Some("served-by-mock".to_string()),
                })
            });

        let facts = Facts::default();
        let est = FixedEstimator(Default::default());
        run_step_graph(
            &mut steps,
            &tasks,
            &kinds,
            &facts,
            &est,
            8,
            &mock_host_factory,
            &mut |_r| {},
            &mut |_s| {},
            None,
            Some(override_fn),
            &[],
        )
        .unwrap();

        let step = &steps["mapped-step"];
        assert_eq!(step.status, NodeStatus::Complete, "the map step completed via the override");
        let results: Vec<crate::step_kinds::MapItemResult> =
            serde_json::from_str(step.output.as_deref().unwrap()).expect("map output parses");
        assert_eq!(results.len(), 2);
        for r in &results {
            assert!(r.ok, "no connection error — the transport was replaced: {:?}", r.error);
            assert!(r.content.starts_with("mocked reply for check "));
            assert_eq!(r.total_tokens, Some(7));
            assert_eq!(r.served_model.as_deref(), Some("served-by-mock"), "hosted item surfaces the mock's served model");
        }
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls.len(), 2, "one override call per item");
        assert!(calls.iter().all(|c| c.0 == "override-model" && c.2), "hosted dialect surfaced to the override");
    }

    /// (#1442 gate C3) Emits `n` per-item records LIVE through the ctx during
    /// its run — the streaming-seam probe. If the seam batched at wave-drain
    /// instead, these would land AFTER the step-complete bookend.
    struct StreamingKind {
        n: usize,
    }
    impl StepKind for StreamingKind {
        fn id(&self) -> &'static str {
            "test.streaming"
        }
        fn run(&self, _s: &Step, _t: &Task, _i: &BTreeMap<String, String>) -> Result<StepOutcome> {
            Ok(StepOutcome { output: "ctx-free".to_string(), flow_records: vec![] })
        }
        fn run_streaming(
            &self,
            step: &Step,
            _t: &Task,
            _i: &BTreeMap<String, String>,
            ctx: &StepRunCtx,
        ) -> Result<StepOutcome> {
            for i in 0..self.n {
                let mut rec = step_lifecycle_record(step, "item");
                rec.payload = Some(json!({ "i": i }));
                ctx.emit(rec);
            }
            Ok(StepOutcome { output: "done".to_string(), flow_records: vec![] })
        }
    }

    #[test]
    fn streaming_records_reach_emit_before_the_step_completes() {
        let log_kind = Arc::new(StreamingKind { n: 5 });
        let (ta, sa) = kinded_step("s", "test.streaming", json!({}), &[]);
        let (tasks, mut steps) = graph(vec![(ta, sa)]);

        let emitted = run_graph_with_kind(log_kind, &tasks, &mut steps);

        let item_positions: Vec<usize> = emitted
            .iter()
            .enumerate()
            .filter(|(_, r)| r.action == "item")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(item_positions.len(), 5, "all five streamed items reached emit");
        let complete_pos = emitted
            .iter()
            .position(|r| r.action == "step complete")
            .expect("step complete emitted");
        assert!(
            item_positions.iter().all(|&p| p < complete_pos),
            "every streamed item lands BEFORE the step-complete bookend (live, not batched at drain): \
             items at {item_positions:?}, complete at {complete_pos}"
        );
        // Emission ORDER preserved: item i=0..5 in sequence.
        let item_indices: Vec<u64> = emitted
            .iter()
            .filter(|r| r.action == "item")
            .map(|r| r.payload.as_ref().unwrap()["i"].as_u64().unwrap())
            .collect();
        assert_eq!(item_indices, vec![0, 1, 2, 3, 4], "records visible in emission order");
    }

    // ─── (#1442 gate) dispatch.map scheduler integration ────────────────

    /// Hands its `config.output_json` back verbatim — a dependency-fed
    /// collection SOURCE for the dispatch.map integration test, exercising
    /// the real `gather_inputs` path (dispatch.map reads THIS step's output
    /// as its single dependency input).
    struct EmitCollectionKind;
    impl StepKind for EmitCollectionKind {
        fn id(&self) -> &'static str {
            "test.emit-collection"
        }
        fn run(&self, step: &Step, _t: &Task, _i: &BTreeMap<String, String>) -> Result<StepOutcome> {
            let out = step
                .config
                .get("output_json")
                .and_then(|v| v.as_str())
                .unwrap_or("[]")
                .to_string();
            Ok(StepOutcome { output: out, flow_records: vec![] })
        }
    }

    /// A `host_factory` that records every model_key it is asked to load into
    /// a shared log — so a test can prove the wave loader loaded (or did not
    /// load) a model for a dispatch.map step.
    #[derive(Default)]
    struct RecordingHost {
        loads: Arc<Mutex<Vec<String>>>,
        residents: Vec<darkmux_gestalt::ResidentFact>,
    }
    impl ModelHost for RecordingHost {
        fn list_resident(&mut self) -> std::result::Result<Vec<darkmux_gestalt::ResidentFact>, darkmux_gestalt::HostError> {
            Ok(self.residents.clone())
        }
        fn list_catalog(&mut self) -> std::result::Result<Vec<darkmux_gestalt::CatalogFact>, darkmux_gestalt::HostError> {
            Ok(vec![])
        }
        fn load(
            &mut self,
            model_key: &str,
            identifier: &str,
            min_ctx: u32,
            _deadline: darkmux_gestalt::Deadline,
        ) -> std::result::Result<darkmux_gestalt::LoadReport, darkmux_gestalt::HostError> {
            self.loads.lock().unwrap().push(model_key.to_string());
            self.residents.push(darkmux_gestalt::ResidentFact {
                identifier: identifier.to_string(),
                model_key: model_key.to_string(),
                ctx: u64::from(min_ctx),
                est_bytes: None,
            });
            Ok(darkmux_gestalt::LoadReport { resolved_ctx: Some(u64::from(min_ctx)), ..Default::default() })
        }
        fn unload(
            &mut self,
            _target: &darkmux_gestalt::plan::OwnedTarget,
            _deadline: darkmux_gestalt::Deadline,
        ) -> std::result::Result<(), darkmux_gestalt::HostError> {
            Ok(())
        }
    }

    #[test]
    fn dispatch_map_empty_dependency_collection_loads_no_model_at_the_wave_loader() {
        // Upstream produces an EMPTY collection; the downstream dispatch.map
        // (a LOCAL model with an n_ctx residency hint) must load NOTHING —
        // its `residency()` returns `None` on the empty input, so the wave
        // loader is never asked for the model. Proven at the ACTUAL loader via
        // the recording host.
        let loads: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let loads_for_factory = loads.clone();
        let factory = move || -> Box<dyn ModelHost> {
            Box::new(RecordingHost { loads: loads_for_factory.clone(), residents: vec![] })
        };

        let (t_src, s_src) = kinded_step(
            "src",
            "test.emit-collection",
            json!({ "output_json": "[]" }),
            &[],
        );
        let (t_map, s_map) = kinded_step(
            "map",
            "dispatch.map",
            json!({ "model": "map-model", "user_template": "check {item}", "n_ctx": 8000 }),
            &["src"],
        );
        let (tasks, mut steps) = graph(vec![(t_src, s_src), (t_map, s_map)]);

        let kinds = StepKindRegistry::with_builtins();
        kinds.register(Arc::new(EmitCollectionKind)).unwrap();
        let facts = Facts { budget: darkmux_gestalt::Budget { max_darkmux_bytes: Some(20_000_000_000) }, ..Default::default() };
        let est = FixedEstimator(BTreeMap::from([("map-model".to_string(), 5_000_000_000)]));
        run_step_graph(&mut steps, &tasks, &kinds, &facts, &est, 8, &factory, &mut |_r| {}, &mut |_s| {},
            None, None, &[]).unwrap();

        assert!(loads.lock().unwrap().is_empty(), "empty-collection dispatch.map loads no model");
        assert_eq!(steps["map-step"].status, NodeStatus::Complete, "the empty map short-circuits to Complete");
        assert_eq!(steps["map-step"].output.as_deref(), Some("[]"), "empty output");
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_map_nonempty_dependency_collection_wave_loads_the_right_model() {
        // Upstream produces a NON-EMPTY collection; the dispatch.map step's
        // `residency()` now resolves a Local placement, so the wave loader
        // loads "map-model" before running it. The dispatch itself is pointed
        // at an unroutable URL so no real model is contacted — per-item error
        // isolation captures the connect failure; the LOAD is what we assert.
        let url_key = "DARKMUX_LMSTUDIO_URL";
        let prev_url = std::env::var(url_key).ok();
        unsafe {
            std::env::set_var(url_key, "http://127.0.0.1:1");
        }
        let loads: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let loads_for_factory = loads.clone();
        let factory = move || -> Box<dyn ModelHost> {
            Box::new(RecordingHost { loads: loads_for_factory.clone(), residents: vec![] })
        };

        let (t_src, s_src) = kinded_step(
            "src",
            "test.emit-collection",
            json!({ "output_json": "[\"x\"]" }),
            &[],
        );
        let (t_map, s_map) = kinded_step(
            "map",
            "dispatch.map",
            json!({ "model": "map-model", "user_template": "check {item}", "n_ctx": 8000, "timeout_seconds": 1 }),
            &["src"],
        );
        let (tasks, mut steps) = graph(vec![(t_src, s_src), (t_map, s_map)]);

        let kinds = StepKindRegistry::with_builtins();
        kinds.register(Arc::new(EmitCollectionKind)).unwrap();
        let facts = Facts { budget: darkmux_gestalt::Budget { max_darkmux_bytes: Some(20_000_000_000) }, ..Default::default() };
        let est = FixedEstimator(BTreeMap::from([("map-model".to_string(), 5_000_000_000)]));
        run_step_graph(&mut steps, &tasks, &kinds, &facts, &est, 8, &factory, &mut |_r| {}, &mut |_s| {},
            None, None, &[]).unwrap();

        unsafe {
            match prev_url {
                Some(v) => std::env::set_var(url_key, v),
                None => std::env::remove_var(url_key),
            }
        }
        let loaded = loads.lock().unwrap().clone();
        assert!(loaded.contains(&"map-model".to_string()), "the wave loader loaded the map's model: {loaded:?}");
    }
    /// (#1530) A kind that REQUIRES an artifact no step in the graph
    /// provides, and no caller seeds, must fail BEFORE any step runs —
    /// naming the artifact and who needs it.
    ///
    /// Until this check existed `requires()` was declared by nine kinds and
    /// read by nothing, so this composition reached `run_streaming` and
    /// panicked mid-run — after the mission had minted. Generalizing the
    /// graph is what made it reachable: mixing kinds across configs is now
    /// an ordinary thing an operator does.
    struct NeedsArtifactKind;
    impl StepKind for NeedsArtifactKind {
        fn id(&self) -> &'static str {
            "test.needs-artifact"
        }
        fn requires(&self) -> &'static [crate::step_kinds::Port] {
            const PORTS: [crate::step_kinds::Port; 1] = [crate::step_kinds::Port::artifact("test.absent", || {
                std::sync::Arc::new(0u32) as std::sync::Arc<dyn std::any::Any + Send + Sync>
            })];
            &PORTS
        }
        fn run(
            &self,
            _s: &Step,
            _t: &Task,
            _i: &BTreeMap<String, String>,
        ) -> anyhow::Result<StepOutcome> {
            Ok(StepOutcome { output: String::new(), flow_records: Vec::new() })
        }
    }

    #[test]
    fn unmet_required_artifact_fails_before_any_step_runs() {
        let (t, st) = kinded_step("needy", "test.needs-artifact", json!({}), &[]);
        let (tasks, mut steps) = graph(vec![(t, st)]);
        let kinds = StepKindRegistry::with_builtins();
        kinds.register(Arc::new(NeedsArtifactKind)).unwrap();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let factory = || -> Box<dyn ModelHost> { Box::new(RecordingHost::default()) };

        let err = run_step_graph(
            &mut steps, &tasks, &kinds, &facts, &est, 1, &factory,
            &mut |_r| {}, &mut |_s| {},
            None, None, &[])
        .expect_err("an unmet required artifact must fail the run");
        let msg = format!("{err:#}");
        assert!(msg.contains("test.absent"), "must name the missing artifact: {msg}");
        assert!(msg.contains("test.needs-artifact"), "must name the kind that needs it: {msg}");
        assert_eq!(
            steps["needy-step"].status,
            NodeStatus::Planned,
            "the check must fire BEFORE any step runs — nothing should have executed"
        );
    }

    #[test]
    fn a_seeded_required_artifact_satisfies_the_composition_check() {
        // The same graph, with the caller seeding what the kind requires —
        // exactly how the review and coder-phase launchers supply theirs.
        let (t, st) = kinded_step("needy", "test.needs-artifact", json!({}), &[]);
        let (tasks, mut steps) = graph(vec![(t, st)]);
        let kinds = StepKindRegistry::with_builtins();
        kinds.register(Arc::new(NeedsArtifactKind)).unwrap();
        let facts = Facts::default();
        let est = FixedEstimator::default();
        let factory = || -> Box<dyn ModelHost> { Box::new(RecordingHost::default()) };
        // (#1530) Seeded at a DIFFERENT concrete type than the port's factory
        // produces (`u32`). That is deliberate: the check uses
        // `ArtifactBus::has` (presence, type-agnostic), not `get::<T>()`
        // (which returns `None` for a type mismatch too). Seeding `u32` here
        // would pass under either implementation and prove nothing; a
        // `String` passes ONLY under `has`, so this pins the distinction the
        // check is built on.
        let seed: Vec<(&'static str, std::sync::Arc<dyn std::any::Any + Send + Sync>)> =
            vec![("test.absent", std::sync::Arc::new(String::from("seeded")))];

        run_step_graph(
            &mut steps, &tasks, &kinds, &facts, &est, 1, &factory,
            &mut |_r| {}, &mut |_s| {},
            None, None, &seed)
        .expect("a seeded artifact satisfies the requirement");
        assert_eq!(steps["needy-step"].status, NodeStatus::Complete);
    }

}
