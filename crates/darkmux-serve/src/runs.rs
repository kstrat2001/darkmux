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
use darkmux_crew::step_kinds::StepKindRegistry;
use darkmux_crew::types::{Mission, MissionStatus, Phase, PhaseStatus, Step, Task};
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

/// (#1907) Which of two genuinely different situations produced a
/// [`RunStatus::Abandoned`] row — see [`Run::abandoned_reason`]'s own doc
/// for the wire contract and each construction site
/// (`mission_to_run`/`flow_mission_to_run`/`lab_summary_to_run`/
/// `ghost_runs`) for how each is decided. Only meaningful when `status ==
/// RunStatus::Abandoned`; every other status leaves this `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "lowercase")]
pub enum AbandonReason {
    /// A human explicitly tore the run down: a local `mission abort`
    /// (`MissionStatus::Aborted`), or the remote-mission flow twin of it
    /// (`FlowMissionAgg::terminal_was_abort`). Deliberate teardown, not a
    /// run that failed to finish — #1627's own distinction, now carried to
    /// the wire instead of collapsing into the same word as the case below.
    Aborted,
    /// No terminal record was ever written and nothing is live — killed,
    /// crashed, timed out, or the terminal record simply never landed.
    /// darkmux does not know how (or whether) this run actually ended; this
    /// is the honest "no ending recorded" case, never a claim that the run
    /// gave up on purpose.
    NoTerminal,
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
    /// (#1810) This field carries TWO different meanings depending on
    /// `kind`, both labeled "machine" on the wire. A locally-tracked
    /// mission row (`mission_to_run`) reports the MINT host — the durable
    /// `Mission.machine`, stamped once at creation and never overwritten
    /// by whichever host later executes the dispatches. A remote-mission
    /// row (`flow_mission_to_run`, from `FlowMissionAgg.machine`) and a
    /// lab row (`lab_summary_to_run`, the daemon's own declared
    /// `machine_id`) both report the EXECUTION host instead — there is no
    /// separate durable mint-site fact for either of those paths. A
    /// machine-pinned filter/lens built on this field should know which
    /// question it is answering for a given `kind`, not assume one
    /// consistent meaning across all three.
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
    /// `#dispatch=<id>` (#1974 renamed it from `#session=<id>`; that
    /// spelling survives as a one-release parser alias) — the SAME
    /// representative-session pick
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
    /// (#1907) Set only when `status == RunStatus::Abandoned` — see
    /// [`AbandonReason`]'s own doc. `RunStatus::Abandoned` alone collapses
    /// "someone ran `mission abort`" and "no terminal record was ever
    /// written" into one word; this field carries the distinction the
    /// server already computes (`terminal_was_abort` / `MissionStatus::Aborted`)
    /// instead of dropping it on the wire. Absent for every other status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub abandoned_reason: Option<AbandonReason>,
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
            // (#1982) Claim the lab run's OWN inner-dispatch session — read
            // back from its `manifest.json` — the same way the mission loop
            // above claims its step sessions. Without this, a lab run's
            // dispatch bookends have no claimant and `ghost_runs` (below)
            // synthesizes a SECOND, untracked row for the exact same work.
            // An absent session_id (a provider that never recorded one)
            // claims nothing — the ghost persists, which is the honest
            // degradation named in the issue's acceptance criteria.
            if let Some(sid) = &summary.session_id {
                known_session_ids.insert(sid.clone());
            }
            runs.push(lab_summary_to_run(&summary, lab_machine.clone(), now_ms));
        }
    }

    // (#1705) Missions seen only in the record stream — i.e. executing on a
    // peer. Emitted BEFORE ghosts so their sessions are claimed and don't
    // also surface as loose dispatch rows.
    let (peer_runs, remote_mission_ids) =
        peer_runs_from_index(&flow_missions, &known_mission_ids, &flow_index, now_ms);
    runs.extend(peer_runs);

    runs.extend(ghost_runs(
        &flow_index,
        &known_mission_ids,
        &known_session_ids,
        &remote_mission_ids,
        now_ms,
    ));

    runs
}

/// (#1711) Every `flow_missions` entry NOT in `known_mission_ids` — i.e. a
/// mission this reader can SEE via the merged flow record set but does not
/// OWN, because no durable `Mission` JSON for it exists here (either it
/// executed on a peer, or its record survived a locally-deleted mission).
/// Returns the `Run`s alongside the set of ids emitted, so [`build_runs`]
/// can feed that set to [`ghost_runs`] without a second pass.
///
/// Split out so [`peer_mission_runs`] (a narrower public entry point for a
/// caller that already has its own `known_mission_ids` in hand — `mission
/// status`, #1711) shares this ONE implementation with [`build_runs`]
/// rather than each maintaining its own copy of the filter+map.
fn peer_runs_from_index(
    flow_missions: &HashMap<String, FlowMissionAgg>,
    known_mission_ids: &HashSet<String>,
    flow_index: &HashMap<String, SessionAgg>,
    now_ms: u64,
) -> (Vec<Run>, HashSet<String>) {
    // (#1711) A "peer" claim requires knowing WHO ran it — an entry whose
    // `machine` matches THIS reader's own identity (resolved the same way
    // records are stamped at write time: `DARKMUX_MACHINE_ID` >
    // `config.machine_id` > `hostname(1)`) is not a peer at all, it is an
    // ORPHAN: a mission with no durable JSON here that ALSO ran here — a
    // deleted mission dir, a malformed `mission.json` the loader silently
    // skipped, or a subsystem that stamped `mission_id` without minting
    // under `missions_dir()`. Mislabeling that "observed on the fleet, not
    // owned by this machine" is actively wrong: it IS this machine. `None`
    // (no `machine` on the record at all) is left alone — that is exactly
    // the case this reader cannot rule out, and the honest answer under
    // uncertainty is to keep showing it, not to guess it away.
    let local_machine = darkmux_flow::resolve_machine_id();
    let mut runs = Vec::new();
    let mut remote_mission_ids: HashSet<String> = HashSet::new();
    for (mission_id, agg) in flow_missions {
        if known_mission_ids.contains(mission_id) {
            continue;
        }
        if local_machine.is_some() && agg.machine.as_deref() == local_machine.as_deref() {
            // Not claimed as remote — its session(s) fall through to the
            // ordinary per-session ghost-dispatch synthesis below, exactly
            // as they would have before the peer-mission concept existed.
            continue;
        }
        remote_mission_ids.insert(mission_id.clone());
        runs.push(flow_mission_to_run(mission_id, agg, flow_index, now_ms));
    }
    (runs, remote_mission_ids)
}

/// (#1711) The peer-mission half of [`build_runs`], standalone — for a
/// caller that wants ONLY the "observed, not owned" rows and already has
/// its own local mission id set in hand (`mission status`'s board, which
/// loads `Mission`/`Phase` JSON itself for its own local half). Calling
/// `build_runs` for this would pay for the local mission→`Run` build
/// (`classify_mission`/`collect_mission_step_sessions` per mission) and a
/// SECOND `load_missions()`/`load_phases()` disk read the caller's own
/// load already did — both wasted work for a caller that only wants this
/// slice.
///
/// `known_mission_ids` is the caller's own already-resolved local mission
/// id set, so a mission this reader OWNS never double-appears as a
/// "peer" row (the same guarantee [`build_runs`]'s `known_mission_ids`
/// gives its own callers).
pub fn peer_mission_runs(
    flows_dir: &StdPath,
    fleet: &[serde_json::Value],
    known_mission_ids: &HashSet<String>,
) -> Vec<Run> {
    let flow_index = build_flow_session_index(flows_dir, fleet);
    let flow_missions = build_flow_mission_index(flows_dir, fleet);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    peer_runs_from_index(&flow_missions, known_mission_ids, &flow_index, now_ms).0
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
    // (#1907) Both `Abandoned` arms above are already told apart by
    // `agg.terminal_was_abort` — a real `mission abort` record versus no
    // terminal record landing at all — so carry that to the wire instead of
    // re-deriving it from `status` alone (which can't tell the two apart
    // after the match above has already collapsed them).
    let abandoned_reason = if status == RunStatus::Abandoned {
        Some(if agg.terminal_was_abort { AbandonReason::Aborted } else { AbandonReason::NoTerminal })
    } else {
        None
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
        abandoned_reason,
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
    // Built once per mission, not per step — the registry allocates a
    // handful of `Arc`s and a map.
    let registry = attribution_registry();
    for phase_id in &mission.phase_ids {
        let Ok(steps) = darkmux_crew::lifecycle::load_steps_for_phase(&mission.id, phase_id) else {
            continue;
        };
        for step in steps {
            // (#1918 — considered, not applied) `step_session_id`
            // reconstructs the per-KIND default (`session_id::task`/
            // `session_id::step`) exactly as the producer computes it
            // BEFORE composing this run's own identity in via
            // `scope_to_run`. This predictor does NOT also predict the
            // scoped form: every producer that applies `scope_to_run`
            // (the launcher's `emit`-wrap, and `dispatch_internal::
            // dispatch`'s own resolved-mission composition) does so in
            // lock-step with populating `FlowRecord.mission_id` — the
            // SAME resolved value drives both, unconditionally in the
            // launcher's case and gated on the SAME `resolve_mission_
            // for_phase` call in `dispatch_internal`'s. So a record's
            // `session_id` is the SCOPED form if and only if that record
            // ALSO carries `mission_id` — and a session with `mission_id`
            // present is already correctly attributed and ghost-
            // suppressed via `build_mission_id_index`/`known_mission_ids`,
            // with no need for this predictor to also guess the scoped
            // string. Only the UNSCOPED default below is ever needed here
            // (the pre-existing #1523 "Gap 2": a step whose phase→mission
            // resolution fails keeps the raw form on both mission_id AND
            // session_id, together).
            //
            // (#1918 QA) This reasoning is specific to a MISSION-
            // attribution join. It does NOT generalize: `mission_graph::
            // step_for_record` asks WHICH STEP a record belongs to, which
            // `mission_id` cannot answer, so that consumer does have to
            // accept the scoped spelling and peels the suffix itself.
            if let Some(sid) = step_session_id(&step, &registry) {
                out.insert(sid);
            }
        }
    }
    out
}

/// (#2310 swarm F / S2-1) The registry [`step_session_id`] asks.
///
/// `with_builtins()` alone is not enough. `records.gather`, `mods.gate`
/// and `deliver.github_review` are Tier-3 kinds registered per-launch by
/// `mission launch` (`src/mission_launch.rs::all_step_kinds`), so they are
/// absent here — and an ABSENT kind falls through to the trait DEFAULT,
/// which is step-scoped. Each of those three declares
/// `dispatch_session_id -> None` because it never dispatches a model at
/// all; without registering them, this module claimed a session per
/// gather/gate/deliver step that no record will ever carry, which is the
/// exact "two files encoding one convention" failure #1979 set out to
/// end, running in the other direction.
///
/// Only the kinds that live in `darkmux-crew` are registrable here — the
/// review pipeline's own kinds live in `darkmux-lab` and coder-phase's in
/// the binary, neither of which this crate may depend on. Those still
/// take the documented default, which is the safe direction (over-claiming
/// a session that never appears costs nothing; under-claiming one that
/// does produces a phantom run on the board).
fn attribution_registry() -> StepKindRegistry {
    let registry = StepKindRegistry::with_builtins();
    // Best-effort: a duplicate-id error here is a programming bug in the
    // registrars, not a reason to fail a read-only board query.
    let _ = darkmux_crew::step_kinds::register_records_gather_kind(&registry);
    let _ = darkmux_crew::step_kinds::register_mods_gate_kind(&registry);
    let _ = darkmux_crew::step_kinds::register_deliver_kind(&registry);
    registry
}

/// A `Step`'s dispatch session_id — **asked of the kind, never re-derived
/// here** (#1979).
///
/// This used to be a `match step.kind.as_str()` with a `_ => None` arm, so
/// the convention lived in two files that nothing kept agreeing.
///
/// **Honest scope of the defect** (narrowed by the #1979 QA gate, which
/// could not reproduce the original claim): an unlisted kind's session went
/// unclaimed by [`collect_mission_step_sessions`], but that alone does NOT
/// produce a ghost row. [`ghost_runs`] has a SECOND gate —
/// `known_mission_ids.contains(agg.mission_id)` — and every production
/// `run_step_graph` caller backfills `mission_id` (#1641), so a
/// locally-launched mission's records were already suppressed by mission id.
/// This is therefore defense-in-depth and a de-duplication of the
/// convention, not a fix for a reproducible doubled row. Asking the kind is
/// still right: two files encoding one convention with a silent catch-all is
/// how the next emitter that skips the `mission_id` backfill becomes a bug
/// nobody notices.
///
/// A kind not in the registry (a Tier 3 kind registered per-launch, e.g.
/// review's or coder-phase's) falls back to the trait's own default rather
/// than to `None`, so it is claimed by construction. That is the opposite
/// of the old catch-all: an unknown kind is now assumed to dispatch under
/// the documented default, not assumed to be invisible.
fn step_session_id(step: &Step, registry: &StepKindRegistry) -> Option<String> {
    match registry.get(&step.kind) {
        Ok(kind) => kind.dispatch_session_id(step),
        // Not a built-in. Reproduce the trait default rather than dropping
        // the step: explicit config wins, else the step-scoped default.
        Err(_) => {
            if let Some(sid) = step.config.get("session_id").and_then(|v| v.as_str()) {
                if !sid.is_empty() {
                    return Some(sid.to_string());
                }
            }
            Some(darkmux_types::session_id::step(&step.id))
        }
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

    // (#2487) Filtered to unambiguous sessions BEFORE picking `representative`
    // — the SAME `is_ambiguous()` guard `sessions_by_start` below already
    // applies to role/model, moved up front instead of applied only to that
    // copy. `representative` feeds `machine`, `start_ts_str` and (via
    // `mission.started_ts`'s fallback) this row's SORT position, so leaving
    // it unfiltered let an ambiguous session's machine/start win here while
    // the identical session was already refused as a source for role/model a
    // few lines down — one row mixing filtered and unfiltered provenance.
    // There is no principled reason machine/start_ts are less contaminated
    // than role/model: all four come from the exact same shared record
    // bucket, so the #1918 corruption taints ordering exactly as much as
    // attribution. `session_id` (below) no longer needs its own separate
    // filter as a result — `representative` already guarantees it.
    //
    // This pool is the single source for EVERY attribute and time on the
    // row: `representative` (machine, start), `remote` (route, #2558),
    // `sessions_by_start` (role, model), `terminal_ts_str` (completion, and
    // via `updated_ts` the row's first sort key) and `sessions_bare`
    // (status). Half-filtering was worse than not filtering: an
    // all-ambiguous row whose start came from the filtered pool and whose
    // terminal and endpoint did not rendered "completed 09:00, via
    // tainted-endpoint, no start, no machine, no role" — internally
    // contradictory, where the pre-fix row was at least wrong-but-consistent
    // from one source. Add a new field here and it reads this pool too.
    let unambiguous_sessions: Vec<(&str, &SessionAgg)> =
        sessions.iter().copied().filter(|(_, s)| !s.is_ambiguous()).collect();
    let representative = earliest_by_start(&unambiguous_sessions);
    // TODO(step-4): a mission whose dispatches span MULTIPLE distinct
    // endpoints (mixed local/remote seats across phases) collapses to one
    // representative endpoint here — the Runs lens can't yet show per-seat
    // routing. Picking the first remote session is a reasonable
    // single-value summary for a flat row; don't overbuild this for a
    // view-model step 4 will replace with a richer render.
    //
    // (#2558, folded into #2487) Drawn from `unambiguous_sessions` for the
    // same reason every other attribute here is: `endpoint` sits on the
    // shared bucket's records exactly like `machine_id`, so a session
    // spanning N missions hands the same endpoint to all of them. Leaving
    // this one field unfiltered made an all-ambiguous row VISIBLY
    // inconsistent rather than merely wrong — `runSubtitle`
    // (ui/src/lenses/runs/format.ts) concatenates role · model · route ·
    // machine, so the row rendered `via <endpoint>` and nothing else: the
    // one attribute still sourced from the pool every sibling had refused.
    let remote = earliest_by_start(
        &unambiguous_sessions
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
    //
    // (#1918 QA, widened by #2487) Built from `unambiguous_sessions` (above)
    // rather than re-filtering `sessions` here — `representative` and this
    // list now draw from the SAME ambiguity-filtered pool, by the SAME
    // detector `sessions_bare` (status, #1979) already uses, so role/model
    // and machine/start_ts can no longer disagree about which sessions are
    // trustworthy. The write-side scoping alone does not close this for a
    // MIXED day file — the state every operator has for
    // `RUNS_FLOW_SCAN_WINDOW_DAYS` after upgrading. A pre-1.43.0 record
    // set still carries the bare `task-<id>`/`step-<id>` bucket that N
    // missions shared; a NEW mission's structural prediction
    // (`collect_mission_step_sessions`, which by design still predicts the
    // unscoped form) claims that bucket, and because the legacy records
    // are OLDER they sort FIRST here and would win every attribute over the
    // new mission's own correctly-scoped session — role and model included,
    // and (before #2487) machine and start_ts too. Proved: a new mission
    // rendered `model: legacy-model-a` / `role: legacy-role` beside its own
    // `correct-model` records. Membership (`sessions`, unfiltered) still
    // keeps these aggs — claiming the session suppresses a ghost row; only
    // ATTRIBUTION and ORDERING are narrowed, mirroring `sessions_bare`.
    let mut sessions_by_start: Vec<&SessionAgg> =
        unambiguous_sessions.iter().map(|(_, s)| *s).filter(|s| s.start_ts.is_some()).collect();
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
    // never the one blanking this field the way it blanks role/model. But
    // "representative-only" no longer means "unfiltered": `representative`
    // (above) is now drawn from `unambiguous_sessions`, so an ambiguous
    // session can no longer win `machine` even though it auto-stamps a
    // value — the #2487 fix (see `unambiguous_sessions`'s own comment).
    //
    // (#1810) `mission.machine` — stamped durably at mint time — wins FIRST.
    // Before this, `machine` was ENTIRELY flow-derived, so a mission whose
    // dispatches all predate `RUNS_FLOW_SCAN_WINDOW_DAYS` lost this field
    // even though the mission record and the flow day-file holding the fact
    // were both still fully intact on disk — the durable record's
    // EXISTENCE survived the window; this one ATTRIBUTE on it did not. The
    // flow-derived fallback stays for missions minted before this field
    // existed (or where `resolve_machine_id()` had nothing to stamp).
    let machine = mission
        .machine
        .clone()
        .or_else(|| representative.and_then(|(_, s)| s.machine.clone()));
    let route = remote.and_then(|(_, s)| s.endpoint.clone());
    let start_ts_str = representative.and_then(|(_, s)| s.start_ts.clone());
    // (#2487) Filtered too — and this one is the load-bearing half of the
    // ordering claim, not a tidying pass. `terminal_ts_str` feeds
    // `completed_ts`, which feeds `updated_ts`, which is the FIRST sort key
    // in both the viewer and the CLI (start time is only the third). So an
    // unfiltered maximum here would set an all-ambiguous row's sort position
    // outright, while `representative` — the field this commit filtered to
    // fix ordering — contributes only the third-place tiebreak. Worse, a
    // filtered start beside an unfiltered terminal renders a row that
    // contradicts itself (`completed 09:00, no start, no machine, no role`),
    // where the pre-fix row was at least wrong-but-consistent from one
    // source. Same pool, same detector, so every time field on the row now
    // comes from the same trustworthy candidate set.
    let terminal_ts_str = unambiguous_sessions.iter().filter_map(|(_, s)| s.terminal_ts.clone()).max();
    // (#1915) The drill target — see `Run::session_id`'s own doc for why
    // this is populated for every mission row, tracked or not.
    //
    // (#1918, simplified by #2487) No separate ambiguity filter needed here
    // any more — `representative` above already only ever holds an
    // unambiguous session, so its id is always a valid drill target. Kept
    // as a plain map rather than re-deriving the guard a second time, which
    // is exactly the two-implementations-that-could-disagree risk
    // `earliest_by_start`'s own doc warns against.
    let session_id = representative.map(|(sid, _)| sid.to_string());

    let started_ts = mission
        .started_ts
        .or_else(|| start_ts_str.as_deref().and_then(parse_flow_ts));
    let completed_ts = mission
        .finalized_ts
        .or_else(|| terminal_ts_str.as_deref().and_then(parse_flow_ts));

    // (#1979 QA gate) STATUS is computed only over sessions this mission can
    // legitimately claim. A `session_id` whose records span more than one
    // mission is corrupt for this purpose — `session_id::task` hashes only
    // `task_id`, which comes straight out of a mission CONFIG
    // (`crew::scheduler`'s own doc), so every run of `coder-phase.json`
    // shares `task-build-coder`. Feeding such an agg into
    // `mission_run_status` lets one mission read Running off ANOTHER
    // mission's activity clock: the agg is permanently non-terminal (its
    // records are `step start`/`step complete`, which
    // `terminal_status_for_action` maps to `None`), so it both disables the
    // all-terminal branch and keeps `session_is_live` true. `is_ambiguous`
    // is the detector that already exists for exactly this corruption.
    // Membership (`sessions`) deliberately keeps them — claiming the
    // session still suppresses a ghost row; only STATUS is narrowed.
    //
    // (#2487) Mapped off `unambiguous_sessions` rather than re-running the
    // same `!is_ambiguous()` filter 130 lines below where it was already
    // computed. The reason is the one `session_id`'s own doc gives above:
    // two separately-written implementations of one guard can drift apart,
    // and a drift here would silently split STATUS off from the attribute
    // and ordering fields that are supposed to describe the same sessions.
    let sessions_bare: Vec<&SessionAgg> =
        unambiguous_sessions.iter().map(|(_, s)| *s).collect();
    let status = mission_run_status(mission, &sessions_bare, now_ms);
    // (#1907) `mission_run_status` has exactly ONE arm that reaches
    // `Abandoned` via a deliberate teardown — `MissionStatus::Aborted =>
    // RunStatus::Abandoned` — every other `Abandoned` result it returns
    // (the staleness gate, or a bubbled-up session `terminal_status ==
    // Abandoned`) means "no terminal record ever landed", never an abort.
    // `mission.status` is the mission's own OWN lifecycle field, known here
    // independent of the match `mission_run_status` already ran, so this
    // reads it directly rather than re-deriving the same decision from
    // `status` (which can no longer tell the two apart once collapsed).
    let abandoned_reason = if status == RunStatus::Abandoned {
        Some(if mission.status == MissionStatus::Aborted { AbandonReason::Aborted } else { AbandonReason::NoTerminal })
    } else {
        None
    };
    Run {
        id: mission.id.clone(),
        kind,
        status,
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
        abandoned_reason,
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
/// finalization's write) is no longer a single "genuinely no data, degrade
/// to Complete" case — see the `Ok(None)` arm's own doc (#1564) for why it
/// now checks the mission's actual on-disk phases before guessing.
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
            // (#1564) No envelope at all — not a parse failure (that's the
            // `Err` arm above), but no longer a SINGLE "genuinely no data"
            // case either. Two genuinely different situations reach here
            // with no `envelope.json` on disk:
            //
            // - `mission finalize` (`src/coder_phase.rs`'s whole-mission
            //   success verb, and pre-#1284 records generally) drives EVERY
            //   phase to `Complete` before the mission ever reaches
            //   `Finalized`, and never writes an envelope — an intentional
            //   gap, documented at `finalize_mission_if_complete`'s own doc.
            // - `reconcile_mint_failure` (`crates/darkmux-crew/src/
            //   lifecycle.rs`, the ONLY other production path that leaves a
            //   `Finalized` mission with no envelope) is a `mission launch`
            //   MINT failure backstop: it closes straight to `Finalized` —
            //   the SUCCESS terminal — after force-abandoning every phase
            //   it managed to mint (`mission_close_with_reasoning` ->
            //   `reconcile_mission_phases_terminal`). Before this fix, that
            //   collapsed onto the exact same `Complete` the happy path
            //   above gets — a mission that abandoned every phase read
            //   identically to one that did nothing wrong, #1564's own
            //   conflation, one layer under #2406's phase-level rollup.
            //
            // Neither case has an envelope to consult, but both leave a
            // real signal on disk: `reconcile_mission_phases_terminal`
            // guarantees every phase is terminal (`Complete`/`Abandoned`)
            // by the time a mission reaches `Finalized` through either
            // path, so "did at least one phase actually complete" tells
            // them apart without guessing. No phases at all stays
            // `Complete`, unchanged — a genuinely dataless mint/dispatch-
            // shape mission, the same "no data, don't invent a verdict"
            // reasoning this arm has always applied.
            Ok(None) => {
                let phases: Vec<Phase> = darkmux_crew::loader::load_phases()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|p| p.mission_id == mission.id)
                    .collect();
                if phases.is_empty() || phases.iter().any(|p| p.status == PhaseStatus::Complete) {
                    RunStatus::Complete
                } else {
                    RunStatus::Abandoned
                }
            }
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
    let status = lab_run_status(summary, now_ms);
    // (#1907, corrected #2462/#1946) `lab_run_status` used to have no abort
    // concept at all — every `Abandoned` arm was the staleness gate (the
    // run's artifact trail went quiet past the budget with no
    // `scores.json` ever written), so `NoTerminal` was always the honest
    // read. That stopped being true the moment `lab_run_status` grew a
    // `Some(Lc::Interrupted) => RunStatus::Abandoned` arm: a lifecycle
    // record written by `finish_interrupted` DID reach a terminal write —
    // reading it as `NoTerminal` renders "no ending recorded" for a run
    // whose ending IS recorded, the exact self-contradiction #1946 named.
    //
    // (#2462 review) But the STATUS alone does not license `Aborted`, whose
    // own doc reads "a human explicitly tore the run down" and which the
    // viewer renders as the literal word "aborted". `Interrupted` has TWO
    // writers, and only one of them knows a human was involved:
    //
    // * `finish_interrupted(err)` — a caught SIGINT/SIGTERM/SIGHUP, always
    //   `error: Some(..)`. A human tearing the run down is exactly what
    //   this means.
    // * `RunLifecycle::drop` — the #1930 RAII backstop, firing on any early
    //   return, `?`, or unwinding panic, always `error: None`. Inside
    //   `lab_run` alone that includes `cow_clone_dir_excluding(..)?` and
    //   `fs::create_dir_all(..)?` (ENOSPC, EPERM, an unsupported
    //   filesystem) before the provider is ever called, plus any panic a
    //   provider unwinds with — `with_provider` does not catch unwind.
    //   Nothing here implies intent.
    //
    // Gating on `lifecycle_error` keeps `NoTerminal` for the second — the
    // honest "darkmux does not know how this ended" — instead of
    // manufacturing a claim about operator intent. This is also why the
    // whole `interrupted` backlog on disk today (every one of which
    // predates `finish_interrupted` and therefore came from `Drop`) keeps
    // reading "no ending recorded" rather than being retroactively
    // relabeled as somebody's deliberate teardown.
    let interrupted_by_signal = summary.lifecycle_status
        == Some(darkmux_lab::lab::lifecycle::LifecycleStatus::Interrupted)
        && summary.lifecycle_error.is_some();
    let abandoned_reason = (status == RunStatus::Abandoned)
        .then_some(if interrupted_by_signal { AbandonReason::Aborted } else { AbandonReason::NoTerminal });
    Run {
        id: summary.dir.clone(),
        kind: RunKind::Lab,
        status,
        machine,
        route,
        role,
        model,
        // `LabRunSummary` carries no run-START timestamp today (only the
        // newest-artifact `mtime_ms`) — leaving `started_ts` absent is
        // honest; a wrong guess (e.g. mtime as start) would be worse than
        // no value. `mtime_ms` becomes `completed_ts` once the run reached
        // its terminal artifact write (`scores.json`).
        //
        // (#2462 review) One narrow window can still produce the
        // self-contradicting row #1946 named — `status: Abandoned` with a
        // `completed_ts` populated — and it is worth naming rather than
        // calling unreachable. `finished` means "scores.json exists", and
        // `tool_bench.rs`'s `run` writes `scores.json` and then performs
        // TWO more fallible operations in the same call
        // (`serde_json::to_string_pretty(..)?` and the `manifest.json`
        // `fs::write(..)?`). An I/O failure THERE, with the interrupt flag
        // already set, returns an `Err` that `lab_run` archives as
        // `Interrupted` — so the row reads Abandoned while carrying the
        // completion timestamp its own `scores.json` earned. It needs a
        // signal AND an I/O failure inside those few lines, so it is rare,
        // not impossible. Inside `lab_run` itself no such window exists:
        // its own fallible steps (`cow_clone_dir_excluding`,
        // `create_dir_all`) all run BEFORE the provider, when no terminal
        // artifact can exist yet.
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
        // (#1915, corrected #1982) NOT because a lab run has no flow session
        // — the original comment here claimed exactly that, and #1982
        // disproves it: a completed `coding-task`/`prompt` run DOES have one
        // (`summary.session_id`), and `build_runs` above now reads it to
        // claim the run's own dispatch bookends.
        //
        // It stays `None` because populating it would change NOTHING that is
        // rendered. `runDestination` (`ui/src/lenses/runs/format.ts`)
        // short-circuits every `kind === "lab"` row to the in-page
        // `LabRunDetail` on its FIRST line, before it ever looks at
        // `session_id`; and `LabRunDetail` takes only a `dir`, renders no
        // `#dispatch=` link, and has no other route to a session replay.
        //
        // The honest consequence, recorded rather than papered over: the
        // runs lens currently offers NO door to a lab run's session replay.
        // Those records are still reachable — the fleet lens's activity
        // timeline (`FleetLens.tsx`) navigates to `#dispatch=<sid>` — but
        // restoring the drill-in from HERE is a `LabRunDetail` change
        // (carry the session in, render a link) plus a `runDestination`
        // decision about which of the two destinations wins, not a one-line
        // field assignment.
        session_id: None,
        abandoned_reason,
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
    // (#1930) The run's OWN terminal record wins over every inference below.
    // `finished` only ever meant "scores.json exists", so a run that ERRORED
    // never set it and fell through to the idle heuristic — reporting
    // `Running` while fresh and `Abandoned` once stale, neither of which is
    // what happened. A run that says how it ended is not something to guess at.
    //
    // `Running` and `Unknown` deliberately fall THROUGH to the staleness check
    // below: the first may be a hard-killed run whose `Drop` never ran (the one
    // gap RAII cannot close), and the second must never be read as a verdict.
    use darkmux_lab::lab::lifecycle::LifecycleStatus as Lc;
    match summary.lifecycle_status {
        Some(Lc::Complete) => {
            return if summary.degenerate { RunStatus::Error } else { RunStatus::Complete };
        }
        Some(Lc::Error) => return RunStatus::Error,
        Some(Lc::Interrupted) => return RunStatus::Abandoned,
        _ => {}
    }
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
pub(crate) fn stale_after_ms() -> u64 {
    darkmux_types::config_access::inactivity_timeout_seconds().saturating_mul(2_000)
}

/// (#2413) Best-effort, LOCAL-ONLY: is at least one dispatch bookend-open
/// (a `dispatch start` with no matching `dispatch complete`/`error` yet,
/// within [`stale_after_ms`] of `now_ms`) on THIS machine? Feeds the
/// daemon host sampler's live-vs-idle emission cadence
/// (`host_sampler::spawn`) — reusing the SAME dispatch-bookend vocabulary
/// (`darkmux_flow::is_dispatch_start`/`is_dispatch_terminal`) and the SAME
/// staleness budget [`session_is_live`] already judges run liveness by,
/// rather than inventing a second liveness definition.
///
/// **What this can see:** any dispatch whose start bookend landed in
/// TODAY's local JSONL file (`darkmux_types::config_access::flows_dir()`)
/// without a terminal yet, judged against the timestamp of its own most
/// recent record (any action) — so a genuinely long-running dispatch that
/// keeps emitting stays "live" past `stale_after_ms` from its START, the
/// same way `session_is_live` judges last ACTIVITY rather than start age.
///
/// **What this cannot see:** a dispatch that crossed the UTC midnight
/// boundary mid-run (its start bookend is in YESTERDAY's file — reading
/// only today's file is a deliberate cost bound, not an oversight: this
/// runs on every sampler tick, so it stays a single small file read rather
/// than a multi-day scan); dispatches on OTHER machines (correct on
/// purpose — sampler cadence is a PER-MACHINE decision, so a busy fleet
/// peer must not speed up an idle machine's own sampler); a dispatch whose
/// records land ONLY in a non-local sink (theoretical: `LocalFileSink` is
/// always-on regardless of what else is configured).
pub(crate) fn any_dispatch_live_locally(now_ms: u64, max_age_ms: u64) -> bool {
    any_dispatch_live_in(&darkmux_types::config_access::flows_dir(), &darkmux_flow::day_utc_now(), now_ms, max_age_ms)
}

fn any_dispatch_live_in(dir: &std::path::Path, day: &str, now_ms: u64, max_age_ms: u64) -> bool {
    let path = dir.join(format!("{day}.jsonl"));
    let Ok(text) = std::fs::read_to_string(&path) else { return false };
    let mut last_activity_ms: HashMap<String, u64> = HashMap::new();
    // (self-QA catch, #2413) A session is "open" ONLY from its OWN
    // `dispatch start` bookend through its OWN `dispatch complete`/`error`
    // — never inferred from mere PRESENCE in the file. The first cut of
    // this function tracked `last_activity_ms` for every record carrying a
    // `session_id`, live-checking "not yet seen a terminal" — which
    // wrongly read a `mission close` record's `mission-<id>` session (a
    // mission session is never `dispatch`-terminated at all) as an
    // eternally-open dispatch, always live. Red-proved against a fixture
    // with only `mission start`/`mission close` records: the buggy
    // version returned `true`; this one correctly returns `false`.
    let mut open: HashSet<String> = HashSet::new();
    for line in text.lines() {
        let Ok(rec) = serde_json::from_str::<darkmux_flow::FlowRecord>(line) else { continue };
        let Some(sid) = rec.session_id.clone() else { continue };
        let Some(ts_secs) = parse_flow_ts(&rec.ts) else { continue };
        let ts_ms = ts_secs.saturating_mul(1000);
        if darkmux_flow::is_dispatch_start(&rec.action) {
            open.insert(sid.clone());
            last_activity_ms.insert(sid, ts_ms);
        } else if darkmux_flow::is_dispatch_terminal(&rec.action) {
            open.remove(&sid);
        } else if open.contains(&sid) {
            // Any OTHER record for an ALREADY-open dispatch session is
            // proof of work — bumps its last-activity, same convention
            // `session_is_live`'s `last_activity_ts` uses.
            last_activity_ms.insert(sid, ts_ms);
        }
    }
    open.into_iter()
        .any(|sid| last_activity_ms.get(&sid).is_some_and(|ts| now_ms.saturating_sub(*ts) <= max_age_ms))
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
///
/// Tracked runs (missions, lab runs) are unaffected in EXISTENCE — they're
/// durable records, listed and readable in full regardless of age. They
/// are NOT unaffected in ATTRIBUTION (#1810 — an earlier version of this
/// comment claimed `route`, `role` AND `model` were all windowed; that was
/// also wrong, just wrong in a different direction). `route` and `model`
/// really are resolved ONLY by joining a mission to its flow SESSIONS
/// (`mission_to_run`), and that join is exactly as windowed as ghost
/// synthesis is — a mission whose dispatches all predate this window
/// loses both fields even though the mission record and the flow
/// day-file holding the fact are both still fully intact on disk. `role`
/// is different: for a Dispatch-kind mission (a crew-of-one — see
/// `classify_mission`), `mission_to_run` prefers the STRUCTURAL
/// `Task.role_id` — the operator's requested role, on disk on the Task
/// itself, independent of flow retention — over the flow-derived value,
/// so role survives this window for the majority of rows. It stays
/// flow-derived (and therefore windowed) only for a Mission-kind run,
/// where a single mission can span many steps and therefore many
/// distinct roles with no one durable value to prefer. `machine` is the
/// other exception: since #1810 it is stamped durably on the mission
/// record itself at creation (`Mission::machine`), so it survives the
/// window; `lab_summary_to_run` was never affected either way, because it
/// reads the daemon's own `machine_id` directly rather than deriving it
/// from flow. A discoverable knob (a named const, not a magic number
/// scattered inline) rather than adaptive-silent, per CLAUDE.md's
/// "cadence is a recorded knob" observability doctrine.
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
    /// flow index's key and appears in `#dispatch=<id>` deep links — a
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
    // (silent-miss audit, 2026-09-06) Was four hand-spelled literal
    // comparisons alongside the start check — every one a spot that could
    // silently drift from `darkmux-flow`'s own bookend vocabulary (see
    // `CLAUDE.md`'s "Dispatch liveness" contract), the same drift risk
    // `mission_graph.rs`'s `is_complete` line already had before this
    // audit. `is_dispatch_terminal` already covers BOTH spellings of
    // BOTH complete and error.
    is_dispatch_start_action(action) || darkmux_flow::is_dispatch_terminal(action)
}

/// The `RunStatus` a session's TERMINAL flow action implies — `None` for
/// any non-terminal action (turns, tools, telemetry, the start itself).
fn terminal_status_for_action(action: &str) -> Option<RunStatus> {
    // (silent-miss audit, 2026-09-06) Hand-spelled literals replaced with
    // the shared `darkmux_flow` matchers — see `is_dispatch_lifecycle_
    // action`'s own comment just above.
    if darkmux_flow::is_dispatch_complete(action) {
        return Some(RunStatus::Complete);
    }
    if darkmux_flow::is_dispatch_error(action) {
        return Some(RunStatus::Error);
    }
    match action {
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
        // A terminal status always wins over the liveness gate; only a
        // session with NO terminal yet falls through to it.
        let status = agg.terminal_status.unwrap_or_else(|| {
            if session_is_live(agg, now_ms) {
                RunStatus::Running
            } else {
                RunStatus::Abandoned
            }
        });
        // (#1907) There is no per-dispatch abort action — `mission abort`
        // is mission-scoped, and a standalone session's only terminals are
        // `dispatch complete`/`dispatch error`/the presence reconciler's
        // `session.end` crash-close-edge (see `terminal_status_for_action`'s
        // own doc) — so an Abandoned ghost always means "no ending
        // recorded", whether it came from `session.end` or the staleness
        // gate above.
        let abandoned_reason = (status == RunStatus::Abandoned).then_some(AbandonReason::NoTerminal);
        out.push(Run {
            id: session_id.clone(),
            kind: RunKind::Dispatch,
            status,
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
            abandoned_reason,
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
pub(crate) fn parse_flow_ts(ts: &str) -> Option<u64> {
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
/// the Unix epoch (same Howard Hinnant algorithm, public domain). Used by
/// [`cutoff_date_string`] to format the scan-window boundary as a
/// `YYYY-MM-DD` day-file-name prefix, and by [`day_string_from_epoch_ms`]
/// below for the same purpose from an arbitrary record timestamp.
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

/// `YYYY-MM-DD` (UTC) day-file-name for an epoch-milliseconds timestamp —
/// the same [`civil_from_days`] calendar math [`cutoff_date_string`] uses,
/// exposed `pub(crate)` so `lib.rs`'s bounded day-range walker
/// (`for_each_flow_record_in_day_range`) can compute a run's own
/// `[start_ms, end_ms]` window as a day-file-name range without
/// re-deriving the calendar algorithm.
pub(crate) fn day_string_from_epoch_ms(ms: u64) -> String {
    let days = (ms / 1000 / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// (#2413 M4) Whether a raw flow record `v` is a `machine.telemetry`
/// sample that belongs to a run's SYSTEM pane: same `machine_uid`,
/// timestamped inside `[start_ms, end_ms]`. M3 retired the per-dispatch
/// `telemetry.process` producer these CPU/RAM/GPU tiles used to read
/// directly (a record carrying the run's own `session_id`) — the
/// machine-scoped replacement carries no `session_id` at all, so a
/// consumer must join it in BY TIME instead. Pure over one record + the
/// window bounds so it's testable without exercising the day-file walk
/// `join_host_samples_into_session_records` (`lib.rs`) wraps this in.
pub(crate) fn is_host_sample_in_window(v: &serde_json::Value, machine_uid: &str, start_ms: u64, end_ms: u64) -> bool {
    if v.get("action").and_then(|a| a.as_str()) != Some("machine.telemetry") {
        return false;
    }
    if v.get("machine_uid").and_then(|m| m.as_str()) != Some(machine_uid) {
        return false;
    }
    let Some(ts_ms) = v
        .get("ts")
        .and_then(|t| t.as_str())
        .and_then(parse_flow_ts)
        .map(|secs| secs.saturating_mul(1000))
    else {
        return false;
    };
    ts_ms >= start_ms && ts_ms <= end_ms
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_crew::envelope::{MissionEnvelope, MissionOutcomeStatus};
    use darkmux_crew::types::{MissionSpec, NodeStatus, PhaseStatus};
    use std::io::Write;
    use tempfile::TempDir;

    // ── literal-bookend tripwire (silent-miss audit, 2026-09-06) ────────

    /// Strips every `#[cfg(test)]`-gated item out of `src` before the
    /// tripwire below scans for hand-spelled bookend literals — test
    /// fixtures legitimately construct raw JSON records naming BOTH
    /// spellings on purpose (that is the whole point of a dual-spelling
    /// fixture), and those must not trip a check aimed at PRODUCTION code
    /// re-inventing `darkmux_flow`'s matchers. From the line AFTER each
    /// `#[cfg(test)]` attribute, brace-depth-tracks forward through the
    /// item it gates and drops every line up to and including whichever
    /// closes first: the matching close brace (`mod tests { ... }`, a
    /// `fn ... { ... }`), OR — round-2 audit, 2026-09-06, C2 — a BARE
    /// `;`-terminated item with no braces at all (`mod wire_fixtures;`,
    /// a `#[path = "..."] mod tests;` pair). Before this fix, a
    /// brace-less gated item made the scan keep hunting forward for the
    /// NEXT `{` in the file — which is ordinary PRODUCTION code's own
    /// brace, not this item's — silently swallowing every line in
    /// between out of the scan. `lib.rs`'s own `#[cfg(test)] mod
    /// wire_fixtures;` (no body — the module lives in a separate file)
    /// hit exactly this: the stripper used to hunt past it for the next
    /// `{`, dropping a real stretch of production code from the
    /// tripwire's coverage with nothing ever reporting it missing.
    fn strip_cfg_test_regions(src: &str) -> String {
        let lines: Vec<&str> = src.lines().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < lines.len() {
            if lines[i].trim_start().starts_with("#[cfg(test)]") {
                let mut depth: i32 = 0;
                let mut opened = false;
                // Scanning starts at the line AFTER the attribute itself
                // — the attribute line can never open a brace or end the
                // item, and starting here (rather than at `i`) is what
                // lets a lone semicolon on the very next line close a
                // brace-less item without first needing a `{` anywhere.
                let mut j = i + 1;
                while j < lines.len() {
                    let line = lines[j];
                    for ch in line.chars() {
                        match ch {
                            '{' => {
                                depth += 1;
                                opened = true;
                            }
                            '}' => depth -= 1,
                            _ => {}
                        }
                    }
                    // A line that never opened a brace and ends the
                    // statement with `;` is a COMPLETE bare item on its
                    // own — `mod wire_fixtures;`, or (when this is the
                    // line right after a stacked `#[path = "..."]`
                    // attribute) `mod tests;`. Stop right there instead
                    // of continuing to hunt for a `{` that may be pages
                    // away, in code this item has nothing to do with.
                    let bare_semicolon_close = !opened && line.trim_end().ends_with(';');
                    j += 1;
                    if opened && depth <= 0 {
                        break;
                    }
                    if bare_semicolon_close {
                        break;
                    }
                }
                i = j;
                continue;
            }
            out.push_str(lines[i]);
            out.push('\n');
            i += 1;
        }
        out
    }

    /// (round-2 audit, 2026-09-06 — C2) A brace-less `#[cfg(test)] mod x;`
    /// item — `lib.rs`'s real `#[cfg(test)] mod wire_fixtures;` shape —
    /// must strip ONLY itself, never hunt forward for the next `{`
    /// (which belongs to unrelated PRODUCTION code and would get
    /// silently swallowed along with it). Red-proved by reverting the
    /// scan-start back to `let mut j = i;` and dropping the
    /// `bare_semicolon_close` check (the pre-fix behavior): this test
    /// then fails because `"dispatch complete"` inside `fn after()`
    /// disappears from the stripped output along with everything between
    /// the two lines.
    #[test]
    fn strip_cfg_test_regions_stops_at_a_bare_semicolon_not_the_next_brace() {
        let src = "#[cfg(test)]\nmod wire_fixtures;\n\nfn after() {\n    let x = \"dispatch complete\";\n}\n";
        let stripped = strip_cfg_test_regions(src);
        assert!(!stripped.contains("wire_fixtures"), "the gated bare item itself must be stripped: {stripped:?}");
        assert!(
            stripped.contains("dispatch complete"),
            "production code AFTER the bare `;`-terminated gated item must survive stripping, \
             not be swallowed while hunting for a distant unrelated `{{`: {stripped:?}"
        );
    }

    /// The stacked-attribute form (`#[cfg(test)]` immediately followed by
    /// another attribute, THEN the bare `;`-terminated item) — `lib.rs`'s
    /// real `#[cfg(test)] #[path = "lib_tests.rs"] mod tests;` shape.
    #[test]
    fn strip_cfg_test_regions_handles_a_stacked_attribute_before_the_bare_semicolon() {
        let src = "#[cfg(test)]\n#[path = \"lib_tests.rs\"]\nmod tests;\n\nfn after() {\n    let x = \"dispatch start\";\n}\n";
        let stripped = strip_cfg_test_regions(src);
        assert!(!stripped.contains("lib_tests.rs"), "{stripped:?}");
        assert!(!stripped.contains("mod tests;"), "{stripped:?}");
        assert!(
            stripped.contains("dispatch start"),
            "production code after the stacked-attribute bare item must survive: {stripped:?}"
        );
    }

    /// The ORIGINAL brace-block form must keep working unchanged — a
    /// `#[cfg(test)] mod tests { ... }` body (including a nested literal
    /// that legitimately names both spellings on purpose) is stripped in
    /// full, and code after its closing brace survives.
    #[test]
    fn strip_cfg_test_regions_still_strips_a_full_brace_delimited_mod() {
        let src = "#[cfg(test)]\nmod tests {\n    fn t() {\n        let x = \"dispatch complete\";\n    }\n}\n\nfn after() {\n    let y = \"dispatch start\";\n}\n";
        let stripped = strip_cfg_test_regions(src);
        assert!(!stripped.contains("\"dispatch complete\""), "the test-gated body must be gone: {stripped:?}");
        assert!(
            stripped.contains("\"dispatch start\""),
            "code after the closing brace must survive: {stripped:?}"
        );
    }

    /// (silent-miss audit, 2026-09-06) `darkmux-flow`'s `is_dispatch_start`/
    /// `is_dispatch_complete`/`is_dispatch_error`/`is_dispatch_terminal`
    /// exist EXACTLY because a hand-spelled `action == "dispatch complete"
    /// || action == "dispatch.complete"` silently stops matching the
    /// instant either spelling drifts (a schema rename, a third spelling
    /// added upstream) with no error anywhere — `mission_graph.rs`'s
    /// `is_complete` line and two spots in this file (`runs.rs`'s
    /// `is_dispatch_lifecycle_action`/`terminal_status_for_action`) had
    /// exactly this shape until this audit fixed them. This test pins
    /// that fix by scanning every non-test `.rs` file in this crate's
    /// `src/` for the QUOTED LITERAL ANYWHERE in non-test source —
    /// comments included, deliberately not restricted to executable code
    /// — and failing on any (round-2 audit, 2026-09-06, C1: a re-spelled
    /// literal sitting in a comment is just as much a drift risk as one
    /// in a live `==` comparison — someone copies the comment's example
    /// into new code next).
    ///
    /// Six literals: three bookends (start/complete/error) × two
    /// spellings each (round-2 audit, 2026-09-06, M2 — the original list
    /// omitted `"dispatch error"`/`"dispatch.error"`, so restoring a
    /// hand-spelled error-bookend comparison stayed green here).
    ///
    /// Deliberately non-recursive (`src/` has no subdirectories today) and
    /// deliberately excludes `lib_tests.rs` and `wire_fixtures.rs` by name
    /// — both are ENTIRELY `#[cfg(test)]`-gated from their very first item
    /// (verified: `lib.rs`'s `#[cfg(test)] #[path = "lib_tests.rs"] mod
    /// tests;`, `wire_fixtures.rs`'s own leading `#[cfg(test)] mod tests`),
    /// so `strip_cfg_test_regions` alone already empties them — named here
    /// too so the intent doesn't rely solely on the stripper being
    /// correct.
    #[test]
    fn no_hand_spelled_dispatch_bookend_literals_outside_flow_helpers() {
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let literals = [
            "\"dispatch complete\"",
            "\"dispatch.complete\"",
            "\"dispatch start\"",
            "\"dispatch.start\"",
            "\"dispatch error\"",
            "\"dispatch.error\"",
        ];
        let mut offenders: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&src_dir).expect("reading darkmux-serve's own src dir") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if name == "lib_tests.rs" || name == "wire_fixtures.rs" {
                continue; // entirely test-gated — see this test's own doc.
            }
            let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
            let production_only = strip_cfg_test_regions(&raw);
            for (lineno, line) in production_only.lines().enumerate() {
                for lit in literals {
                    if line.contains(lit) {
                        offenders.push(format!("{}:{}: {}", path.display(), lineno + 1, line.trim()));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "quoted dispatch-bookend literal found ANYWHERE in non-test source (comments \
             included) outside `darkmux_flow`'s matchers — use `is_dispatch_start`/\
             `is_dispatch_complete`/`is_dispatch_error`/`is_dispatch_terminal` instead, or, if \
             this really is prose naming the literal spelling on purpose, keep it out of a \
             quoted string:\n{}",
            offenders.join("\n")
        );
    }

    // ── parse_flow_ts / civil calendar round-trip ───────────────────────

    #[test]
    fn parse_flow_ts_epoch_zero() {
        assert_eq!(parse_flow_ts("1970-01-01T00:00:00Z"), Some(0));
    }

    // ── is_host_sample_in_window (#2413 M4) ─────────────────────────────

    fn host_sample(machine_uid: &str, ts: &str) -> serde_json::Value {
        serde_json::json!({ "action": "machine.telemetry", "machine_uid": machine_uid, "ts": ts })
    }

    #[test]
    fn is_host_sample_in_window_true_when_machine_and_time_match() {
        let v = host_sample("m-1", "1970-01-01T00:00:05Z");
        assert!(is_host_sample_in_window(&v, "m-1", 0, 10_000));
    }

    #[test]
    fn is_host_sample_in_window_false_for_a_different_machine() {
        let v = host_sample("m-2", "1970-01-01T00:00:05Z");
        assert!(!is_host_sample_in_window(&v, "m-1", 0, 10_000));
    }

    #[test]
    fn is_host_sample_in_window_false_outside_the_time_bounds() {
        let before = host_sample("m-1", "1970-01-01T00:00:00Z");
        let after = host_sample("m-1", "1970-01-01T00:00:20Z");
        assert!(!is_host_sample_in_window(&before, "m-1", 5_000, 10_000));
        assert!(!is_host_sample_in_window(&after, "m-1", 5_000, 10_000));
    }

    #[test]
    fn is_host_sample_in_window_false_for_a_non_telemetry_action() {
        let v = serde_json::json!({ "action": "dispatch.start", "machine_uid": "m-1", "ts": "1970-01-01T00:00:05Z" });
        assert!(!is_host_sample_in_window(&v, "m-1", 0, 10_000));
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

    // ─── (#2413) any_dispatch_live_in — the daemon sampler's cadence signal ─

    #[test]
    fn any_dispatch_live_is_false_with_no_flows_dir_at_all() {
        let tmp = TempDir::new().unwrap();
        // No day file written at all.
        assert!(!any_dispatch_live_in(tmp.path(), &today(), 10_000, 60_000));
    }

    #[test]
    fn any_dispatch_live_is_true_for_an_open_bookend_within_the_window() {
        let tmp = TempDir::new().unwrap();
        write_day_file(
            tmp.path(),
            &today(),
            &[serde_json::json!({"level": "info", "category": "work", "tier": "local", "stage": "dispatch", "handle": "h", "ts": "2026-01-01T00:00:00Z", "action": "dispatch start", "session_id": "s1"})],
        );
        let start_ms = parse_flow_ts("2026-01-01T00:00:00Z").unwrap() * 1000;
        assert!(any_dispatch_live_in(tmp.path(), &today(), start_ms + 5_000, 60_000));
    }

    #[test]
    fn any_dispatch_live_is_false_once_terminated() {
        let tmp = TempDir::new().unwrap();
        write_day_file(
            tmp.path(),
            &today(),
            &[
                serde_json::json!({"level": "info", "category": "work", "tier": "local", "stage": "dispatch", "handle": "h", "ts": "2026-01-01T00:00:00Z", "action": "dispatch start", "session_id": "s1"}),
                serde_json::json!({"level": "info", "category": "work", "tier": "local", "stage": "dispatch", "handle": "h", "ts": "2026-01-01T00:00:05Z", "action": "dispatch complete", "session_id": "s1"}),
            ],
        );
        let start_ms = parse_flow_ts("2026-01-01T00:00:00Z").unwrap() * 1000;
        assert!(
            !any_dispatch_live_in(tmp.path(), &today(), start_ms + 5_000, 60_000),
            "a terminated dispatch is not live even within the age window"
        );
    }

    #[test]
    fn any_dispatch_live_is_false_once_last_activity_ages_out() {
        let tmp = TempDir::new().unwrap();
        write_day_file(
            tmp.path(),
            &today(),
            &[serde_json::json!({"level": "info", "category": "work", "tier": "local", "stage": "dispatch", "handle": "h", "ts": "2026-01-01T00:00:00Z", "action": "dispatch start", "session_id": "s1"})],
        );
        let start_ms = parse_flow_ts("2026-01-01T00:00:00Z").unwrap() * 1000;
        assert!(
            !any_dispatch_live_in(tmp.path(), &today(), start_ms + 61_000, 60_000),
            "an open bookend whose last activity is past max_age_ms no longer counts as live"
        );
    }

    #[test]
    fn any_dispatch_live_ignores_a_different_session_that_terminated() {
        let tmp = TempDir::new().unwrap();
        write_day_file(
            tmp.path(),
            &today(),
            &[
                serde_json::json!({"level": "info", "category": "work", "tier": "local", "stage": "dispatch", "handle": "h", "ts": "2026-01-01T00:00:00Z", "action": "dispatch start", "session_id": "s1"}),
                serde_json::json!({"level": "info", "category": "work", "tier": "local", "stage": "dispatch", "handle": "h", "ts": "2026-01-01T00:00:01Z", "action": "dispatch complete", "session_id": "s1"}),
                serde_json::json!({"level": "info", "category": "work", "tier": "local", "stage": "dispatch", "handle": "h", "ts": "2026-01-01T00:00:02Z", "action": "dispatch start", "session_id": "s2"}),
            ],
        );
        let start_ms = parse_flow_ts("2026-01-01T00:00:02Z").unwrap() * 1000;
        assert!(
            any_dispatch_live_in(tmp.path(), &today(), start_ms + 1_000, 60_000),
            "s2 is still open even though s1 (a different session) terminated"
        );
    }

    /// (self-QA catch, #2413) Regression for the bug the daemon-suite
    /// integration tests caught live: a `mission start`/`mission close`
    /// pair (a MISSION session, never `dispatch`-terminated by
    /// definition) must NOT read as an open dispatch just because its
    /// session_id was recently active. The first cut of
    /// `any_dispatch_live_in` tracked last-activity for EVERY
    /// session-carrying record and checked "no terminal seen yet" — which
    /// made a mission's own session look permanently live.
    #[test]
    fn any_dispatch_live_is_false_for_a_mission_session_with_no_dispatch_bookend_at_all() {
        let tmp = TempDir::new().unwrap();
        write_day_file(
            tmp.path(),
            &today(),
            &[
                serde_json::json!({"level": "info", "category": "work", "tier": "operator", "stage": "scope", "handle": "h", "ts": "2026-01-01T00:00:00Z", "action": "mission start", "session_id": "mission-m1"}),
                serde_json::json!({"level": "info", "category": "work", "tier": "operator", "stage": "scope", "handle": "h", "ts": "2026-01-01T00:00:01Z", "action": "mission close", "session_id": "mission-m1"}),
            ],
        );
        let ts_ms = parse_flow_ts("2026-01-01T00:00:01Z").unwrap() * 1000;
        assert!(
            !any_dispatch_live_in(tmp.path(), &today(), ts_ms + 500, 60_000),
            "a mission session with no dispatch start/terminal bookend at all is never 'live' by this signal"
        );
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
            // (#1810) Intentionally None by default: existing tests exercise
            // the flow-derived fallback path (the behavior every mission
            // minted before this field existed still needs). Tests pinning
            // the NEW durable-machine behavior set `.machine` explicitly
            // after calling this helper.
            machine: None,
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
            run_on: darkmux_crew::types::default_run_on(),
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
        assert_eq!(step_session_id(&step, &StepKindRegistry::with_builtins()), Some("explicit-sid".to_string()));
    }

    #[test]
    fn step_session_id_defaults_dispatch_internal_to_the_step_scoped_session() {
        // (#1523 gate must-fix 2) `interpret::push_step` never injects a
        // session_id — this default is what `DispatchInternalStepKind::run`
        // itself falls back to when `config.session_id` is absent.
        let mut step = minimal_step("s-generic", "t1", None);
        step.kind = "dispatch.internal".to_string();
        assert_eq!(step_session_id(&step, &StepKindRegistry::with_builtins()), Some(darkmux_types::session_id::step("s-generic")));
    }

    #[test]
    fn step_session_id_defaults_dispatch_single_shot_to_the_task_scoped_session() {
        let mut step = minimal_step("s-single", "t-owner", None);
        step.kind = "dispatch.single_shot".to_string();
        assert_eq!(step_session_id(&step, &StepKindRegistry::with_builtins()), Some(darkmux_types::session_id::task("t-owner")));
    }

    #[test]
    fn step_session_id_procedural_kind_opts_out_explicitly() {
        // (#1979) Renamed from `..._unknown_kind_has_no_default`, which
        // conflated two different things under one `None`: a kind that
        // DECLARES it never dispatches, and a kind nobody taught the
        // resolver about. `procedural.noop` is the first — a registered
        // kind whose own `dispatch_session_id` returns `None` on purpose.
        // The second case is now the opposite behavior; see the next test.
        let mut step = minimal_step("s-proc", "t1", None);
        step.kind = "procedural.noop".to_string();
        assert_eq!(step_session_id(&step, &StepKindRegistry::with_builtins()), None);
    }

    /// (#2310 swarm F / S2-1) `records.gather` and `mods.gate` declare
    /// `dispatch_session_id -> None` — neither dispatches a model — and
    /// the run view's step→session attribution must HONOR that
    /// declaration, not fall through to the step-scoped default because
    /// the kind happens to be registered per-launch rather than in
    /// `with_builtins`. A session claimed for a step that emits no record
    /// is a claim on nothing; `deliver.github_review` is the third kind
    /// with the same contract and is pinned alongside them.
    ///
    /// Mutate any of those three kinds' `dispatch_session_id` to
    /// `Some(..)`, or drop its registration from `attribution_registry`,
    /// and this goes red.
    #[test]
    fn step_session_id_excludes_the_non_dispatching_review_kinds() {
        let registry = attribution_registry();
        for kind in ["records.gather", "mods.gate", "deliver.github_review"] {
            let mut step = minimal_step("s-nd", "t-nd", None);
            step.kind = kind.to_string();
            assert_eq!(
                step_session_id(&step, &registry),
                None,
                "`{kind}` never dispatches a model, so the run view must claim no session for it",
            );
        }
    }

    /// The other half: those three must actually BE in the attribution
    /// registry. Without this, the test above would still pass the day
    /// someone dropped a registration — an unregistered kind resolves
    /// through the trait default, which is `Some(..)`, so it would go red
    /// there; but if a kind's own override were ALSO deleted the two
    /// failures could cancel. Pin the registration itself.
    #[test]
    fn the_attribution_registry_knows_the_crew_side_tier3_kinds() {
        let registry = attribution_registry();
        for kind in ["records.gather", "mods.gate", "deliver.github_review"] {
            assert!(
                registry.get(kind).is_ok(),
                "`{kind}` must be registered here or its `None` declaration is never consulted",
            );
        }
        // And the builtins are still all present.
        assert!(registry.get("dispatch.internal").is_ok());
    }

    #[test]
    fn step_session_id_unregistered_kind_is_claimed_by_the_default_not_dropped() {
        // (#1979) THE behavior change. The old `match step.kind.as_str()`
        // ended in `_ => None`, so any kind it had not been taught about —
        // a Tier 3 kind registered per-launch, or simply a new one — had
        // its session left unclaimed by `collect_mission_step_sessions`,
        // and its records then surfaced as a duplicate untracked ghost row
        // (`ghost_runs`'s `known_session_ids` gate). Nothing failed until
        // an operator noticed the doubled row.
        //
        // An unregistered kind now falls back to the TRAIT DEFAULT, so it
        // is claimed by construction. Assumed-dispatching is the safe
        // direction: over-claiming a session that never appears costs
        // nothing, while under-claiming one that does produces a phantom
        // run on the board.
        let mut step = minimal_step("s-tier3", "t1", None);
        step.kind = "mission.coder".to_string();
        assert_eq!(
            step_session_id(&step, &StepKindRegistry::with_builtins()),
            Some(darkmux_types::session_id::step("s-tier3")),
        );
    }

    #[test]
    fn step_session_id_unregistered_kind_still_honors_an_explicit_config_session() {
        // The fallback must not swallow a caller-named session — the
        // launch-owned Tier-3 steps DO set one, and claiming the wrong id
        // would reintroduce the ghost this fixes from the other side.
        let mut step = minimal_step("s-tier3", "t1", Some("mission-run-m1-p1"));
        step.kind = "mission.coder".to_string();
        assert_eq!(
            step_session_id(&step, &StepKindRegistry::with_builtins()),
            Some("mission-run-m1-p1".to_string()),
        );
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

    /// (#1564) The mint-failure backstop's actual on-disk shape:
    /// `reconcile_mint_failure` (`crates/darkmux-crew/src/lifecycle.rs`)
    /// closes a mission straight to `Finalized` — the SUCCESS terminal —
    /// after force-abandoning every phase it managed to mint, and writes NO
    /// envelope at all. Before this fix, the `Ok(None)` arm's blanket
    /// `RunStatus::Complete` collapsed that onto the exact same status a
    /// genuinely successful envelope-less finalize gets: a mission that
    /// abandoned every phase read identically to one that did nothing
    /// wrong — the run-level shape of the conflation #1564 named, one
    /// layer under #2406's phase-level `Degraded` rollup.
    ///
    /// Mutating the fix's `phases.iter().any(|p| p.status ==
    /// PhaseStatus::Complete)` to always `true` (i.e. reverting to the old
    /// blanket `Complete`) must fail this test.
    #[test]
    #[serial_test::serial]
    fn mission_run_status_finalized_no_envelope_all_phases_abandoned_reads_abandoned() {
        let _g = CrewGuard::new();
        darkmux_crew::lifecycle::save_mission(&minimal_mission("m10", vec!["m10-p1".to_string()], None))
            .unwrap();
        let mut phase = minimal_phase("m10-p1", "m10", vec![]);
        phase.status = PhaseStatus::Abandoned;
        darkmux_crew::lifecycle::save_phase(&phase).unwrap();

        let mut m = minimal_mission("m10", vec!["m10-p1".to_string()], None);
        m.status = MissionStatus::Finalized;
        assert_eq!(
            mission_run_status(&m, &[], now_unix() * 1_000),
            RunStatus::Abandoned,
            "a Finalized mission with no envelope and every phase Abandoned must read Abandoned, \
             never the same Complete a genuinely successful envelope-less finalize gets"
        );
    }

    /// The inverted case, pinned alongside the one above so a fix that
    /// makes EVERY envelope-less Finalized mission read Abandoned (over-
    /// correcting #1564) is caught too. `mission finalize` (the whole-
    /// mission CLI success verb, `src/coder_phase.rs`) drives every phase
    /// to `Complete` before the mission reaches `Finalized`, envelope-less
    /// by design (`finalize_mission_if_complete`'s own doc) — that
    /// genuinely successful case must keep reading `Complete`.
    #[test]
    #[serial_test::serial]
    fn mission_run_status_finalized_no_envelope_a_completed_phase_still_reads_complete() {
        let _g = CrewGuard::new();
        darkmux_crew::lifecycle::save_mission(&minimal_mission(
            "m11",
            vec!["m11-p1".to_string(), "m11-p2".to_string()],
            None,
        ))
        .unwrap();
        let mut done = minimal_phase("m11-p1", "m11", vec![]);
        done.status = PhaseStatus::Complete;
        darkmux_crew::lifecycle::save_phase(&done).unwrap();
        let mut abandoned = minimal_phase("m11-p2", "m11", vec![]);
        abandoned.status = PhaseStatus::Abandoned;
        darkmux_crew::lifecycle::save_phase(&abandoned).unwrap();

        let mut m = minimal_mission(
            "m11",
            vec!["m11-p1".to_string(), "m11-p2".to_string()],
            None,
        );
        m.status = MissionStatus::Finalized;
        assert_eq!(
            mission_run_status(&m, &[], now_unix() * 1_000),
            RunStatus::Complete,
            "at least one genuinely completed phase must still read Complete, even with no envelope"
        );
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
        lab_summary_with_lifecycle(dir, finished, degenerate, None)
    }

    /// The same fixture with an explicit lifecycle status — `None` is a run
    /// recorded BEFORE the lifecycle record existed, which must keep the old
    /// artifact-and-staleness inference.
    ///
    /// `lifecycle_error` stays `None` here, which is exactly what
    /// `RunLifecycle::drop` writes: this helper's `Interrupted` is the RAII
    /// backstop's record, not a caught signal's.
    fn lab_summary_with_lifecycle(
        dir: &str,
        finished: bool,
        degenerate: bool,
        lifecycle_status: Option<darkmux_lab::lab::lifecycle::LifecycleStatus>,
    ) -> LabRunSummary {
        lab_summary_with_lifecycle_error(dir, finished, degenerate, lifecycle_status, None)
    }

    /// (#2462 review) The same fixture, with the lifecycle record's `error`
    /// too — the ONLY thing that distinguishes `finish_interrupted`'s caught
    /// signal (`Some(..)`) from `RunLifecycle::drop`'s early-return /
    /// panic backstop (`None`), both of which write status `Interrupted`.
    fn lab_summary_with_lifecycle_error(
        dir: &str,
        finished: bool,
        degenerate: bool,
        lifecycle_status: Option<darkmux_lab::lab::lifecycle::LifecycleStatus>,
        lifecycle_error: Option<&str>,
    ) -> LabRunSummary {
        LabRunSummary {
            lifecycle_status,
            lifecycle_error: lifecycle_error.map(str::to_string),
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
            session_id: None,
        }
    }

    /// (#1930) The run's own terminal record outranks every inference.
    ///
    /// The bug this pins: `finished` only ever meant "scores.json exists", so
    /// a run that ERRORED never set it, fell through to the idle heuristic,
    /// and reported `Running` while its artifacts were fresh — then
    /// `Abandoned` once they aged. Neither is what happened. A run that says
    /// how it ended is not something to guess about.
    #[test]
    fn a_lifecycle_record_outranks_the_artifact_and_staleness_inference() {
        use darkmux_lab::lab::lifecycle::LifecycleStatus as Lc;
        let now = 1_700_000_000_000u64;

        // The exact shape of the bug: unfinished, artifacts still fresh.
        // Without a record this is `Running` (asserted last, as the control).
        let errored = lab_summary_with_lifecycle("d", false, false, Some(Lc::Error));
        assert_eq!(
            lab_run_status(&errored, now),
            RunStatus::Error,
            "an errored run must not read as live just because it died recently"
        );

        let interrupted = lab_summary_with_lifecycle("d", false, false, Some(Lc::Interrupted));
        assert_eq!(lab_run_status(&interrupted, now), RunStatus::Abandoned);

        // Complete without `scores.json` — a lab run that finished but whose
        // bench artifacts were never part of its shape.
        let done = lab_summary_with_lifecycle("d", false, false, Some(Lc::Complete));
        assert_eq!(lab_run_status(&done, now), RunStatus::Complete);

        // `degenerate` still downgrades a completed run.
        let degen = lab_summary_with_lifecycle("d", false, true, Some(Lc::Complete));
        assert_eq!(lab_run_status(&degen, now), RunStatus::Error);

        // CONTROL — the same summary with no record keeps the old behavior.
        // Without this the four assertions above could all pass against a
        // function that ignored the record and happened to agree.
        let no_record = lab_summary_with_lifecycle("d", false, false, None);
        assert_eq!(
            lab_run_status(&no_record, now),
            RunStatus::Running,
            "a pre-lifecycle run keeps the artifact-and-staleness inference"
        );
    }

    /// `Running` and `Unknown` deliberately DO NOT short-circuit: the first
    /// may be a hard-killed run whose `Drop` never ran (the one gap RAII
    /// cannot close), and the second must never be read as a verdict.
    #[test]
    fn running_and_unknown_fall_through_to_the_staleness_check() {
        use darkmux_lab::lab::lifecycle::LifecycleStatus as Lc;
        let now = 1_700_000_000_000u64;
        let stale = now + stale_after_ms() + 1;

        for st in [Lc::Running, Lc::Unknown] {
            let fresh = lab_summary_with_lifecycle("d", false, false, Some(st));
            assert_eq!(lab_run_status(&fresh, now), RunStatus::Running, "{st:?} fresh");

            let old = lab_summary_with_lifecycle("d", false, false, Some(st));
            assert_eq!(
                lab_run_status(&old, stale),
                RunStatus::Abandoned,
                "{st:?} whose trail stopped is abandoned, not eternally live"
            );
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
        // (#1907) A Complete row must never carry a leftover reason.
        assert_eq!(run.abandoned_reason, None);
    }

    /// (#1907) The staleness-gate `Abandoned` arm (no lifecycle record at
    /// all, or one still reading `Running`/`Unknown` past the idle budget)
    /// has no terminal record of any kind — the honest reason stays
    /// "no ending recorded".
    #[test]
    fn lab_summary_to_run_abandoned_carries_no_terminal_reason() {
        let summary = minimal_lab_summary("dead/case-1", false, false);
        // Comfortably past `stale_after_ms()` from `mtime_ms`.
        let far_future_ms = summary.mtime_ms + stale_after_ms() + 5_000;
        let run = lab_summary_to_run(&summary, None, far_future_ms);
        assert_eq!(run.status, RunStatus::Abandoned);
        assert_eq!(run.abandoned_reason, Some(AbandonReason::NoTerminal));
    }

    /// (#2462/#1946) The OTHER `Abandoned` arm — a lifecycle record written
    /// by `finish_interrupted` after a caught SIGINT/SIGTERM/SIGHUP — must
    /// not carry the same reason as the staleness gate above. `NoTerminal`
    /// literally means "no ending recorded" (`ui/src/lenses/runs/format.ts`
    /// renders it as exactly that string); this record has an `ended_at_ms`
    /// AND an `error` naming the signal, so `NoTerminal` is the
    /// row-contradicts-itself bug #1946 named. `Aborted` — "a human
    /// explicitly tore the run down" — is what a caught operator signal is.
    #[test]
    fn lab_summary_to_run_signal_interrupted_carries_aborted_not_no_terminal() {
        use darkmux_lab::lab::lifecycle::LifecycleStatus as Lc;
        // Fresh `mtime_ms` (not stale) — proves the reason comes from the
        // lifecycle record, not a staleness-gate coincidence.
        let summary = lab_summary_with_lifecycle_error(
            "dead/case-2",
            false,
            false,
            Some(Lc::Interrupted),
            Some("hosted dispatch interrupted by an operator signal (SIGINT/SIGTERM/SIGHUP)"),
        );
        let run = lab_summary_to_run(&summary, None, summary.mtime_ms);
        assert_eq!(run.status, RunStatus::Abandoned);
        assert_eq!(
            run.abandoned_reason,
            Some(AbandonReason::Aborted),
            "a signal-interrupted lifecycle record reached a terminal write and names its cause \
             — it must not render as \"no ending recorded\""
        );
    }

    /// (#2462 review) The half the test above would otherwise leave
    /// unconstrained, and the reason `abandoned_reason` cannot key on the
    /// status alone. `RunLifecycle::drop` — #1930's RAII backstop, firing on
    /// an early return, a `?` (`cow_clone_dir_excluding`, `create_dir_all`),
    /// or an unwinding panic in a provider — writes the SAME `Interrupted`
    /// status with `error: None`, and knows nothing about human intent.
    ///
    /// Rendering that as "aborted" would fabricate a claim, and would do it
    /// retroactively: every `interrupted` record on disk today predates
    /// `finish_interrupted` and therefore came from this path. "No ending
    /// recorded" is the honest read for a record that genuinely does not say
    /// how the run ended.
    #[test]
    fn lab_summary_to_run_drop_written_interrupted_stays_no_terminal() {
        use darkmux_lab::lab::lifecycle::LifecycleStatus as Lc;
        let summary =
            lab_summary_with_lifecycle_error("dead/case-3", false, false, Some(Lc::Interrupted), None);
        let run = lab_summary_to_run(&summary, None, summary.mtime_ms);
        assert_eq!(run.status, RunStatus::Abandoned);
        assert_eq!(
            run.abandoned_reason,
            Some(AbandonReason::NoTerminal),
            "a Drop-written Interrupted record carries no cause at all — calling it a deliberate \
             human teardown invents intent the record never claimed"
        );
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
        // (#1907) A standalone dispatch has no per-session abort action —
        // `mission abort` is mission-scoped — so an Abandoned ghost from the
        // staleness gate must always read "no ending recorded".
        assert_eq!(stale[0].abandoned_reason, Some(AbandonReason::NoTerminal));
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
        // (#1907) `abandoned_reason` is ONLY ever set alongside `Abandoned` —
        // a Complete row must carry `None`, not a stale reason left over
        // from some other branch.
        assert_eq!(ghosts[0].abandoned_reason, None);
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

    /// (#1810) A tracked mission whose flow day-file predates
    /// `RUNS_FLOW_SCAN_WINDOW_DAYS` must still report its machine, because
    /// #1810 makes `machine` a durable fact on the `Mission` record itself
    /// (stamped at mint time) rather than something ONLY derivable by
    /// joining to flow sessions. Before the fix, `machine` came exclusively
    /// from `representative.and_then(|(_, s)| s.machine.clone())` — a
    /// windowed flow-index lookup — so a mission this old read `machine:
    /// None` even though the mission record and the (still-on-disk, just
    /// out-of-window) day-file both had the fact.
    ///
    /// The day-file here is dated 2020-01-01 — `for_each_recent_flow_record`
    /// bounds its walk to day-FILE NAMES within the window, so this file is
    /// never even opened, which is exactly the "still on disk, unreachable"
    /// shape the issue measured (a real 2026-06-12 day-file, 84 days old,
    /// on an install whose scan window is 14).
    #[test]
    #[serial_test::serial]
    fn build_runs_a_mission_older_than_the_flow_window_still_reports_its_durable_machine() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();

        let mut mission = minimal_mission(
            "ancient-mission-1",
            vec!["ancient-mission-1-phase".to_string()],
            Some(MissionSpec { config_id: "dispatch".to_string(), inputs_fingerprint: "fp".to_string(), origin: None }),
        );
        mission.machine = Some("durable-studio".to_string());
        darkmux_crew::lifecycle::save_mission(&mission).unwrap();
        let phase = minimal_phase(
            "ancient-mission-1-phase",
            "ancient-mission-1",
            vec!["ancient-mission-1-task".to_string()],
        );
        darkmux_crew::lifecycle::save_phase(&phase).unwrap();
        let task = minimal_task(
            "ancient-mission-1-task",
            "ancient-mission-1-phase",
            vec!["ancient-mission-1-step".to_string()],
            Some("coder"),
        );
        darkmux_crew::lifecycle::save_task("ancient-mission-1", &task).unwrap();
        let step = minimal_step(
            "ancient-mission-1-step",
            "ancient-mission-1-task",
            Some("crew-dispatch-ancient-xyz"),
        );
        darkmux_crew::lifecycle::save_step("ancient-mission-1", "ancient-mission-1-phase", &step).unwrap();

        // The dispatch's own flow records exist, with a REAL (different)
        // machine_id stamped on them — but they're filed under a date far
        // outside RUNS_FLOW_SCAN_WINDOW_DAYS, so the windowed scan never
        // reads this file at all. If `machine` were still purely
        // flow-derived, this row would read `machine: None`.
        write_day_file(
            flows.path(),
            "2020-01-01",
            &[
                serde_json::json!({
                    "ts": "2020-01-01T09:00:00Z",
                    "action": "dispatch start",
                    "session_id": "crew-dispatch-ancient-xyz",
                    "handle": "coder",
                    "machine_id": "flow-derived-machine-should-not-win",
                }),
                serde_json::json!({
                    "ts": "2020-01-01T09:10:00Z",
                    "action": "dispatch complete",
                    "session_id": "crew-dispatch-ancient-xyz",
                    "handle": "coder",
                    "model": "qwen3.6-35b-a3b",
                    "machine_id": "flow-derived-machine-should-not-win",
                }),
            ],
        );

        let runs = build_runs(flows.path(), None, &[]);
        assert_eq!(runs.len(), 1, "exactly one Run — the tracked mission, no ghost duplicate: {runs:?}");
        assert_eq!(runs[0].id, "ancient-mission-1");
        assert!(runs[0].tracked);
        assert_eq!(
            runs[0].machine.as_deref(),
            Some("durable-studio"),
            "machine must come from the mission's OWN durable field, surviving the flow retention \
             window entirely — not from the (unreachable, out-of-window) flow session: {runs:?}"
        );
    }

    /// (#1810 QA must-fix 2) `machine` prefers the durable `Mission.machine`
    /// even when a LIVE, in-window flow session exists and DISAGREES with
    /// it — not just when the flow session is unreachable (the prior
    /// test's shape, where precedence was untested: a flow-first-with-
    /// durable-fallback order would have passed that test identically).
    /// Pins the precedence actually chosen at `mission_to_run`'s
    /// `let machine = mission.machine.clone().or_else(...)` line: the
    /// mint-time host wins unconditionally. Rationale: `Mission.machine`
    /// is stamped once, at creation, by whichever machine ran
    /// `dispatch_as_crew_of_one::build_graph` (or `mission_launch`'s
    /// equivalent) — it answers "where was this mission minted", a fact
    /// that cannot be changed by a later dispatch executing somewhere
    /// else. A future feature that wants "which host actually EXECUTED
    /// the work" (distinct from "which host minted it") needs its own
    /// field, not a change to this precedence.
    #[test]
    #[serial_test::serial]
    fn build_runs_durable_machine_wins_over_a_disagreeing_live_flow_session() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();

        let mut mission = minimal_mission(
            "disagree-mission-1",
            vec!["disagree-mission-1-phase".to_string()],
            Some(MissionSpec { config_id: "dispatch".to_string(), inputs_fingerprint: "fp".to_string(), origin: None }),
        );
        mission.machine = Some("orchestrator-mint-host".to_string());
        darkmux_crew::lifecycle::save_mission(&mission).unwrap();
        let phase = minimal_phase(
            "disagree-mission-1-phase",
            "disagree-mission-1",
            vec!["disagree-mission-1-task".to_string()],
        );
        darkmux_crew::lifecycle::save_phase(&phase).unwrap();
        let task = minimal_task(
            "disagree-mission-1-task",
            "disagree-mission-1-phase",
            vec!["disagree-mission-1-step".to_string()],
            Some("coder"),
        );
        darkmux_crew::lifecycle::save_task("disagree-mission-1", &task).unwrap();
        let step = minimal_step(
            "disagree-mission-1-step",
            "disagree-mission-1-task",
            Some("crew-dispatch-disagree-xyz"),
        );
        darkmux_crew::lifecycle::save_step("disagree-mission-1", "disagree-mission-1-phase", &step).unwrap();

        // Entirely INSIDE the flow retention window (today's day-file),
        // stamped with a DIFFERENT machine — the peer that actually ran
        // the dispatch — so this is a live disagreement, not an absence.
        let day = today();
        write_day_file(
            flows.path(),
            &day,
            &[
                serde_json::json!({
                    "ts": format!("{day}T09:00:00Z"),
                    "action": "dispatch start",
                    "session_id": "crew-dispatch-disagree-xyz",
                    "handle": "coder",
                    "machine_id": "peer-runner-that-actually-ran-it",
                }),
                serde_json::json!({
                    "ts": format!("{day}T09:10:00Z"),
                    "action": "dispatch complete",
                    "session_id": "crew-dispatch-disagree-xyz",
                    "handle": "coder",
                    "model": "qwen3.6-35b-a3b",
                    "machine_id": "peer-runner-that-actually-ran-it",
                }),
            ],
        );

        let runs = build_runs(flows.path(), None, &[]);
        assert_eq!(runs.len(), 1, "exactly one Run: {runs:?}");
        assert_eq!(runs[0].id, "disagree-mission-1");
        assert_eq!(
            runs[0].machine.as_deref(),
            Some("orchestrator-mint-host"),
            "durable Mission.machine must win over a live, in-window, DISAGREEING flow session \
             — pinning the precedence chosen at mission_to_run's `let machine = ...` line: {runs:?}"
        );
    }

    /// (#1982) A lab run dispatches its inner work through the ordinary
    /// `crew::dispatch::dispatch` primitive, so it emits real `dispatch
    /// start`/`dispatch complete` flow bookends under the session_id the
    /// provider minted and recorded into the run's own `manifest.json`.
    /// Nothing claimed that session on the lab row's behalf, so it ALSO
    /// surfaced as an untracked ghost — the same underlying work counted
    /// twice. This is the acceptance case from the issue: one lab summary
    /// plus the flow records of the dispatch it made must fold into exactly
    /// one row.
    fn write_lab_run_with_dispatch_session(lab_dir: &StdPath, dir: &str, session_id: &str) {
        let run_dir = lab_dir.join(dir);
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(
            run_dir.join("lifecycle.json"),
            serde_json::to_string(&serde_json::json!({
                "schema_version": "1.0",
                "run_id": dir,
                "kind": "lab",
                "workload": "crawl-error-discard",
                "profile": "default",
                "started_at_ms": 1_700_000_000_000u64,
                "status": "complete",
                "ended_at_ms": 1_700_000_010_000u64,
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            run_dir.join("manifest.json"),
            serde_json::to_string(&serde_json::json!({
                "schema_version": 3,
                "run_id": dir,
                "workload": "crawl-error-discard",
                "provider": "coding-task",
                "profile": "default",
                "duration_ms": 10_000,
                "ok": true,
                "session_id": session_id,
                "sandbox": "/tmp/sandbox",
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn build_runs_lab_run_dispatch_session_is_not_also_listed_as_a_ghost() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();
        let lab = TempDir::new().unwrap();

        write_lab_run_with_dispatch_session(
            lab.path(),
            "crawl-error-discard-deep-1",
            "darkmux-coding-crawl-error-discard-1787676109556",
        );

        write_day_file(
            flows.path(),
            &today(),
            &[
                serde_json::json!({
                    "ts": "2026-07-24T09:00:00Z",
                    "action": "dispatch start",
                    "session_id": "darkmux-coding-crawl-error-discard-1787676109556",
                    "handle": "crawler",
                }),
                serde_json::json!({
                    "ts": "2026-07-24T09:10:00Z",
                    "action": "dispatch complete",
                    "session_id": "darkmux-coding-crawl-error-discard-1787676109556",
                    "handle": "crawler",
                    "model": "qwen3.6-35b-a3b",
                }),
            ],
        );

        let runs = build_runs(flows.path(), Some(lab.path()), &[]);
        assert_eq!(
            runs.len(),
            1,
            "exactly one Run — the lab run, no untracked dispatch ghost of its own inner dispatch: {runs:?}"
        );
        assert_eq!(runs[0].id, "crawl-error-discard-deep-1");
        assert_eq!(runs[0].kind, RunKind::Lab);
        assert!(runs[0].tracked);
    }

    /// The inverted case (issue's own instruction): de-duplication must
    /// never swallow a genuinely DIFFERENT dispatch that merely happens to
    /// be live alongside a lab run. A real, unrelated standalone dispatch
    /// session (never claimed by any lab summary) must still surface as its
    /// own ghost row.
    #[test]
    #[serial_test::serial]
    fn build_runs_lab_claim_does_not_swallow_an_unrelated_dispatch_session() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();
        let lab = TempDir::new().unwrap();

        write_lab_run_with_dispatch_session(
            lab.path(),
            "crawl-error-discard-deep-1",
            "darkmux-coding-crawl-error-discard-1787676109556",
        );

        write_day_file(
            flows.path(),
            &today(),
            &[
                serde_json::json!({
                    "ts": "2026-07-24T09:00:00Z",
                    "action": "dispatch start",
                    "session_id": "darkmux-coding-crawl-error-discard-1787676109556",
                    "handle": "crawler",
                }),
                serde_json::json!({
                    "ts": "2026-07-24T09:10:00Z",
                    "action": "dispatch complete",
                    "session_id": "darkmux-coding-crawl-error-discard-1787676109556",
                    "handle": "crawler",
                    "model": "qwen3.6-35b-a3b",
                }),
                serde_json::json!({
                    "ts": "2026-07-24T09:05:00Z",
                    "action": "dispatch start",
                    "session_id": "an-entirely-unrelated-standalone-session",
                    "handle": "coder",
                }),
                serde_json::json!({
                    "ts": "2026-07-24T09:06:00Z",
                    "action": "dispatch complete",
                    "session_id": "an-entirely-unrelated-standalone-session",
                    "handle": "coder",
                    "model": "qwen3.6-35b-a3b",
                }),
            ],
        );

        let runs = build_runs(flows.path(), Some(lab.path()), &[]);
        assert_eq!(
            runs.len(),
            2,
            "the lab run's own claimed session must not swallow a distinct, unrelated dispatch: {runs:?}"
        );
        let ids: Vec<&str> = runs.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"crawl-error-discard-deep-1"));
        assert!(ids.contains(&"an-entirely-unrelated-standalone-session"));
        let ghost = runs
            .iter()
            .find(|r| r.id == "an-entirely-unrelated-standalone-session")
            .unwrap();
        assert_eq!(ghost.kind, RunKind::Dispatch);
        assert!(!ghost.tracked);
    }

    /// (#1982) The DEGRADATION this fix promises, asserted end-to-end rather
    /// than only described in a comment: a lab run that recorded no
    /// `session_id` claims nothing, so its dispatch bookends still surface
    /// as their own untracked ghost. Both rows are expected here — the
    /// duplicate is the honest outcome of having no claim to make, NOT a
    /// display filter hiding a still-double-counted total.
    ///
    /// This is also the live-run window, which is the case that actually
    /// occurs: `lifecycle.json` lands at run start and `manifest.json` at
    /// run end, so a RUNNING lab run has exactly this shape for its whole
    /// duration (producer-side gap #2511). The guard that matters: nothing
    /// here may invent a claim from the run id or any other guessable
    /// string, which would swallow whichever session happened to match.
    #[test]
    #[serial_test::serial]
    fn build_runs_lab_run_with_no_recorded_session_claims_nothing_and_the_ghost_persists() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();
        let lab = TempDir::new().unwrap();

        // Same fixture as the two tests above, minus `manifest.json` — the
        // shape of a run that has started and not yet finished.
        let run_dir = lab.path().join("crawl-error-discard-deep-1");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(
            run_dir.join("lifecycle.json"),
            serde_json::to_string(&serde_json::json!({
                "schema_version": "1.0",
                "run_id": "crawl-error-discard-deep-1",
                "kind": "lab",
                "workload": "crawl-error-discard",
                "profile": "default",
                "started_at_ms": 1_700_000_000_000u64,
                "status": "running",
            }))
            .unwrap(),
        )
        .unwrap();

        write_day_file(
            flows.path(),
            &today(),
            &[
                serde_json::json!({
                    "ts": "2026-07-24T09:00:00Z",
                    "action": "dispatch start",
                    "session_id": "darkmux-coding-crawl-error-discard-1787676109556",
                    "handle": "crawler",
                }),
                serde_json::json!({
                    "ts": "2026-07-24T09:10:00Z",
                    "action": "dispatch complete",
                    "session_id": "darkmux-coding-crawl-error-discard-1787676109556",
                    "handle": "crawler",
                    "model": "qwen3.6-35b-a3b",
                }),
            ],
        );

        let runs = build_runs(flows.path(), Some(lab.path()), &[]);
        let ids: Vec<&str> = runs.iter().map(|r| r.id.as_str()).collect();
        assert!(
            ids.contains(&"crawl-error-discard-deep-1"),
            "the lab row itself is unaffected by having no session to claim: {runs:?}"
        );
        assert!(
            ids.contains(&"darkmux-coding-crawl-error-discard-1787676109556"),
            "with nothing claimed, the dispatch ghost MUST persist — a silent claim \
             synthesized from the run id would be worse than the duplicate: {runs:?}"
        );
        assert_eq!(runs.len(), 2, "{runs:?}");
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

    /// (#1918) The actual reported harm, reproduced end to end: TWO
    /// missions launched from the SAME config run the SAME task/step id
    /// (`s-shared`) with DIFFERENT roles/models. Before the fix, both
    /// missions' step records landed under the identical config-derived
    /// `session_id` (`step-s-shared`), so `SessionAgg` folded them into
    /// ONE bucket and `mission_to_run`'s role/model attribution for
    /// EITHER mission could read the OTHER's. This test writes the
    /// POST-FIX (scoped) session ids each mission's own step-lifecycle
    /// records now carry and asserts `build_runs` attributes role/model
    /// to the mission that actually ran it — never the sibling's.
    #[test]
    #[serial_test::serial]
    fn build_runs_two_missions_from_the_same_config_never_cross_attribute_role_or_model() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();

        let raw_session = darkmux_types::session_id::step("s-shared");
        let session_x = darkmux_types::session_id::scope_to_run(&raw_session, "mission-x");
        let session_y = darkmux_types::session_id::scope_to_run(&raw_session, "mission-y");
        assert_ne!(session_x, session_y, "the two missions' scoped session ids must differ");

        // `write_day_file` truncates the day file on every call — collect
        // BOTH missions' records and write the file exactly once, the way
        // a real day's flow stream actually accumulates.
        let mut all_records: Vec<serde_json::Value> = Vec::new();
        for (mission_id, phase_id, task_id, role, model, session_id) in [
            ("mission-x", "px", "tx", "coder", "model-x", &session_x),
            ("mission-y", "py", "ty", "reviewer", "model-y", &session_y),
        ] {
            let mission = minimal_mission(
                mission_id,
                vec![phase_id.to_string()],
                Some(MissionSpec {
                    config_id: "shared-config".to_string(),
                    inputs_fingerprint: format!("fp-{mission_id}"),
                    origin: None,
                }),
            );
            darkmux_crew::lifecycle::save_mission(&mission).unwrap();
            let phase = minimal_phase(phase_id, mission_id, vec![task_id.to_string()]);
            darkmux_crew::lifecycle::save_phase(&phase).unwrap();
            let task = minimal_task(task_id, phase_id, vec!["s-shared".to_string()], Some(role));
            darkmux_crew::lifecycle::save_task(mission_id, &task).unwrap();
            // Same step id, same kind default, no explicit session_id —
            // the exact config-derived collision shape.
            let step = minimal_step("s-shared", task_id, None);
            darkmux_crew::lifecycle::save_step(mission_id, phase_id, &step).unwrap();

            all_records.push(serde_json::json!({
                "ts": "2026-07-24T09:00:00Z",
                "action": "dispatch start",
                "session_id": session_id,
                "handle": role,
                "mission_id": mission_id,
            }));
            all_records.push(serde_json::json!({
                "ts": "2026-07-24T09:10:00Z",
                "action": "dispatch complete",
                "session_id": session_id,
                "handle": role,
                "mission_id": mission_id,
                "model": model,
            }));
        }
        write_day_file(flows.path(), &today(), &all_records);

        let runs = build_runs(flows.path(), None, &[]);
        let run_x = runs.iter().find(|r| r.id == "mission-x").expect("mission-x must produce a Run");
        let run_y = runs.iter().find(|r| r.id == "mission-y").expect("mission-y must produce a Run");

        assert_eq!(run_x.model.as_deref(), Some("model-x"), "mission-x must never read mission-y's model");
        assert_eq!(run_y.model.as_deref(), Some("model-y"), "mission-y must never read mission-x's model");
        assert_ne!(
            run_x.model, run_y.model,
            "two missions running the SAME shared task/step id must attribute DISTINCT models — \
             the #1918 cross-mission contamination this fix closes"
        );
    }

    /// (#1918 QA) The MIXED day file — the state every operator has for
    /// `RUNS_FLOW_SCAN_WINDOW_DAYS` after upgrading past FLOW 1.43.0:
    /// pre-1.43.0 records still carry the bare `step-<id>` bucket that N
    /// missions shared, beside a new mission's correctly-scoped ones. A
    /// new mission's structural prediction (`collect_mission_step_
    /// sessions`, which by design still predicts the UNSCOPED form)
    /// claims that legacy bucket, and its records are OLDER — so before
    /// the `is_ambiguous` filter on `sessions_by_start` they sorted FIRST
    /// and won BOTH `role` and `model`, which are precisely the two
    /// attributes #1918 exists to stop cross-contaminating. Measured
    /// before the fix: `model: legacy-model-a`, `role: legacy-role` on a
    /// mission whose own records say `correct-model`/`coder`.
    #[test]
    #[serial_test::serial]
    fn build_runs_a_mixed_day_never_attributes_the_legacy_shared_bucket_to_a_new_mission() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();

        let raw = darkmux_types::session_id::step("s-shared");
        let scoped_new = darkmux_types::session_id::scope_to_run(&raw, "mission-new");
        assert_ne!(raw, scoped_new);

        let mission = minimal_mission(
            "mission-new",
            vec!["pn".to_string()],
            Some(MissionSpec {
                config_id: "shared-config".to_string(),
                inputs_fingerprint: "fp-new".to_string(),
                origin: None,
            }),
        );
        darkmux_crew::lifecycle::save_mission(&mission).unwrap();
        darkmux_crew::lifecycle::save_phase(&minimal_phase("pn", "mission-new", vec!["tn".to_string()])).unwrap();
        darkmux_crew::lifecycle::save_task(
            "mission-new",
            &minimal_task("tn", "pn", vec!["s-shared".to_string()], Some("coder")),
        )
        .unwrap();
        darkmux_crew::lifecycle::save_step("mission-new", "pn", &minimal_step("s-shared", "tn", None)).unwrap();

        let mut recs: Vec<serde_json::Value> = Vec::new();
        // LEGACY: two OLD missions folded into the ONE bare session id —
        // the 49-missions/1-session bucket #1918 measured live.
        for (mid, model) in [("mission-old-a", "legacy-model-a"), ("mission-old-b", "legacy-model-b")] {
            recs.push(serde_json::json!({
                "ts": "2026-07-20T09:00:00Z", "action": "dispatch start",
                "session_id": raw, "handle": "legacy-role", "mission_id": mid,
            }));
            recs.push(serde_json::json!({
                "ts": "2026-07-20T09:10:00Z", "action": "dispatch complete",
                "session_id": raw, "handle": "legacy-role", "mission_id": mid, "model": model,
            }));
        }
        // The new mission's OWN, correctly scoped records — LATER, so
        // only the ambiguity filter (not ordering) can save them.
        recs.push(serde_json::json!({
            "ts": "2026-07-24T09:00:00Z", "action": "dispatch start",
            "session_id": scoped_new, "handle": "coder", "mission_id": "mission-new",
        }));
        recs.push(serde_json::json!({
            "ts": "2026-07-24T09:10:00Z", "action": "dispatch complete",
            "session_id": scoped_new, "handle": "coder", "mission_id": "mission-new",
            "model": "correct-model",
        }));
        write_day_file(flows.path(), &today(), &recs);

        let runs = build_runs(flows.path(), None, &[]);
        let run = runs.iter().find(|r| r.id == "mission-new").expect("mission-new must produce a Run");
        assert_eq!(
            run.model.as_deref(),
            Some("correct-model"),
            "a new mission must never read a legacy shared bucket's model"
        );
        assert_eq!(
            run.role.as_deref(),
            Some("coder"),
            "a new mission must never read a legacy shared bucket's role"
        );
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
        let created_ts = mission.created_ts;
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
                // (#2487/#2558) The tainted session's TERMINAL, carrying an
                // endpoint — the two remaining fields that used to read the
                // unfiltered `sessions` pool. Without these records the
                // fixture could not tell a filtered `completed_ts`/`route`
                // from a field that was simply never populated.
                serde_json::json!({
                    "ts": "2026-01-01T09:00:00Z",
                    "action": "dispatch complete",
                    "session_id": "collision-mission-1",
                    "handle": "coder-phase",
                    "mission_id": "collision-mission-1",
                    "source": "mission",
                    "machine_id": "studio",
                    "payload": { "endpoint": "tainted-endpoint" },
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
        // (#2487) Before the fix, `machine` and `started_ts` still read the
        // SAME ambiguous session `session_id` was just suppressed for — the
        // one session in this fixture is the mission's ONLY candidate, so
        // once it is correctly excluded there is nothing left to fall back
        // to. `None` is the deliberate, honest answer here (see
        // `unambiguous_sessions`'s own doc in `mission_to_run`): a blank
        // field beats a wrong one, and every other consumer of a `None`
        // `machine`/`started_ts` already renders it as unknown, the same
        // way a `None` role/model already does.
        assert_eq!(
            row.machine, None,
            "an ambiguous representative session must not win `machine` either — same contamination role/model already refuse: {row:?}"
        );
        assert_eq!(
            row.started_ts, None,
            "an ambiguous representative session must not win `started_ts` either — this mission's own `started_ts` was explicitly unset, so the ONLY source was the tainted session: {row:?}"
        );
        // (#2487) The SORT key. `completed_ts` feeds `updated_ts`, which
        // both the viewer and the CLI sort on FIRST — start time is only the
        // third key — so a `terminal_ts_str` taken over the unfiltered pool
        // would have set this row's position outright while every displayed
        // field beside it went blank. That row also contradicts itself on
        // screen: "completed 09:00, no start, no machine, no role."
        assert_eq!(
            row.completed_ts, None,
            "an ambiguous session must not win `completed_ts` — it is the row's FIRST sort key via `updated_ts`, which is precisely the ordering this fix is about: {row:?}"
        );
        assert_eq!(
            row.updated_ts,
            Some(created_ts),
            "with every session-derived time refused, the row falls back to the mission's own mint time (#1584's last arm) rather than inheriting a tainted position: {row:?}"
        );
        // (#2558, folded in here) `route` was the last field still reading
        // the unfiltered pool. Left alone it rendered an all-ambiguous row as
        // literally `via tainted-endpoint` and nothing else, since
        // `runSubtitle` concatenates role · model · route · machine.
        assert_eq!(
            row.route, None,
            "an ambiguous session must not win `route` either — otherwise it is the only surviving field on a row whose every sibling went blank: {row:?}"
        );
    }

    /// (#2487) The production shape the issue actually reports: an OLDER,
    /// ambiguous shared-bucket session (the legacy pre-1.43.0 corruption
    /// #1918 diagnosed) sorts FIRST by `earliest_by_start` and, before this
    /// fix, won `machine`/`started_ts` (and therefore this row's SORT
    /// position) even though the identical session was already refused as
    /// a source for role/model. Reuses the #1877 bookend fixture almost
    /// verbatim (bookend + a real coder-step session), with one addition: a
    /// SECOND `mission_id` folded into the bookend's own session, making it
    /// ambiguous — after the fix, `machine`/`started_ts` must recover from
    /// the coder session (the only unambiguous one), the same way role and
    /// model already do.
    ///
    /// RED PROVED (see the mutation restoring `let representative =
    /// earliest_by_start(&sessions);` unfiltered): with the mutation in
    /// place, `machine` reported `Some("stale-bookend-machine")` and
    /// `started_ts` reported the bookend's 08:00 timestamp — the tainted
    /// session's own values — against this test's assertions of
    /// `"real-coder-machine"` / 09:00.
    #[test]
    #[serial_test::serial]
    fn build_runs_2487_ambiguous_bookend_never_wins_machine_or_start_ts() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();

        let mut mission = minimal_mission(
            "ambig-bookend-mission",
            vec!["p-ambig".to_string()],
            Some(MissionSpec {
                config_id: "coder-phase".to_string(),
                inputs_fingerprint: "fpa".to_string(),
                origin: None,
            }),
        );
        // Same reason the #1877 fixture unsets this: without it,
        // `mission.started_ts` (the mission's own durable field) always
        // wins first and the session-derived fallback this test pins would
        // never actually be exercised.
        mission.started_ts = None;
        darkmux_crew::lifecycle::save_mission(&mission).unwrap();
        let phase = minimal_phase("p-ambig", "ambig-bookend-mission", vec!["t-ambig".to_string()]);
        darkmux_crew::lifecycle::save_phase(&phase).unwrap();
        let task = minimal_task("t-ambig", "p-ambig", vec!["s-ambig".to_string()], Some("coder"));
        darkmux_crew::lifecycle::save_task("ambig-bookend-mission", &task).unwrap();
        let step = minimal_step("s-ambig", "t-ambig", Some("crew-dispatch-coder-ambig"));
        darkmux_crew::lifecycle::save_step("ambig-bookend-mission", "p-ambig", &step).unwrap();

        write_day_file(
            flows.path(),
            &today(),
            &[
                // This mission's own bookend — earliest ts, would win
                // `earliest_by_start`'s pick if it weren't ambiguous.
                serde_json::json!({
                    "ts": "2026-01-01T08:00:00Z",
                    "action": "dispatch start",
                    "session_id": "ambig-bookend-mission",
                    "handle": "coder-phase",
                    "mission_id": "ambig-bookend-mission",
                    "source": "mission",
                    "machine_id": "stale-bookend-machine",
                    // (#2558) An endpoint on the tainted session, EARLIER
                    // than the coder's — `remote` is an `earliest_by_start`
                    // pick, so unfiltered this one wins `route`.
                    "payload": { "endpoint": "stale-bookend-endpoint" },
                }),
                // The #1918 collision: a DIFFERENT mission's record folded
                // into the SAME bookend session, making it ambiguous.
                serde_json::json!({
                    "ts": "2026-01-01T08:05:00Z",
                    "action": "step start",
                    "session_id": "ambig-bookend-mission",
                    "mission_id": "some-other-mission",
                    "source": "scheduler",
                }),
                // The coder step's OWN dispatch — unambiguous, later, and
                // on a DIFFERENT machine, so this test can tell whether
                // `machine`/`started_ts` actually recovered from it.
                serde_json::json!({
                    "ts": "2026-01-01T09:00:00Z",
                    "action": "dispatch start",
                    "session_id": "crew-dispatch-coder-ambig",
                    "handle": "coder",
                    "mission_id": "ambig-bookend-mission",
                    "machine_id": "real-coder-machine",
                }),
                serde_json::json!({
                    "ts": "2026-01-01T09:10:00Z",
                    "action": "dispatch complete",
                    "session_id": "crew-dispatch-coder-ambig",
                    "handle": "coder",
                    "mission_id": "ambig-bookend-mission",
                    "model": "qwen3.6-35b-a3b",
                    "machine_id": "real-coder-machine",
                    "payload": { "endpoint": "real-coder-endpoint" },
                }),
                // (#2487) The tainted session's terminal, LATER than the
                // coder's — `terminal_ts_str` is a `max()`, so unfiltered
                // this one wins `completed_ts` and therefore `updated_ts`,
                // the row's first sort key.
                serde_json::json!({
                    "ts": "2026-01-01T09:30:00Z",
                    "action": "dispatch complete",
                    "session_id": "ambig-bookend-mission",
                    "handle": "coder-phase",
                    "mission_id": "ambig-bookend-mission",
                    "source": "mission",
                    "machine_id": "stale-bookend-machine",
                }),
            ],
        );

        let runs = build_runs(flows.path(), None, &[]);
        let row = runs.iter().find(|r| r.id == "ambig-bookend-mission").expect("row for ambig-bookend-mission");
        assert!(row.tracked);

        // Already correct before this fix — pinned here so the fixture's
        // own premise (role/model DO recover) stays visible next to the
        // machine/start_ts assertions this fix adds.
        assert_eq!(row.role.as_deref(), Some("coder"));
        assert_eq!(row.model.as_deref(), Some("qwen3.6-35b-a3b"));

        // The #2487 fix: machine/start_ts must ALSO recover from the
        // unambiguous coder session, not stay pinned to the tainted
        // bookend.
        assert_eq!(
            row.machine.as_deref(),
            Some("real-coder-machine"),
            "machine must recover from the unambiguous coder session, not the ambiguous bookend: {row:?}"
        );
        assert_eq!(
            row.started_ts,
            parse_flow_ts("2026-01-01T09:00:00Z"),
            "started_ts must recover from the unambiguous coder session's start, not the ambiguous bookend's earlier one: {row:?}"
        );

        // (#2487) The ordering half. `completed_ts` is what `updated_ts` —
        // the FIRST sort key in both the viewer and the CLI — reads, so a
        // `terminal_ts_str` taken as a `max()` over the unfiltered pool put
        // this row at the tainted session's 09:30 rather than its own
        // dispatch's 09:10. The filter is what actually delivers this
        // commit's ordering claim; `representative` only reaches the third
        // sort key.
        assert_eq!(
            row.completed_ts,
            parse_flow_ts("2026-01-01T09:10:00Z"),
            "completed_ts must come from the unambiguous coder session's terminal, not the later ambiguous one: {row:?}"
        );
        assert_eq!(
            row.updated_ts,
            parse_flow_ts("2026-01-01T09:10:00Z"),
            "and therefore so must the row's first sort key: {row:?}"
        );

        // (#2558) Route recovers the same way — and does NOT keep the
        // tainted session's earlier endpoint just because `remote` picks by
        // earliest start.
        assert_eq!(
            row.route.as_deref(),
            Some("real-coder-endpoint"),
            "route must recover from the unambiguous coder session, not the ambiguous bookend's earlier endpoint: {row:?}"
        );

        // (#1915) The drill target follows the same recovered session.
        assert_eq!(
            row.session_id.as_deref(),
            Some("crew-dispatch-coder-ambig"),
            "session_id must also recover from the unambiguous coder session: {row:?}"
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
                config_id: "cmd-gate-approve".to_string(),
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
                    "handle": "cmd-gate-approve",
                    "mission_id": "bookend-only-1",
                    "source": "mission",
                }),
                serde_json::json!({
                    "ts": "2026-01-01T08:00:05Z",
                    "action": "dispatch complete",
                    "session_id": "bookend-only-1",
                    "handle": "cmd-gate-approve",
                    "mission_id": "bookend-only-1",
                    "source": "mission",
                }),
            ],
        );

        let runs = build_runs(flows.path(), None, &[]);
        assert_eq!(runs.len(), 1, "exactly one Run for the bookend-only mission: {runs:?}");
        assert_eq!(runs[0].role.as_deref(), Some("cmd-gate-approve"), "with no other session, the bookend's own handle IS the best available role: {runs:?}");
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
        // the now-deleted dedicated review launcher, #2310 P4d).
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
        // (#1907) `mission.json` was never touched again — it stays
        // `MissionStatus::Active`, never `Aborted` — so this Abandoned row
        // must carry the honest "no ending recorded" reason, never the
        // deliberate-teardown one nobody actually asked for.
        assert_eq!(
            runs[0].abandoned_reason,
            Some(AbandonReason::NoTerminal),
            "a process crash is not a `mission abort` — must not read as a deliberate abort"
        );
    }

    /// (#1907) `mission_run_status_pins_every_terminal_mission_status_variant`
    /// (above) already pins `MissionStatus::Aborted => RunStatus::Abandoned`
    /// through `mission_run_status` in isolation — but `abandoned_reason` is
    /// decided at the `mission_to_run` CALL SITE, not inside
    /// `mission_run_status` itself, so only a full `build_runs` run
    /// (through `mission_to_run`) can catch a regression in that wiring.
    /// This is the LOCAL twin of `an_aborted_peer_mission_reads_abandoned_not_complete`.
    #[test]
    #[serial_test::serial]
    fn build_runs_local_aborted_mission_carries_aborted_reason() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();

        let mut mission = minimal_mission("m1907-aborted", vec![], None);
        mission.status = MissionStatus::Aborted;
        darkmux_crew::lifecycle::save_mission(&mission).unwrap();

        let runs = build_runs(flows.path(), None, &[]);
        let row = runs.iter().find(|r| r.id == "m1907-aborted").unwrap();
        assert_eq!(row.status, RunStatus::Abandoned);
        assert_eq!(
            row.abandoned_reason,
            Some(AbandonReason::Aborted),
            "a deliberately torn-down LOCAL mission must carry Aborted, not the generic no-terminal reason"
        );
    }

    /// (#2123) An ACTIVE review mission whose per-step dispatches reuse the
    /// SAME fixed session ids (`task-review-probe-mid-task` etc — #1918's
    /// named reused-task-id defect) an OLDER, already-finished review
    /// mission also used. Reproduces the operator's real report verbatim:
    /// `review-1788018390-d47a61` had a `mission start` + `dispatch start`
    /// x4 + `step start`/`step result` streaming continuously for 20+
    /// minutes with NO `mission close`/`mission abort` yet, while an OLDER
    /// mission (an hour earlier) had already run the exact same probe task
    /// id to completion. The reused id makes that probe-step SessionAgg
    /// `is_ambiguous()` (#1918) — this test locks in that `mission_to_run`
    /// still reports `Running`, first in the list, off the mission's OTHER,
    /// non-ambiguous session (the run-level dispatch bookend, keyed on the
    /// unique commit sha in the real report) rather than silently vanishing
    /// or reading Abandoned.
    ///
    /// (red-proved: deleting the fresh dispatch-level `telemetry.process`
    /// record below flips this mission to `Abandoned` — confirmed live.)
    #[test]
    #[serial_test::serial]
    fn build_runs_2123_active_review_mission_survives_reused_step_session_ids() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();

        let older = minimal_mission(
            "review-1000000000-older",
            vec![],
            Some(MissionSpec { config_id: "review".to_string(), inputs_fingerprint: "fpo".to_string(), origin: None }),
        );
        darkmux_crew::lifecycle::save_mission(&older).unwrap();

        let active = minimal_mission(
            "review-2000000000-active",
            vec![],
            Some(MissionSpec { config_id: "review".to_string(), inputs_fingerprint: "fpn".to_string(), origin: None }),
        );
        darkmux_crew::lifecycle::save_mission(&active).unwrap();

        let now = darkmux_flow::ts_utc_now();

        write_day_file(
            flows.path(),
            &today(),
            &[
                // older mission — fully terminal, reusing the SAME probe
                // task session id every review mission uses.
                serde_json::json!({"ts":"2026-01-01T08:00:00Z","action":"mission start","session_id":"mission-review-1000000000-older","mission_id":"review-1000000000-older","source":"mission_lifecycle"}),
                serde_json::json!({"ts":"2026-01-01T08:00:01Z","action":"dispatch start","session_id":"task-review-probe-mid-task","mission_id":"review-1000000000-older","role":"review-probe-mid"}),
                serde_json::json!({"ts":"2026-01-01T08:20:00Z","action":"dispatch complete","session_id":"task-review-probe-mid-task","mission_id":"review-1000000000-older","model":"m-old"}),
                // (#2487) The older mission's OWN run-level dispatch bookend,
                // mirroring the active mission's `owner/repo@deadbeef` below.
                // Before #2487 this row's `completed_ts` was read off the
                // SHARED probe session — the very session #1918 says belongs
                // to two missions — which is what put it behind the active
                // row. Now that an ambiguous session can no longer supply a
                // time, the fixture supplies the terminal the real report
                // actually had: a bookend keyed on the older run's own commit
                // sha, never reused across missions. The ordering assertion
                // below therefore now tests ordering rather than tainted data.
                serde_json::json!({"ts":"2026-01-01T08:00:00Z","action":"dispatch start","session_id":"owner/repo@cafebabe","mission_id":"review-1000000000-older","role":"deep+diff-review+probe-mid","handle":"deep+diff-review+probe-mid"}),
                serde_json::json!({"ts":"2026-01-01T08:20:00Z","action":"dispatch complete","session_id":"owner/repo@cafebabe","mission_id":"review-1000000000-older"}),
                serde_json::json!({"ts":"2026-01-01T08:20:01Z","action":"mission close","session_id":"mission-review-1000000000-older","mission_id":"review-1000000000-older"}),
                // active mission — mission-level bookend (its own unique
                // session id, embeds the mission id — never reused) plus
                // the run-level dispatch bookend (keyed on the commit sha in
                // the real report — also never reused across missions) with
                // fresh ("now") activity. Its PROBE step reuses the older
                // mission's exact session id and gets no fresh activity of
                // its own here — exactly the ambiguous, excluded-from-
                // liveness shape the real report hit.
                serde_json::json!({"ts":"2026-01-01T09:39:00Z","action":"mission start","session_id":"mission-review-2000000000-active","mission_id":"review-2000000000-active","source":"mission_lifecycle"}),
                serde_json::json!({"ts":"2026-01-01T09:39:01Z","action":"dispatch start","session_id":"owner/repo@deadbeef","mission_id":"review-2000000000-active","role":"deep+diff-review+probe-mid","handle":"deep+diff-review+probe-mid"}),
                serde_json::json!({"ts": now, "action":"telemetry.process","session_id":"owner/repo@deadbeef","mission_id":"review-2000000000-active"}),
                serde_json::json!({"ts":"2026-01-01T09:39:02Z","action":"step start","session_id":"task-review-probe-mid-task","mission_id":"review-2000000000-active","role":"review-probe-mid"}),
            ],
        );

        let runs = build_runs(flows.path(), None, &[]);
        assert_eq!(runs.len(), 2, "{runs:?}");

        let active_run =
            runs.iter().find(|r| r.id == "review-2000000000-active").expect("active mission row present");
        assert_eq!(
            active_run.status,
            RunStatus::Running,
            "an active mission with a live dispatch-level session must read Running, not vanish or read Abandoned: {active_run:?}"
        );
        assert_eq!(active_run.kind, RunKind::Mission);
        assert!(active_run.tracked);

        // (#2123 gate) The active row must sort AHEAD of the older,
        // terminal one — the client (`runsFiltered`/`runActivity`,
        // ui/src/lenses/runs/format.ts) sorts by
        // `updated_ts||completed_ts||started_ts` descending, so this is the
        // server-side half of "renders FIRST".
        let older_run = runs.iter().find(|r| r.id == "review-1000000000-older").unwrap();
        let active_key = active_run.updated_ts.or(active_run.completed_ts).or(active_run.started_ts).unwrap();
        let older_key = older_run.updated_ts.or(older_run.completed_ts).or(older_run.started_ts).unwrap();
        assert!(
            active_key > older_key,
            "the active mission must rank newer than the older, closed one: active={active_key} older={older_key}"
        );
    }

    /// (#2123) The SAME mission as above, once it later gets a `mission
    /// close` (the happy-path finalize, not the operator's actual `mission
    /// abort` — both terminals are covered elsewhere; this one locks in
    /// that a close transitions the row out of Running once the mission
    /// record itself is finalized).
    #[test]
    #[serial_test::serial]
    fn build_runs_2123_active_review_mission_reads_complete_after_mission_close() {
        let _g = CrewGuard::new();
        let flows = TempDir::new().unwrap();

        let mut mission = minimal_mission(
            "review-2000000000-active",
            vec![],
            Some(MissionSpec { config_id: "review".to_string(), inputs_fingerprint: "fpn".to_string(), origin: None }),
        );
        mission.status = MissionStatus::Finalized;
        mission.finalized_ts = Some(now_unix());
        darkmux_crew::lifecycle::save_mission(&mission).unwrap();
        // `MissionStatus::Finalized` reads its outcome off the envelope
        // (`mission_run_status`'s own doc) — Clean is what a real
        // happy-path `mission finalize` writes.
        let env = MissionEnvelope::new("review-2000000000-active", MissionOutcomeStatus::Clean, &[]);
        darkmux_crew::envelope::finalize_mission(&env);

        let runs = build_runs(flows.path(), None, &[]);
        let row = runs.iter().find(|r| r.id == "review-2000000000-active").expect("row present after close");
        assert_eq!(row.status, RunStatus::Complete, "{row:?}");
        assert_eq!(
            row.updated_ts, row.completed_ts,
            "a closed mission's sort key is its completion time, not its start — so it sorts by WHEN it finished"
        );
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

    // ─── #1711: `peer_mission_runs` — the standalone narrow entry point ────

    #[test]
    fn peer_mission_runs_matches_build_runs_peer_half_for_the_same_input() {
        // The narrow entry point must not silently diverge from the
        // aggregation `build_runs` already ships — `mission status` and
        // `/runs`/`darkmux run list` are answering the SAME question, and
        // two different answers is exactly #1711's own complaint.
        let flows = TempDir::new().unwrap();
        let fleet = vec![peer_record("mission start", &darkmux_flow::ts_utc_now())];
        let known: HashSet<String> = HashSet::new();

        let via_build_runs = build_runs(flows.path(), None, &fleet);
        let from_build_runs =
            via_build_runs.iter().find(|r| r.id == "review-on-the-hub").expect("peer row from build_runs");

        let narrow = peer_mission_runs(flows.path(), &fleet, &known);
        let from_narrow =
            narrow.iter().find(|r| r.id == "review-on-the-hub").expect("peer row from peer_mission_runs");

        assert_eq!(from_narrow.status, from_build_runs.status);
        assert_eq!(from_narrow.machine, from_build_runs.machine);
        assert_eq!(from_narrow.tracked, from_build_runs.tracked);
        assert!(!from_narrow.tracked);
    }

    #[test]
    fn peer_mission_runs_excludes_a_known_local_mission_id() {
        // The exact bug this function exists to make impossible: a caller
        // that already knows a mission is LOCAL (its own `known_mission_ids`
        // from `load_missions()`) must never see it echoed back as a "peer"
        // row just because it also has flow records.
        let flows = TempDir::new().unwrap();
        let fleet = vec![peer_record("mission start", &darkmux_flow::ts_utc_now())];
        let mut known: HashSet<String> = HashSet::new();
        known.insert("review-on-the-hub".to_string());

        let narrow = peer_mission_runs(flows.path(), &fleet, &known);
        assert!(
            !narrow.iter().any(|r| r.id == "review-on-the-hub"),
            "a mission in `known_mission_ids` must never appear as a peer row: {narrow:?}"
        );
    }

    #[test]
    fn peer_mission_runs_is_empty_with_no_fleet_records_and_no_local_orphans() {
        let flows = TempDir::new().unwrap();
        let known: HashSet<String> = HashSet::new();
        let narrow = peer_mission_runs(flows.path(), &[], &known);
        assert!(narrow.is_empty(), "{narrow:?}");
    }

    /// (#1711 review finding) A mission whose only flow records are LOCAL
    /// (in `flows_dir`, not the fleet) and stamped with THIS reader's own
    /// resolved machine id must never render as a "peer" — that machine
    /// is not a peer, it is this one. This is an orphan (no durable
    /// `Mission` JSON here for a mission that DID run here), and the
    /// honest fallback is the same untracked-ghost-dispatch synthesis any
    /// unclaimed session already gets — never a mislabeled peer row.
    #[test]
    #[serial_test::serial]
    fn peer_mission_runs_never_labels_a_same_machine_orphan_as_a_peer() {
        let prev = std::env::var("DARKMUX_MACHINE_ID").ok();
        // SAFETY: serialized via #[serial]; restored below.
        unsafe { std::env::set_var("DARKMUX_MACHINE_ID", "this-reader") };

        let flows = TempDir::new().unwrap();
        let rec = serde_json::json!({
            "ts": darkmux_flow::ts_utc_now(), "level": "info", "category": "work",
            "tier": "local", "stage": "dispatch", "action": "mission start",
            "handle": "review", "session_id": "orphan-s1", "machine_id": "this-reader",
            "mission_id": "orphan-local-1",
        });
        write_day_file(flows.path(), &today(), &[rec]);

        let known: HashSet<String> = HashSet::new();
        let narrow = peer_mission_runs(flows.path(), &[], &known);

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_MACHINE_ID", v),
                None => std::env::remove_var("DARKMUX_MACHINE_ID"),
            }
        }

        assert!(
            !narrow.iter().any(|r| r.id == "orphan-local-1"),
            "a same-machine orphan must never render as a peer row: {narrow:?}"
        );
    }

    /// The inverted case: a FOREIGN machine id on the identical shape of
    /// record must still surface. Without this, the exclusion above could
    /// have been over-broad (e.g. matching on `mission_id` alone) and this
    /// file would have no test proving peer visibility still works at all.
    #[test]
    #[serial_test::serial]
    fn peer_mission_runs_still_surfaces_a_genuinely_foreign_machine() {
        let prev = std::env::var("DARKMUX_MACHINE_ID").ok();
        unsafe { std::env::set_var("DARKMUX_MACHINE_ID", "this-reader") };

        let flows = TempDir::new().unwrap();
        let rec = serde_json::json!({
            "ts": darkmux_flow::ts_utc_now(), "level": "info", "category": "work",
            "tier": "local", "stage": "dispatch", "action": "mission start",
            "handle": "review", "session_id": "peer-s1", "machine_id": "genuinely-a-peer",
            "mission_id": "peer-local-1",
        });
        write_day_file(flows.path(), &today(), &[rec]);

        let known: HashSet<String> = HashSet::new();
        let narrow = peer_mission_runs(flows.path(), &[], &known);

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_MACHINE_ID", v),
                None => std::env::remove_var("DARKMUX_MACHINE_ID"),
            }
        }

        assert!(
            narrow.iter().any(|r| r.id == "peer-local-1"),
            "a genuinely foreign machine id must still surface as a peer row: {narrow:?}"
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
        // (#1907) No `mission abort`/`mission close` record ever landed —
        // this row's Abandoned status came purely from the staleness gate,
        // so it must read "no ending recorded", not "aborted".
        assert_eq!(row.abandoned_reason, Some(AbandonReason::NoTerminal));
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
        // (#1907) A real `mission abort` record landed — this row's reason
        // must say so explicitly, not the generic "no ending recorded" the
        // stale-peer test above pins for the OTHER Abandoned shape.
        assert_eq!(
            row.abandoned_reason,
            Some(AbandonReason::Aborted),
            "a `mission abort` record must carry the Aborted reason on the wire, not just the collapsed status"
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
