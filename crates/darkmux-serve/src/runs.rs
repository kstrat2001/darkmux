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
//!
//! ## Two callers, one union (#1905)
//!
//! [`build_runs`] and [`Run`] (with [`RunKind`]/[`RunStatus`]) are `pub` —
//! this crate's own `runs_handler` (`GET /runs`) AND the root binary's
//! `darkmux run list` verb both call this SAME function against the SAME
//! inputs. Neither may compute its own union: a view that needs a field
//! this module doesn't expose widens the response, it never re-derives
//! membership/status/identity alongside it (operator direction, #1905's
//! settled design). See `src/run_list.rs` (root binary crate) for the CLI
//! side of that contract.

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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "lowercase")]
pub enum RunKind {
    Mission,
    Dispatch,
    Lab,
}

/// The run's flat lifecycle status. See each source's own mapping:
/// [`mission_run_status`] (missions/dispatches), [`lab_run_status`] (lab
/// runs), [`ghost_runs`] (untracked flow-only sessions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Planned,
    Running,
    Complete,
    Error,
    Abandoned,
    /// (#1881) This binary could not determine the run's real outcome —
    /// either its `envelope.json` failed to deserialize at all (a newer
    /// darkmux wrote a shape this reader's `MissionEnvelope`/`RunOutcome`
    /// don't recognize and doesn't yet have `#[serde(other)]` cover for),
    /// or it parsed but reported a `status` value this reader's
    /// `MissionOutcomeStatus` doesn't recognize. Deliberately distinct from
    /// every other value here: `Complete`/`Error`/`Abandoned` are all
    /// verdicts this binary is CONFIDENT in; `Unparseable` is the honest
    /// "I don't know" a viewer must never fold into a green run. See
    /// `mission_run_status`'s `MissionStatus::Finalized` arm for where this
    /// is decided.
    Unparseable,
}

/// One row of the `/runs` view-model. Lenient-on-read WIRE shape (every
/// field but `id`/`kind`/`status`/`tracked` is optional) — this is NEVER
/// persisted, so there's no schema-version discipline to carry; a future
/// consumer (the step-4 Runs lens) just reads whatever's present.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct Run {
    pub id: String,
    pub kind: RunKind,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub machine: Option<String>,
    /// Endpoint label (e.g. `"azure:host/gpt-4o"`) when any of the run's
    /// dispatches used a hosted endpoint; `None` = local LMStudio (or no
    /// flow session found at all). See the module doc's join-key section
    /// for how this is resolved per `kind`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub route: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub model: Option<String>,
    // (UI port Packet 1) `#[ts(type = "number")]` overrides ts-rs's default
    // u64 -> bigint mapping. The wire format is plain `JSON.parse` (never
    // serde_json's stringify-large-ints convention), so the browser always
    // sees a JS `number` here, not a `bigint` — these are Unix EPOCH SECONDS,
    // safe within `Number.MAX_SAFE_INTEGER` for millennia. Leaving the
    // default `bigint` mapping would type-check against a value `JSON.parse`
    // never actually produces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional, type = "number"))]
    pub started_ts: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional, type = "number"))]
    pub completed_ts: Option<u64>,
    /// (#1584) **When this run was last active** — the one field the runs
    /// lens can always order by, across all three sources.
    ///
    /// `started_ts`/`completed_ts` are deliberately absent whenever the
    /// source doesn't genuinely know them, which was honest while nothing
    /// sorted on them — but a run with NEITHER is unorderable, and that is
    /// not a rare corner: an unfinished lab run has no start timestamp
    /// (`LabRunSummary` records none) and no completion timestamp (it never
    /// reached `scores.json`), so on a real machine dozens of rows carry no
    /// time at all. This field is populated for every source with the best
    /// activity signal each one actually has — newest-artifact mtime for a
    /// lab run, completion-else-start for a mission/dispatch/ghost — so
    /// "newest first" is a total order rather than one with a large
    /// arbitrarily-ordered tail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional, type = "number"))]
    pub updated_ts: Option<u64>,
    /// `false` = a flow-only ghost with no durable record backing it (see
    /// the module doc's "untracked" synthesis). `true` for every mission
    /// and lab run — both have a durable artifact on disk.
    pub tracked: bool,
    /// (#1915) The flow session this row can be drilled into via
    /// `#session=<id>` — the SAME representative-session pick
    /// [`mission_to_run`]/[`flow_mission_to_run`] already make for
    /// role/model/route, now also carried out to the client instead of
    /// being computed and thrown away. Populated for every `Mission` row
    /// (tracked or not) from its representative session, and for a
    /// [`ghost_runs`] dispatch row from the row's OWN id (a ghost's `id`
    /// already IS a session id — see that function's own `Run` literal).
    /// Always `None` for a lab row: a lab run has no flow session backing
    /// it to drill into at all.
    ///
    /// **Why every mission carries this, not just untracked ones:** a
    /// TRACKED mission never actually needs it — `runDestination`
    /// (`ui/src/lenses/runs/format.ts`) resolves a tracked row to
    /// `#mission=<id>` (the mission GRAPH) before this field is ever
    /// consulted, and that stays true: `/mission/<id>/graph.json` is
    /// served from THIS machine's own durable `Mission`/`Phase`/`Task`/
    /// `Step` state, which an untracked row — by definition — does not
    /// have here (it either ran on a peer, #1705, or never got a durable
    /// record at all). An untracked mission can open its representative
    /// SESSION, never its graph; that limit is structural, not a gap this
    /// field closes. Carrying it uniformly means the client's own rule
    /// ("untracked and has a `session_id`" — see `runDestination`'s doc)
    /// never needs a kind-specific carve-out, for missions OR any future
    /// kind that gains the same shape.
    ///
    /// **`None` also when the representative session is ambiguous
    /// (#1918).** A flow-emitter defect (the scheduler stamps
    /// `session_id` from the TASK id, which carries no per-run identity)
    /// means a "session" can in practice be a bucket several different
    /// missions' records collapsed into — measured live at 49 missions
    /// sharing one session id. `mission_to_run`/`flow_mission_to_run`/
    /// `ghost_runs` each check `SessionAgg::is_ambiguous` before handing
    /// this field a value; when it fires, this stays `None` even though a
    /// representative session technically exists, because that session
    /// cannot be attributed to any one mission. No destination is the
    /// honest answer: an inert row is a smaller failure than a row that
    /// opens a DIFFERENT mission's work while looking like it opened this
    /// one. The root cause (the scheduler's id scheme) is a separate,
    /// deliberately-versioned fix — this field only refuses to act on the
    /// corruption, it does not repair it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub session_id: Option<String>,
}

/// Build the full run union — the SAME `Vec<Run>` both `runs_handler`
/// (`GET /runs`, from a `spawn_blocking` task) and the root binary's
/// `darkmux run list` verb (`src/run_list.rs`) call. `pub` since #1905:
/// neither caller may compute its own union or filter at this layer — see
/// the module doc's "Two callers, one union" section. Never panics on a
/// missing/malformed source: `load_missions`/`load_phases` degrade to empty
/// via `unwrap_or_default` (matching `missions_handler`'s own posture), and
/// `crate::scan_lab_runs` is already resilient (best-effort scan, #1247).
pub fn build_runs(
    flows_dir: &StdPath,
    lab_dir: Option<&StdPath>,
    fleet: &[serde_json::Value],
) -> Vec<Run> {
    let flow_index = build_flow_session_index(flows_dir, fleet);
    // (#1705) Mission-level rollup over the SAME merged record set. A
    // mission owned by another machine has no durable record here — its
    // `Mission` JSON lives on the machine that ran it — so without this it
    // could only ever appear as a scatter of per-session ghosts. One
    // review = one row, wherever it ran.
    let flow_missions = build_flow_mission_index(flows_dir, fleet);
    // (#1523 gate CONSIDER 2) Pre-group flow sessions by `mission_id` ONCE
    // — an O(sessions) pass — rather than filtering the whole `flow_index`
    // per mission (O(missions × sessions), the shape a Studio-scale flow
    // archive with many missions would make genuinely slow).
    let mission_id_index = build_mission_id_index(&flow_index);

    // (#1621, widened #1642/#1633) Read the clock ONCE for the whole build,
    // so every row in a response — mission, lab, OR ghost — is judged
    // against the same instant, and so the SAME staleness decision
    // (`stale_after_ms`/`session_is_live`) gates all three `Run` kinds
    // rather than just lab runs.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

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
        let run = mission_to_run(
            mission,
            kind,
            shape.as_ref(),
            &step_sessions,
            &mission_id_index,
            &flow_index,
            now_ms,
        );
        runs.push(run);
    }

    // (#1523 gate CONSIDER 7) Resolved ONCE — every lab run is
    // machine-local by the SAME construction, so there's no reason to
    // re-read config for each one.
    let lab_machine = darkmux_types::config_access::machine_id();
    if let Some(dir) = lab_dir {
        for summary in crate::scan_lab_runs(dir) {
            runs.push(lab_summary_to_run(&summary, lab_machine.clone(), now_ms));
        }
    }

    // (#1705) Missions seen only in the record stream — i.e. executing on a
    // peer. Emitted BEFORE ghosts so their sessions are claimed and don't
    // also surface as loose dispatch rows.
    let mut remote_mission_ids: HashSet<String> = HashSet::new();
    for (mission_id, agg) in &flow_missions {
        if known_mission_ids.contains(mission_id) {
            continue;
        }
        remote_mission_ids.insert(mission_id.clone());
        runs.push(flow_mission_to_run(mission_id, agg, &flow_index, now_ms));
    }

    runs.extend(ghost_runs(
        &flow_index,
        &known_mission_ids,
        &known_session_ids,
        &remote_mission_ids,
        now_ms,
    ));

    runs
}

/// Per-`mission_id` rollup over the merged record stream (#1705) — the
/// substrate for missions this daemon can SEE but does not OWN.
///
/// A mission's durable `Mission`/`Phase`/`Task`/`Step` JSON lives on the
/// machine that ran it, so a peer's mission is invisible to
/// `load_missions()` here no matter how much of its work crosses the flow
/// stream. This index is deliberately thin: identity, machine, span, and
/// whether a terminal mission-lifecycle record was seen. Everything richer
/// (the task graph, per-step config) genuinely is not available off-machine,
/// and inventing it would be worse than omitting it.
#[derive(Debug, Default, Clone)]
struct FlowMissionAgg {
    machine: Option<String>,
    first_ts: Option<String>,
    last_ts: Option<String>,
    /// The terminal mission-lifecycle record, if one was seen: its stamp and
    /// whether it was an ABORT.
    ///
    /// The distinction is load-bearing, not cosmetic (#1627, mirrored from
    /// the tracked path's `MissionStatus::Aborted => RunStatus::Abandoned`):
    /// a torn-down mission is not a completed one, and collapsing the two
    /// would let a killed run inherit a success verdict it never earned —
    /// on every peer's viewer, while the owning machine correctly showed it
    /// Abandoned.
    terminal_ts: Option<String>,
    terminal_was_abort: bool,
    /// Session ids observed under this mission, used to borrow role/model/
    /// endpoint for the row without a second pass.
    session_ids: Vec<String>,
}

fn build_flow_mission_index(
    flows_dir: &StdPath,
    fleet: &[serde_json::Value],
) -> HashMap<String, FlowMissionAgg> {
    let mut idx: HashMap<String, FlowMissionAgg> = HashMap::new();
    // (#1707 gate MUST FIX 2) The fleet half obeys the SAME
    // `RUNS_FLOW_SCAN_WINDOW_DAYS` bound the local walk does. Without this
    // the two sources disagree about how far back `/runs` reaches, and the
    // stream is the WORSE offender: `XADD MAXLEN ~` trims lazily, only on
    // write, so a fleet that has gone quiet still holds month-old records —
    // which would resurface dead missions and un-terminated sessions as
    // Abandoned rows that never age out. This bites the single-machine
    // redis-enabled operator too, not just a fleet.
    let fleet_cutoff = cutoff_date_string(RUNS_FLOW_SCAN_WINDOW_DAYS);
    let within_window = |v: &serde_json::Value| -> bool {
        match v.get("ts").and_then(|t| t.as_str()) {
            // Lexical compare on the `YYYY-MM-DD` prefix — the same trick
            // `for_each_recent_flow_record` uses on day-file names.
            Some(ts) if ts.len() >= 10 => ts[..10] >= fleet_cutoff[..],
            // No parseable ts: keep it. Dropping an unattributable record
            // would silently narrow the view, which is this issue's own bug.
            _ => true,
        }
    };

    let fleet_seen: std::collections::HashSet<String> =
        fleet.iter().filter(|v| within_window(v)).map(crate::flow_record_identity).collect();

    let fold = |idx: &mut HashMap<String, FlowMissionAgg>, v: &serde_json::Value| {
        let Some(mid) = v.get("mission_id").and_then(|m| m.as_str()) else {
            return;
        };
        if mid.is_empty() {
            return;
        }
        let ts = v.get("ts").and_then(|t| t.as_str()).unwrap_or("");
        let agg = idx.entry(mid.to_string()).or_default();
        if agg.machine.is_none() {
            if let Some(m) = v.get("machine_id").and_then(|m| m.as_str()) {
                if !m.is_empty() {
                    agg.machine = Some(m.to_string());
                }
            }
        }
        if !ts.is_empty() {
            if agg.first_ts.as_deref().map(|cur| ts < cur).unwrap_or(true) {
                agg.first_ts = Some(ts.to_string());
            }
            if agg.last_ts.as_deref().map(|cur| ts > cur).unwrap_or(true) {
                agg.last_ts = Some(ts.to_string());
            }
        }
        // The only terminal mission-lifecycle actions the emitter actually
        // writes are `mission close` and `mission abort`
        // (`darkmux_crew::lifecycle`); `mission start` is the opening
        // bookend. Matching vocabulary that is never emitted would read as
        // real coverage to the next person who greps for it.
        let action = v.get("action").and_then(|a| a.as_str()).unwrap_or("");
        if matches!(action, "mission close" | "mission abort") && agg.terminal_ts.is_none() {
            agg.terminal_ts = Some(ts.to_string());
            agg.terminal_was_abort = action == "mission abort";
        }
        if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
            if !sid.is_empty() && !agg.session_ids.iter().any(|s| s == sid) {
                agg.session_ids.push(sid.to_string());
            }
        }
    };

    for v in fleet.iter().filter(|v| within_window(v)) {
        fold(&mut idx, v);
    }
    for_each_recent_flow_record(flows_dir, |v| {
        if !fleet_seen.is_empty() && fleet_seen.contains(&crate::flow_record_identity(v)) {
            return std::ops::ControlFlow::Continue(());
        }
        fold(&mut idx, v);
        std::ops::ControlFlow::Continue(())
    });
    idx
}

/// One [`Run`] for a mission this daemon observed but does not own (#1705).
///
/// `tracked: false` is the honest flag: there is no durable run record on
/// THIS machine backing it. That is the same claim ghost rows make, and it
/// is what lets the viewer distinguish "I have the mission" from "I can see
/// the mission."
///
/// Status is deliberately conservative. A terminal mission-lifecycle record
/// means Complete. Otherwise the row is Running only while its sessions
/// still look live by the SAME `session_is_live` staleness rule every other
/// row obeys — a peer that goes to sleep mid-mission must not leave a row
/// claiming to be running forever.
fn flow_mission_to_run(
    mission_id: &str,
    agg: &FlowMissionAgg,
    flow_index: &HashMap<String, SessionAgg>,
    now_ms: u64,
) -> Run {
    // (#1915) Pairs, not bare aggs — same reason `mission_to_run` carries
    // them: `session_id` below needs to know WHICH session won, not just
    // its fields.
    let sessions: Vec<(&str, &SessionAgg)> = agg
        .session_ids
        .iter()
        .filter_map(|s| flow_index.get(s.as_str()).map(|a| (s.as_str(), a)))
        .collect();
    let any_live = sessions.iter().any(|(_, s)| session_is_live(s, now_ms));
    let status = match (&agg.terminal_ts, agg.terminal_was_abort) {
        // #1627 again: abort is teardown, not success.
        (Some(_), true) => RunStatus::Abandoned,
        (Some(_), false) => RunStatus::Complete,
        (None, _) if any_live => RunStatus::Running,
        (None, _) => RunStatus::Abandoned,
    };
    // Borrow route/model from whichever session first resolved one — a
    // mission-level row has no endpoint of its own, and showing the seat's
    // is more informative than showing nothing.
    let route = sessions.iter().find_map(|(_, s)| s.endpoint.clone());
    let model = sessions.iter().find_map(|(_, s)| s.model.clone());
    // (#1915) This IS the fix: a mission this daemon only sees via the
    // fleet stream is exactly the row #1915 diagnosed as inert — `tracked:
    // false` below with no drill target at all. The SAME representative-
    // session rule `mission_to_run` uses (`earliest_by_start`), so a
    // mission tracked locally and one only seen remotely pick their
    // representative session the same way.
    //
    // (#1918) But only when that session is UNAMBIGUOUS. The scheduler
    // defect #1918 diagnosed means a session can carry records from
    // multiple missions — opening it from this row could land on someone
    // else's work, which is worse than the inert row #1915 fixed. See
    // `SessionAgg::is_ambiguous`'s own doc.
    let session_id = earliest_by_start(&sessions)
        .filter(|(_, agg)| !agg.is_ambiguous())
        .map(|(sid, _)| sid.to_string());
    Run {
        id: mission_id.to_string(),
        kind: RunKind::Mission,
        status,
        machine: agg.machine.clone(),
        route,
        role: None,
        model,
        started_ts: agg.first_ts.as_deref().and_then(parse_flow_ts),
        // An aborted mission has a terminal stamp but no COMPLETION — the
        // tracked path makes the same distinction.
        completed_ts: if agg.terminal_was_abort {
            None
        } else {
            agg.terminal_ts.as_deref().and_then(parse_flow_ts)
        },
        updated_ts: agg
            .terminal_ts
            .as_deref()
            .or(agg.last_ts.as_deref())
            .and_then(parse_flow_ts),
        tracked: false,
        session_id,
    }
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
    now_ms: u64,
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
    // (#1915) Pairs, not bare aggs — see `earliest_by_start`'s own doc for
    // why: carrying the id alongside its agg is what lets `representative`
    // hand its OWN session id to the `Run` (`session_id` below) without a
    // second, separately-implemented search that could disagree about
    // which session actually won.
    let sessions: Vec<(&str, &SessionAgg)> = candidate_ids
        .into_iter()
        .filter_map(|sid| flow_index.get(sid).map(|agg| (sid, agg)))
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
            .filter(|(_, s)| s.endpoint.is_some())
            .collect::<Vec<_>>(),
    );

    // (#1877 regression) `representative` (earliest_by_start) is right for
    // anything that genuinely is about ORDERING — `start_ts` below really
    // should come from the mission's earliest dispatch. Role and model are
    // ATTRIBUTE lookups, not ordering, and the #1877 whole-run bookend is
    // deliberately the mission's earliest record (it opens before any step
    // dispatches) — so reading role/model only off `representative` shows
    // a stale-by-construction value whenever the bookend wins the pick.
    // `sessions` is built from a HashSet (`candidate_ids`), so sorting a
    // copy by `start_ts` (rather than trusting HashSet iteration order)
    // keeps "first" deterministic; ISO-8601 sorts correctly as a plain
    // string, same property `earliest_by_start` itself relies on.
    //
    // (#1877 QA must-fix 2) Filter to `start_ts.is_some()` BEFORE the sort,
    // matching `earliest_by_start`'s own filter — a session with a terminal
    // record but no `dispatch start` (a start truncated out of the
    // `RUNS_FLOW_SCAN_WINDOW_DAYS` window, or evicted by Redis's `XADD
    // MAXLEN ~` while its complete survives) carries `start_ts: None`, and
    // `Option::cmp` orders `None` BEFORE `Some` — so an unfiltered sort put
    // that start-less session first and let its `handle`/`model` win both
    // attributes over every real dispatch session, exactly the corruption
    // `earliest_by_start` itself is already immune to.
    let mut sessions_by_start: Vec<&SessionAgg> =
        sessions.iter().map(|(_, s)| *s).filter(|s| s.start_ts.is_some()).collect();
    sessions_by_start.sort_by(|a, b| a.start_ts.cmp(&b.start_ts));

    // Model is simple: the bookend's own record NEVER carries one
    // (`mission_bookend_record` passes `model: None` unconditionally, one
    // dispatch bookend spans however many per-step model calls a mission
    // makes), so a plain "first session that resolved one" — same idiom
    // as `flow_mission_to_run`'s route/model fallback above — is enough.
    let model = sessions_by_start.iter().find_map(|s| s.model.clone());

    // Role needs one more step: the bookend's `handle` is the LAUNCHED
    // CONFIG ID (`mission_bookend_record`'s `role_id` param), which is a
    // real, non-empty string — so a plain find_map "resolves" it
    // immediately and never reaches the coder/reviewer/etc. step's actual
    // role. Prefer the first NON-bookend session (source != "mission")
    // that resolved a role; only reach for the bookend's own placeholder
    // if nothing else did — which is the honest outcome for a Tier-1-only
    // procedural mission that never dispatches a model at all (#1877's own
    // named gap 2), where the bookend's config-id label is the best
    // available information, not a display bug.
    let is_bookend = |s: &&SessionAgg| s.source.as_deref() == Some("mission");
    let role = dispatch_role
        .or_else(|| sessions_by_start.iter().filter(|s| !is_bookend(s)).find_map(|s| s.role.clone()))
        .or_else(|| sessions_by_start.iter().find_map(|s| s.role.clone()));

    // `machine` deliberately stays representative-only, unlike role/model
    // above: EVERY flow record — the #1877 bookend included — gets
    // `machine_id` auto-stamped at write time whenever the caller left it
    // unset (`darkmux_flow::record`'s provenance stamp, CLAUDE.md's
    // "stamped at record-write time" contract), so the bookend session is
    // never the one blanking this field the way it blanks role/model.
    let machine = representative.and_then(|(_, s)| s.machine.clone());
    let route = remote.and_then(|(_, s)| s.endpoint.clone());
    let start_ts_str = representative.and_then(|(_, s)| s.start_ts.clone());
    let terminal_ts_str = sessions.iter().filter_map(|(_, s)| s.terminal_ts.clone()).max();
    // (#1915) The drill target — see `Run::session_id`'s own doc for why
    // this is populated for every mission row, tracked or not.
    //
    // (#1918) Suppressed when the representative session is ambiguous —
    // same rule and same reasoning as `flow_mission_to_run`'s identical
    // guard: a session spanning more than one mission is not a valid
    // drill target for any of them.
    let session_id = representative.filter(|(_, agg)| !agg.is_ambiguous()).map(|(sid, _)| sid.to_string());

    let started_ts = mission
        .started_ts
        .or_else(|| start_ts_str.as_deref().and_then(parse_flow_ts));
    let completed_ts = mission
        .finalized_ts
        .or_else(|| terminal_ts_str.as_deref().and_then(parse_flow_ts));

    let sessions_bare: Vec<&SessionAgg> = sessions.iter().map(|(_, s)| *s).collect();
    Run {
        id: mission.id.clone(),
        kind,
        status: mission_run_status(mission, &sessions_bare, now_ms),
        machine,
        route,
        role,
        model,
        started_ts,
        completed_ts,
        // (#1584) Completion is the truest "last active" for a finished
        // mission; a still-running one has only its start; a PLANNED mission
        // has never dispatched at all, and falls back to when it was minted
        // — without which it would carry no time and sort below runs that
        // died months ago, which is the exact failure this field exists to
        // prevent. `created_ts` is non-optional on `Mission`, so this arm
        // makes the field's "always populated" contract total for this path.
        updated_ts: completed_ts.or(started_ts).or(Some(mission.created_ts)),
        tracked: true,
        session_id,
    }
}

/// Map a `Mission`'s own lifecycle status to the flat [`RunStatus`],
/// cross-checked against its joined flow `sessions` for two cases the
/// mission record alone can't see (#1523 gate CONSIDERs 3 + 4).
///
/// (#1627, corrected #1660) `mission abort` writes its OWN terminal,
/// `MissionStatus::Aborted`, which `mission_run_status` maps straight to
/// `RunStatus::Abandoned` — this comment previously claimed both verbs
/// drove a mission to `Finalized`, which stopped being true when a
/// teardown stopped being recorded as a success. The envelope reading
/// below applies to a genuinely FINALIZED mission; an abort never reaches
/// it, precisely so a killed run can't inherit a verdict it never earned.
///
/// A `Finalized` mission is told apart from a degraded one by its
/// `MissionEnvelope`'s outcome
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
///
/// **(#1642, #1633) The staleness gate — same one `lab_run_status` and
/// `ghost_runs` apply.** The CONSIDER-3 branch above only catches a crash
/// once every known session already reached a terminal; a mission whose
/// sessions are still nominally "open" (no terminal ever landed, because the
/// process died mid-dispatch) fell straight through to the plain `Running`
/// at the bottom, forever. [`session_is_live`] closes that: when the
/// all-terminal branch doesn't apply, the mission is `Running` only while
/// SOME known session shows recent proof-of-work; otherwise `Abandoned`. A
/// just-launched mission with `started_ts` set but no sessions dispatched
/// yet is real and must not be misread as abandoned — `started_ts` itself
/// is the activity anchor for that case.
fn mission_run_status(mission: &Mission, sessions: &[&SessionAgg], now_ms: u64) -> RunStatus {
    match mission.status {
        MissionStatus::Active | MissionStatus::Paused => {
            let Some(started_ts) = mission.started_ts else {
                return RunStatus::Planned;
            };
            if !sessions.is_empty() && sessions.iter().all(|s| s.terminal_status.is_some()) {
                if sessions.iter().any(|s| s.terminal_status == Some(RunStatus::Abandoned)) {
                    return RunStatus::Abandoned;
                }
                if sessions.iter().any(|s| s.terminal_status == Some(RunStatus::Error)) {
                    return RunStatus::Error;
                }
                return RunStatus::Running;
            }
            // (#1642) A PAUSED mission is deliberately idle, so the staleness
            // gate must not touch it. The gate reads "went quiet without
            // finishing" as abandonment, which is honest for an Active
            // mission and a lie for a paused one — it would relabel the
            // operator's own intent as a failure the moment a pause outlasts
            // the inactivity budget (`mission launch` → `mission pause` →
            // lunch → the board says Abandoned). Not decaying is the lesser
            // error: `RunStatus` has no `Paused` variant, so some imprecision
            // is unavoidable here, and over-reporting a mission the operator
            // KNOWS they paused costs nothing, while calling it abandoned
            // actively misinforms.
            if mission.status == MissionStatus::Paused {
                return RunStatus::Running;
            }
            let live = if sessions.is_empty() {
                let idle_ms = now_ms.saturating_sub(started_ts.saturating_mul(1_000));
                idle_ms <= stale_after_ms()
            } else {
                sessions.iter().any(|s| session_is_live(s, now_ms))
            };
            if live {
                RunStatus::Running
            } else {
                RunStatus::Abandoned
            }
        }
        // (#1627) A torn-down mission is NOT a completed one, and must never
        // resolve through the envelope branch below — an abort leaves whatever
        // envelope the run had written before it died, so reading it would let
        // a killed run inherit a success verdict it never earned.
        MissionStatus::Aborted => RunStatus::Abandoned,
        MissionStatus::Finalized => match darkmux_crew::lifecycle::load_envelope(&mission.id) {
            // (#1881) `load_envelope` failed to deserialize `envelope.json`
            // — a newer darkmux wrote a `status`/`outcome` shape this
            // reader's `MissionEnvelope` doesn't recognize (the fleet's
            // deliberately heterogeneous machines: laptop on a
            // `cargo install`ed main, Studio on brew/stable — CLAUDE.md's
            // "cross-system contracts" section). The PREVIOUS version of
            // this match read the envelope with `.ok().flatten()`, which
            // discarded this exact `Err` and fell through to the `_ =>
            // Complete` arm below — silently rendering a record this
            // binary could NOT read as a completed, green run. This arm is
            // the fix: a genuine parse failure gets its own honest state
            // instead of the happiest available guess.
            //
            // (#1881, QA-considered) The error itself is discarded here on
            // purpose, not by oversight: `build_runs` calls this per
            // mission on EVERY `/runs` poll, so an unconditional
            // `eprintln!` here would spam the daemon's stderr once per
            // request for as long as a broken envelope sits on disk — the
            // exact kind of unbounded, unstructured noise this project's
            // observability doctrine argues against. The breadcrumb lives
            // in `darkmux doctor`'s "mission envelope readability" check
            // instead (below in this crate's sibling
            // `crates/darkmux-doctor/src/lib.rs`): on-demand, names the
            // mission id AND the parse error, and is exactly where an
            // operator investigating an amber dashboard row is already
            // pointed. A future rate-limited or once-per-mission log line
            // here would be a genuine improvement, not ruled out — it just
            // isn't the cheap fix an unconditional `eprintln!` would be.
            Err(_) => RunStatus::Unparseable,
            // No envelope at all — a pre-#1284 mint, or a mint that never
            // reached finalization's write. Genuinely no data, not a parse
            // failure; degrades to `Complete` rather than guessing —
            // unchanged by #1881, which is about records that EXIST but
            // can't be read, not records that were never written.
            Ok(None) => RunStatus::Complete,
            Ok(Some(envelope)) => {
                // (#1877 item 4 — stated decision) `envelope.outcome`'s typed
                // `RunOutcome::Partial` is NOT read here. `RunStatus` has no
                // partial-coverage value among its states
                // (`Planned`/`Running`/`Complete`/`Abandoned`/`Error`/
                // `Unparseable`), and `status` already collapses
                // `Partial` into `Degraded` before this match ever runs
                // (`MissionOutcomeStatus::from_outcome`), so a Partial review's
                // `Degraded` status falls into the `Clean | Degraded => Complete`
                // arm below — same as it did before #1877, when Degraded was
                // purely convention. Widening `RunStatus` to distinguish
                // "complete" from "complete but constrained" is a real,
                // separate feature (a dashboard-visible partial badge) this
                // PR does not add.
                //
                // (#1881) `outcome`'s own leniency (`RunOutcome::Unknown`,
                // `run_outcome.rs`) is likewise not read here — unaffected
                // by this fix, same reasoning as the paragraph above. What
                // #1881 DOES change: `envelope.status` itself can now be
                // `MissionOutcomeStatus::Unknown` (a status value this
                // binary doesn't recognize, degraded via `#[serde(other)]`
                // rather than failing the whole parse) — and unlike
                // `outcome`, `status` IS what this match reads. An unknown
                // status is exactly the "this binary cannot tell you what
                // happened" case `Unparseable` exists for, so it gets its
                // own arm rather than falling into the `Clean | Degraded`
                // wildcard the way a genuinely-known Degraded/Clean does.
                match envelope.status {
                    MissionOutcomeStatus::Error | MissionOutcomeStatus::Degenerate => RunStatus::Error,
                    MissionOutcomeStatus::Unknown => RunStatus::Unparseable,
                    MissionOutcomeStatus::Clean | MissionOutcomeStatus::Degraded => RunStatus::Complete,
                }
            }
        },
    }
}

// ─── Lab normalization ──────────────────────────────────────────────────────

/// Normalize one `LabRunSummary` (the SAME row `/lab/runs` returns) into a
/// [`Run`]. `machine` is resolved ONCE by the caller ([`build_runs`]) and
/// passed in — every lab run shares the same daemon-declared machine
/// (#1523 gate CONSIDER 7).
fn lab_summary_to_run(summary: &LabRunSummary, machine: Option<String>, now_ms: u64) -> Run {
    let (role, model, route) = lab_staffing_role_model_route(summary.staffing.as_ref());
    Run {
        id: summary.dir.clone(),
        kind: RunKind::Lab,
        status: lab_run_status(summary, now_ms),
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
        // (#1584) `mtime_ms` is the newest-artifact time, which is exactly
        // "last active" — and it's the ONLY time an unfinished lab run has.
        // Using it as `completed_ts` for such a run would claim a completion
        // that never happened; as `updated_ts` it's simply true.
        updated_ts: Some(summary.mtime_ms / 1000),
        tracked: true,
        // (#1915) A lab run has no flow session backing it — nothing to
        // drill into via `#session=<id>` — and it already opens its own
        // in-page detail pane regardless (`RunsBoard.tsx::activateRun`'s
        // `"lab"` branch), so there's no destination this field could add.
        session_id: None,
    }
}

/// Map a lab run's own `finished`/`degenerate` fields to the flat
/// [`RunStatus`]. A `degenerate` run (every probe drew nothing usable — see
/// `darkmux_lab::lab::review`'s own doc) reached its terminal artifact
/// write but produced no usable finding; the closest flat-status fit is
/// `Error` (there's no separate "degraded" value in this view-model — the
/// step-4 lens can special-case `degenerate` directly off the richer
/// `/lab/runs` payload if finer granularity turns out to matter).
fn lab_run_status(summary: &LabRunSummary, now_ms: u64) -> RunStatus {
    if summary.finished {
        return if summary.degenerate { RunStatus::Error } else { RunStatus::Complete };
    }
    // (#1621) Unfinished is NOT the same as running, and treating it as such
    // is what made the `running` filter useless: 49 of 52 rows it returned
    // were long-dead bench runs, and the three live ones were lost in them.
    // "Running" is a claim about the PRESENT and needs positive evidence.
    //
    // The threshold is derived, not invented. The runtime's own inactivity
    // watchdog HARD-KILLS a dispatch that goes `inactivity_timeout_seconds`
    // without proof-of-work, so a run whose newest artifact predates that
    // budget cannot have live work under it — there is nothing left running to
    // have written it. Doubled for headroom, because `mtime_ms` tracks marker
    // ARTIFACTS rather than every heartbeat, and a live run legitimately goes
    // quiet between them.
    //
    // Measured when this landed: all 49 unfinished lab runs on the operator's
    // machine were untouched for over an hour, the freshest 2.6h. Not one was
    // plausibly live.
    let idle_ms = now_ms.saturating_sub(summary.mtime_ms);
    if idle_ms > stale_after_ms() {
        // It left a trail and the trail STOPS — that is evidence of
        // abandonment, not absence of evidence, so `Abandoned` is honest here
        // rather than a manufactured verdict.
        return RunStatus::Abandoned;
    }
    RunStatus::Running
}

/// (#1621) How long a lab run's newest artifact may age before the run stops
/// counting as live. Twice the runtime's inactivity budget — see
/// [`lab_run_status`] for why that is the right anchor.
fn stale_after_ms() -> u64 {
    darkmux_types::config_access::inactivity_timeout_seconds().saturating_mul(2_000)
}

/// (#1642, #1633) The ONE liveness decision every `/runs` source — lab,
/// mission, AND ghost — shares, keyed on [`SessionAgg::last_activity_ts`]
/// against the SAME [`stale_after_ms`] budget [`lab_run_status`] already
/// uses. Before this, only lab runs were gated: `mission_run_status` and
/// `ghost_runs` had no per-session activity signal to gate on at all
/// (`SessionAgg` tracked only `start_ts`/`terminal_ts`), so a mission or
/// ghost row whose underlying work died without ever reaching a terminal
/// read as `Running` forever — the exact #1621 defect, reopened for two of
/// the three `Run` kinds. A session with no activity timestamp at all
/// (shouldn't happen for anything actually indexed, but never assume) can't
/// be judged live — absence of evidence is not evidence of life.
fn session_is_live(agg: &SessionAgg, now_ms: u64) -> bool {
    let Some(last_activity_secs) = agg.last_activity_ts.as_deref().and_then(parse_flow_ts) else {
        return false;
    };
    let idle_ms = now_ms.saturating_sub(last_activity_secs.saturating_mul(1_000));
    idle_ms <= stale_after_ms()
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
    /// (#1918) Every DISTINCT `mission_id` seen on a record folded into
    /// this session, not just the first (`mission_id` above keeps only
    /// that). A `HashSet` rather than a running counter: the scheduler
    /// defect #1918 diagnosed stamps the SAME `session_id` on every step
    /// of the SAME mission too (many records, one mission), so a naive
    /// increment-per-record counter would flag an ordinary session as
    /// ambiguous just for having multiple steps. A set gives "distinct"
    /// for free — dedup is the data structure, not a comparison a caller
    /// has to remember to write — and the ambiguity question is then just
    /// `.len() > 1` (see [`SessionAgg::is_ambiguous`]).
    ///
    /// Root cause: the scheduler stamps `session_id` from the TASK id
    /// (`task-<task_id>`), which carries no per-RUN identity, so every
    /// mission that happens to run a task with the same id lands in the
    /// same bucket (98 records / 49 distinct `mission_id` values / 1
    /// `session_id`, measured live). That is a flow-emitter defect, fixed
    /// separately (#1918's own "Fix" section) because the id is also the
    /// flow index's key and appears in `#session=<id>` deep links — a
    /// data-shape change worth versioning deliberately, not slipped in
    /// here. This field exists to DETECT the corruption from the read
    /// side and refuse to act on it, not to repair the write side.
    mission_ids_seen: HashSet<String>,
    role: Option<String>,
    model: Option<String>,
    machine: Option<String>,
    /// The record's `source` field (e.g. `"crew_dispatch"`, `"review"`, or
    /// the #1877 whole-run bookend's `"mission"`) — tracked so
    /// [`mission_to_run`]'s role/model fallback can tell a real per-step
    /// dispatch session apart from the mission-level bookend, whose
    /// `handle` is the launched config id (a real string, never blank),
    /// not an actual per-step role. Simple presence/absence (`role.is_
    /// none()`) can't make that distinction — only the source can.
    source: Option<String>,
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
    /// (#1642, #1633) The newest `ts` seen on ANY record for this session —
    /// not just lifecycle records. Heartbeats and telemetry are exactly the
    /// proof-of-work [`session_is_live`] needs; restricting this to
    /// lifecycle records would blind the liveness gate to a session that's
    /// still actively ticking between its start and its (not-yet-written)
    /// terminal. Same raw-ISO-string convention as `start_ts`/`terminal_ts`
    /// — parsed via [`parse_flow_ts`] only where a numeric is needed.
    last_activity_ts: Option<String>,
}

impl SessionAgg {
    /// (#1918) `true` when this session's records name more than one
    /// distinct `mission_id` — the read-side detector for the scheduler
    /// defect (see [`SessionAgg::mission_ids_seen`]'s own doc). Every
    /// caller that would otherwise hand this session out as a drill target
    /// (`mission_to_run`, `flow_mission_to_run`, `ghost_runs`) checks this
    /// FIRST: a session covering more than one mission is not a valid
    /// drill target for ANY of them, because there is no way to tell,
    /// from the session alone, which mission a click should actually
    /// open. No destination is the honest answer — an inert row is a
    /// smaller failure than a row that opens someone else's work.
    fn is_ambiguous(&self) -> bool {
        self.mission_ids_seen.len() > 1
    }
}

/// One pass over every flow record within [`RUNS_FLOW_SCAN_WINDOW_DAYS`] —
/// from the local day-files AND the fleet stream (#1705), both bounded by
/// that same window so the two sources cannot disagree about how far back
/// `/runs` reaches.
fn build_flow_session_index(
    flows_dir: &StdPath,
    fleet: &[serde_json::Value],
) -> HashMap<String, SessionAgg> {
    let mut idx: HashMap<String, SessionAgg> = HashMap::new();

    // (#1705) Fleet first, then the local day-files minus anything the
    // fleet already supplied — this machine's records land in BOTH sinks,
    // so the shared identity key is what keeps one dispatch from being
    // folded twice. Same precedence as `union_flow_records`.
    // (#1707 gate MUST FIX 2) The fleet half obeys the SAME
    // `RUNS_FLOW_SCAN_WINDOW_DAYS` bound the local walk does. Without this
    // the two sources disagree about how far back `/runs` reaches, and the
    // stream is the WORSE offender: `XADD MAXLEN ~` trims lazily, only on
    // write, so a fleet that has gone quiet still holds month-old records —
    // which would resurface dead missions and un-terminated sessions as
    // Abandoned rows that never age out. This bites the single-machine
    // redis-enabled operator too, not just a fleet.
    let fleet_cutoff = cutoff_date_string(RUNS_FLOW_SCAN_WINDOW_DAYS);
    let within_window = |v: &serde_json::Value| -> bool {
        match v.get("ts").and_then(|t| t.as_str()) {
            // Lexical compare on the `YYYY-MM-DD` prefix — the same trick
            // `for_each_recent_flow_record` uses on day-file names.
            Some(ts) if ts.len() >= 10 => ts[..10] >= fleet_cutoff[..],
            // No parseable ts: keep it. Dropping an unattributable record
            // would silently narrow the view, which is this issue's own bug.
            _ => true,
        }
    };

    let fleet_seen: std::collections::HashSet<String> =
        fleet.iter().filter(|v| within_window(v)).map(crate::flow_record_identity).collect();
    let fold = |idx: &mut HashMap<String, SessionAgg>, v: &serde_json::Value| {
        let Some(session_id) = v.get("session_id").and_then(|s| s.as_str()) else {
            return;
        };
        if session_id.is_empty() {
            return;
        }
        let agg = idx.entry(session_id.to_string()).or_default();

        if agg.mission_id.is_none() {
            if let Some(mid) = v.get("mission_id").and_then(|m| m.as_str()) {
                if !mid.is_empty() {
                    agg.mission_id = Some(mid.to_string());
                }
            }
        }
        // (#1918) Unconditional, unlike `mission_id` above — this tracks
        // EVERY distinct value this session has ever named, not just the
        // first, because the ambiguity question ("does this session belong
        // to more than one mission") can only be answered by seeing them
        // all. A `HashSet` insert of the same value from the session's own
        // other steps is a no-op, so an ordinary multi-step mission never
        // trips this — only a session that genuinely spans more than one
        // mission grows past one entry.
        if let Some(mid) = v.get("mission_id").and_then(|m| m.as_str()) {
            if !mid.is_empty() {
                agg.mission_ids_seen.insert(mid.to_string());
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
        if agg.source.is_none() {
            if let Some(src) = v.get("source").and_then(|s| s.as_str()) {
                if !src.is_empty() {
                    agg.source = Some(src.to_string());
                }
            }
        }

        let action = v.get("action").and_then(|a| a.as_str()).unwrap_or("");
        let ts = v.get("ts").and_then(|t| t.as_str()).unwrap_or("");

        // (#1642, #1633) EVERY record for this session updates the liveness
        // clock — not just lifecycle ones (see `SessionAgg::last_activity_ts`'s
        // doc). ISO-8601 `YYYY-MM-DDTHH:MM:SSZ` sorts correctly as a plain
        // string (same property `earliest_by_start` relies on), so a lexical
        // compare is enough to keep the NEWEST seen even if records are ever
        // visited out of chronological order.
        if !ts.is_empty() {
            let is_newer = match agg.last_activity_ts.as_deref() {
                Some(current) => ts > current,
                None => true,
            };
            if is_newer {
                agg.last_activity_ts = Some(ts.to_string());
            }
        }

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
    };

    for v in fleet.iter().filter(|v| within_window(v)) {
        fold(&mut idx, v);
    }
    for_each_recent_flow_record(flows_dir, |v| {
        if !fleet_seen.is_empty() && fleet_seen.contains(&crate::flow_record_identity(v)) {
            return std::ops::ControlFlow::Continue(());
        }
        fold(&mut idx, v);
        std::ops::ControlFlow::Continue(())
    });
    idx
}

/// The flow stream carries both the dotted (`dispatch.start`) and spaced
/// (`dispatch start`) action forms across schema history — tolerate both,
/// matching `scan_flow_days`/`scan_flow_missions`'s own dual-form checks.
/// (#1852) Delegates to the shared matcher rather than re-spelling the
/// vocabulary — this was one of five independent local defenses.
fn is_dispatch_start_action(action: &str) -> bool {
    darkmux_flow::is_dispatch_start(action)
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
///
/// (#1915) Operates on `(id, agg)` PAIRS, not bare aggs. Before this, a
/// caller that also needed to know WHICH session won — not just its
/// fields — had no way to recover that from the returned `&SessionAgg`
/// alone (`SessionAgg` doesn't carry its own id; it's the `flow_index`
/// map's key). The only fix that can't drift is deriving the id from the
/// SAME comparison that picks the agg, rather than a second, separately
/// written search for "which id maps to this agg" after the fact.
fn earliest_by_start<'a>(sessions: &[(&'a str, &'a SessionAgg)]) -> Option<(&'a str, &'a SessionAgg)> {
    sessions
        .iter()
        .copied()
        .filter(|(_, s)| s.start_ts.is_some())
        .min_by(|(_, a), (_, b)| a.start_ts.cmp(&b.start_ts))
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
///
/// **(#1642, #1633) The staleness gate.** No terminal seen yet used to mean
/// "still running", unconditionally — the SAME #1621 defect `lab_run_status`
/// was fixed for, still open here: a ghost whose session died mid-dispatch
/// with no terminal ever written read as `Running` forever. A missing
/// terminal now means "running" only while [`session_is_live`] says so;
/// otherwise `Abandoned`. A terminal status, when present, always wins —
/// staleness never relabels a run that already reached a real verdict.
fn ghost_runs(
    flow_index: &HashMap<String, SessionAgg>,
    known_mission_ids: &HashSet<String>,
    known_session_ids: &HashSet<String>,
    remote_mission_ids: &HashSet<String>,
    now_ms: u64,
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
            // (#1705) Already represented by its own remote-mission row.
            if remote_mission_ids.contains(mid) {
                continue;
            }
        }
        out.push(Run {
            id: session_id.clone(),
            kind: RunKind::Dispatch,
            // A terminal status always wins over the liveness gate; only a
            // session with NO terminal yet falls through to it.
            status: agg.terminal_status.unwrap_or_else(|| {
                if session_is_live(agg, now_ms) {
                    RunStatus::Running
                } else {
                    RunStatus::Abandoned
                }
            }),
            machine: agg.machine.clone(),
            route: agg.endpoint.clone(),
            role: agg.role.clone(),
            model: agg.model.clone(),
            started_ts: agg.start_ts.as_deref().and_then(parse_flow_ts),
            completed_ts: agg.terminal_ts.as_deref().and_then(parse_flow_ts),
            // (#1584) Same completion-else-start rule as a tracked mission.
            updated_ts: agg
                .terminal_ts
                .as_deref()
                .and_then(parse_flow_ts)
                .or_else(|| agg.start_ts.as_deref().and_then(parse_flow_ts)),
            tracked: false,
            // (#1915) A ghost row's own `id` (above) already IS a session
            // id — it's synthesized directly from `flow_index`'s key, one
            // row per untracked session. Carried here too, redundantly with
            // `id`, so the CLIENT never needs a kind-specific "for a
            // dispatch, drill into `id` itself" special case: "untracked
            // and has a `session_id`" is the one rule every kind obeys.
            //
            // (#1918) Same ambiguity guard as the mission paths, applied
            // uniformly rather than special-cased away here. A ghost row
            // is synthesized one-per-session, so by construction it should
            // never be ambiguous — `has_start` only gates on THIS session
            // having opened a dispatch, not on how many missions' records
            // landed in it. If this ever actually fires on a ghost, that
            // is itself a finding (a session-id COLLISION between an
            // orphaned dispatch and a real mission), not a nuisance to
            // silence.
            session_id: if agg.is_ambiguous() { None } else { Some(session_id.clone()) },
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
            reads: Vec::new(),
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
            gate: None,
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
            Some(MissionSpec { config_id: "dispatch".to_string(), inputs_fingerprint: "x".to_string(), origin: None }),
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
            Some(MissionSpec { config_id: "coder-phase".to_string(), inputs_fingerprint: "x".to_string(), origin: None }),
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
        // `minimal_mission` stamps `started_ts` with the real "now" — judge
        // it against that same instant (idle ~0) so this stays a pure
        // "Active/Paused reads Running" test, independent of the staleness
        // gate exercised separately below.
        let now_ms = now_unix() * 1_000;
        let mut m = minimal_mission("m5", vec![], None);
        assert_eq!(mission_run_status(&m, &[], now_ms), RunStatus::Running);
        m.status = MissionStatus::Paused;
        assert_eq!(mission_run_status(&m, &[], now_ms), RunStatus::Running);
    }

    #[test]
    fn mission_run_status_active_with_no_started_ts_is_planned() {
        // (#1523 gate CONSIDER 4) Minted but never actually started — the
        // dead `Planned` variant made reachable.
        let mut m = minimal_mission("m5b", vec![], None);
        m.started_ts = None;
        assert_eq!(mission_run_status(&m, &[], now_unix() * 1_000), RunStatus::Planned);
    }

    #[test]
    fn mission_run_status_active_with_every_session_terminal_reports_the_terminal_not_running() {
        // (#1523 gate CONSIDER 3) A crashed mission: every dispatch it's
        // known to have made already reached a terminal, yet the mission
        // record itself never got finalized (the process died first).
        let m = minimal_mission("m5c", vec![], None);
        let now_ms = now_unix() * 1_000;
        let abandoned = SessionAgg { terminal_status: Some(RunStatus::Abandoned), ..Default::default() };
        assert_eq!(mission_run_status(&m, &[&abandoned], now_ms), RunStatus::Abandoned);

        let errored = SessionAgg { terminal_status: Some(RunStatus::Error), ..Default::default() };
        assert_eq!(mission_run_status(&m, &[&errored], now_ms), RunStatus::Error);
    }

    #[test]
    fn mission_run_status_active_with_a_still_running_session_stays_running() {
        // A partially-complete multi-session mission (one phase done, one
        // still dispatching) must NOT be flagged as crashed just because
        // ONE of its sessions has a terminal — and the still-dispatching one
        // must show GENUINE recent activity now that liveness is gated
        // (#1642), not just the absence of a terminal.
        let m = minimal_mission("m5d", vec![], None);
        let done = SessionAgg { terminal_status: Some(RunStatus::Complete), ..Default::default() };
        let still_running = SessionAgg {
            terminal_status: None,
            has_start: true,
            last_activity_ts: Some("2000-01-01T00:00:00Z".to_string()),
            ..Default::default()
        };
        // Judged at exactly that instant — idle 0, unambiguously live.
        assert_eq!(
            mission_run_status(&m, &[&done, &still_running], 946_684_800_000),
            RunStatus::Running
        );
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
        // (#1642) Terminal-Complete-not-Abandoned/Error must win over the
        // staleness gate outright — judged FAR in the future (no possible
        // reading of "recent activity") and still Running, because the
        // all-terminal branch returns before the gate is ever consulted.
        let far_future_ms = (now_unix() + 999_999_999) * 1_000;
        assert_eq!(mission_run_status(&m, &[&done], far_future_ms), RunStatus::Running);
    }

    #[test]
    #[serial_test::serial]
    fn mission_run_status_finalized_reads_the_envelope() {
        let _g = CrewGuard::new();
        darkmux_crew::lifecycle::save_mission(&minimal_mission("m6", vec![], None)).unwrap();

        let mut m = minimal_mission("m6", vec![], None);
        m.status = MissionStatus::Finalized;
        let now_ms = now_unix() * 1_000;

        // No envelope written yet -> degrades to Complete.
        assert_eq!(mission_run_status(&m, &[], now_ms), RunStatus::Complete);

        let clean_env = MissionEnvelope::new("m6", MissionOutcomeStatus::Clean, &[]);
        darkmux_crew::envelope::finalize_mission(&clean_env);
        assert_eq!(mission_run_status(&m, &[], now_ms), RunStatus::Complete);
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
        assert_eq!(mission_run_status(&m, &[], now_unix() * 1_000), RunStatus::Error);
    }

    /// (#1877 item 4 — stated decision, pinned) A `RunOutcome::Partial`
    /// envelope collapses into `RunStatus::Complete` here, same as a plain
    /// `Degraded` one — `RunStatus` has no partial-coverage state and this
    /// site deliberately does not read `envelope.outcome` to invent one. If
    /// this test ever needs to change, that is the moment `RunStatus` grows
    /// a real partial state, not an accidental regression.
    #[test]
    #[serial_test::serial]
    fn mission_run_status_finalized_partial_outcome_envelope_reads_complete_not_a_new_state() {
        let _g = CrewGuard::new();
        darkmux_crew::lifecycle::save_mission(&minimal_mission("m8", vec![], None)).unwrap();
        let mut m = minimal_mission("m8", vec![], None);
        m.status = MissionStatus::Finalized;

        let partial_env = MissionEnvelope::from_outcome(
            "m8",
            darkmux_crew::run_outcome::RunOutcome::Partial {
                reasons: vec!["11 of 134 flags went unjudged".to_string()],
            },
            &[],
        );
        assert_eq!(partial_env.status, MissionOutcomeStatus::Degraded);
        darkmux_crew::envelope::finalize_mission(&partial_env);
        assert_eq!(mission_run_status(&m, &[], now_unix() * 1_000), RunStatus::Complete);
    }

    /// (#1892) `MissionStatus` has exactly four variants; no wildcard, so a
    /// fifth variant fails to compile HERE until a human decides whether it
    /// is terminal-abandon-shaped or terminal-success-shaped. This is the
    /// exact shape of bug #1627 fixed once already: `Aborted` used to fall
    /// through a `_ => Complete` arm and silently inherit `Finalized`'s
    /// happy mapping. A wildcard in this classifier would let a *new*
    /// variant repeat that mistake invisibly.
    fn is_abandon_shaped_terminal(status: MissionStatus) -> bool {
        match status {
            MissionStatus::Aborted => true,
            MissionStatus::Finalized => false,
            MissionStatus::Active | MissionStatus::Paused => {
                panic!("not a terminal status; see mission_run_status_active_and_paused_are_running")
            }
        }
    }

    /// (#1892) The gap this closes: nothing in this file ever constructed a
    /// LOCAL `Mission` with `status: MissionStatus::Aborted` and drove it
    /// through `mission_run_status` — the only test that touched `Aborted`
    /// at all (`an_aborted_peer_mission_reads_abandoned_not_complete`) goes
    /// through the separate fleet/PEER `flow_mission_to_run` path, despite
    /// its own docstring claiming it exercises "the tracked path". This
    /// test drives BOTH terminal `MissionStatus` variants — `Aborted` and
    /// `Finalized` — through a real local `Mission`, so the whole match in
    /// `mission_run_status` is pinned rather than only the value someone
    /// happened to audit.
    ///
    /// Mutating `MissionStatus::Aborted => RunStatus::Abandoned` to
    /// `=> RunStatus::Complete` in `mission_run_status` must fail this test.
    #[test]
    #[serial_test::serial]
    fn mission_run_status_pins_every_terminal_mission_status_variant() {
        let _g = CrewGuard::new();
        let now_ms = now_unix() * 1_000;

        assert!(is_abandon_shaped_terminal(MissionStatus::Aborted));
        assert!(!is_abandon_shaped_terminal(MissionStatus::Finalized));

        // Aborted: a torn-down LOCAL mission, never a Finalized one wearing
        // its clothes. No mission needs to exist on disk — `mission_run_status`
        // never consults `load_envelope` for this arm.
        let mut aborted = minimal_mission("m1892-aborted", vec![], None);
        aborted.status = MissionStatus::Aborted;
        assert_eq!(
            mission_run_status(&aborted, &[], now_ms),
            RunStatus::Abandoned,
            "an aborted LOCAL mission must read Abandoned, never Complete (#1627, re-pinned by #1892)"
        );

        // Finalized, no envelope on disk yet: genuinely no data, degrades
        // to Complete — the OTHER terminal variant, so the classifier
        // above and the match it mirrors both stay exercised end to end.
        darkmux_crew::lifecycle::save_mission(&minimal_mission("m1892-finalized", vec![], None)).unwrap();
        let mut finalized = minimal_mission("m1892-finalized", vec![], None);
        finalized.status = MissionStatus::Finalized;
        assert_eq!(mission_run_status(&finalized, &[], now_ms), RunStatus::Complete);
    }

    /// (#1881 RED proof) `envelope.json` exists but is not valid JSON at
    /// all — no leniency, of any kind, can rescue this. Written directly to
    /// disk (bypassing `save_envelope`/`finalize_mission`, which can only
    /// ever write something `MissionEnvelope` itself can parse) to simulate
    /// exactly the scenario the issue names: a NEWER darkmux wrote a record
    /// this OLDER binary's `serde_json::from_str::<MissionEnvelope>` chokes
    /// on. Before the fix, `mission_run_status`'s `.ok().flatten()` +
    /// `_ => RunStatus::Complete` fallback silently renders this as a
    /// completed, green run — this test's ORIGINAL run (pre-fix) observed
    /// exactly that: `assert_eq!(.., RunStatus::Complete)` passed, which is
    /// the bug, not a spec. Fixed: a parse failure gets its own status.
    #[test]
    #[serial_test::serial]
    fn mission_run_status_finalized_envelope_that_fails_to_parse_is_not_complete() {
        let _g = CrewGuard::new();
        darkmux_crew::lifecycle::save_mission(&minimal_mission("m9", vec![], None)).unwrap();
        let mut m = minimal_mission("m9", vec![], None);
        m.status = MissionStatus::Finalized;

        let path = darkmux_crew::lifecycle::envelope_path("m9");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not valid json at all").unwrap();

        let status = mission_run_status(&m, &[], now_unix() * 1_000);
        assert_ne!(status, RunStatus::Complete, "a genuinely unparseable envelope must never read as a completed, green run");
        assert_eq!(status, RunStatus::Unparseable);
    }

    /// (#1881) The issue's own probe, reproduced: a `status` value this
    /// binary's `MissionOutcomeStatus` doesn't recognize (`"throttled"`).
    /// Before ANY fix this fails the WHOLE envelope parse (verified in
    /// `crates/darkmux-crew/src/envelope.rs`'s own
    /// `an_unrecognized_status_degrades_to_unknown_and_the_rest_of_the_document_still_parses`
    /// test), and the old `.ok().flatten()` fallback rendered that as
    /// `Complete`. After the fix, `MissionOutcomeStatus` gains a
    /// `#[serde(other)]` catch-all so the envelope parses, but an unknown
    /// `status` is precisely the case this binary cannot honestly report a
    /// verdict for — it must still never render as `Complete`.
    ///
    /// (#1881, QA-caught) This test used to be mutation-transparent w.r.t.
    /// the `#[serde(other)]` leniency: with `MissionOutcomeStatus`'s
    /// catch-all removed, this SAME fixture fails to deserialize entirely
    /// (`Err`), which the `Err(_) => Unparseable` arm ALSO resolves to
    /// `RunStatus::Unparseable` — so the final assertion couldn't tell the
    /// leniency path from the hard-parse-failure path its sibling test
    /// (`mission_run_status_finalized_envelope_that_fails_to_parse_is_not_complete`)
    /// already covers. The `load_envelope` call below (matching the
    /// doctor-side test's own guard) pins that this fixture really does
    /// parse successfully with `status: Unknown`, not `Err`.
    #[test]
    #[serial_test::serial]
    fn mission_run_status_finalized_unknown_status_variant_is_not_complete() {
        let _g = CrewGuard::new();
        darkmux_crew::lifecycle::save_mission(&minimal_mission("m10", vec![], None)).unwrap();
        let mut m = minimal_mission("m10", vec![], None);
        m.status = MissionStatus::Finalized;

        let path = darkmux_crew::lifecycle::envelope_path("m10");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"mission_id":"m10","schema_version":"1.1","status":"throttled","phases":[]}"#,
        )
        .unwrap();

        let loaded = darkmux_crew::lifecycle::load_envelope("m10");
        match &loaded {
            Ok(Some(envelope)) => {
                assert_eq!(envelope.status, MissionOutcomeStatus::Unknown, "fixture must parse leniently to Unknown status, not some other value")
            }
            other => panic!("fixture must exercise the #[serde(other)] leniency path (Ok(Some(_)) with status Unknown), got {other:?}"),
        }

        let status = mission_run_status(&m, &[], now_unix() * 1_000);
        assert_ne!(status, RunStatus::Complete, "an unrecognized status value must never read as a completed, green run");
        assert_eq!(status, RunStatus::Unparseable);
    }

    /// (#1881) A `RunOutcome` variant this binary doesn't recognize
    /// (`outcome.state: "throttled"`), paired with a KNOWN, valid `status`.
    /// Once `RunOutcome` gains its own `#[serde(other)]` catch-all, this
    /// envelope parses cleanly — `outcome` is a supplementary, typed detail
    /// `mission_run_status` has never read (#1877 item 4, unchanged by this
    /// fix); the `status` field alone is authoritative. So this is the one
    /// case in the whole issue where the honest answer is NOT
    /// `Unparseable`: the binary genuinely does understand this run's
    /// outcome (`status: "degraded"`, a known value that already collapses
    /// into `Complete`, same as before #1877) even though it can't name
    /// the docket-coverage DETAIL. Rendering this as `Complete` is correct,
    /// not a regression of the bug this issue is about — see
    /// `crates/darkmux-crew/src/envelope.rs`'s leniency test for the proof
    /// that the REST of the document (status, mission_id, phases) survives
    /// intact when `outcome` alone is unrecognized.
    #[test]
    #[serial_test::serial]
    fn mission_run_status_finalized_unknown_outcome_variant_with_known_status_still_reads_that_status() {
        let _g = CrewGuard::new();
        darkmux_crew::lifecycle::save_mission(&minimal_mission("m11", vec![], None)).unwrap();
        let mut m = minimal_mission("m11", vec![], None);
        m.status = MissionStatus::Finalized;

        let path = darkmux_crew::lifecycle::envelope_path("m11");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"mission_id":"m11","schema_version":"1.2","status":"degraded","outcome":{"state":"throttled"},"phases":[]}"#,
        )
        .unwrap();

        let status = mission_run_status(&m, &[], now_unix() * 1_000);
        assert_eq!(
            status,
            RunStatus::Complete,
            "outcome is supplementary and never read for RunStatus — a known status must still be trusted even when outcome's own detail is unrecognized"
        );
    }

    // ── mission_run_status: the staleness gate (#1642, #1633) ───────────

    #[test]
    fn mission_run_status_active_no_sessions_fresh_started_ts_is_running() {
        // The edge case named in #1642: a mission with `started_ts` set but
        // NO sessions dispatched yet at all. A just-launched mission
        // legitimately looks like this — falling straight to `Abandoned`
        // here would be a fresh lie in the opposite direction, so
        // `started_ts` itself is the activity anchor when there are no
        // sessions to consult.
        let m = minimal_mission("m5f", vec![], None); // started_ts = now_unix()
        let now_ms = now_unix() * 1_000;
        assert_eq!(mission_run_status(&m, &[], now_ms), RunStatus::Running);
    }

    #[test]
    fn mission_run_status_active_no_sessions_stale_started_ts_is_abandoned() {
        // Same shape, but `started_ts` itself has aged past the budget with
        // still no session ever dispatched — genuinely dead, not a
        // just-launched mission.
        let mut m = minimal_mission("m5g", vec![], None);
        m.started_ts = Some(946_684_800); // 2000-01-01T00:00:00Z
        let stale_now_ms = 946_684_800_000 + stale_after_ms() + 1_000;
        assert_eq!(mission_run_status(&m, &[], stale_now_ms), RunStatus::Abandoned);
    }

    #[test]
    fn mission_run_status_active_all_sessions_stale_is_abandoned() {
        // Every known session is still nominally "open" (no terminal ever
        // landed — the process died mid-dispatch before writing one), and
        // none of them show recent activity. This is the #1642 defect
        // itself: previously this fell straight through to `Running`
        // forever because "not all sessions terminal" was the only check.
        let mut m = minimal_mission("m5h", vec![], None);
        m.started_ts = Some(946_684_800);
        let stale_a = SessionAgg {
            has_start: true,
            terminal_status: None,
            last_activity_ts: Some("2000-01-01T00:00:00Z".to_string()),
            ..Default::default()
        };
        let stale_b = SessionAgg {
            has_start: true,
            terminal_status: None,
            last_activity_ts: Some("2000-01-01T00:00:01Z".to_string()),
            ..Default::default()
        };
        // Comfortably past the budget even accounting for `stale_b`'s
        // 1-second-newer activity — both must read as dead.
        let now_ms = 946_684_800_000 + stale_after_ms() + 5_000;
        assert_eq!(mission_run_status(&m, &[&stale_a, &stale_b], now_ms), RunStatus::Abandoned);
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
        let now = FIXTURE_NOW_MS;
        assert_eq!(lab_run_status(&minimal_lab_summary("d1", false, false), now), RunStatus::Running);
        assert_eq!(lab_run_status(&minimal_lab_summary("d2", true, false), now), RunStatus::Complete);
        assert_eq!(lab_run_status(&minimal_lab_summary("d3", true, true), now), RunStatus::Error);
    }

    #[test]
    fn lab_summary_to_run_uses_dir_as_id_and_kind_lab() {
        let summary = minimal_lab_summary("live/case-1", true, false);
        let run = lab_summary_to_run(&summary, Some("studio".to_string()), FIXTURE_NOW_MS);
        assert_eq!(run.id, "live/case-1");
        assert_eq!(run.kind, RunKind::Lab);
        assert_eq!(run.status, RunStatus::Complete);
        assert!(run.tracked);
        assert_eq!(run.completed_ts, Some(1_700_000_000));
        assert_eq!(run.machine.as_deref(), Some("studio"));
    }

    /// (#1584) The case the `updated_ts` field exists for. An UNFINISHED lab
    /// run has no start timestamp (`LabRunSummary` records none) and no
    /// completion timestamp (it never reached `scores.json`) — so before this
    /// field it carried NO time at all and was unorderable by any consumer.
    /// On a real machine that is not a corner case: dozens of run dirs are
    /// killed mid-flight and stay in exactly this shape forever.
    ///
    /// `updated_ts` must be populated for BOTH states, and `completed_ts`
    /// must stay absent while unfinished — claiming a completion that never
    /// happened would be a worse lie than having no ordering.
    #[test]
    fn lab_summary_to_run_always_carries_an_activity_ts() {
        let unfinished =
            lab_summary_to_run(&minimal_lab_summary("live/wip", false, false), None, FIXTURE_NOW_MS);
        assert_eq!(unfinished.status, RunStatus::Running);
        assert_eq!(unfinished.started_ts, None);
        assert_eq!(unfinished.completed_ts, None, "an unfinished run never completed");
        assert_eq!(
            unfinished.updated_ts,
            Some(1_700_000_000),
            "an unfinished lab run must still be orderable by its newest-artifact time"
        );

        let finished =
            lab_summary_to_run(&minimal_lab_summary("live/done", true, false), None, FIXTURE_NOW_MS);
        assert_eq!(finished.updated_ts, Some(1_700_000_000));
        assert_eq!(finished.completed_ts, Some(1_700_000_000));
    }

    /// The fixture's newest-artifact time is 1_700_000_000_000 ms, so "now" for
    /// a run that is still live is that instant — an idle age of zero.
    const FIXTURE_NOW_MS: u64 = 1_700_000_000_000;

    /// (#1621) The defect: `!finished` was returned as `Running`, so a lab run
    /// that died months ago read as live forever. On the operator's machine
    /// that was 49 of the 52 rows the `running` filter returned — the three
    /// genuinely-live runs were lost in a pile of corpses, which defeats the
    /// one question the filter exists to answer.
    ///
    /// "Running" is a claim about the PRESENT and needs positive evidence.
    #[test]
    fn an_unfinished_lab_run_stops_reading_as_live_once_it_goes_quiet() {
        let summary = minimal_lab_summary("live/killed", false, false);

        // Just now: still live. The floor must not break a real in-flight run.
        assert_eq!(
            lab_run_status(&summary, FIXTURE_NOW_MS),
            RunStatus::Running,
            "a run whose artifact was just written IS live"
        );

        // One second inside the window: still live. A live run legitimately
        // goes quiet between marker artifacts.
        let inside = FIXTURE_NOW_MS + stale_after_ms() - 1_000;
        assert_eq!(lab_run_status(&summary, inside), RunStatus::Running);

        // Past the window: it left a trail and the trail STOPS. The runtime's
        // inactivity watchdog would have killed anything live by now, so there
        // is nothing left that could have written it.
        let outside = FIXTURE_NOW_MS + stale_after_ms() + 1_000;
        assert_eq!(
            lab_run_status(&summary, outside),
            RunStatus::Abandoned,
            "a run untouched for longer than the watchdog budget cannot be live"
        );

        // The operator's actual data: the FRESHEST of 49 stuck runs was 2.6h
        // old. Every one of them must fall out of `running`.
        let two_point_six_hours = FIXTURE_NOW_MS + (2.6 * 3_600_000.0) as u64;
        assert_eq!(lab_run_status(&summary, two_point_six_hours), RunStatus::Abandoned);
    }

    /// Staleness must never override a run's OWN terminal verdict — a finished
    /// run stays finished however long ago it ran, or every completed run in
    /// history would decay into `Abandoned`.
    #[test]
    fn a_finished_lab_run_keeps_its_verdict_no_matter_how_old() {
        let ancient = FIXTURE_NOW_MS + 400 * 24 * 3_600_000;
        assert_eq!(
            lab_run_status(&minimal_lab_summary("old/done", true, false), ancient),
            RunStatus::Complete
        );
        assert_eq!(
            lab_run_status(&minimal_lab_summary("old/degen", true, true), ancient),
            RunStatus::Error
        );
    }

    /// The threshold is DERIVED from the runtime's own inactivity budget, not
    /// invented — so it moves with the operator's config instead of drifting
    /// away from it.
    #[test]
    fn the_staleness_window_tracks_the_runtime_inactivity_budget() {
        let budget = darkmux_types::config_access::inactivity_timeout_seconds();
        assert_eq!(stale_after_ms(), budget * 2_000, "twice the watchdog budget, in ms");
        assert!(stale_after_ms() >= 600 * 2_000, "and never below the shipped default");
    }

    // ── earliest_by_start: pairs, not bare aggs (#1915) ──────────────────

    /// The core claim the #1915 fix rests on: the returned id genuinely
    /// belongs to the SAME session whose agg won the comparison, even when
    /// the winning session isn't the pair listed first. A version that
    /// derived the id from a SEPARATE search after picking the agg could
    /// pass a test built any other way and still drift.
    #[test]
    fn earliest_by_start_returns_the_id_paired_with_the_winning_agg() {
        let later = SessionAgg { start_ts: Some("2026-01-01T09:00:00Z".to_string()), ..Default::default() };
        let earlier = SessionAgg { start_ts: Some("2026-01-01T08:00:00Z".to_string()), ..Default::default() };
        // Listed with the LATER session first, on purpose — a fallback to
        // "just take the first pair" would pass this test for the wrong
        // reason if it happened to also pick the earliest by luck.
        let pairs = [("later-sess", &later), ("earlier-sess", &earlier)];
        let (id, agg) = earliest_by_start(&pairs).expect("a session with a start_ts must win");
        assert_eq!(id, "earlier-sess");
        assert_eq!(agg.start_ts.as_deref(), Some("2026-01-01T08:00:00Z"));
    }

    /// A session with no `start_ts` must never look "earliest" — the SAME
    /// exclusion the bare-agg version already had, still holding after the
    /// pairing change.
    #[test]
    fn earliest_by_start_excludes_a_session_with_no_start_ts_even_when_listed_first() {
        let no_start = SessionAgg::default();
        let has_start = SessionAgg { start_ts: Some("2026-01-01T08:00:00Z".to_string()), ..Default::default() };
        let pairs = [("no-start-sess", &no_start), ("has-start-sess", &has_start)];
        let (id, _) = earliest_by_start(&pairs).unwrap();
        assert_eq!(id, "has-start-sess");
    }

    /// When NONE of the candidates have a `start_ts`, the fallback is an
    /// arbitrary element — still paired correctly with its own id, not the
    /// first pair's agg mismatched to a different id.
    #[test]
    fn earliest_by_start_falls_back_to_the_first_pair_when_nothing_has_a_start_ts() {
        let a = SessionAgg { role: Some("coder".to_string()), ..Default::default() };
        let b = SessionAgg { role: Some("reviewer".to_string()), ..Default::default() };
        let pairs = [("sess-a", &a), ("sess-b", &b)];
        let (id, agg) = earliest_by_start(&pairs).unwrap();
        assert_eq!(id, "sess-a");
        assert_eq!(agg.role.as_deref(), Some("coder"));
    }

    // ── mission_ids_seen / is_ambiguous (#1918) ──────────────────────────
    //
    // Pinning the counting itself, per the coordinator's explicit ask: two
    // records under the same session_id with DIFFERENT mission_id values
    // must mark the session ambiguous; two records under the same
    // session_id with the SAME mission_id (an ordinary multi-step mission
    // writing several records into its own session) must not.

    #[test]
    fn build_flow_session_index_two_records_same_session_different_mission_is_ambiguous() {
        let tmp = TempDir::new().unwrap();
        write_day_file(
            tmp.path(),
            &today(),
            &[
                serde_json::json!({
                    "ts": "2026-08-07T07:57:43Z",
                    "action": "step start",
                    "session_id": "task-__panel_args__",
                    "mission_id": "acp-ephemeral-pr-view-1786089463406811000-1",
                    "source": "scheduler",
                }),
                serde_json::json!({
                    "ts": "2026-08-07T07:58:00Z",
                    "action": "step start",
                    "session_id": "task-__panel_args__",
                    "mission_id": "acp-ephemeral-pr-list-1786091297730112000-2",
                    "source": "scheduler",
                }),
            ],
        );
        let idx = build_flow_session_index(tmp.path(), &[]);
        let agg = idx.get("task-__panel_args__").expect("session indexed");
        assert!(
            agg.is_ambiguous(),
            "two DIFFERENT mission_id values under one session_id must mark it ambiguous: {agg:?}"
        );
    }

    #[test]
    fn build_flow_session_index_two_records_same_session_same_mission_is_not_ambiguous() {
        // The ordinary shape: one mission's own session accumulates several
        // step records, all naming the SAME mission_id. Without this
        // inverted case, a naive "grew past one record" counter would pass
        // the test above for the wrong reason and flag every ordinary
        // multi-step mission as ambiguous.
        let tmp = TempDir::new().unwrap();
        write_day_file(
            tmp.path(),
            &today(),
            &[
                serde_json::json!({
                    "ts": "2026-08-07T07:57:43Z",
                    "action": "step start",
                    "session_id": "crew-dispatch-coder-1",
                    "mission_id": "mission-a",
                    "source": "scheduler",
                }),
                serde_json::json!({
                    "ts": "2026-08-07T07:58:00Z",
                    "action": "step complete",
                    "session_id": "crew-dispatch-coder-1",
                    "mission_id": "mission-a",
                    "source": "scheduler",
                }),
            ],
        );
        let idx = build_flow_session_index(tmp.path(), &[]);
        let agg = idx.get("crew-dispatch-coder-1").expect("session indexed");
        assert!(
            !agg.is_ambiguous(),
            "repeated records naming the SAME mission_id must not be flagged ambiguous: {agg:?}"
        );
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
        let idx = build_flow_session_index(tmp.path(), &[]);
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
        let idx = build_flow_session_index(tmp.path(), &[]);
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
        let idx = build_flow_session_index(tmp.path(), &[]);
        assert!(
            !idx.contains_key("ancient-orphan-sess"),
            "a session older than the scan window must never be indexed at all"
        );
    }

    #[test]
    fn build_flow_session_index_tracks_last_activity_from_a_non_lifecycle_record() {
        // (#1642, #1633) A heartbeat/telemetry record — NOT `dispatch
        // start`/`complete`/`error` — is exactly the proof-of-work the
        // staleness gate needs between a session's start and its (possibly
        // never-written) terminal. Restricting `last_activity_ts` to
        // lifecycle records would blind `session_is_live` to a session
        // that's genuinely still ticking.
        let tmp = TempDir::new().unwrap();
        write_day_file(
            tmp.path(),
            &today(),
            &[
                serde_json::json!({
                    "ts": "2026-07-24T10:00:00Z",
                    "action": "dispatch start",
                    "session_id": "ticking-sess",
                    "handle": "coder",
                }),
                serde_json::json!({
                    "ts": "2026-07-24T10:15:00Z",
                    "action": "tool.completed",
                    "session_id": "ticking-sess",
                }),
            ],
        );
        let idx = build_flow_session_index(tmp.path(), &[]);
        let agg = idx.get("ticking-sess").expect("session indexed");
        assert_eq!(
            agg.last_activity_ts.as_deref(),
            Some("2026-07-24T10:15:00Z"),
            "a non-lifecycle record must still advance the liveness clock"
        );
    }

    #[test]
    fn build_flow_session_index_keeps_the_newest_activity_not_the_last_seen() {
        // (#1642) The test above visits records in chronological order, so it
        // passes against a naive "keep whatever I saw last" implementation as
        // readily as against the newest-wins compare it means to assert. That
        // is the exact defect class this codebase keeps hitting: an assertion
        // that holds for a reason other than the one it names.
        //
        // Records are NOT guaranteed chronological within a day file — a
        // concurrent writer interleaves sessions, and a per-session view of
        // that stream can land out of order. Feed them out of order so the
        // compare is the only thing that can produce a pass.
        let tmp = TempDir::new().unwrap();
        write_day_file(
            tmp.path(),
            &today(),
            &[
                serde_json::json!({
                    "ts": "2026-07-24T10:00:00Z",
                    "action": "dispatch start",
                    "session_id": "outoforder-sess",
                    "handle": "coder",
                }),
                serde_json::json!({
                    "ts": "2026-07-24T10:30:00Z",
                    "action": "dispatch.turn.heartbeat",
                    "session_id": "outoforder-sess",
                }),
                // Older than the one above, and written after it.
                serde_json::json!({
                    "ts": "2026-07-24T10:05:00Z",
                    "action": "tool.completed",
                    "session_id": "outoforder-sess",
                }),
            ],
        );
        let idx = build_flow_session_index(tmp.path(), &[]);
        let agg = idx.get("outoforder-sess").expect("session indexed");
        assert_eq!(
            agg.last_activity_ts.as_deref(),
            Some("2026-07-24T10:30:00Z"),
            "an older record arriving late must not rewind the liveness clock — \
             rewinding it would age a live session into Abandoned"
        );
    }

    #[test]
    fn a_paused_mission_never_decays_into_abandoned() {
        // (#1642) `mission pause` is an operator verb, and a paused mission is
        // deliberately idle — so the staleness gate, which reads "went quiet
        // without finishing" as abandonment, must not touch it. Without this,
        // `mission launch` → `mission pause` → lunch makes the board report
        // the operator's own intent as a failure.
        //
        // Asserted at an absurd `now` so it cannot pass by sitting inside the
        // budget: if the gate applied to Paused at all, this fails.
        let mut mission = minimal_mission("paused-1", vec![], None);
        mission.status = MissionStatus::Paused;
        mission.started_ts = Some(parse_flow_ts("2000-01-01T00:00:00Z").unwrap());

        let ancient = SessionAgg {
            has_start: true,
            terminal_status: None,
            last_activity_ts: Some("2000-01-01T00:00:00Z".to_string()),
            ..Default::default()
        };
        let now_ms = u64::from(u32::MAX) * 1_000;

        assert_eq!(
            mission_run_status(&mission, &[], now_ms),
            RunStatus::Running,
            "a paused mission with no sessions must not read as abandoned"
        );
        assert_eq!(
            mission_run_status(&mission, &[&ancient], now_ms),
            RunStatus::Running,
            "a paused mission with a long-quiet open session must not read as abandoned"
        );

        // And the control: the SAME shape while Active does decay. Without
        // this line the test above would still pass if the gate were removed
        // outright, which would silently undo #1642.
        mission.status = MissionStatus::Active;
        assert_eq!(
            mission_run_status(&mission, &[&ancient], now_ms),
            RunStatus::Abandoned,
            "the pause exemption must not disable the gate for Active missions"
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
        let ghosts = ghost_runs(&idx, &known_missions, &HashSet::new(), &HashSet::new(), now_unix() * 1_000);
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
        let ghosts = ghost_runs(&idx, &HashSet::new(), &known_sessions, &HashSet::new(), now_unix() * 1_000);
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
                last_activity_ts: Some("2026-07-24T10:00:00Z".to_string()),
                ..Default::default()
            },
        );
        // Judged at exactly the session's own activity instant — idle 0,
        // unambiguously live.
        let now_ms = parse_flow_ts("2026-07-24T10:00:00Z").unwrap() * 1_000;
        let ghosts = ghost_runs(&idx, &HashSet::new(), &HashSet::new(), &HashSet::new(), now_ms);
        assert_eq!(ghosts.len(), 1);
        let g = &ghosts[0];
        assert_eq!(g.id, "orphan-sess");
        assert_eq!(g.kind, RunKind::Dispatch);
        assert_eq!(g.status, RunStatus::Running);
        assert!(!g.tracked);
        assert_eq!(g.role.as_deref(), Some("coder"));
        // (#1915) A ghost row's own id already IS its session id — carried
        // explicitly anyway so the client's drill rule never needs a
        // dispatch-specific "use `id` itself" carve-out.
        assert_eq!(g.session_id.as_deref(), Some("orphan-sess"));
    }

    /// (#1918) By construction a ghost SHOULD never be ambiguous — one row
    /// per untracked session, and `has_start` only gates on THIS session
    /// having opened a dispatch, not on how many missions landed in it. But
    /// the guard is applied uniformly (per the coordinator's explicit ask)
    /// rather than assumed-safe and skipped here, so this test forces the
    /// adversarial shape by hand: a session that is BOTH a live ghost
    /// (unmatched by any known/remote mission) AND carries records from two
    /// distinct `mission_id`s — the session-id COLLISION #1918's own doc
    /// names as the finding this would actually represent, not a nuisance
    /// case. The row still gets synthesized (it IS a real, live dispatch
    /// session); only its `session_id` drill target goes honest-None.
    #[test]
    fn ghost_runs_suppresses_session_id_when_the_session_is_ambiguous() {
        let mut idx = HashMap::new();
        let mut collided = SessionAgg {
            mission_id: None,
            has_start: true,
            role: Some("coder".to_string()),
            start_ts: Some("2026-07-24T10:00:00Z".to_string()),
            last_activity_ts: Some("2026-07-24T10:00:00Z".to_string()),
            ..Default::default()
        };
        collided.mission_ids_seen.insert("mission-a".to_string());
        collided.mission_ids_seen.insert("mission-b".to_string());
        idx.insert("colliding-sess".to_string(), collided);
        let now_ms = parse_flow_ts("2026-07-24T10:00:00Z").unwrap() * 1_000;
        let ghosts = ghost_runs(&idx, &HashSet::new(), &HashSet::new(), &HashSet::new(), now_ms);
        assert_eq!(ghosts.len(), 1, "the row itself is still real and still emitted: {ghosts:?}");
        let g = &ghosts[0];
        assert_eq!(g.id, "colliding-sess");
        assert_eq!(
            g.session_id, None,
            "an ambiguous session must never be handed out as a drill target, ghost or not: {g:?}"
        );
    }

    #[test]
    fn ghost_runs_never_synthesizes_a_session_with_no_start() {
        let mut idx = HashMap::new();
        idx.insert(
            "no-start-sess".to_string(),
            SessionAgg { has_start: false, ..Default::default() },
        );
        let ghosts = ghost_runs(&idx, &HashSet::new(), &HashSet::new(), &HashSet::new(), now_unix() * 1_000);
        assert!(ghosts.is_empty());
    }

    // ── ghost_runs: the staleness gate (#1642, #1633) ───────────────────

    #[test]
    fn ghost_runs_fresh_session_is_running_stale_session_is_abandoned() {
        let base_ts = "2000-01-01T00:00:00Z";
        let base_ms = parse_flow_ts(base_ts).unwrap() * 1_000;
        let mut idx = HashMap::new();
        idx.insert(
            "fresh-or-stale".to_string(),
            SessionAgg {
                has_start: true,
                terminal_status: None,
                last_activity_ts: Some(base_ts.to_string()),
                ..Default::default()
            },
        );

        // Just inside the budget: still live.
        let inside_ms = base_ms + stale_after_ms() - 1_000;
        let fresh = ghost_runs(&idx, &HashSet::new(), &HashSet::new(), &HashSet::new(), inside_ms);
        assert_eq!(fresh[0].status, RunStatus::Running, "no terminal + recent activity must read live");

        // Past the budget: the trail stops, and that is evidence of
        // abandonment — the #1642/#1633 defect this test guards.
        let outside_ms = base_ms + stale_after_ms() + 1_000;
        let stale = ghost_runs(&idx, &HashSet::new(), &HashSet::new(), &HashSet::new(), outside_ms);
        assert_eq!(
            stale[0].status,
            RunStatus::Abandoned,
            "a ghost whose session died with no terminal must not read as live forever"
        );
    }

    #[test]
    fn ghost_runs_terminal_status_always_wins_over_the_staleness_gate() {
        // A session that DID reach a real terminal must never be relabeled
        // by staleness, however old it is — a completed run stays completed.
        let mut idx = HashMap::new();
        idx.insert(
            "long-done".to_string(),
            SessionAgg {
                has_start: true,
                terminal_status: Some(RunStatus::Complete),
                last_activity_ts: Some("2000-01-01T00:00:00Z".to_string()),
                ..Default::default()
            },
        );
        let far_future_ms = (now_unix() + 999_999_999) * 1_000;
        let ghosts = ghost_runs(&idx, &HashSet::new(), &HashSet::new(), &HashSet::new(), far_future_ms);
        assert_eq!(ghosts[0].status, RunStatus::Complete, "a terminal verdict must never decay into Abandoned");
    }

    #[test]
    fn lab_mission_and_ghost_agree_on_liveness_at_the_same_idle_age() {
        // (#1642, #1633) The regression this guards against: a future FOURTH
        // `Run` kind reopening this hole by drifting from the other three's
        // threshold. All three EXISTING sources are judged at the exact same
        // idle age off the exact same `stale_after_ms` budget and must reach
        // the same verdict, every time.
        //
        // Honest about its own reach: this is a CONVENTION TRIPWIRE, not a
        // structural guarantee. A fourth kind has to be added to the asserts
        // below by hand, and nothing in the compiler makes anyone do it —
        // whoever adds one is expected to find this test by grepping the
        // shared helper. Claiming more than that would be the same
        // over-promise that let three kinds drift apart in the first place.
        const REF_TS: &str = "2000-01-01T00:00:00Z"; // the known reference point
        let ref_secs = parse_flow_ts(REF_TS).unwrap();
        let ref_ms = ref_secs * 1_000;

        let mut mission = minimal_mission("agree-1", vec![], None);
        mission.started_ts = Some(ref_secs);
        let session = SessionAgg {
            has_start: true,
            terminal_status: None,
            last_activity_ts: Some(REF_TS.to_string()),
            ..Default::default()
        };
        let mut lab_summary = minimal_lab_summary("agree-1-lab", false, false);
        lab_summary.mtime_ms = ref_ms;
        let mut ghost_idx = HashMap::new();
        ghost_idx.insert("agree-1-ghost".to_string(), session.clone());

        for (label, now_ms, want) in [
            ("just inside the budget", ref_ms + stale_after_ms() - 1_000, RunStatus::Running),
            ("just outside the budget", ref_ms + stale_after_ms() + 1_000, RunStatus::Abandoned),
            // (#1642) EXACTLY at the budget. The ±1s cases above cannot
            // detect an inclusivity drift (`<` vs `<=`) between two kinds —
            // both sides agree at ±1s no matter which comparison each uses,
            // so the one boundary where the kinds can silently disagree is
            // the only one the other two rows structurally cannot see. That
            // drift is precisely the class this test exists to prevent.
            ("exactly at the budget", ref_ms + stale_after_ms(), RunStatus::Running),
        ] {
            assert_eq!(lab_run_status(&lab_summary, now_ms), want, "lab disagreed {label}");
            assert_eq!(mission_run_status(&mission, &[&session], now_ms), want, "mission disagreed {label}");
            let ghosts = ghost_runs(&ghost_idx, &HashSet::new(), &HashSet::new(), &HashSet::new(), now_ms);
            assert_eq!(ghosts[0].status, want, "ghost disagreed {label}");
        }
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
            Some(MissionSpec { config_id: "dispatch".to_string(), inputs_fingerprint: "fp".to_string(), origin: None }),
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

        let runs = build_runs(flows.path(), None, &[]);
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
            Some(MissionSpec { config_id: "some-custom-config".to_string(), inputs_fingerprint: "fpg".to_string(), origin: None }),
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

        let runs = build_runs(flows.path(), None, &[]);
        assert_eq!(runs.len(), 1, "exactly one Run for the generic-config mission, no per-step ghost: {runs:?}");
        assert_eq!(runs[0].id, "generic-config-1");
        assert_eq!(runs[0].kind, RunKind::Mission);
        assert!(runs[0].tracked);
        assert_eq!(runs[0].model.as_deref(), Some("qwen3.6-35b-a3b"));
    }

    /// (#1877 regression, fixed here) The whole-run `dispatch start`
    /// bookend `launch()` now emits unconditionally opens BEFORE any step
    /// dispatches — so it is always the mission's earliest session, and
    /// wins `earliest_by_start`'s pick as `representative`. Its record
    /// carries `handle = <launched config id>` (a real, non-empty string
    /// — never the actual per-step role) and NO `model` at all
    /// (`mission_bookend_record` always passes `model: None`; one bookend
    /// spans however many per-step model calls the mission makes).
    /// Reading role/model straight off `representative` therefore shows
    /// the config id as "role" and blanks "model" on the dashboard for
    /// every mission the new bookend touches — a real, operator-visible
    /// regression this test pins the fix for.
    ///
    /// Red-proved by temporarily reverting `mission_to_run`'s role/model
    /// lines to `representative.and_then(|s| s.role.clone())` /
    /// `representative.and_then(|s| s.model.clone())`: role then reported
    /// `Some("coder-phase")` (the bookend's config-id handle, not the
    /// coder step's real role) and model reported `None`, both against
    /// this test's assertions.
    #[test]
    #[serial_test::serial]
    fn build_runs_1877_bookend_does_not_blank_a_previously_shown_role_or_model() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();

        let mut mission = minimal_mission(
            "bookend-mission-1",
            vec!["p-bookend".to_string()],
            Some(MissionSpec {
                config_id: "coder-phase".to_string(),
                inputs_fingerprint: "fpb".to_string(),
                origin: None,
            }),
        );
        // Unmask `start_ts_str`'s contribution to `Run.started_ts`: with
        // `mission.started_ts` set (as `minimal_mission` does by default),
        // the mission record's own field always wins first and the
        // session-derived fallback this test also pins never gets
        // exercised. `mission_run_status` maps an Active mission with no
        // `started_ts` straight to `Planned` (CONSIDER 4) regardless of
        // sessions, which is why `status` isn't asserted below — that's an
        // accepted, orthogonal side effect of unmasking the field.
        mission.started_ts = None;
        darkmux_crew::lifecycle::save_mission(&mission).unwrap();
        let phase = minimal_phase("p-bookend", "bookend-mission-1", vec!["t-bookend".to_string()]);
        darkmux_crew::lifecycle::save_phase(&phase).unwrap();
        let task = minimal_task("t-bookend", "p-bookend", vec!["s-bookend".to_string()], Some("coder"));
        darkmux_crew::lifecycle::save_task("bookend-mission-1", &task).unwrap();
        let step = minimal_step("s-bookend", "t-bookend", Some("crew-dispatch-coder-bookend"));
        darkmux_crew::lifecycle::save_step("bookend-mission-1", "p-bookend", &step).unwrap();

        write_day_file(
            flows.path(),
            &today(),
            &[
                // The #1877 whole-run bookend — matches `mission_bookend_record`'s
                // real shape: `handle` = the launched config id, `session_id` =
                // `mission_id`, `source: "mission"`, no `model`. Earliest ts, so
                // it wins the `earliest_by_start` pick. `machine_id` present, as
                // it would be in production (`darkmux_flow::record` auto-stamps
                // it on every record whose caller left it unset — not something
                // `mission_bookend_record` itself sets).
                serde_json::json!({
                    "ts": "2026-01-01T08:00:00Z",
                    "action": "dispatch start",
                    "session_id": "bookend-mission-1",
                    "handle": "coder-phase",
                    "mission_id": "bookend-mission-1",
                    "source": "mission",
                    "machine_id": "studio",
                }),
                // The coder step's OWN dispatch — a real role and model,
                // starting after the bookend. `machine_id` deliberately
                // DIFFERENT from the bookend's, to pin that `machine` reads
                // only the representative (earliest) session and does not
                // borrow a later session's value the way role/model now do.
                serde_json::json!({
                    "ts": "2026-01-01T09:00:00Z",
                    "action": "dispatch start",
                    "session_id": "crew-dispatch-coder-bookend",
                    "handle": "coder",
                    "machine_id": "different-peer",
                }),
                serde_json::json!({
                    "ts": "2026-01-01T09:10:00Z",
                    "action": "dispatch complete",
                    "session_id": "crew-dispatch-coder-bookend",
                    "handle": "coder",
                    "model": "qwen3.6-35b-a3b",
                    "machine_id": "different-peer",
                }),
            ],
        );

        let runs = build_runs(flows.path(), None, &[]);
        assert_eq!(runs.len(), 1, "exactly one Run — the bookend session joins, it doesn't ghost: {runs:?}");
        assert_eq!(runs[0].id, "bookend-mission-1");
        assert_eq!(runs[0].kind, RunKind::Mission);
        assert!(runs[0].tracked);

        // The fix: role/model recover from the coder step's session, not
        // the bookend's own placeholder handle / absent model.
        assert_eq!(runs[0].role.as_deref(), Some("coder"), "role must recover the coder step's real role, not the bookend's config-id handle: {runs:?}");
        assert_eq!(runs[0].model.as_deref(), Some("qwen3.6-35b-a3b"), "model must recover from the coder step's session, not stay blanked by the bookend: {runs:?}");

        // The deliberate machine decision: representative-only, no
        // fallback — because every record, bookend included, gets
        // `machine_id` auto-stamped at write time in production, so the
        // bookend session is never the one blanking it.
        assert_eq!(runs[0].machine.as_deref(), Some("studio"), "machine must read the representative (earliest/bookend) session's value, not borrow the coder session's: {runs:?}");

        // (#1915) `session_id` follows the SAME representative-only rule as
        // `machine` above — the bookend's own id, not the later coder
        // session's, and not the mission's own id (which happens to be the
        // same string here by construction, `mission_bookend_record`'s own
        // shape — pinned as "the representative session's id" rather than
        // "the mission id" so the two don't get silently conflated).
        assert_eq!(runs[0].session_id.as_deref(), Some("bookend-mission-1"), "session_id must be the representative (earliest) session's own id: {runs:?}");

        // Ordering is untouched by this fix: start_ts still comes from the
        // EARLIEST session (the bookend), same as before.
        assert_eq!(
            runs[0].started_ts,
            parse_flow_ts("2026-01-01T08:00:00Z"),
            "start_ts must still come from the earliest session — only role/model attribution changed: {runs:?}"
        );
    }

    /// (#1918) The SAME uniform guard applied to the LOCAL tracked-mission
    /// path, not just the fleet/untracked one above — the coordinator's
    /// explicit ask was "apply uniformly at every population site," and
    /// `mission_to_run` is the third. Reuses the bookend fixture above
    /// almost verbatim; the only change is a second flow record under the
    /// SAME `session_id` naming a DIFFERENT `mission_id` — the #1918 shape.
    /// A tracked mission never actually consults `session_id` for its own
    /// drill (it resolves via `#mission=<id>` first — see `Run::session_id`'s
    /// doc), so this pins the FIELD's correctness for any other consumer
    /// (a future doctor check, an operator inspecting `/runs` directly)
    /// rather than a client-visible behavior change.
    #[test]
    #[serial_test::serial]
    fn build_runs_1918_a_tracked_missions_ambiguous_representative_session_gets_no_drill_target() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();

        let mut mission = minimal_mission(
            "collision-mission-1",
            vec!["p-collision".to_string()],
            Some(MissionSpec {
                config_id: "coder-phase".to_string(),
                inputs_fingerprint: "fpc".to_string(),
                origin: None,
            }),
        );
        mission.started_ts = None;
        darkmux_crew::lifecycle::save_mission(&mission).unwrap();
        let phase = minimal_phase("p-collision", "collision-mission-1", vec!["t-collision".to_string()]);
        darkmux_crew::lifecycle::save_phase(&phase).unwrap();
        let task = minimal_task("t-collision", "p-collision", vec!["s-collision".to_string()], Some("coder"));
        darkmux_crew::lifecycle::save_task("collision-mission-1", &task).unwrap();
        let step = minimal_step("s-collision", "t-collision", Some("collision-mission-1"));
        darkmux_crew::lifecycle::save_step("collision-mission-1", "p-collision", &step).unwrap();

        write_day_file(
            flows.path(),
            &today(),
            &[
                // This mission's own bookend — same shape as the #1877
                // fixture above.
                serde_json::json!({
                    "ts": "2026-01-01T08:00:00Z",
                    "action": "dispatch start",
                    "session_id": "collision-mission-1",
                    "handle": "coder-phase",
                    "mission_id": "collision-mission-1",
                    "source": "mission",
                    "machine_id": "studio",
                }),
                // The #1918 collision: a DIFFERENT mission's step landed
                // under the SAME session_id (the scheduler's task-derived
                // id scheme colliding across missions).
                serde_json::json!({
                    "ts": "2026-01-01T08:30:00Z",
                    "action": "step start",
                    "session_id": "collision-mission-1",
                    "mission_id": "some-other-mission",
                    "source": "scheduler",
                }),
            ],
        );

        let runs = build_runs(flows.path(), None, &[]);
        let row = runs.iter().find(|r| r.id == "collision-mission-1").expect("row for collision-mission-1");
        assert!(row.tracked, "this test's own premise: a LOCAL durable mission, tracked: true");
        assert_eq!(
            row.session_id, None,
            "a tracked mission's representative session must ALSO go None when it is ambiguous — the guard is uniform across every population site, not a fleet-only special case: {row:?}"
        );
    }

    /// (#1877 QA must-fix 2) A session with a TERMINAL record but no
    /// `dispatch start` at all must never win role OR model — reachable via
    /// `RUNS_FLOW_SCAN_WINDOW_DAYS` truncating the start out of the scan
    /// window while the complete stays inside it, or a Redis `XADD MAXLEN ~`
    /// eviction of the oldest (the start) while the complete survives.
    /// `sessions_by_start`'s sort used to compare `Option<String>` directly
    /// with no filter, and `Option::cmp` orders `None` before `Some`, so a
    /// start-less session sorted to the front and its `find_map` lookups
    /// won both attributes ahead of the mission's real dispatch session.
    ///
    /// RED PROVED: against the pre-fix `sessions_by_start.sort_by(|a, b|
    /// a.start_ts.cmp(&b.start_ts))` (no filter), this test's role/model
    /// assertions failed with `Some("STALE-ROLE")` / `Some("STALE-MODEL")`
    /// — exactly the injected startless session's own handle/model, in
    /// place of the real coder session's `"coder"` / `"qwen3.6-35b-a3b"`.
    #[test]
    #[serial_test::serial]
    fn build_runs_1877_startless_session_never_wins_role_or_model() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();

        let mut mission = minimal_mission(
            "bookend-mission-2",
            vec!["p-bookend2".to_string()],
            Some(MissionSpec {
                config_id: "coder-phase".to_string(),
                inputs_fingerprint: "fpb2".to_string(),
                origin: None,
            }),
        );
        mission.started_ts = None;
        darkmux_crew::lifecycle::save_mission(&mission).unwrap();
        let phase = minimal_phase("p-bookend2", "bookend-mission-2", vec!["t-bookend2".to_string()]);
        darkmux_crew::lifecycle::save_phase(&phase).unwrap();
        let task = minimal_task("t-bookend2", "p-bookend2", vec!["s-bookend2".to_string()], Some("coder"));
        darkmux_crew::lifecycle::save_task("bookend-mission-2", &task).unwrap();
        let step = minimal_step("s-bookend2", "t-bookend2", Some("crew-dispatch-coder-bookend2"));
        darkmux_crew::lifecycle::save_step("bookend-mission-2", "p-bookend2", &step).unwrap();

        write_day_file(
            flows.path(),
            &today(),
            &[
                // The #1877 whole-run bookend — earliest ts, wins
                // `earliest_by_start`'s `representative` pick.
                serde_json::json!({
                    "ts": "2026-01-01T08:00:00Z",
                    "action": "dispatch start",
                    "session_id": "bookend-mission-2",
                    "handle": "coder-phase",
                    "mission_id": "bookend-mission-2",
                    "source": "mission",
                    "machine_id": "studio",
                }),
                // The coder step's OWN dispatch — a real role and model.
                serde_json::json!({
                    "ts": "2026-01-01T09:00:00Z",
                    "action": "dispatch start",
                    "session_id": "crew-dispatch-coder-bookend2",
                    "handle": "coder",
                    "machine_id": "different-peer",
                }),
                serde_json::json!({
                    "ts": "2026-01-01T09:10:00Z",
                    "action": "dispatch complete",
                    "session_id": "crew-dispatch-coder-bookend2",
                    "handle": "coder",
                    "model": "qwen3.6-35b-a3b",
                    "machine_id": "different-peer",
                }),
                // (#1877 QA must-fix 2) A terminal-only record — NO
                // matching `dispatch start` for this `session_id` anywhere
                // in this scenario — timestamped LATEST of all three so a
                // buggy `None`-sorts-first comparison still puts it at the
                // FRONT of `sessions_by_start` despite the late `ts`.
                serde_json::json!({
                    "ts": "2026-01-01T09:20:00Z",
                    "action": "dispatch complete",
                    "session_id": "startless-session",
                    "handle": "STALE-ROLE",
                    "model": "STALE-MODEL",
                    "mission_id": "bookend-mission-2",
                }),
            ],
        );

        let runs = build_runs(flows.path(), None, &[]);
        assert_eq!(runs.len(), 1, "exactly one Run: {runs:?}");
        assert_eq!(runs[0].id, "bookend-mission-2");
        assert_eq!(
            runs[0].role.as_deref(),
            Some("coder"),
            "a start-less terminal-only session must never win role: {runs:?}"
        );
        assert_eq!(
            runs[0].model.as_deref(),
            Some("qwen3.6-35b-a3b"),
            "a start-less terminal-only session must never win model: {runs:?}"
        );
        // `machine` already goes through `earliest_by_start`, which already
        // filters `start_ts.is_some()` — unaffected by this bug either way,
        // pinned here so a future regression on THIS sort doesn't silently
        // also break the field that was never broken.
        assert_eq!(runs[0].machine.as_deref(), Some("studio"), "{runs:?}");
    }

    /// A mission with NO other dispatching session at all — a Tier-1-only
    /// procedural graph (#1877's own named gap 2: no model dispatch,
    /// bookend-only). The bookend's config-id handle is the ONLY
    /// information available, so it is legitimately the fallback here —
    /// this is the "nothing else resolved" arm of the role fallback, not a
    /// display bug.
    #[test]
    #[serial_test::serial]
    fn build_runs_bookend_only_mission_falls_back_to_the_bookends_own_config_id_role() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();

        let mission = minimal_mission(
            "bookend-only-1",
            vec![],
            Some(MissionSpec {
                config_id: "gh-verb-approve".to_string(),
                inputs_fingerprint: "fpo".to_string(),
                origin: None,
            }),
        );
        darkmux_crew::lifecycle::save_mission(&mission).unwrap();

        write_day_file(
            flows.path(),
            &today(),
            &[
                serde_json::json!({
                    "ts": "2026-01-01T08:00:00Z",
                    "action": "dispatch start",
                    "session_id": "bookend-only-1",
                    "handle": "gh-verb-approve",
                    "mission_id": "bookend-only-1",
                    "source": "mission",
                }),
                serde_json::json!({
                    "ts": "2026-01-01T08:00:05Z",
                    "action": "dispatch complete",
                    "session_id": "bookend-only-1",
                    "handle": "gh-verb-approve",
                    "mission_id": "bookend-only-1",
                    "source": "mission",
                }),
            ],
        );

        let runs = build_runs(flows.path(), None, &[]);
        assert_eq!(runs.len(), 1, "exactly one Run for the bookend-only mission: {runs:?}");
        assert_eq!(runs[0].role.as_deref(), Some("gh-verb-approve"), "with no other session, the bookend's own handle IS the best available role: {runs:?}");
        assert_eq!(runs[0].model.as_deref(), None, "a procedural-only mission genuinely has no model to show: {runs:?}");
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
            Some(MissionSpec { config_id: "review".to_string(), inputs_fingerprint: "fpr".to_string(), origin: None }),
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

        let runs = build_runs(flows.path(), None, &[]);
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
            Some(MissionSpec { config_id: "dispatch".to_string(), inputs_fingerprint: "fpc".to_string(), origin: None }),
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

        let runs = build_runs(flows.path(), None, &[]);
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
            Some(MissionSpec { config_id: "dispatch".to_string(), inputs_fingerprint: "fp2".to_string(), origin: None }),
        );
        darkmux_crew::lifecycle::save_mission(&mission).unwrap();
        let phase = minimal_phase("p-2", "dispatch-coder-2", vec!["t-2".to_string()]);
        darkmux_crew::lifecycle::save_phase(&phase).unwrap();
        let task = minimal_task("t-2", "p-2", vec!["s-2".to_string()], Some("coder"));
        darkmux_crew::lifecycle::save_task("dispatch-coder-2", &task).unwrap();
        let step = minimal_step("s-2", "t-2", Some("crew-dispatch-coder-known"));
        darkmux_crew::lifecycle::save_step("dispatch-coder-2", "p-2", &step).unwrap();

        // (#1642) `build_runs` now gates a ghost's liveness against the REAL
        // wall clock (it computes `now_ms` from `SystemTime::now()`, not a
        // fixture), so the orphan's `ts` must be genuinely recent — not the
        // old hardcoded literal — or it would read `Abandoned` on any run of
        // this suite, defeating the assertion below.
        let orphan_ts = darkmux_flow::ts_utc_now();
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
                    "ts": orphan_ts,
                    "action": "dispatch start",
                    "session_id": "crew-dispatch-reviewer-orphan",
                    "handle": "reviewer",
                }),
            ],
        );

        let runs = build_runs(flows.path(), None, &[]);
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

    // ── #1705: the fleet half — records this machine never wrote ─────────

    /// A record shaped like one a PEER emitted: it exists only in the fleet
    /// stream, never in this machine's day-files.
    fn peer_record(action: &str, ts: &str) -> serde_json::Value {
        serde_json::json!({
            "ts": ts,
            "level": "info",
            "category": "work",
            "stage": "dispatch",
            "action": action,
            "handle": "azure-review",
            "session_id": "peer-session-1",
            "source": "review",
            "model": "gpt-4o",
            "mission_id": "review-on-the-hub",
            "machine_id": "m1-max-32gb-studio",
            "machine_uid": "PEER-UID-1",
        })
    }

    #[test]
    fn a_peers_mission_becomes_one_run_row_from_the_fleet_stream() {
        let flows = TempDir::new().unwrap(); // deliberately EMPTY: the peer's
        // records were never written to this machine's flows dir.
        let fleet = vec![
            peer_record("dispatch start", &darkmux_flow::ts_utc_now()),
            peer_record("dispatch complete", &darkmux_flow::ts_utc_now()),
        ];
        let runs = build_runs(flows.path(), None, &fleet);
        let row = runs
            .iter()
            .find(|r| r.id == "review-on-the-hub")
            .expect("a mission seen only in the fleet stream must still produce a run row");
        assert_eq!(row.kind, RunKind::Mission, "one row per mission, not one per session");
        assert!(!row.tracked, "this machine has no durable record of a peer's mission");
        assert_eq!(row.machine.as_deref(), Some("m1-max-32gb-studio"));
        assert_eq!(row.model.as_deref(), Some("gpt-4o"), "route/model borrowed from its sessions");
        // and NOT also a loose per-session ghost for the same work
        assert!(
            !runs.iter().any(|r| r.id == "peer-session-1"),
            "the peer's session is represented by its mission row, not duplicated as a ghost"
        );
    }

    /// (#1915) The defect this whole issue is about: an untracked mission
    /// row used to carry NO way to open it at all — `flow_mission_to_run`
    /// already computed a representative session for route/model
    /// attribution and simply never carried its id out. On the reported
    /// machine 40 of 104 mission rows were exactly this shape, and the
    /// board sorts newest-first, so this was the first page a person saw.
    #[test]
    fn a_peers_untracked_mission_carries_its_representative_session_as_the_drill_target() {
        let flows = TempDir::new().unwrap();
        let fleet = vec![
            peer_record("dispatch start", &darkmux_flow::ts_utc_now()),
            peer_record("dispatch complete", &darkmux_flow::ts_utc_now()),
        ];
        let runs = build_runs(flows.path(), None, &fleet);
        let row = runs.iter().find(|r| r.id == "review-on-the-hub").unwrap();
        assert!(!row.tracked, "this test's own premise: the row must be untracked");
        assert_eq!(
            row.session_id.as_deref(),
            Some("peer-session-1"),
            "an untracked mission row must carry its representative session as a drill target, not None: {row:?}"
        );
    }

    /// (#1918) The direction that makes #1915 actually shippable: the SAME
    /// `peer-session-1` this test's sibling above proves opens cleanly, but
    /// now a SECOND mission's record also lands under that session_id (the
    /// scheduler defect's shape — a session bucket collapsing more than one
    /// mission's records together). The representative pick still succeeds
    /// mechanically; the ambiguity guard is what has to catch that the pick
    /// is no longer trustworthy as a drill target.
    #[test]
    fn an_untracked_missions_ambiguous_representative_session_gets_no_drill_target() {
        let flows = TempDir::new().unwrap();
        let mut collided_record = peer_record("dispatch start", &darkmux_flow::ts_utc_now());
        collided_record["mission_id"] = serde_json::json!("review-on-a-different-hub");
        let fleet = vec![
            peer_record("dispatch start", &darkmux_flow::ts_utc_now()),
            peer_record("dispatch complete", &darkmux_flow::ts_utc_now()),
            // Same session_id ("peer-session-1", from `peer_record`), a
            // DIFFERENT mission_id — the collision.
            collided_record,
        ];
        let runs = build_runs(flows.path(), None, &fleet);
        let row = runs.iter().find(|r| r.id == "review-on-the-hub").expect("row for review-on-the-hub");
        assert!(!row.tracked, "this test's own premise: the row must be untracked");
        assert_eq!(
            row.session_id, None,
            "a representative session shared by another mission must never be handed out as a drill target, \
             even though the mechanical representative-session pick still succeeds: {row:?}"
        );
    }

    /// The inverted case. Without it, the test above would pass just as
    /// happily if `build_runs` fabricated rows from somewhere other than the
    /// fleet input — the assertion would be measuring nothing.
    #[test]
    fn with_no_fleet_records_there_is_no_peer_row() {
        let flows = TempDir::new().unwrap();
        let runs = build_runs(flows.path(), None, &[]);
        assert!(
            !runs.iter().any(|r| r.id == "review-on-the-hub"),
            "the peer row must come from the fleet stream, not from thin air"
        );
    }

    #[test]
    fn a_closed_peer_mission_reads_complete_not_running() {
        let flows = TempDir::new().unwrap();
        let fleet = vec![
            peer_record("dispatch start", &darkmux_flow::ts_utc_now()),
            serde_json::json!({
                "ts": darkmux_flow::ts_utc_now(),
                "action": "mission close",
                "source": "mission_lifecycle",
                "session_id": "mission-review-on-the-hub",
                "mission_id": "review-on-the-hub",
                "machine_id": "m1-max-32gb-studio",
            }),
        ];
        let runs = build_runs(flows.path(), None, &fleet);
        let row = runs.iter().find(|r| r.id == "review-on-the-hub").unwrap();
        assert_eq!(row.status, RunStatus::Complete);
        assert!(row.completed_ts.is_some(), "a closed mission carries its completion stamp");
    }

    /// A peer that fell asleep mid-mission must not leave a row claiming to
    /// be running forever — the same staleness rule every other row obeys.
    #[test]
    fn a_stale_unclosed_peer_mission_is_abandoned_not_running() {
        let flows = TempDir::new().unwrap();
        // Yesterday: comfortably INSIDE the 14-day scan window, comfortably
        // OUTSIDE the liveness budget. The two bounds answer different
        // questions and this test pins the second one — `cutoff_date_string(1)`
        // is the same date arithmetic the window itself uses.
        let stale_ts = format!("{}T00:00:00Z", cutoff_date_string(1));
        let fleet = vec![peer_record("dispatch start", &stale_ts)];
        let runs = build_runs(flows.path(), None, &fleet);
        let row = runs.iter().find(|r| r.id == "review-on-the-hub").unwrap();
        assert_eq!(row.status, RunStatus::Abandoned);
    }

    /// #1627, applied to a mission this machine did not run: a torn-down
    /// mission is not a completed one. Before the #1707 gate caught it, an
    /// aborted peer mission read `complete` on every OTHER machine while the
    /// owning machine correctly read `abandoned` — a killed run inheriting a
    /// success verdict it never earned.
    #[test]
    fn an_aborted_peer_mission_reads_abandoned_not_complete() {
        let flows = TempDir::new().unwrap();
        let fleet = vec![
            peer_record("dispatch start", &darkmux_flow::ts_utc_now()),
            serde_json::json!({
                "ts": darkmux_flow::ts_utc_now(),
                "action": "mission abort",
                "source": "mission_lifecycle",
                "session_id": "mission-review-on-the-hub",
                "mission_id": "review-on-the-hub",
                "machine_id": "m1-max-32gb-studio",
            }),
        ];
        let runs = build_runs(flows.path(), None, &fleet);
        let row = runs.iter().find(|r| r.id == "review-on-the-hub").unwrap();
        assert_eq!(
            row.status,
            RunStatus::Abandoned,
            "an abort is teardown, not success — this is what the tracked path does \
             (MissionStatus::Aborted => RunStatus::Abandoned)"
        );
        assert!(
            row.completed_ts.is_none(),
            "a torn-down mission never completed, so it carries no completion stamp"
        );
    }

    /// The fleet half must obey the same 14-day bound the local walk does.
    /// `XADD MAXLEN ~` trims lazily — only on write — so a quiet fleet keeps
    /// month-old records that would otherwise resurface as rows that never
    /// age out.
    #[test]
    fn a_fleet_record_older_than_the_scan_window_never_enters_runs() {
        let flows = TempDir::new().unwrap();
        let fleet = vec![peer_record("dispatch start", "2020-01-01T00:00:00Z")];
        let runs = build_runs(flows.path(), None, &fleet);
        assert!(
            !runs.iter().any(|r| r.id == "review-on-the-hub"),
            "a fleet record far outside RUNS_FLOW_SCAN_WINDOW_DAYS must not build a row"
        );
    }

    #[test]
    fn a_record_present_in_both_sinks_is_counted_once() {
        let flows = TempDir::new().unwrap();
        // The SAME record in the local day-file and in the fleet stream —
        // exactly what happens for this machine's own work, which is
        // written to both.
        let rec = peer_record("dispatch start", &darkmux_flow::ts_utc_now());
        write_day_file(flows.path(), &today(), std::slice::from_ref(&rec));
        let idx = build_flow_session_index(flows.path(), std::slice::from_ref(&rec));
        let agg = idx.get("peer-session-1").expect("session present");
        assert!(agg.has_start);
        // The dedup is what this asserts: two sources, one session, and the
        // mission row must still be singular.
        let runs = build_runs(flows.path(), None, std::slice::from_ref(&rec));
        assert_eq!(
            runs.iter().filter(|r| r.id == "review-on-the-hub").count(),
            1,
            "a record in both sinks must not produce two runs"
        );
    }
}
