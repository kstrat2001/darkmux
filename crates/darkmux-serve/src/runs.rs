//! `GET /runs` — the flat, kind-tagged, normalized run view-model (#1508
//! step 3, the run-view consolidation arc). A READ-SIDE UNION over three
//! existing sources, computed fresh per request:
//!
//! 1. **Durable run records** — `darkmux_crew::loader::load_missions()`.
//!    Every `Mission` is one `Run`. Post-#1509, a standalone `darkmux
//!    dispatch` is a crew-of-one mission (one phase, one task, one step)
//!    and shows up here too — see [`RunKind`]'s doc for how the two are
//!    told apart.
//! 2. **Lab runs** — the SAME scan `GET /lab/runs` already does
//!    (`crate::scan_lab_runs`), gated on the daemon's `--lab-dir`. Zero
//!    contribution when unconfigured — never an error.
//! 3. **Flow** — read (never written) to (a) resolve `route` for a tracked
//!    run and (b) synthesize UNTRACKED runs: flow sessions that opened a
//!    dispatch but have no durable run record backing them.
//!
//! **No new persistence.** This module reads JSON off disk (the same
//! sources their own existing endpoints already scan) and normalizes in
//! memory — no SQLite, no `runs.db`, no derived index. A derived index is a
//! possible FUTURE optimization (out of scope here; the JSON files stay the
//! sole source of truth per operator direction).
//!
//! **Flat, no tree.** A run's internal Phase/Task/Step graph is NOT
//! flattened into separate top-level entries — that detail lives behind the
//! run's own detail/graph view (`GET /mission/:id/graph.json`). This module
//! only ever emits ONE [`Run`] per mission/lab-run/ghost session.
//!
//! ## The mission_id gap (a load-bearing finding, not a redesign)
//!
//! The obvious join key from a flow session back to its owning mission is
//! `FlowRecord.mission_id`. Two GENUINELY DIFFERENT gaps in how that field
//! gets populated both surfaced during review (fresh-context gate, #1523) —
//! neither is a flow-emission bug worth fixing at the source for THIS PR;
//! both are closed read-side here instead.
//!
//! **Gap 1 — crew-of-one dispatches (fixed read-side).**
//! `dispatch_as_crew_of_one::build_graph` only sets `Step.config["phase_id"]`
//! when the CLI's OWN `--phase-id` flag names some OTHER, pre-existing
//! mission's phase (external attribution) — never for the crew-of-one's own
//! internally-minted phase. With no `phase_id` in the step config,
//! `crew::dispatch::resolve_mission_for_phase(None)` returns `None`, so the
//! dispatch's `dispatch start`/`dispatch complete` flow records carry
//! `mission_id: null`.
//!
//! **Gap 2 — generic config-launched missions (fixed read-side).**
//! `mission_config::interpret::push_step` (the generic `mission launch
//! <config>` graph builder — NOT the Tier-3 bespoke coder-phase/review
//! launchers) never injects `phase_id` into a `dispatch.internal` or
//! `dispatch.single_shot` step's config either. Any config-launched mission
//! whose steps don't explicitly set `config.phase_id` hits the exact same
//! `resolve_mission_for_phase(None) -> None` gap as gap 1, for every one of
//! its steps.
//!
//! **The fix for both is the SAME read-side mechanism: join by
//! `session_id`, not `mission_id`.** Every `Step` — crew-of-one OR
//! generic-config — dispatches under a KNOWN session_id: the explicit
//! `Step.config["session_id"]` when the step sets one, else the exact
//! default its own step kind falls back to at dispatch time
//! (`DispatchInternalStepKind` -> `session_id::step(&step.id)`;
//! `DispatchSingleShotStepKind`'s hosted branch -> `session_id::task(&step.task_id)`
//! — see `crates/darkmux-crew/src/step_kinds/builtins.rs`).
//! [`collect_mission_step_sessions`] reconstructs that same session_id for
//! EVERY step of EVERY loaded mission (not just the crew-of-one case), so a
//! mission's own dispatches are always recognized and never double-listed
//! as untracked ghosts — regardless of which gap (or neither, e.g.
//! coder-phase/review, which DO pass a real `--phase-id` and so already
//! carry `mission_id` correctly) produced its flow records.
//!
//! `Mission`-kind runs ALSO still join by `mission_id` (works today for
//! coder-phase/review) — [`mission_to_run`] unions BOTH join keys per
//! mission, so whichever mechanism actually stamped a session lands the
//! same Run row exactly once.

use crate::LabRunSummary;
use darkmux_crew::envelope::MissionOutcomeStatus;
use darkmux_crew::types::{Mission, MissionStatus, Phase, Step, Task};
use std::collections::{HashMap, HashSet};
use std::path::{Path as StdPath, PathBuf};

/// Which of the three sources a [`Run`] came from, and — for a durable run
/// record — whether it's a standalone dispatch or a real multi-phase
/// mission.
///
/// **Kind derivation for a loaded `Mission`** (see [`classify_mission`]):
/// prefer the EXPLICIT marker `Mission.spec.config_id == "dispatch"` — every
/// crew-of-one run (#1509's `dispatch_as_crew_of_one::build_graph`) stamps
/// this literal `config_id` on its `MissionSpec`, and every mission-launch
/// path stamps its OWN config's real id (`"coder-phase"`, `"review"`, …) —
/// so a non-`"dispatch"` spec is unambiguously `Mission`. Only when `spec`
/// is entirely absent (a pre-#1503 hand-authored or very old mission with
/// no spec at all) does this fall back to the STRUCTURAL shape: exactly one
/// phase, whose one task has exactly one step — the same shape
/// `build_graph` always produces — read as `Dispatch`; anything else reads
/// as `Mission`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RunKind {
    Mission,
    Dispatch,
    Lab,
}

/// The run's flat lifecycle status. See each source's own mapping:
/// [`mission_run_status`] (missions/dispatches), [`lab_run_status`] (lab
/// runs), [`ghost_runs`] (untracked flow-only sessions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RunStatus {
    Planned,
    Running,
    Complete,
    Error,
    Abandoned,
}

/// One row of the `/runs` view-model. Lenient-on-read WIRE shape (every
/// field but `id`/`kind`/`status`/`tracked` is optional) — this is NEVER
/// persisted, so there's no schema-version discipline to carry; a future
/// consumer (the step-4 Runs lens) just reads whatever's present.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Run {
    pub(crate) id: String,
    pub(crate) kind: RunKind,
    pub(crate) status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) machine: Option<String>,
    /// Endpoint label (e.g. `"azure:host/gpt-4o"`) when any of the run's
    /// dispatches used a hosted endpoint; `None` = local LMStudio (or no
    /// flow session found at all). See the module doc's join-key section
    /// for how this is resolved per `kind`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) route: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) started_ts: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) completed_ts: Option<u64>,
    /// `false` = a flow-only ghost with no durable record backing it (see
    /// the module doc's "untracked" synthesis). `true` for every mission
    /// and lab run — both have a durable artifact on disk.
    pub(crate) tracked: bool,
}

/// Build the full `/runs` list — the top-level entry point `runs_handler`
/// calls from a `spawn_blocking` task. Never panics on a missing/malformed
/// source: `load_missions`/`load_phases` degrade to empty via
/// `unwrap_or_default` (matching `missions_handler`'s own posture), and
/// `crate::scan_lab_runs` is already resilient (best-effort scan, #1247).
pub(crate) fn build_runs(flows_dir: &StdPath, lab_dir: Option<&StdPath>) -> Vec<Run> {
    let flow_index = build_flow_session_index(flows_dir);
    // (#1523 gate CONSIDER 2) Pre-group flow sessions by `mission_id` ONCE
    // — an O(sessions) pass — rather than filtering the whole `flow_index`
    // per mission (O(missions × sessions), the shape a Studio-scale flow
    // archive with many missions would make genuinely slow).
    let mission_id_index = build_mission_id_index(&flow_index);

    let missions = darkmux_crew::loader::load_missions().unwrap_or_default();
    let phases_by_id: HashMap<String, Phase> = darkmux_crew::loader::load_phases()
        .unwrap_or_default()
        .into_iter()
        .map(|p| (p.id.clone(), p))
        .collect();

    let mut runs: Vec<Run> = Vec::with_capacity(missions.len());
    // Dedup bookkeeping (see the module doc's "mission_id gap" section):
    // a session already accounted for by a tracked run — either because its
    // `mission_id` matches a loaded mission, or because it's one of that
    // mission's OWN step sessions (reconstructed structurally, covering
    // BOTH gap 1 and gap 2) — must never ALSO produce an untracked ghost
    // for the same underlying work.
    let mut known_mission_ids: HashSet<String> = HashSet::new();
    let mut known_session_ids: HashSet<String> = HashSet::new();

    for mission in &missions {
        known_mission_ids.insert(mission.id.clone());
        let (kind, shape) = classify_mission(mission, &phases_by_id);
        // (#1523 gate must-fix 2) Registered for EVERY mission, not just
        // Dispatch-kind — a generic config-launched Mission-kind mission's
        // `dispatch.internal`/`dispatch.single_shot` steps hit the SAME
        // mission_id gap crew-of-one dispatches do (see module doc, gap 2).
        let step_sessions = collect_mission_step_sessions(mission);
        known_session_ids.extend(step_sessions.iter().cloned());
        let run = mission_to_run(mission, kind, shape.as_ref(), &step_sessions, &mission_id_index, &flow_index);
        runs.push(run);
    }

    // (#1523 gate CONSIDER 7) Resolved ONCE — every lab run is
    // machine-local by the SAME construction, so there's no reason to
    // re-read config for each one.
    let lab_machine = darkmux_types::config_access::machine_id();
    if let Some(dir) = lab_dir {
        for summary in crate::scan_lab_runs(dir) {
            runs.push(lab_summary_to_run(&summary, lab_machine.clone()));
        }
    }

    runs.extend(ghost_runs(&flow_index, &known_mission_ids, &known_session_ids));

    runs
}

// ─── Mission / dispatch normalization ──────────────────────────────────────

/// Decide a loaded `Mission`'s [`RunKind`] and, for a `Dispatch`, its
/// structural `(Task, Step)` pair (source of `role_id` — see the module
/// doc). See [`RunKind`]'s own doc for the marker-first, counts-as-fallback
/// rule this implements.
fn classify_mission(mission: &Mission, phases_by_id: &HashMap<String, Phase>) -> (RunKind, Option<(Task, Step)>) {
    let shape = crew_of_one_shape(mission, phases_by_id);
    let kind = match &mission.spec {
        Some(spec) if spec.config_id == "dispatch" => RunKind::Dispatch,
        Some(_) => RunKind::Mission,
        None => {
            if shape.is_some() {
                RunKind::Dispatch
            } else {
                RunKind::Mission
            }
        }
    };
    // Only surface the shape when the FINAL kind is Dispatch — a marker-
    // driven Mission with an (unlikely) accidental crew-of-one structural
    // shape must not borrow that shape's role/session for its Run.
    let shape = if kind == RunKind::Dispatch { shape } else { None };
    (kind, shape)
}

/// `Some((task, step))` only when `mission` has EXACTLY the crew-of-one
/// structural shape `dispatch_as_crew_of_one::build_graph` always produces:
/// one phase, whose one task has exactly one step. Real multi-phase
/// missions short-circuit at the first check with zero file I/O; only a
/// single-phase mission pays the `load_tasks_for_phase`/`load_steps_for_phase`
/// cost (bounded, same per-mission I/O shape `mission_graph::build_mission_graph`
/// already pays for the graph lens).
fn crew_of_one_shape(mission: &Mission, phases_by_id: &HashMap<String, Phase>) -> Option<(Task, Step)> {
    if mission.phase_ids.len() != 1 {
        return None;
    }
    let phase = phases_by_id.get(&mission.phase_ids[0])?;
    if phase.task_ids.len() != 1 {
        return None;
    }
    let tasks = darkmux_crew::lifecycle::load_tasks_for_phase(&mission.id, &phase.id).ok()?;
    if tasks.len() != 1 {
        return None;
    }
    let task = tasks.into_iter().next()?;
    if task.step_ids.len() != 1 {
        return None;
    }
    let steps = darkmux_crew::lifecycle::load_steps_for_phase(&mission.id, &phase.id).ok()?;
    let step = steps.into_iter().find(|s| s.task_id == task.id)?;
    Some((task, step))
}

/// Every session_id this mission's OWN steps dispatch under (#1523 gate
/// must-fix 2) — read from `Step.config["session_id"]` when explicit, else
/// the SAME per-kind default the step kind itself falls back to at
/// dispatch time. Walks every phase in `mission.phase_ids`; a phase whose
/// steps can't be loaded (deleted, malformed) contributes nothing rather
/// than erroring — best-effort, matching this module's posture everywhere
/// else. Bounded by the mission's own phase count, the same per-mission I/O
/// shape `crew_of_one_shape` and `mission_graph::build_mission_graph`
/// already pay.
fn collect_mission_step_sessions(mission: &Mission) -> HashSet<String> {
    let mut out = HashSet::new();
    for phase_id in &mission.phase_ids {
        let Ok(steps) = darkmux_crew::lifecycle::load_steps_for_phase(&mission.id, phase_id) else {
            continue;
        };
        for step in steps {
            if let Some(sid) = step_session_id(&step) {
                out.insert(sid);
            }
        }
    }
    out
}

/// A `Step`'s dispatch session_id: the explicit `config["session_id"]` when
/// present, else the default ITS OWN step kind falls back to at dispatch
/// time (see `crates/darkmux-crew/src/step_kinds/builtins.rs`:
/// `DispatchInternalStepKind::run` -> `session_id::step(&step.id)` when no
/// `config.session_id`; `DispatchSingleShotStepKind::run`'s hosted branch ->
/// `session_id::task(&step.task_id)`). `None` for a step kind with no known
/// session-id convention (e.g. a purely procedural kind that never
/// dispatches at all) — nothing to register for those.
fn step_session_id(step: &Step) -> Option<String> {
    if let Some(sid) = step.config.get("session_id").and_then(|v| v.as_str()) {
        if !sid.is_empty() {
            return Some(sid.to_string());
        }
    }
    match step.kind.as_str() {
        "dispatch.internal" => Some(darkmux_types::session_id::step(&step.id)),
        "dispatch.single_shot" => Some(darkmux_types::session_id::task(&step.task_id)),
        _ => None,
    }
}

/// Pre-group the flow session index by `mission_id` (#1523 gate CONSIDER
/// 2) — one O(sessions) pass, read back in O(1) per mission by
/// [`mission_to_run`] instead of a linear `flow_index` scan per mission.
fn build_mission_id_index(flow_index: &HashMap<String, SessionAgg>) -> HashMap<String, Vec<String>> {
    let mut idx: HashMap<String, Vec<String>> = HashMap::new();
    for (session_id, agg) in flow_index {
        if let Some(mid) = &agg.mission_id {
            idx.entry(mid.clone()).or_default().push(session_id.clone());
        }
    }
    idx
}

/// Normalize one loaded `Mission` into a [`Run`]. Joins to its flow
/// session(s) by the UNION of `step_sessions` (structural — covers both
/// mission_id gaps, see the module doc) and `mission_id_index`'s lookup
/// (covers the paths that already stamp `mission_id` correctly, e.g.
/// coder-phase/review) — whichever mechanism produced the session, this
/// finds it exactly once.
fn mission_to_run(
    mission: &Mission,
    kind: RunKind,
    shape: Option<&(Task, Step)>,
    step_sessions: &HashSet<String>,
    mission_id_index: &HashMap<String, Vec<String>>,
    flow_index: &HashMap<String, SessionAgg>,
) -> Run {
    // Prefer the structural Task.role_id (the operator's REQUESTED role,
    // always present by construction for a Dispatch-kind mission) over the
    // flow-derived `handle` (present only once a dispatch record actually
    // landed) — same value in practice, but the structural source never
    // depends on flow retention. `shape` (and therefore `dispatch_role`) is
    // always `None` for a Mission-kind run (see `classify_mission`), so
    // this falls through to the flow-derived role there, same as before.
    let dispatch_role = shape.and_then(|(task, _)| task.role_id.clone());

    let mut candidate_ids: HashSet<&str> = step_sessions.iter().map(String::as_str).collect();
    if let Some(ids) = mission_id_index.get(&mission.id) {
        candidate_ids.extend(ids.iter().map(String::as_str));
    }
    let sessions: Vec<&SessionAgg> = candidate_ids
        .into_iter()
        .filter_map(|sid| flow_index.get(sid))
        .collect();

    let representative = earliest_by_start(&sessions);
    // TODO(step-4): a mission whose dispatches span MULTIPLE distinct
    // endpoints (mixed local/remote seats across phases) collapses to one
    // representative endpoint here — the Runs lens can't yet show per-seat
    // routing. Picking the first remote session is a reasonable
    // single-value summary for a flat row; don't overbuild this for a
    // view-model step 4 will replace with a richer render.
    let remote = earliest_by_start(
        &sessions
            .iter()
            .copied()
            .filter(|s| s.endpoint.is_some())
            .collect::<Vec<_>>(),
    );

    let role = dispatch_role.or_else(|| representative.and_then(|s| s.role.clone()));
    let model = representative.and_then(|s| s.model.clone());
    let machine = representative.and_then(|s| s.machine.clone());
    let route = remote.and_then(|s| s.endpoint.clone());
    let start_ts_str = representative.and_then(|s| s.start_ts.clone());
    let terminal_ts_str = sessions.iter().filter_map(|s| s.terminal_ts.clone()).max();

    let started_ts = mission
        .started_ts
        .or_else(|| start_ts_str.as_deref().and_then(parse_flow_ts));
    let completed_ts = mission
        .finalized_ts
        .or_else(|| terminal_ts_str.as_deref().and_then(parse_flow_ts));

    Run {
        id: mission.id.clone(),
        kind,
        status: mission_run_status(mission, &sessions),
        machine,
        route,
        role,
        model,
        started_ts,
        completed_ts,
        tracked: true,
    }
}

/// Map a `Mission`'s own lifecycle status to the flat [`RunStatus`],
/// cross-checked against its joined flow `sessions` for two cases the
/// mission record alone can't see (#1523 gate CONSIDERs 3 + 4).
///
/// `MissionStatus` has no separate `Abandoned` variant — `mission abort`
/// and `mission finalize` both drive a mission to `Finalized` (terminal);
/// they're told apart only by the mission's `MissionEnvelope`'s outcome
/// (`Error`/`Degenerate` for an abort-shaped close, `Clean`/`Degraded` for a
/// happy finalize — see `darkmux_crew::envelope`'s own doc). So a
/// `Finalized` mission's flat status is read off its envelope; a mission
/// with no envelope at all (pre-#1284, or a mint that never reached
/// finalization's write) degrades to `Complete` rather than guessing —
/// `Finalized` is itself the durable, higher-confidence signal here.
///
/// **CONSIDER 4 — the dead `Planned` variant.** An `Active` mission
/// (`MissionStatus`'s own default) with `started_ts: None` was minted but
/// never actually started (`darkmux mission start` — or the launcher's own
/// equivalent — hasn't run yet). Mapping that to `Planned` makes the
/// variant reachable and distinguishes "queued" from "genuinely running".
///
/// **CONSIDER 3 — a crashed mission can't stay `Running` forever.** A hard
/// process kill (host crash, OOM) before `finalize_mission` ever runs
/// leaves a mission record permanently `Active` — the record itself can't
/// see that. Its dispatch's flow session CAN: when every session this
/// mission is known to have dispatched has ALREADY reached a terminal, the
/// mission is not genuinely still running. Reports the worst observed
/// session outcome (`Abandoned` > `Error`) rather than eternal `Running`;
/// deliberately does NOT report `Complete` in that case (a `Complete`
/// mission implies a real finalize happened, which — by construction of
/// this branch — it didn't; staying `Running` there matches `mission
/// status`'s existing "drift, needs `mission finalize`" framing rather than
/// claiming a success that was never recorded).
fn mission_run_status(mission: &Mission, sessions: &[&SessionAgg]) -> RunStatus {
    match mission.status {
        MissionStatus::Active | MissionStatus::Paused => {
            if mission.started_ts.is_none() {
                return RunStatus::Planned;
            }
            if !sessions.is_empty() && sessions.iter().all(|s| s.terminal_status.is_some()) {
                if sessions.iter().any(|s| s.terminal_status == Some(RunStatus::Abandoned)) {
                    return RunStatus::Abandoned;
                }
                if sessions.iter().any(|s| s.terminal_status == Some(RunStatus::Error)) {
                    return RunStatus::Error;
                }
            }
            RunStatus::Running
        }
        MissionStatus::Finalized => {
            let envelope = darkmux_crew::lifecycle::load_envelope(&mission.id)
                .ok()
                .flatten();
            match envelope.map(|e| e.status) {
                Some(MissionOutcomeStatus::Error) | Some(MissionOutcomeStatus::Degenerate) => RunStatus::Error,
                _ => RunStatus::Complete,
            }
        }
    }
}

// ─── Lab normalization ──────────────────────────────────────────────────────

/// Normalize one `LabRunSummary` (the SAME row `/lab/runs` returns) into a
/// [`Run`]. `machine` is resolved ONCE by the caller ([`build_runs`]) and
/// passed in — every lab run shares the same daemon-declared machine
/// (#1523 gate CONSIDER 7).
fn lab_summary_to_run(summary: &LabRunSummary, machine: Option<String>) -> Run {
    let (role, model, route) = lab_staffing_role_model_route(summary.staffing.as_ref());
    Run {
        id: summary.dir.clone(),
        kind: RunKind::Lab,
        status: lab_run_status(summary),
        machine,
        route,
        role,
        model,
        // `LabRunSummary` carries no run-START timestamp today (only the
        // newest-artifact `mtime_ms`) — leaving `started_ts` absent is
        // honest; a wrong guess (e.g. mtime as start) would be worse than
        // no value. `mtime_ms` becomes `completed_ts` once the run reached
        // its terminal artifact write (`scores.json`).
        started_ts: None,
        completed_ts: if summary.finished {
            Some(summary.mtime_ms / 1000)
        } else {
            None
        },
        tracked: true,
    }
}

/// Map a lab run's own `finished`/`degenerate` fields to the flat
/// [`RunStatus`]. A `degenerate` run (every probe drew nothing usable — see
/// `darkmux_lab::lab::review`'s own doc) reached its terminal artifact
/// write but produced no usable finding; the closest flat-status fit is
/// `Error` (there's no separate "degraded" value in this view-model — the
/// step-4 lens can special-case `degenerate` directly off the richer
/// `/lab/runs` payload if finer granularity turns out to matter).
fn lab_run_status(summary: &LabRunSummary) -> RunStatus {
    if !summary.finished {
        return RunStatus::Running;
    }
    if summary.degenerate {
        return RunStatus::Error;
    }
    RunStatus::Complete
}

/// Representative role/model/route for a lab run's `/runs` row, off its
/// `StaffingSnapshot` — the judge seat (the load-bearing one) when present,
/// else the first probe. `route` specifically prefers a REMOTE seat's
/// endpoint (judge first, else the first remote probe); `None` when every
/// staffed seat is local.
fn lab_staffing_role_model_route(
    staffing: Option<&darkmux_lab::lab::review::StaffingSnapshot>,
) -> (Option<String>, Option<String>, Option<String>) {
    let Some(staffing) = staffing else {
        return (None, None, None);
    };
    let seat = staffing.judge.as_ref().or_else(|| staffing.probes.first());
    let role = seat.and_then(|s| s.role_id.clone());
    let model = seat.map(|s| s.model.clone());
    let route = staffing
        .judge
        .as_ref()
        .filter(|s| s.remote)
        .or_else(|| staffing.probes.iter().find(|s| s.remote))
        .and_then(|s| s.endpoint.clone());
    (role, model, route)
}

// ─── Flow scan: session index + untracked ghosts ───────────────────────────

/// (#1523 gate scale-cap CONSIDER) How far back the flow scan looks when
/// building the session index for route resolution + ghost synthesis.
/// Every darkmux install accumulates flow history indefinitely — without a
/// bound, `/runs` would re-parse a machine's ENTIRE flow archive on every
/// request (a real #925-style per-request-timeout risk on a Studio-scale
/// install with months of history), and every dispatch that predates the
/// #1508/#1509 unification would become a PERMANENT untracked ghost.
/// Tracked runs (missions, lab runs) are UNAFFECTED — they're durable
/// records, read in full regardless of age; only the flow-derived
/// route/role/model resolution and ghost synthesis are windowed. A
/// discoverable knob (a named const, not a magic number scattered inline)
/// rather than adaptive-silent, per CLAUDE.md's "cadence is a recorded
/// knob" observability doctrine.
const RUNS_FLOW_SCAN_WINDOW_DAYS: i64 = 14;

/// Per-session_id rollup built by ONE pass over the flow stream
/// ([`build_flow_session_index`]) — the shared substrate both the
/// tracked-run route/role/model resolution (above) and the untracked-ghost
/// synthesis (below) read from.
#[derive(Debug, Default, Clone)]
struct SessionAgg {
    mission_id: Option<String>,
    role: Option<String>,
    model: Option<String>,
    machine: Option<String>,
    /// From the FIRST non-empty `payload.endpoint` seen on any dispatch
    /// lifecycle record (start, complete, OR error) for this session — the
    /// #1518 lesson applied server-side: the review pipeline stamps
    /// `endpoint` only on the terminal record, not the start, so checking
    /// only `dispatch start` would silently miss a remote-run session.
    endpoint: Option<String>,
    /// `true` once a `dispatch start`/`dispatch.start` record is seen —
    /// the gate for whether this session is a real dispatch at all (see
    /// [`ghost_runs`]'s `has_start` check).
    has_start: bool,
    /// The `dispatch start` record's `ts` — kept as the raw ISO string;
    /// parsed to epoch seconds only where a `Run`'s numeric timestamp is
    /// actually needed ([`parse_flow_ts`]).
    start_ts: Option<String>,
    /// The terminal outcome this session reached, from whichever of
    /// `dispatch complete` / `dispatch error` / `session.end` landed first
    /// (see [`terminal_status_for_action`]) — `None` while still running.
    terminal_status: Option<RunStatus>,
    terminal_ts: Option<String>,
}

/// One pass over every flow record within [`RUNS_FLOW_SCAN_WINDOW_DAYS`],
/// building a per-`session_id` rollup.
fn build_flow_session_index(flows_dir: &StdPath) -> HashMap<String, SessionAgg> {
    let mut idx: HashMap<String, SessionAgg> = HashMap::new();
    for_each_recent_flow_record(flows_dir, |v| {
        let Some(session_id) = v.get("session_id").and_then(|s| s.as_str()) else {
            return std::ops::ControlFlow::Continue(());
        };
        if session_id.is_empty() {
            return std::ops::ControlFlow::Continue(());
        }
        let agg = idx.entry(session_id.to_string()).or_default();

        if agg.mission_id.is_none() {
            if let Some(mid) = v.get("mission_id").and_then(|m| m.as_str()) {
                if !mid.is_empty() {
                    agg.mission_id = Some(mid.to_string());
                }
            }
        }
        if agg.role.is_none() {
            if let Some(handle) = v.get("handle").and_then(|h| h.as_str()) {
                if !handle.is_empty() {
                    agg.role = Some(handle.to_string());
                }
            }
        }
        if agg.model.is_none() {
            if let Some(model) = v.get("model").and_then(|m| m.as_str()) {
                if !model.is_empty() {
                    agg.model = Some(model.to_string());
                }
            }
        }
        if agg.machine.is_none() {
            if let Some(mach) = v.get("machine_id").and_then(|m| m.as_str()) {
                if !mach.is_empty() {
                    agg.machine = Some(mach.to_string());
                }
            }
        }

        let action = v.get("action").and_then(|a| a.as_str()).unwrap_or("");
        let ts = v.get("ts").and_then(|t| t.as_str()).unwrap_or("");

        // Check EVERY dispatch lifecycle record's payload for `endpoint` —
        // not just start (#1518, applied server-side; see `SessionAgg::endpoint`'s doc).
        if agg.endpoint.is_none() && is_dispatch_lifecycle_action(action) {
            if let Some(ep) = v
                .get("payload")
                .and_then(|p| p.get("endpoint"))
                .and_then(|e| e.as_str())
            {
                if !ep.is_empty() {
                    agg.endpoint = Some(ep.to_string());
                }
            }
        }

        if is_dispatch_start_action(action) {
            agg.has_start = true;
            if agg.start_ts.is_none() && !ts.is_empty() {
                agg.start_ts = Some(ts.to_string());
            }
        } else if let Some(status) = terminal_status_for_action(action) {
            // Keep the FIRST terminal seen — a session emits at most one in
            // practice; favoring the first keeps this deterministic if a
            // replay/retry ever produced more than one.
            if agg.terminal_status.is_none() {
                agg.terminal_status = Some(status);
                agg.terminal_ts = Some(ts.to_string());
            }
        }

        std::ops::ControlFlow::Continue(())
    });
    idx
}

/// The flow stream carries both the dotted (`dispatch.start`) and spaced
/// (`dispatch start`) action forms across schema history — tolerate both,
/// matching `scan_flow_days`/`scan_flow_missions`'s own dual-form checks.
fn is_dispatch_start_action(action: &str) -> bool {
    action == "dispatch start" || action == "dispatch.start"
}

fn is_dispatch_lifecycle_action(action: &str) -> bool {
    is_dispatch_start_action(action)
        || action == "dispatch complete"
        || action == "dispatch.complete"
        || action == "dispatch error"
        || action == "dispatch.error"
}

/// The `RunStatus` a session's TERMINAL flow action implies — `None` for
/// any non-terminal action (turns, tools, telemetry, the start itself).
fn terminal_status_for_action(action: &str) -> Option<RunStatus> {
    match action {
        "dispatch complete" | "dispatch.complete" => Some(RunStatus::Complete),
        "dispatch error" | "dispatch.error" => Some(RunStatus::Error),
        // The presence reconciler's crash/kill/timeout close-edge — a
        // session whose heartbeat disappeared with no clean dispatch
        // terminal ever landing (`presence_reconciler.rs`'s own doc).
        "session.end" => Some(RunStatus::Abandoned),
        _ => None,
    }
}

/// The chronologically-EARLIEST session by `start_ts` (lexical compare —
/// the flow schema's ISO-8601 `YYYY-MM-DDTHH:MM:SSZ` sorts correctly as a
/// plain string). Sessions with no `start_ts` at all are excluded from the
/// comparison (a `None` `start_ts` must never look "earliest"); falls back
/// to an arbitrary element only when NONE of the candidates have one.
fn earliest_by_start<'a>(sessions: &[&'a SessionAgg]) -> Option<&'a SessionAgg> {
    sessions
        .iter()
        .copied()
        .filter(|s| s.start_ts.is_some())
        .min_by(|a, b| a.start_ts.cmp(&b.start_ts))
        .or_else(|| sessions.first().copied())
}

/// Synthesize an untracked [`Run`] for every flow session that opened a
/// dispatch (`has_start`) but isn't accounted for by an already-listed
/// tracked run — see the module doc's dedup rationale. `kind` is always
/// `Dispatch`: a raw flow session with no mission ever minted for it is,
/// structurally, exactly what a standalone dispatch is. Bounded to
/// [`RUNS_FLOW_SCAN_WINDOW_DAYS`] because `flow_index` itself is (built by
/// [`build_flow_session_index`]) — a session older than the window was
/// never indexed at all, so it can't reach this function to begin with.
fn ghost_runs(
    flow_index: &HashMap<String, SessionAgg>,
    known_mission_ids: &HashSet<String>,
    known_session_ids: &HashSet<String>,
) -> Vec<Run> {
    let mut out = Vec::new();
    for (session_id, agg) in flow_index {
        if !agg.has_start {
            continue;
        }
        if known_session_ids.contains(session_id) {
            continue;
        }
        if let Some(mid) = &agg.mission_id {
            if known_mission_ids.contains(mid) {
                continue;
            }
        }
        out.push(Run {
            id: session_id.clone(),
            kind: RunKind::Dispatch,
            // No terminal seen yet -> still running; see
            // `terminal_status_for_action` for the three terminal shapes.
            status: agg.terminal_status.unwrap_or(RunStatus::Running),
            machine: agg.machine.clone(),
            route: agg.endpoint.clone(),
            role: agg.role.clone(),
            model: agg.model.clone(),
            started_ts: agg.start_ts.as_deref().and_then(parse_flow_ts),
            completed_ts: agg.terminal_ts.as_deref().and_then(parse_flow_ts),
            tracked: false,
        });
    }
    out
}

// ─── Bounded day-file scan (#1523 gate scale-cap) ──────────────────────────

/// Like `crate::for_each_flow_record_across_days`, but bounded to day files
/// whose date is within [`RUNS_FLOW_SCAN_WINDOW_DAYS`] of now. A SEPARATE,
/// smaller day-file walk rather than extending the shared primitive —
/// that primitive's OTHER callers (`/flow-mission/:id`, `/flow-session/:id`,
/// the full-history catalog endpoints) must keep seeing a run's COMPLETE
/// history; bounding is specific to THIS module's route-resolution/
/// ghost-synthesis use, not a general flow-reading behavior change that
/// would ripple into those unrelated endpoints.
fn for_each_recent_flow_record(
    flows_dir: &StdPath,
    mut visit: impl FnMut(&serde_json::Value) -> std::ops::ControlFlow<()>,
) {
    use std::io::BufRead;
    let Ok(entries) = std::fs::read_dir(flows_dir) else {
        return;
    };
    let cutoff = cutoff_date_string(RUNS_FLOW_SCAN_WINDOW_DAYS);
    let mut day_files: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else { continue };
        let Some(date) = name.strip_suffix(".jsonl") else {
            continue;
        };
        // A plain length + lexical-compare check — not full calendar
        // validation (`is_valid_date`'s job elsewhere) — is enough here:
        // the goal is bounding which files get OPENED, and a malformed
        // name that happens to compare >= cutoff just gets read (harmless,
        // same as any other unreadable/malformed file below) while one
        // that doesn't compare is skipped either way.
        if date.len() != 10 || date < cutoff.as_str() {
            continue;
        }
        day_files.push(entry.path());
    }
    day_files.sort();
    for path in day_files {
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        for line in std::io::BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v.get("_type").and_then(|t| t.as_str()) == Some("schema") {
                continue;
            }
            if visit(&v).is_break() {
                return;
            }
        }
    }
}

/// `YYYY-MM-DD` for `window_days` before today (UTC) — the day-file-name
/// cutoff [`for_each_recent_flow_record`] filters on.
fn cutoff_date_string(window_days: i64) -> String {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let cutoff_days = now_secs.div_euclid(86_400) - window_days;
    let (y, m, d) = civil_from_days(cutoff_days);
    format!("{y:04}-{m:02}-{d:02}")
}

// ─── Timestamp parsing ──────────────────────────────────────────────────────

/// Parse a flow record's `ts` field (`YYYY-MM-DDTHH:MM:SSZ`, second
/// precision — see `darkmux_flow::schema::ts_utc_now`) into Unix epoch
/// seconds. Hand-rolled rather than pulling in `chrono`/`time` (CLAUDE.md's
/// "don't add dependencies casually" — a 10-line inline module beats a
/// crate for a one-off need) using the Howard Hinnant civil-calendar
/// algorithm — the inverse of the SAME algorithm `darkmux-flow`'s own
/// `epoch_to_yyyymmdd` uses in the forward direction (that function is
/// `pub(crate)` to its own crate, not reachable from here, hence this
/// independently-tested re-derivation rather than a shared dependency).
/// Returns `None` on anything that doesn't match the exact fixed-width
/// shape — a malformed/absent `ts` degrades to "no flow-derived timestamp",
/// never a panic.
fn parse_flow_ts(ts: &str) -> Option<u64> {
    let b = ts.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let y: i64 = ts.get(0..4)?.parse().ok()?;
    let mo: i64 = ts.get(5..7)?.parse().ok()?;
    let d: i64 = ts.get(8..10)?.parse().ok()?;
    let h: i64 = ts.get(11..13)?.parse().ok()?;
    let mi: i64 = ts.get(14..16)?.parse().ok()?;
    let s: i64 = ts.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + h * 3600 + mi * 60 + s;
    u64::try_from(secs).ok()
}

/// Days since the Unix epoch for a UTC civil date — Howard Hinnant's
/// algorithm (public domain); see [`parse_flow_ts`]'s doc for why this is a
/// local re-derivation rather than a shared crate dependency.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`] — a UTC civil date from days since
/// the Unix epoch (same Howard Hinnant algorithm, public domain). Used only
/// by [`cutoff_date_string`] to format the scan-window boundary as a
/// `YYYY-MM-DD` day-file-name prefix.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z / 146_097 } else { (z - 146_096) / 146_097 };
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_crew::envelope::{MissionEnvelope, MissionOutcomeStatus};
    use darkmux_crew::types::{MissionSpec, NodeStatus, PhaseStatus};
    use std::io::Write;
    use tempfile::TempDir;

    // ── parse_flow_ts / civil calendar round-trip ───────────────────────

    #[test]
    fn parse_flow_ts_epoch_zero() {
        assert_eq!(parse_flow_ts("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn parse_flow_ts_known_reference_point() {
        // 2000-01-01T00:00:00Z is the well-known 946684800.
        assert_eq!(parse_flow_ts("2000-01-01T00:00:00Z"), Some(946_684_800));
    }

    #[test]
    fn parse_flow_ts_round_trips_through_the_real_emitter() {
        let now = darkmux_flow::ts_utc_now();
        let parsed = parse_flow_ts(&now).expect("a freshly-emitted ts must parse");
        let actual = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Second-precision ts + two calls a moment apart — allow a couple
        // seconds of drift rather than asserting exact equality.
        assert!(actual.abs_diff(parsed) <= 3, "parsed={parsed} actual={actual}");
    }

    #[test]
    fn parse_flow_ts_rejects_malformed_input() {
        assert_eq!(parse_flow_ts(""), None);
        assert_eq!(parse_flow_ts("not-a-timestamp"), None);
        assert_eq!(parse_flow_ts("2026-07-24T12:34:56"), None); // missing Z
        assert_eq!(parse_flow_ts("2026-13-01T00:00:00Z"), None); // bad month
    }

    #[test]
    fn civil_from_days_is_the_exact_inverse_of_days_from_civil() {
        // Round-trip across a range spanning leap years, month-length
        // boundaries, and both eras (#1523 gate scale-cap knob's own
        // machinery) — every date must map to itself through both
        // directions.
        let cases: &[(i64, i64, i64)] = &[
            (1970, 1, 1),
            (2000, 1, 1),
            (2000, 2, 29), // leap day
            (2024, 2, 29), // leap day
            (2023, 3, 1),  // day after a non-leap Feb
            (2026, 7, 24),
            (2026, 12, 31),
            (2027, 1, 1),
        ];
        for &(y, m, d) in cases {
            let days = days_from_civil(y, m, d);
            let (ry, rm, rd) = civil_from_days(days);
            assert_eq!((ry, rm as i64, rd as i64), (y, m, d), "round-trip failed for {y:04}-{m:02}-{d:02}");
        }
    }

    // ── crew dir test harness (mirrors dispatch_as_crew_of_one's RunGuard) ─

    struct CrewGuard {
        _tmp: TempDir,
        prev: Option<String>,
    }
    impl CrewGuard {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let prev = std::env::var("DARKMUX_CREW_DIR").ok();
            unsafe {
                std::env::set_var("DARKMUX_CREW_DIR", tmp.path());
            }
            Self { _tmp: tmp, prev }
        }
    }
    impl Drop for CrewGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                    None => std::env::remove_var("DARKMUX_CREW_DIR"),
                }
            }
        }
    }

    fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Today's `YYYY-MM-DD`, UTC — the SAME function real flow records are
    /// day-filed under. Tests must use this (not a hardcoded literal date)
    /// so they stay valid regardless of when they actually run, now that
    /// `build_flow_session_index` bounds itself to a recent window
    /// (`RUNS_FLOW_SCAN_WINDOW_DAYS`) — a hardcoded past date would
    /// eventually age out of the window and start silently failing.
    fn today() -> String {
        darkmux_flow::day_utc_now()
    }

    fn write_day_file(dir: &StdPath, date: &str, lines: &[serde_json::Value]) {
        let mut f = std::fs::File::create(dir.join(format!("{date}.jsonl"))).unwrap();
        for line in lines {
            writeln!(f, "{}", serde_json::to_string(line).unwrap()).unwrap();
        }
    }

    fn minimal_mission(id: &str, phase_ids: Vec<String>, spec: Option<MissionSpec>) -> Mission {
        Mission {
            id: id.to_string(),
            description: format!("test mission {id}"),
            status: MissionStatus::Active,
            phase_ids,
            created_ts: now_unix(),
            started_ts: Some(now_unix()),
            finalized_ts: None,
            paused_ts: None,
            source_input: None,
            ticket: None,
            spec,
        }
    }

    fn minimal_phase(id: &str, mission_id: &str, task_ids: Vec<String>) -> Phase {
        Phase {
            id: id.to_string(),
            mission_id: mission_id.to_string(),
            description: format!("phase {id}"),
            display_name: None,
            status: PhaseStatus::Running,
            created_ts: now_unix(),
            started_ts: Some(now_unix()),
            completed_ts: None,
            abandoned_ts: None,
            task_ids,
        }
    }

    fn minimal_task(id: &str, phase_id: &str, step_ids: Vec<String>, role_id: Option<&str>) -> Task {
        Task {
            id: id.to_string(),
            phase_id: phase_id.to_string(),
            description: format!("task {id}"),
            display_name: None,
            step_ids,
            depends_on: Vec::new(),
            role_id: role_id.map(String::from),
            profile_name: None,
            workdir: None,
            image: None,
        }
    }

    fn minimal_step(id: &str, task_id: &str, session_id: Option<&str>) -> Step {
        Step {
            id: id.to_string(),
            task_id: task_id.to_string(),
            kind: "dispatch.internal".to_string(),
            status: NodeStatus::Complete,
            config: match session_id {
                Some(sid) => serde_json::json!({ "session_id": sid }),
                None => serde_json::Value::Null,
            },
            started_ts: Some(now_unix()),
            completed_ts: Some(now_unix()),
            output: None,
        }
    }

    // ── classify_mission / crew_of_one_shape ────────────────────────────

    #[test]
    #[serial_test::serial]
    fn classify_mission_marker_dispatch_wins_even_with_multi_phase_shape() {
        let _g = CrewGuard::new();
        // Spec says "dispatch" but the mission has TWO phases — the marker
        // still wins per RunKind's doc (explicit marker before structural
        // fallback), even though `crew_of_one_shape` would return None.
        let mission = minimal_mission(
            "m1",
            vec!["p1".to_string(), "p2".to_string()],
            Some(MissionSpec { config_id: "dispatch".to_string(), inputs_fingerprint: "x".to_string() }),
        );
        let phases_by_id = HashMap::new();
        let (kind, shape) = classify_mission(&mission, &phases_by_id);
        assert_eq!(kind, RunKind::Dispatch);
        assert!(shape.is_none(), "no crew-of-one shape available, so no (task, step) pair");
    }

    #[test]
    #[serial_test::serial]
    fn classify_mission_marker_names_a_real_config_is_mission_kind() {
        let _g = CrewGuard::new();
        let mission = minimal_mission(
            "m2",
            vec!["p1".to_string()],
            Some(MissionSpec { config_id: "coder-phase".to_string(), inputs_fingerprint: "x".to_string() }),
        );
        let phases_by_id = HashMap::new();
        let (kind, _) = classify_mission(&mission, &phases_by_id);
        assert_eq!(kind, RunKind::Mission);
    }

    #[test]
    #[serial_test::serial]
    fn classify_mission_no_spec_falls_back_to_crew_of_one_counts() {
        let _g = CrewGuard::new();
        let mission = minimal_mission("m3", vec!["p1".to_string()], None);
        darkmux_crew::lifecycle::save_mission(&mission).unwrap();
        let phase = minimal_phase("p1", "m3", vec!["t1".to_string()]);
        darkmux_crew::lifecycle::save_phase(&phase).unwrap();
        let task = minimal_task("t1", "p1", vec!["s1".to_string()], Some("coder"));
        darkmux_crew::lifecycle::save_task("m3", &task).unwrap();
        let step = minimal_step("s1", "t1", Some("crew-dispatch-coder-abc"));
        darkmux_crew::lifecycle::save_step("m3", "p1", &step).unwrap();

        let mut phases_by_id = HashMap::new();
        phases_by_id.insert("p1".to_string(), phase);
        let (kind, shape) = classify_mission(&mission, &phases_by_id);
        assert_eq!(kind, RunKind::Dispatch);
        let (got_task, got_step) = shape.expect("crew-of-one shape found");
        assert_eq!(got_task.role_id.as_deref(), Some("coder"));
        assert_eq!(got_step.config["session_id"], "crew-dispatch-coder-abc");
    }

    #[test]
    #[serial_test::serial]
    fn classify_mission_no_spec_multi_phase_is_mission_kind() {
        let _g = CrewGuard::new();
        let mission = minimal_mission("m4", vec!["p1".to_string(), "p2".to_string()], None);
        let phases_by_id = HashMap::new();
        let (kind, shape) = classify_mission(&mission, &phases_by_id);
        assert_eq!(kind, RunKind::Mission);
        assert!(shape.is_none());
    }

    // ── step_session_id / collect_mission_step_sessions ─────────────────

    #[test]
    fn step_session_id_prefers_explicit_config_over_the_kind_default() {
        let step = minimal_step("s1", "t1", Some("explicit-sid"));
        assert_eq!(step_session_id(&step), Some("explicit-sid".to_string()));
    }

    #[test]
    fn step_session_id_defaults_dispatch_internal_to_the_step_scoped_session() {
        // (#1523 gate must-fix 2) `interpret::push_step` never injects a
        // session_id — this default is what `DispatchInternalStepKind::run`
        // itself falls back to when `config.session_id` is absent.
        let mut step = minimal_step("s-generic", "t1", None);
        step.kind = "dispatch.internal".to_string();
        assert_eq!(step_session_id(&step), Some(darkmux_types::session_id::step("s-generic")));
    }

    #[test]
    fn step_session_id_defaults_dispatch_single_shot_to_the_task_scoped_session() {
        let mut step = minimal_step("s-single", "t-owner", None);
        step.kind = "dispatch.single_shot".to_string();
        assert_eq!(step_session_id(&step), Some(darkmux_types::session_id::task("t-owner")));
    }

    #[test]
    fn step_session_id_unknown_kind_has_no_default() {
        let mut step = minimal_step("s-proc", "t1", None);
        step.kind = "procedural.noop".to_string();
        assert_eq!(step_session_id(&step), None);
    }

    // ── mission_run_status ──────────────────────────────────────────────

    #[test]
    fn mission_run_status_active_and_paused_are_running() {
        let mut m = minimal_mission("m5", vec![], None);
        assert_eq!(mission_run_status(&m, &[]), RunStatus::Running);
        m.status = MissionStatus::Paused;
        assert_eq!(mission_run_status(&m, &[]), RunStatus::Running);
    }

    #[test]
    fn mission_run_status_active_with_no_started_ts_is_planned() {
        // (#1523 gate CONSIDER 4) Minted but never actually started — the
        // dead `Planned` variant made reachable.
        let mut m = minimal_mission("m5b", vec![], None);
        m.started_ts = None;
        assert_eq!(mission_run_status(&m, &[]), RunStatus::Planned);
    }

    #[test]
    fn mission_run_status_active_with_every_session_terminal_reports_the_terminal_not_running() {
        // (#1523 gate CONSIDER 3) A crashed mission: every dispatch it's
        // known to have made already reached a terminal, yet the mission
        // record itself never got finalized (the process died first).
        let m = minimal_mission("m5c", vec![], None);
        let abandoned = SessionAgg { terminal_status: Some(RunStatus::Abandoned), ..Default::default() };
        assert_eq!(mission_run_status(&m, &[&abandoned]), RunStatus::Abandoned);

        let errored = SessionAgg { terminal_status: Some(RunStatus::Error), ..Default::default() };
        assert_eq!(mission_run_status(&m, &[&errored]), RunStatus::Error);
    }

    #[test]
    fn mission_run_status_active_with_a_still_running_session_stays_running() {
        // A partially-complete multi-session mission (one phase done, one
        // still dispatching) must NOT be flagged as crashed just because
        // ONE of its sessions has a terminal.
        let m = minimal_mission("m5d", vec![], None);
        let done = SessionAgg { terminal_status: Some(RunStatus::Complete), ..Default::default() };
        let still_running = SessionAgg { terminal_status: None, has_start: true, ..Default::default() };
        assert_eq!(mission_run_status(&m, &[&done, &still_running]), RunStatus::Running);
    }

    #[test]
    fn mission_run_status_active_all_complete_stays_running_not_a_fabricated_complete() {
        // Every session finished cleanly, but the mission was never
        // ACTUALLY finalized (no crash — just a `mission finalize` the
        // operator hasn't run yet). Reporting `Complete` here would
        // fabricate a finalize that never happened; `Running` matches
        // `mission status`'s existing "drift" framing.
        let m = minimal_mission("m5e", vec![], None);
        let done = SessionAgg { terminal_status: Some(RunStatus::Complete), ..Default::default() };
        assert_eq!(mission_run_status(&m, &[&done]), RunStatus::Running);
    }

    #[test]
    #[serial_test::serial]
    fn mission_run_status_finalized_reads_the_envelope() {
        let _g = CrewGuard::new();
        darkmux_crew::lifecycle::save_mission(&minimal_mission("m6", vec![], None)).unwrap();

        let mut m = minimal_mission("m6", vec![], None);
        m.status = MissionStatus::Finalized;

        // No envelope written yet -> degrades to Complete.
        assert_eq!(mission_run_status(&m, &[]), RunStatus::Complete);

        let clean_env = MissionEnvelope::new("m6", MissionOutcomeStatus::Clean, &[]);
        darkmux_crew::envelope::finalize_mission(&clean_env);
        assert_eq!(mission_run_status(&m, &[]), RunStatus::Complete);
    }

    #[test]
    #[serial_test::serial]
    fn mission_run_status_finalized_error_envelope_is_error() {
        let _g = CrewGuard::new();
        darkmux_crew::lifecycle::save_mission(&minimal_mission("m7", vec![], None)).unwrap();
        let mut m = minimal_mission("m7", vec![], None);
        m.status = MissionStatus::Finalized;

        let err_env = MissionEnvelope::new("m7", MissionOutcomeStatus::Error, &[]);
        darkmux_crew::envelope::finalize_mission(&err_env);
        assert_eq!(mission_run_status(&m, &[]), RunStatus::Error);
    }

    // ── lab normalization ───────────────────────────────────────────────

    fn minimal_lab_summary(dir: &str, finished: bool, degenerate: bool) -> LabRunSummary {
        LabRunSummary {
            dir: dir.to_string(),
            mtime_ms: 1_700_000_000_000,
            case_ids: vec![],
            crew: None,
            exec_mode: None,
            profile: None,
            staffing: None,
            bundles: 0,
            raw_flags: 0,
            deduped_flags: 0,
            confirmed: 0,
            needs_check: 0,
            archived: 0,
            degenerate,
            finished,
            has_funnels: true,
            has_events: true,
        }
    }

    #[test]
    fn lab_run_status_maps_finished_and_degenerate() {
        assert_eq!(lab_run_status(&minimal_lab_summary("d1", false, false)), RunStatus::Running);
        assert_eq!(lab_run_status(&minimal_lab_summary("d2", true, false)), RunStatus::Complete);
        assert_eq!(lab_run_status(&minimal_lab_summary("d3", true, true)), RunStatus::Error);
    }

    #[test]
    fn lab_summary_to_run_uses_dir_as_id_and_kind_lab() {
        let summary = minimal_lab_summary("live/case-1", true, false);
        let run = lab_summary_to_run(&summary, Some("studio".to_string()));
        assert_eq!(run.id, "live/case-1");
        assert_eq!(run.kind, RunKind::Lab);
        assert_eq!(run.status, RunStatus::Complete);
        assert!(run.tracked);
        assert_eq!(run.completed_ts, Some(1_700_000_000));
        assert_eq!(run.machine.as_deref(), Some("studio"));
    }

    // ── flow session index + route (#1518 start-OR-complete) ────────────

    #[test]
    fn build_flow_session_index_resolves_endpoint_from_complete_only() {
        let tmp = TempDir::new().unwrap();
        write_day_file(
            tmp.path(),
            &today(),
            &[
                serde_json::json!({
                    "ts": "2026-07-24T10:00:00Z",
                    "action": "dispatch start",
                    "session_id": "sess-1",
                    "handle": "reviewer",
                }),
                serde_json::json!({
                    "ts": "2026-07-24T10:05:00Z",
                    "action": "dispatch complete",
                    "session_id": "sess-1",
                    "handle": "reviewer",
                    "model": "gpt-4o",
                    "payload": { "endpoint": "azure:host/gpt-4o" },
                }),
            ],
        );
        let idx = build_flow_session_index(tmp.path());
        let agg = idx.get("sess-1").expect("session indexed");
        assert_eq!(agg.endpoint.as_deref(), Some("azure:host/gpt-4o"));
        assert_eq!(agg.terminal_status, Some(RunStatus::Complete));
        assert_eq!(agg.model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn build_flow_session_index_session_end_only_is_abandoned() {
        let tmp = TempDir::new().unwrap();
        write_day_file(
            tmp.path(),
            &today(),
            &[
                serde_json::json!({
                    "ts": "2026-07-24T10:00:00Z",
                    "action": "dispatch start",
                    "session_id": "sess-2",
                    "handle": "coder",
                }),
                serde_json::json!({
                    "ts": "2026-07-24T10:20:00Z",
                    "action": "session.end",
                    "session_id": "sess-2",
                }),
            ],
        );
        let idx = build_flow_session_index(tmp.path());
        assert_eq!(idx["sess-2"].terminal_status, Some(RunStatus::Abandoned));
    }

    #[test]
    fn build_flow_session_index_never_indexes_a_session_from_beyond_the_scan_window() {
        // (#1523 gate scale-cap) 2000-01-01 is always more than
        // RUNS_FLOW_SCAN_WINDOW_DAYS in the past, whenever this test
        // actually runs.
        let tmp = TempDir::new().unwrap();
        write_day_file(
            tmp.path(),
            "2000-01-01",
            &[serde_json::json!({
                "ts": "2000-01-01T09:00:00Z",
                "action": "dispatch start",
                "session_id": "ancient-orphan-sess",
                "handle": "coder",
            })],
        );
        let idx = build_flow_session_index(tmp.path());
        assert!(
            !idx.contains_key("ancient-orphan-sess"),
            "a session older than the scan window must never be indexed at all"
        );
    }

    // ── dedup: a mission-internal session is never ALSO a ghost ─────────

    #[test]
    fn ghost_runs_skips_a_session_already_covered_by_mission_id() {
        let mut idx = HashMap::new();
        idx.insert(
            "sess-3".to_string(),
            SessionAgg {
                mission_id: Some("real-mission-1".to_string()),
                has_start: true,
                start_ts: Some("2026-07-24T10:00:00Z".to_string()),
                ..Default::default()
            },
        );
        let mut known_missions = HashSet::new();
        known_missions.insert("real-mission-1".to_string());
        let ghosts = ghost_runs(&idx, &known_missions, &HashSet::new());
        assert!(ghosts.is_empty(), "a session covered by a loaded mission must not double-list");
    }

    #[test]
    fn ghost_runs_skips_a_session_already_covered_by_session_id() {
        // The Dispatch-kind (crew-of-one) case: mission_id is None on the
        // flow record (the module doc's "mission_id gap"), so dedup must
        // key on session_id instead.
        let mut idx = HashMap::new();
        idx.insert(
            "crew-dispatch-coder-abc".to_string(),
            SessionAgg { mission_id: None, has_start: true, ..Default::default() },
        );
        let mut known_sessions = HashSet::new();
        known_sessions.insert("crew-dispatch-coder-abc".to_string());
        let ghosts = ghost_runs(&idx, &HashSet::new(), &known_sessions);
        assert!(ghosts.is_empty());
    }

    #[test]
    fn ghost_runs_synthesizes_an_untracked_dispatch_run() {
        let mut idx = HashMap::new();
        idx.insert(
            "orphan-sess".to_string(),
            SessionAgg {
                mission_id: None,
                has_start: true,
                role: Some("coder".to_string()),
                model: Some("qwen3.6".to_string()),
                start_ts: Some("2026-07-24T10:00:00Z".to_string()),
                ..Default::default()
            },
        );
        let ghosts = ghost_runs(&idx, &HashSet::new(), &HashSet::new());
        assert_eq!(ghosts.len(), 1);
        let g = &ghosts[0];
        assert_eq!(g.id, "orphan-sess");
        assert_eq!(g.kind, RunKind::Dispatch);
        assert_eq!(g.status, RunStatus::Running);
        assert!(!g.tracked);
        assert_eq!(g.role.as_deref(), Some("coder"));
    }

    #[test]
    fn ghost_runs_never_synthesizes_a_session_with_no_start() {
        let mut idx = HashMap::new();
        idx.insert(
            "no-start-sess".to_string(),
            SessionAgg { has_start: false, ..Default::default() },
        );
        let ghosts = ghost_runs(&idx, &HashSet::new(), &HashSet::new());
        assert!(ghosts.is_empty());
    }

    // ── build_runs end to end: mission + ghost, no double-listing ───────
    //
    // One test per launch path (#1523 gate — the miss the fresh-context
    // review found: confirmatory tests only covered the crew-of-one and
    // implicit coder-phase shapes; the review-shaped and generic-config
    // paths were never independently exercised).

    #[test]
    #[serial_test::serial]
    fn build_runs_dispatch_mission_is_not_also_listed_as_a_ghost() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();

        // Mint a crew-of-one mission the way #1509's build_graph does:
        // spec.config_id == "dispatch", one phase/task/step, the step
        // carrying the minted session_id.
        let mission = minimal_mission(
            "dispatch-coder-1",
            vec!["dispatch-coder-1-phase".to_string()],
            Some(MissionSpec { config_id: "dispatch".to_string(), inputs_fingerprint: "fp".to_string() }),
        );
        darkmux_crew::lifecycle::save_mission(&mission).unwrap();
        let phase = minimal_phase(
            "dispatch-coder-1-phase",
            "dispatch-coder-1",
            vec!["dispatch-coder-1-task".to_string()],
        );
        darkmux_crew::lifecycle::save_phase(&phase).unwrap();
        let task = minimal_task(
            "dispatch-coder-1-task",
            "dispatch-coder-1-phase",
            vec!["dispatch-coder-1-step".to_string()],
            Some("coder"),
        );
        darkmux_crew::lifecycle::save_task("dispatch-coder-1", &task).unwrap();
        let step = minimal_step(
            "dispatch-coder-1-step",
            "dispatch-coder-1-task",
            Some("crew-dispatch-coder-xyz"),
        );
        darkmux_crew::lifecycle::save_step("dispatch-coder-1", "dispatch-coder-1-phase", &step).unwrap();

        // The dispatch's own flow records — mission_id DELIBERATELY absent
        // (the mission_id gap), joined only by session_id.
        write_day_file(
            flows.path(),
            &today(),
            &[
                serde_json::json!({
                    "ts": "2026-07-24T09:00:00Z",
                    "action": "dispatch start",
                    "session_id": "crew-dispatch-coder-xyz",
                    "handle": "coder",
                }),
                serde_json::json!({
                    "ts": "2026-07-24T09:10:00Z",
                    "action": "dispatch complete",
                    "session_id": "crew-dispatch-coder-xyz",
                    "handle": "coder",
                    "model": "qwen3.6-35b-a3b",
                }),
            ],
        );

        let runs = build_runs(flows.path(), None);
        assert_eq!(runs.len(), 1, "exactly one Run — the tracked mission, no ghost duplicate: {runs:?}");
        assert_eq!(runs[0].id, "dispatch-coder-1");
        assert_eq!(runs[0].kind, RunKind::Dispatch);
        assert!(runs[0].tracked);
        assert_eq!(runs[0].role.as_deref(), Some("coder"));
        assert_eq!(runs[0].model.as_deref(), Some("qwen3.6-35b-a3b"));
    }

    /// Launch path 2/4: a GENERIC `mission launch <config>` mission whose
    /// `dispatch.internal` step config carries NO explicit `session_id` —
    /// mirrors `interpret::push_step`'s real behavior (must-fix 2). The
    /// step's flow records use the step kind's own default session_id
    /// (`session_id::step(step.id)`) and carry `mission_id: null`, exactly
    /// as the real emitter does.
    #[test]
    #[serial_test::serial]
    fn build_runs_generic_config_mission_dispatch_step_is_not_also_listed_as_a_ghost() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();

        let mission = minimal_mission(
            "generic-config-1",
            vec!["p-generic".to_string()],
            Some(MissionSpec { config_id: "some-custom-config".to_string(), inputs_fingerprint: "fpg".to_string() }),
        );
        darkmux_crew::lifecycle::save_mission(&mission).unwrap();
        let phase = minimal_phase("p-generic", "generic-config-1", vec!["t-generic".to_string()]);
        darkmux_crew::lifecycle::save_phase(&phase).unwrap();
        let task = minimal_task("t-generic", "p-generic", vec!["s-generic".to_string()], Some("coder"));
        darkmux_crew::lifecycle::save_task("generic-config-1", &task).unwrap();
        // NO explicit session_id — the real `interpret::push_step` gap.
        let step = minimal_step("s-generic", "t-generic", None);
        darkmux_crew::lifecycle::save_step("generic-config-1", "p-generic", &step).unwrap();

        let default_session = darkmux_types::session_id::step("s-generic");
        write_day_file(
            flows.path(),
            &today(),
            &[
                serde_json::json!({
                    "ts": "2026-01-01T09:00:00Z",
                    "action": "dispatch start",
                    "session_id": default_session,
                    "handle": "coder",
                    // mission_id DELIBERATELY absent — matches
                    // resolve_mission_for_phase(None)'s real gap.
                }),
                serde_json::json!({
                    "ts": "2026-01-01T09:05:00Z",
                    "action": "dispatch complete",
                    "session_id": default_session,
                    "handle": "coder",
                    "model": "qwen3.6-35b-a3b",
                }),
            ],
        );

        let runs = build_runs(flows.path(), None);
        assert_eq!(runs.len(), 1, "exactly one Run for the generic-config mission, no per-step ghost: {runs:?}");
        assert_eq!(runs[0].id, "generic-config-1");
        assert_eq!(runs[0].kind, RunKind::Mission);
        assert!(runs[0].tracked);
        assert_eq!(runs[0].model.as_deref(), Some("qwen3.6-35b-a3b"));
    }

    /// Launch path 1/4 (the flagship path): a review-shaped mission whose
    /// run-level bookend session is keyed on the CASE STRING (not any
    /// step's structural session_id) — proves the `mission_id`-index join
    /// (must-fix 1: `review_bookend_record` now stamps `mission_id`, so
    /// this session is findable via `mission_id_index`, not `step_sessions`).
    #[test]
    #[serial_test::serial]
    fn build_runs_review_shaped_mission_case_bookend_session_is_not_also_listed_as_a_ghost() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();

        let mission = minimal_mission(
            "review-1700000000-abcdef",
            vec!["p-investigate".to_string()],
            Some(MissionSpec { config_id: "review".to_string(), inputs_fingerprint: "fpr".to_string() }),
        );
        darkmux_crew::lifecycle::save_mission(&mission).unwrap();
        let phase = minimal_phase("p-investigate", "review-1700000000-abcdef", vec![]);
        darkmux_crew::lifecycle::save_phase(&phase).unwrap();

        // The run-level case-string bookend session — post-fix, carries
        // mission_id (see `review_bookend_record`'s doc in
        // `src/mission_launch_review.rs`).
        write_day_file(
            flows.path(),
            &today(),
            &[
                serde_json::json!({
                    "ts": "2026-01-01T08:00:00Z",
                    "action": "dispatch start",
                    "session_id": "owner/repo@deadbeef",
                    "handle": "review-probe-mid,review-judge",
                    "mission_id": "review-1700000000-abcdef",
                }),
                serde_json::json!({
                    "ts": "2026-01-01T08:20:00Z",
                    "action": "dispatch complete",
                    "session_id": "owner/repo@deadbeef",
                    "handle": "review-probe-mid,review-judge",
                    "model": "gpt-4o",
                    "mission_id": "review-1700000000-abcdef",
                    "payload": { "endpoint": "azure:myorg.cognitiveservices.azure.com/gpt-4o" },
                }),
            ],
        );

        let runs = build_runs(flows.path(), None);
        assert_eq!(
            runs.len(),
            1,
            "exactly one Run for the review mission, no ghost from the case-bookend session: {runs:?}"
        );
        assert_eq!(runs[0].id, "review-1700000000-abcdef");
        assert_eq!(runs[0].kind, RunKind::Mission);
        assert!(runs[0].tracked);
        assert_eq!(runs[0].route.as_deref(), Some("azure:myorg.cognitiveservices.azure.com/gpt-4o"));
        assert_eq!(runs[0].model.as_deref(), Some("gpt-4o"));
    }

    /// Launch path 5: a mission whose process crashed mid-dispatch — the
    /// mission record is stuck `Active` forever, but its dispatch's
    /// `session.end` close-edge tells the true story (#1523 gate
    /// CONSIDER 3).
    #[test]
    #[serial_test::serial]
    fn build_runs_crashed_active_mission_reports_abandoned_not_eternal_running() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();

        let mission = minimal_mission(
            "dispatch-crashed-1",
            vec!["p-crash".to_string()],
            Some(MissionSpec { config_id: "dispatch".to_string(), inputs_fingerprint: "fpc".to_string() }),
        );
        darkmux_crew::lifecycle::save_mission(&mission).unwrap();
        let phase = minimal_phase("p-crash", "dispatch-crashed-1", vec!["t-crash".to_string()]);
        darkmux_crew::lifecycle::save_phase(&phase).unwrap();
        let task = minimal_task("t-crash", "p-crash", vec!["s-crash".to_string()], Some("coder"));
        darkmux_crew::lifecycle::save_task("dispatch-crashed-1", &task).unwrap();
        let step = minimal_step("s-crash", "t-crash", Some("crew-dispatch-coder-crashed"));
        darkmux_crew::lifecycle::save_step("dispatch-crashed-1", "p-crash", &step).unwrap();
        // mission.json is never touched again — it stays Active forever,
        // exactly as it would after a hard host crash mid-dispatch.

        write_day_file(
            flows.path(),
            &today(),
            &[
                serde_json::json!({
                    "ts": "2026-01-01T09:00:00Z",
                    "action": "dispatch start",
                    "session_id": "crew-dispatch-coder-crashed",
                    "handle": "coder",
                }),
                serde_json::json!({
                    "ts": "2026-01-01T09:05:00Z",
                    "action": "session.end",
                    "session_id": "crew-dispatch-coder-crashed",
                }),
            ],
        );

        let runs = build_runs(flows.path(), None);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].status, RunStatus::Abandoned, "a crashed session must not read as eternal Running");
    }

    #[test]
    #[serial_test::serial]
    fn build_runs_includes_an_untracked_ghost_alongside_a_tracked_mission() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();

        let mission = minimal_mission(
            "dispatch-coder-2",
            vec!["p-2".to_string()],
            Some(MissionSpec { config_id: "dispatch".to_string(), inputs_fingerprint: "fp2".to_string() }),
        );
        darkmux_crew::lifecycle::save_mission(&mission).unwrap();
        let phase = minimal_phase("p-2", "dispatch-coder-2", vec!["t-2".to_string()]);
        darkmux_crew::lifecycle::save_phase(&phase).unwrap();
        let task = minimal_task("t-2", "p-2", vec!["s-2".to_string()], Some("coder"));
        darkmux_crew::lifecycle::save_task("dispatch-coder-2", &task).unwrap();
        let step = minimal_step("s-2", "t-2", Some("crew-dispatch-coder-known"));
        darkmux_crew::lifecycle::save_step("dispatch-coder-2", "p-2", &step).unwrap();

        write_day_file(
            flows.path(),
            &today(),
            &[
                // The tracked mission's own session.
                serde_json::json!({
                    "ts": "2026-07-24T09:00:00Z",
                    "action": "dispatch start",
                    "session_id": "crew-dispatch-coder-known",
                }),
                // A genuinely orphaned session — no mission ever minted.
                serde_json::json!({
                    "ts": "2026-07-24T11:00:00Z",
                    "action": "dispatch start",
                    "session_id": "crew-dispatch-reviewer-orphan",
                    "handle": "reviewer",
                }),
            ],
        );

        let runs = build_runs(flows.path(), None);
        assert_eq!(runs.len(), 2, "{runs:?}");
        let tracked = runs.iter().find(|r| r.id == "dispatch-coder-2").expect("tracked run present");
        assert!(tracked.tracked);
        let ghost = runs
            .iter()
            .find(|r| r.id == "crew-dispatch-reviewer-orphan")
            .expect("ghost run present");
        assert!(!ghost.tracked);
        assert_eq!(ghost.status, RunStatus::Running);
    }
}
