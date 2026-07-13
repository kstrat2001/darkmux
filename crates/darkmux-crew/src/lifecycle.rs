//! Phase + Mission lifecycle transitions (#95).
//!
//! Phase/Mission state lives in JSON files under `<crew_root>/phases/<id>.json`
//! and `<crew_root>/missions/<id>.json` respectively. This module provides the
//! operator-facing verbs that transition status + timestamp those entities:
//!
//! - `phase start <id>` / `phase complete <id>` / `phase abandon <id>`
//! - `mission start <id>` / `mission close <id>` / `mission pause <id>` /
//!   `mission resume <id>`
//!
//! Each transition does three things in order:
//!
//!   1. Loads the entity's JSON; validates the transition is legal per the
//!      state machine; mutates status + the appropriate `*_ts` timestamp.
//!   2. Saves the JSON atomically (write-tmp + rename) so a crash mid-write
//!      can't leave an entity in a half-written state.
//!   3. Emits a flow record describing the transition so the activity stream
//!      (consumed by `darkmux serve` + the `/flow` viewer) sees it.
//!
//! Flow record emission is non-fatal (`let _ = flow::record(...)`) — losing
//! a record never blocks the state transition. The JSON source-of-truth is
//! the authority; the activity stream is observability.
//!
//! Operator-sovereignty: nothing auto-transitions. A phase stays `Running`
//! forever until the operator runs `complete` or `abandon`. The wall-clock
//! UI (consuming this data via Track B's API exposure) renders an unbounded
//! sweep on stale `Running` phases — clearly-wrong-looking signals that
//! the operator forgot to close one. By design.
//!
//! State machines:
//!
//! Phase:
//!   | from        | start                | complete                | abandon              |
//!   |-------------|----------------------|-------------------------|----------------------|
//!   | `Planned`   | → Running ✓          | error: must start first | → Abandoned ✓        |
//!   | `Running`   | error: already       | → Complete ✓            | → Abandoned ✓        |
//!   | `Complete`  | error: terminal      | error: already          | error: complete is   |
//!   |             |                      |                         |    terminal          |
//!   | `Abandoned` | → Running ✓ ¹        | error: must start first | error: already       |
//!
//! ¹ `Abandoned → Running` clears `abandoned_ts` (the operator changed
//!   their mind; the prior abandonment isn't part of the new attempt).
//!   This is the only transition that clears a `*_ts` field.
//!
//! Mission:
//!   | from     | start             | close                | pause                 | resume                |
//!   |----------|-------------------|----------------------|-----------------------|-----------------------|
//!   | `Active` | error if          | → Closed ✓           | → Paused ✓            | error: already Active |
//!   |          | started_ts set ²  |                      |                       |                       |
//!   | `Paused` | error: use resume | → Closed ✓           | error: already Paused | → Active ³            |
//!   | `Closed` | error: terminal   | error: already       | error: terminal       | error: terminal       |
//!
//! ² `mission_start` requires a fresh start — it stamps `started_ts=now()`
//!   on first invocation and errors thereafter. Use `mission resume` to
//!   come out of a paused state; use neither to keep a still-Active
//!   mission running.
//! ³ Resume does NOT clear `paused_ts` — operator may want to see when
//!   the most recent pause occurred even after resuming.
//!
//! Missions have no `created_ts → started_ts` distinction at creation
//! time; `start` is the explicit "this mission is now being worked on"
//! transition, not "this mission exists." That's why creation alone
//! leaves `started_ts: None`.

use crate::loader::load_phases;
use crate::types::{Mission, MissionStatus, Phase, PhaseStatus};
use darkmux_flow as flow;
use darkmux_flow::{Category, FlowRecord, Level, Stage, Tier};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

// ─── Paths ──────────────────────────────────────────────────────────────
//
// Per-mission nested layout (the target — see #148):
//   <crew_root>/
//     missions/
//       <mission-id>/
//         mission.json
//         phases/
//           <phase-id>.json
//
// Legacy flat layout (pre-#148):
//   <crew_root>/
//     missions/<mission-id>.json
//     phases/<phase-id>.json
//
// Loaders + writers go through the new helpers below. The `legacy_*`
// helpers exist for the `darkmux mission migrate` verb (Task 8) and the
// `legacy-mission-layout` doctor check (Task 9). They are NOT used by any
// normal CRUD path.
//
// Test isolation: `crew_root()` honors `DARKMUX_CREW_DIR` (see
// `crew::loader::crew_root`). Tests should `std::env::set_var(
// "DARKMUX_CREW_DIR", tmp.path())` and mark themselves
// `#[serial_test::serial]` since env var mutation isn't thread-safe.

/// Directory holding the mission's JSON and its phases/ subdir.
pub(crate) fn mission_dir(mission_id: &str) -> PathBuf {
    crate::loader::missions_dir().join(mission_id)
}

/// The mission JSON path under the per-mission directory.
pub fn mission_path(mission_id: &str) -> PathBuf {
    mission_dir(mission_id).join("mission.json")
}

/// Directory holding the mission's phase JSONs.
///
/// **Sprint→Phase rename read-fallback:** pre-rename mission data nests its
/// phase JSONs under `sprints/` (the old directory name), not `phases/`. If
/// the canonical `phases/` subdir doesn't exist yet for this mission but the
/// legacy `sprints/` one does, reads (and any subsequent writes, via
/// `save_json`'s create-on-write) route there instead — same "writes follow
/// reads" convention `loader::resolve_user_subdir` uses for the Beat-33
/// flatten, so the rename never orphans an operator's real existing mission
/// data. A mission with neither subdir yet (brand new) gets the canonical
/// path so a fresh write creates the new-name layout.
pub fn phases_dir(mission_id: &str) -> PathBuf {
    let canonical = mission_dir(mission_id).join("phases");
    if canonical.is_dir() {
        return canonical;
    }
    let legacy = mission_dir(mission_id).join("sprints");
    if legacy.is_dir() {
        return legacy;
    }
    canonical
}

/// Path to a single phase JSON within a mission.
pub fn phase_path(mission_id: &str, phase_id: &str) -> PathBuf {
    phases_dir(mission_id).join(format!("{phase_id}.json"))
}

// ─── Task / Step storage (#1230 Packet 2) ──────────────────────────────
//
// Mirrors the phase layout above, nested one level deeper under the
// owning phase so a Task/Step id only has to be unique WITHIN its
// phase (matching `Task.phase_id` / `Step.task_id`'s own scoping):
//
//   <crew_root>/
//     missions/
//       <mission-id>/
//         mission.json
//         phases/
//           <phase-id>.json
//         tasks/
//           <phase-id>/
//             <task-id>.json
//         steps/
//           <phase-id>/
//             <step-id>.json
//
// One file per Task and one file per Step — never a combined DAG blob —
// so a scheduler status flip (`Planned` → `Running` → `Complete`/`Error`)
// is a small atomic `save_json` write, not a read-modify-write of the
// whole graph. Uses the same atomic-rename `save_json` every other
// entity in this module uses.

/// Directory holding a phase's Task JSONs.
pub fn tasks_dir(mission_id: &str, phase_id: &str) -> PathBuf {
    mission_dir(mission_id).join("tasks").join(phase_id)
}

/// Path to a single Task JSON within a phase.
pub fn task_path(mission_id: &str, phase_id: &str, task_id: &str) -> PathBuf {
    tasks_dir(mission_id, phase_id).join(format!("{task_id}.json"))
}

/// Directory holding a phase's Step JSONs.
pub fn steps_dir(mission_id: &str, phase_id: &str) -> PathBuf {
    mission_dir(mission_id).join("steps").join(phase_id)
}

/// Path to a single Step JSON within a phase.
pub fn step_path(mission_id: &str, phase_id: &str, step_id: &str) -> PathBuf {
    steps_dir(mission_id, phase_id).join(format!("{step_id}.json"))
}

/// Persist a Task via the same atomic-rename `save_json` every other
/// entity uses.
pub fn save_task(mission_id: &str, task: &crate::types::Task) -> Result<()> {
    save_json(&task_path(mission_id, &task.phase_id, &task.id), task)
}

/// Load a single Task by its fully-qualified (mission, phase, task)
/// coordinates.
pub fn load_task(mission_id: &str, phase_id: &str, task_id: &str) -> Result<crate::types::Task> {
    let path = task_path(mission_id, phase_id, task_id);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Load every Task under a phase. Missing `tasks/<phase-id>/` dir
/// (a phase with no Task/Step graph yet) is NOT an error — returns
/// an empty `Vec`, matching the additive/backward-compatible nature of
/// the Task/Step schema (a pre-#1230 phase has none).
pub fn load_tasks_for_phase(mission_id: &str, phase_id: &str) -> Result<Vec<crate::types::Task>> {
    load_json_dir(&tasks_dir(mission_id, phase_id))
}

/// Persist a Step via the same atomic-rename `save_json` every other
/// entity uses. The caller resolves `phase_id` (a `Step` doesn't carry
/// its own phase id directly — only `task_id`; callers hold the phase
/// id from context, e.g. the scheduler loop or the Task the step
/// belongs to) since `step_path` is scoped by phase, mirroring
/// `task_path`.
pub fn save_step(mission_id: &str, phase_id: &str, step: &crate::types::Step) -> Result<()> {
    save_json(&step_path(mission_id, phase_id, &step.id), step)
}

/// Load a single Step by its fully-qualified (mission, phase, step)
/// coordinates.
pub fn load_step(mission_id: &str, phase_id: &str, step_id: &str) -> Result<crate::types::Step> {
    let path = step_path(mission_id, phase_id, step_id);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Load every Step under a phase (across all of that phase's Tasks —
/// `steps_dir` is scoped by phase, not by task). Missing dir → empty
/// `Vec`, same convention as `load_tasks_for_phase`.
pub fn load_steps_for_phase(mission_id: &str, phase_id: &str) -> Result<Vec<crate::types::Step>> {
    load_json_dir(&steps_dir(mission_id, phase_id))
}

/// Shared "read every `*.json` file directly in `dir`, deserialize as
/// `T`" helper backing `load_tasks_for_phase`/`load_steps_for_phase`.
/// A missing directory is NOT an error (empty `Vec`) — the normal state
/// for any phase that predates the Task/Step schema or simply has no
/// graph yet.
fn load_json_dir<T: serde::de::DeserializeOwned>(dir: &std::path::Path) -> Result<Vec<T>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("reading dir {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    // Deterministic order (byte comparison over the path string):
    // downstream consumers (e.g. cycle-detection error messages) should
    // not depend on filesystem readdir ordering, which is unspecified.
    entries.sort();
    for path in entries {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let value: T = serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        out.push(value);
    }
    Ok(out)
}

/// Pre-#148 flat mission path: `<missions_dir>/<id>.json`. Two layers
/// of "legacy" stack here: this describes pre-#148 flat *content shape*,
/// while the parent `missions_dir()` itself resolves the Beat-33 dual-
/// read (canonical-first, pre-flatten-`crew/`-fallback). Held as public
/// API for symmetry with the dir helpers; the migration verb constructs
/// target paths via `mission_path(id)` and walks `legacy_missions_dir()`,
/// so this per-id resolver isn't currently called.
#[allow(dead_code)]
pub(crate) fn legacy_mission_path(mission_id: &str) -> PathBuf {
    crate::loader::missions_dir().join(format!("{mission_id}.json"))
}

/// Pre-#148 flat phase path: `<phases_dir>/<id>.json`. Held as
/// public API for symmetry with the dir helpers; see
/// `legacy_mission_path` for why.
#[allow(dead_code)]
pub(crate) fn legacy_phase_path(phase_id: &str) -> PathBuf {
    crate::loader::phases_dir().join(format!("{phase_id}.json"))
}

/// Pre-#148 flat missions dir: `<missions_dir>/` (containing flat
/// `<id>.json` files at the top level).
pub fn legacy_missions_dir() -> PathBuf {
    crate::loader::missions_dir()
}

/// Pre-#148 flat phases dir: `<phases_dir>/`.
pub fn legacy_phases_dir() -> PathBuf {
    crate::loader::phases_dir()
}

// ─── Time ───────────────────────────────────────────────────────────────

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─── Atomic save ────────────────────────────────────────────────────────

/// Durable atomic-rename save: serialize → write `.json.tmp` with mode
/// `0o600` + fsync → rename onto target → fsync parent directory. The
/// fsyncs (Wave-E.13 / PR-B M-2) make the rename durable across power
/// failure; without them a crash between rename(2) and the next
/// dirty-page flush could leave the directory entry inconsistent.
fn save_json<T: serde::Serialize>(path: &std::path::Path, value: &T) -> Result<()> {
    let parent = path.parent().map(|p| p.to_path_buf());
    if let Some(parent) = parent.as_ref() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(value)
        .with_context(|| format!("serializing to {}", path.display()))?;
    let tmp = path.with_extension("json.tmp");
    write_owner_only(&tmp, (body + "\n").as_bytes())
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    if let Some(parent) = parent {
        fsync_dir(&parent)
            .with_context(|| format!("fsync parent dir {}", parent.display()))?;
    }
    Ok(())
}

/// Write `bytes` to `path` with mode `0o600` on POSIX (owner read/write
/// only) AND `fsync(2)` the file before the handle drops. Falls back to
/// the default umask-respecting write on non-POSIX. Wave-E.11 added the
/// mode; Wave-E.13 added the sync_all call so the doc claim about
/// "durable atomic rename" actually holds across power failure.
fn write_owner_only(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("opening {} for owner-only write", path.display()))?;
        // Defensive: enforce mode even on pre-existing files (where
        // `OpenOptions::mode` is a no-op).
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)
            .with_context(|| format!("setting 0o600 on {}", path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing bytes to {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("fsync {}", path.display()))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, bytes)
            .with_context(|| format!("writing {}", path.display()))
    }
}

/// `fsync(2)` a directory so a rename(2) that landed inside it reaches
/// stable storage. POSIX-only — on non-Unix this is a no-op.
fn fsync_dir(dir: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        let f = fs::File::open(dir)
            .with_context(|| format!("open dir {} for fsync", dir.display()))?;
        f.sync_all()
            .with_context(|| format!("sync_all on dir {}", dir.display()))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

// ─── Load-by-id ────────────────────────────────────────────────────────

/// Load a phase by its fully-qualified (mission, phase) coordinates. O(1).
/// Currently every internal caller routes through `load_phase_by_id` (since
/// the CLI verbs accept a bare phase id and discover the mission from the
/// JSON itself). Kept as the primary public surface for code that *does*
/// have both ids in scope (e.g., crew dispatch when `--mission` lands).
#[allow(dead_code)]
pub(crate) fn load_phase(mission_id: &str, phase_id: &str) -> Result<Phase> {
    let path = phase_path(mission_id, phase_id);
    if !path.is_file() {
        anyhow::bail!(
            "no phase with id `{phase_id}` found at {} (mission `{mission_id}`)",
            path.display()
        );
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let phase: Phase = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(phase)
}

/// Load a phase by phase-id alone — scans every mission's phases dir
/// and returns the first match. Used by the operator CLI which accepts
/// `darkmux phase start <id>` without a mission qualifier.
///
/// If two missions both contain a phase with this id (possible only
/// during a partial #148 migration since post-migration the uniqueness
/// is per-mission), the first match in `(mission_id, phase_id)` sort
/// order wins and a flow record is emitted.
pub(crate) fn load_phase_by_id(phase_id: &str) -> Result<Phase> {
    let all = load_phases()?;
    let mut matches: Vec<&Phase> = all.iter().filter(|s| s.id == phase_id).collect();
    match matches.as_slice() {
        [] => anyhow::bail!(
            "no phase with id `{phase_id}` found in any mission under {}",
            crate::loader::missions_dir().display()
        ),
        [_one] => Ok(matches.remove(0).clone()),
        _ => {
            // Multiple matches — should be impossible post-migration. Pick
            // deterministically and emit a warning flow record.
            matches.sort_by_key(|s| (s.mission_id.as_str(), s.id.as_str()));
            let chosen = matches[0];
            let count = matches.len();
            let _ = flow::record(FlowRecord {
                ts: flow::ts_utc_now(),
                level: Level::Warn,
                category: Category::Machinery,
                tier: Tier::Operator,
                stage: Stage::Scope,
                action: "ambiguous-phase-id".into(),
                handle: format!(
                    "phase id `{phase_id}` found in {count} missions; using `{}`",
                    chosen.mission_id
                ),
                phase_id: Some(phase_id.to_string()),
                session_id: None,
                source: Some("phase_lifecycle".to_string()),
                model: None,
                reasoning: None,
                mission_id: Some(chosen.mission_id.clone()),
                machine_id: None,
                machine_uid: None,
                orchestrator: None,
                prev_hash: None,
                hash: None,
                payload: None,
                work_id: None,
                attempt: None,
            });
            Ok(chosen.clone())
        }
    }
}

fn load_mission(id: &str) -> Result<Mission> {
    let path = mission_path(id);
    if !path.is_file() {
        anyhow::bail!("no mission with id `{id}` found at {}", path.display());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let mission: Mission = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(mission)
}

// ─── Flow record emission ──────────────────────────────────────────────

fn emit_phase_transition_record(phase_id: &str, mission_id: &str, action: &str) {
    let _ = flow::record(FlowRecord {
        ts: flow::ts_utc_now(),
        level: Level::Info,
        category: Category::Work,
        tier: Tier::Operator,
        stage: Stage::Scope,
        action: action.to_string(),
        handle: phase_id.to_string(),
        phase_id: Some(phase_id.to_string()),
        session_id: Some(format!("mission:{mission_id}")),
        source: Some("phase_lifecycle".to_string()),
        model: None,
        reasoning: None,
        // #136 schema 1.3: first-class mission_id. The session_id
        // workaround above (`mission:<id>`) stays in place this PR for
        // back-compat with existing readers; future readers should
        // prefer this field. The redundancy retires once readers
        // migrate (follow-up).
        mission_id: Some(mission_id.to_string()),
        machine_id: None,
        machine_uid: None,
        orchestrator: None,
        prev_hash: None,
        hash: None,
        payload: None,
        work_id: None,
        attempt: None,
    });
}

#[allow(dead_code)]
// Kept for back-compat + test ergonomics; CLI path uses
// `emit_mission_transition_record_with_reasoning` directly.
fn emit_mission_transition_record(mission_id: &str, action: &str) {
    emit_mission_transition_record_with_reasoning(mission_id, action, None);
}

/// Reasoning-aware variant. Optional operator-supplied prose explains
/// *why* the mission transition happened — populates the audit substrate's
/// WHY layer for lifecycle events, parallel to tier-decision records
/// for routing events (#136).
fn emit_mission_transition_record_with_reasoning(
    mission_id: &str,
    action: &str,
    reasoning: Option<&str>,
) {
    let _ = flow::record(FlowRecord {
        ts: flow::ts_utc_now(),
        level: Level::Info,
        category: Category::Work,
        tier: Tier::Operator,
        stage: Stage::Scope,
        action: action.to_string(),
        handle: mission_id.to_string(),
        phase_id: None,
        session_id: Some(format!("mission:{mission_id}")),
        source: Some("mission_lifecycle".to_string()),
        model: None,
        reasoning: reasoning.map(String::from),
        mission_id: Some(mission_id.to_string()),
        machine_id: None,
        machine_uid: None,
        orchestrator: None,
        prev_hash: None,
        hash: None,
        payload: None,
        work_id: None,
        attempt: None,
    });
}

/// Distinct from `emit_phase_transition_record` (which uses
/// `source: phase_lifecycle` for status flips on an already-tracked
/// phase). This one fires when the *mission's shape* changes — a
/// new phase joining the plan — so the `source` field reflects that
/// the change came from the mission_lifecycle surface even though the
/// `handle` is the new phase id.
#[allow(dead_code)]
// Kept for back-compat + test ergonomics; CLI path uses
// `emit_phase_added_record_with_reasoning` directly.
fn emit_phase_added_record(phase_id: &str, mission_id: &str) {
    emit_phase_added_record_with_reasoning(phase_id, mission_id, None);
}

fn emit_phase_added_record_with_reasoning(
    phase_id: &str,
    mission_id: &str,
    reasoning: Option<&str>,
) {
    let _ = flow::record(FlowRecord {
        ts: flow::ts_utc_now(),
        level: Level::Info,
        category: Category::Work,
        tier: Tier::Operator,
        stage: Stage::Scope,
        action: "phase added".to_string(),
        handle: phase_id.to_string(),
        phase_id: Some(phase_id.to_string()),
        session_id: Some(format!("mission:{mission_id}")),
        source: Some("mission_lifecycle".to_string()),
        model: None,
        reasoning: reasoning.map(String::from),
        mission_id: Some(mission_id.to_string()),
        machine_id: None,
        machine_uid: None,
        orchestrator: None,
        prev_hash: None,
        hash: None,
        payload: None,
        work_id: None,
        attempt: None,
    });
}

// ─── Phase transitions ─────────────────────────────────────────────────

/// `phase start <id>` — Planned/Abandoned → Running.
/// Sets `started_ts = now()`; clears `abandoned_ts` if it was set.
pub fn phase_start(id: &str) -> Result<Phase> {
    let mut phase = load_phase_by_id(id)?;
    match phase.status {
        PhaseStatus::Planned | PhaseStatus::Abandoned => {}
        PhaseStatus::Running => bail!("phase `{id}` is already Running"),
        PhaseStatus::Complete => bail!("phase `{id}` is Complete (terminal) — create a new phase instead"),
    }
    phase.status = PhaseStatus::Running;
    phase.started_ts = Some(now_unix());
    phase.abandoned_ts = None; // restart clears the prior abandonment
    save_json(&phase_path(&phase.mission_id, id), &phase)?;
    emit_phase_transition_record(id, &phase.mission_id, "phase start");
    Ok(phase)
}

/// `phase complete <id>` — Running → Complete (terminal).
pub fn phase_complete(id: &str) -> Result<Phase> {
    let mut phase = load_phase_by_id(id)?;
    match phase.status {
        PhaseStatus::Running => {}
        PhaseStatus::Planned => bail!("phase `{id}` is Planned — must `phase start` first"),
        PhaseStatus::Abandoned => bail!("phase `{id}` is Abandoned — `phase start` to restart, then complete"),
        PhaseStatus::Complete => bail!("phase `{id}` is already Complete"),
    }
    phase.status = PhaseStatus::Complete;
    phase.completed_ts = Some(now_unix());
    save_json(&phase_path(&phase.mission_id, id), &phase)?;
    emit_phase_transition_record(id, &phase.mission_id, "phase complete");
    Ok(phase)
}

/// `phase abandon <id>` — Planned/Running → Abandoned.
/// Cannot abandon a `Complete` phase (terminal in the other direction).
pub fn phase_abandon(id: &str) -> Result<Phase> {
    let mut phase = load_phase_by_id(id)?;
    match phase.status {
        PhaseStatus::Planned | PhaseStatus::Running => {}
        PhaseStatus::Abandoned => bail!("phase `{id}` is already Abandoned"),
        PhaseStatus::Complete => bail!("phase `{id}` is Complete — can't abandon a finished phase"),
    }
    phase.status = PhaseStatus::Abandoned;
    phase.abandoned_ts = Some(now_unix());
    save_json(&phase_path(&phase.mission_id, id), &phase)?;
    emit_phase_transition_record(id, &phase.mission_id, "phase abandon");
    Ok(phase)
}

// ─── Mission transitions ───────────────────────────────────────────────

/// `mission start <id>` — the explicit "begin working on this mission" verb.
/// Requires a fresh start: errors when `started_ts` is already set (the
/// mission has been started before). Sets `started_ts = now()` on success.
///
/// This is NOT idempotent — calling it twice raises an error. The reason:
/// `started_ts` should reflect a single ground-truth instant; overwriting
/// it on every call would erase the start time the operator presumably
/// cares about. To restart a Paused mission, use `mission resume`. To
/// continue an already-running mission, do nothing (status is preserved
/// across other lifecycle operations).
#[allow(dead_code)]
pub fn mission_start(id: &str) -> Result<Mission> {
    mission_start_with_reasoning(id, None)
}

pub fn mission_start_with_reasoning(id: &str, reasoning: Option<&str>) -> Result<Mission> {
    let mut mission = load_mission(id)?;
    match mission.status {
        MissionStatus::Active if mission.started_ts.is_some() => {
            bail!("mission `{id}` is already Active and was started at ts={:?}", mission.started_ts)
        }
        MissionStatus::Paused => bail!("mission `{id}` is Paused — use `mission resume` instead"),
        MissionStatus::Closed => bail!("mission `{id}` is Closed (terminal) — create a new mission instead"),
        MissionStatus::Active => {}
    }
    mission.status = MissionStatus::Active;
    mission.started_ts = Some(now_unix());
    save_json(&mission_path(id), &mission)?;
    emit_mission_transition_record_with_reasoning(id, "mission start", reasoning);
    Ok(mission)
}

/// `mission close <id>` — Active/Paused → Closed (terminal).
#[allow(dead_code)]
pub(crate) fn mission_close(id: &str) -> Result<Mission> {
    mission_close_with_reasoning(id, None)
}

pub fn mission_close_with_reasoning(id: &str, reasoning: Option<&str>) -> Result<Mission> {
    let mut mission = load_mission(id)?;
    match mission.status {
        MissionStatus::Active | MissionStatus::Paused => {}
        MissionStatus::Closed => bail!("mission `{id}` is already Closed"),
    }
    mission.status = MissionStatus::Closed;
    mission.closed_ts = Some(now_unix());
    save_json(&mission_path(id), &mission)?;
    emit_mission_transition_record_with_reasoning(id, "mission close", reasoning);
    Ok(mission)
}

/// `mission pause <id>` — Active → Paused. Updates `paused_ts` to now even
/// if a prior pause was recorded (operator gets the most-recent pause time).
#[allow(dead_code)]
pub(crate) fn mission_pause(id: &str) -> Result<Mission> {
    mission_pause_with_reasoning(id, None)
}

pub fn mission_pause_with_reasoning(id: &str, reasoning: Option<&str>) -> Result<Mission> {
    let mut mission = load_mission(id)?;
    match mission.status {
        MissionStatus::Active => {}
        MissionStatus::Paused => bail!("mission `{id}` is already Paused"),
        MissionStatus::Closed => bail!("mission `{id}` is Closed — can't pause a finished mission"),
    }
    mission.status = MissionStatus::Paused;
    mission.paused_ts = Some(now_unix());
    save_json(&mission_path(id), &mission)?;
    emit_mission_transition_record_with_reasoning(id, "mission pause", reasoning);
    Ok(mission)
}

/// `mission resume <id>` — Paused → Active. Does NOT clear `paused_ts` —
/// the operator may want to see how long the mission was paused.
#[allow(dead_code)]
pub(crate) fn mission_resume(id: &str) -> Result<Mission> {
    mission_resume_with_reasoning(id, None)
}

pub fn mission_resume_with_reasoning(id: &str, reasoning: Option<&str>) -> Result<Mission> {
    let mut mission = load_mission(id)?;
    match mission.status {
        MissionStatus::Paused => {}
        MissionStatus::Active => bail!("mission `{id}` is already Active"),
        MissionStatus::Closed => bail!("mission `{id}` is Closed — can't resume a finished mission"),
    }
    mission.status = MissionStatus::Active;
    save_json(&mission_path(id), &mission)?;
    emit_mission_transition_record_with_reasoning(id, "mission resume", reasoning);
    Ok(mission)
}

// ─── Mission scope growth ──────────────────────────────────────────────

/// `mission add-phase` — operator-sovereign scope growth (#107).
///
/// Adds a new Phase to an existing Mission mid-flight. The alternative
/// today is one of: (a) hand-edit the Mission JSON to append a phase id
/// and hand-author a new Phase JSON, (b) create a separate Mission that
/// loses the *"this composes with what we're already doing"* signal, or
/// (c) leave the discovery as a GH issue with no Mission/Phase
/// representation. None reflect how engineering work actually evolves
/// during a phase.
///
/// Behavior:
///   - Writes a new Phase JSON at `<crew_root>/phases/<phase-id>.json`
///     with `status: Planned`, `mission_id`, `description`,
///     `created_ts: now()`.
///   - Inserts the phase id into the Mission's `phase_ids` array at
///     `after`'s position (or appends — see `resolve_insert_position`),
///     atomic write per the existing `save_json` semantics. (#1341)
///     Phases are strictly linear — list POSITION is the only ordering;
///     there is no separate dependency declaration.
///   - Emits a `phase added` flow record (tier=operator, source=
///     mission_lifecycle) so the addition is observable in the viewer.
///
/// Idempotent on EXACT match: re-adding a phase with the same id +
/// mission + description is a no-op (no error). The phase_ids array
/// gets the phase appended if it had drifted off (defensive). Non-
/// matching descriptions error — we don't silently overwrite operator
/// content.
///
/// Errors loudly when:
///   - Mission doesn't exist.
///   - Phase id is already in use under a *different* mission (would
///     break the unique-id-per-phase invariant the loader relies on).
///   - Phase id is in use under SAME mission but with different
///     description (operator probably meant a different id or wants to
///     explicitly edit; either way, don't paper over the conflict).
///   - `after` names a phase id not already in the mission's `phase_ids`.
#[allow(dead_code)]
pub(crate) fn add_phase_to_mission(
    mission_id: &str,
    phase_id: &str,
    description: &str,
    after: Option<&str>,
) -> Result<Phase> {
    add_phase_to_mission_with_reasoning(mission_id, phase_id, description, after, None)
}

/// `add_phase_to_mission` with operator-supplied reasoning for the
/// scope growth. Reasoning lands on the emitted flow record so the
/// audit substrate captures *why* the mission grew here.
pub fn add_phase_to_mission_with_reasoning(
    mission_id: &str,
    phase_id: &str,
    description: &str,
    after: Option<&str>,
    reasoning: Option<&str>,
) -> Result<Phase> {
    let mut mission = load_mission(mission_id)?;

    let all_phases = load_phases()?;

    // Idempotency / collision check.
    if let Some(existing) = all_phases.iter().find(|s| s.id == phase_id) {
        if existing.mission_id != mission_id {
            bail!(
                "phase id `{phase_id}` already in use under mission `{}`; pick a fresh id",
                existing.mission_id
            );
        }
        if existing.description != description {
            bail!(
                "phase id `{phase_id}` already exists under this mission with a different description; \
                 edit the existing JSON or pick a fresh id"
            );
        }
        // Exact match — idempotent path. Defensive: ensure the mission's
        // phase_ids still includes this id (operator may have hand-
        // edited the JSON and removed it). Idempotent re-adds do NOT
        // reposition; the operator has to remove the existing entry
        // first if they want a different position. That keeps re-runs
        // safe even when `--after` is non-default.
        let already_listed = mission.phase_ids.iter().any(|s| s == phase_id);
        if !already_listed {
            let pos = resolve_insert_position(&mission.phase_ids, after)?;
            mission.phase_ids.insert(pos, phase_id.to_string());
            save_json(&mission_path(mission_id), &mission)?;
        }
        return Ok(existing.clone());
    }

    // Pre-validate the `--after` target BEFORE writing any state, so a
    // typo'd `--after` doesn't leave a phase JSON on disk that's not
    // referenced from any mission. Resolve to the insertion index now,
    // then use it after the phase JSON is written.
    let insert_pos = resolve_insert_position(&mission.phase_ids, after)?;

    let phase = Phase {
        id: phase_id.to_string(),
        mission_id: mission_id.to_string(),
        description: description.to_string(),
        status: PhaseStatus::Planned,
        created_ts: now_unix(),
        started_ts: None,
        completed_ts: None,
        abandoned_ts: None,
        task_ids: Vec::new(),
    };
    save_json(&phase_path(mission_id, phase_id), &phase)?;

    // Position into the mission's phase_ids. Belt-and-suspenders: the
    // collision check above means we shouldn't see a duplicate here,
    // but the idempotent guard makes the operation safe to retry on
    // partial-failure paths.
    if !mission.phase_ids.iter().any(|s| s == phase_id) {
        mission.phase_ids.insert(insert_pos, phase_id.to_string());
        save_json(&mission_path(mission_id), &mission)?;
    }

    emit_phase_added_record_with_reasoning(phase_id, mission_id, reasoning);

    Ok(phase)
}

/// Compute the index at which a new phase should be inserted into
/// the mission's `phase_ids` array. Pure — does NOT mutate the
/// vector. Pre-validates `--after` so callers can write the new
/// phase JSON without risking an orphan record when the reference
/// is stale.
///
/// When `after` is `None`, returns `phase_ids.len()` (append). When
/// `after` names a present phase, returns the index immediately
/// after it. Errors when `after` names an absent phase — silently
/// appending would obscure a typo or stale reference, which violates
/// operator sovereignty (don't substitute system judgment for
/// operator intent).
fn resolve_insert_position(phase_ids: &[String], after: Option<&str>) -> Result<usize> {
    match after {
        None => Ok(phase_ids.len()),
        Some(target) => {
            let pos = phase_ids
                .iter()
                .position(|s| s == target)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "--after references phase `{target}` which isn't in this mission's phase_ids; \
                         pick an id that's already in the plan (or omit --after to append)"
                    )
                })?;
            Ok(pos + 1)
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Mission, MissionStatus, Phase, PhaseStatus};
    use std::env;
    use tempfile::TempDir;

    /// Test fixture: temp crew_root + DARKMUX_FLOWS_DIR so flow records
    /// emitted during transitions don't pollute the operator's actual
    /// flow records. Both env vars get restored on drop.
    struct CrewGuard {
        _tmp_crew: TempDir,
        _tmp_flows: TempDir,
        prev_crew: Option<String>,
        prev_flows: Option<String>,
    }

    impl CrewGuard {
        fn new() -> Self {
            let tmp_crew = TempDir::new().unwrap();
            let tmp_flows = TempDir::new().unwrap();
            let prev_crew = env::var("DARKMUX_CREW_DIR").ok();
            let prev_flows = env::var("DARKMUX_FLOWS_DIR").ok();
            // SAFETY: serialized via `#[serial_test::serial]` on every test
            // that uses CrewGuard; matches the FlowsDirGuard pattern.
            unsafe {
                env::set_var("DARKMUX_CREW_DIR", tmp_crew.path());
                env::set_var("DARKMUX_FLOWS_DIR", tmp_flows.path());
            }
            Self {
                _tmp_crew: tmp_crew,
                _tmp_flows: tmp_flows,
                prev_crew,
                prev_flows,
            }
        }
    }

    impl Drop for CrewGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_crew {
                    Some(v) => env::set_var("DARKMUX_CREW_DIR", v),
                    None => env::remove_var("DARKMUX_CREW_DIR"),
                }
                match &self.prev_flows {
                    Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
                    None => env::remove_var("DARKMUX_FLOWS_DIR"),
                }
            }
        }
    }

    fn seed_phase(id: &str, status: PhaseStatus) -> Phase {
        let s = Phase {
            id: id.to_string(),
            mission_id: "test-mission".to_string(),
            description: "test phase".to_string(),
            status,
            created_ts: 1_700_000_000,
            started_ts: None,
            completed_ts: None,
            abandoned_ts: None,
            task_ids: Vec::new(),
        };
        save_json(&phase_path("test-mission", id), &s).unwrap();
        s
    }

    fn seed_mission(id: &str, status: MissionStatus) -> Mission {
        let m = Mission {
            id: id.to_string(),
            description: "test mission".to_string(),
            status,
            phase_ids: Vec::new(),
            created_ts: 1_700_000_000,
            started_ts: None,
            closed_ts: None,
            paused_ts: None,
            source_input: None,
            ticket: None,
        };
        save_json(&mission_path(id), &m).unwrap();
        m
    }

    // ─── Phase state machine ─────────────────────────────────────────

    #[serial_test::serial]
    #[test]
    fn phase_start_from_planned_sets_running_and_started_ts() {
        let _g = CrewGuard::new();
        seed_phase("s1", PhaseStatus::Planned);

        let updated = phase_start("s1").unwrap();

        assert_eq!(updated.status, PhaseStatus::Running);
        assert!(updated.started_ts.is_some());
        assert!(updated.completed_ts.is_none());
        assert!(updated.abandoned_ts.is_none());
    }

    #[serial_test::serial]
    #[test]
    fn phase_start_from_running_errors() {
        let _g = CrewGuard::new();
        seed_phase("s2", PhaseStatus::Running);
        let err = phase_start("s2").unwrap_err();
        assert!(err.to_string().contains("already Running"));
    }

    #[serial_test::serial]
    #[test]
    fn phase_start_from_complete_errors() {
        let _g = CrewGuard::new();
        seed_phase("s3", PhaseStatus::Complete);
        let err = phase_start("s3").unwrap_err();
        assert!(err.to_string().contains("Complete"));
    }

    #[serial_test::serial]
    #[test]
    fn phase_start_from_abandoned_clears_abandoned_ts() {
        let _g = CrewGuard::new();
        let mut s = seed_phase("s4", PhaseStatus::Abandoned);
        s.abandoned_ts = Some(1_700_000_500);
        save_json(&phase_path("test-mission", "s4"), &s).unwrap();

        let updated = phase_start("s4").unwrap();
        assert_eq!(updated.status, PhaseStatus::Running);
        assert!(updated.started_ts.is_some());
        assert!(updated.abandoned_ts.is_none(), "restart clears abandoned_ts");
    }

    #[serial_test::serial]
    #[test]
    fn phase_complete_from_running_sets_complete_and_completed_ts() {
        let _g = CrewGuard::new();
        let mut s = seed_phase("s5", PhaseStatus::Running);
        s.started_ts = Some(1_700_000_100);
        save_json(&phase_path("test-mission", "s5"), &s).unwrap();

        let updated = phase_complete("s5").unwrap();
        assert_eq!(updated.status, PhaseStatus::Complete);
        assert!(updated.completed_ts.is_some());
        assert!(updated.completed_ts.unwrap() >= updated.started_ts.unwrap());
    }

    #[serial_test::serial]
    #[test]
    fn phase_complete_from_planned_errors() {
        let _g = CrewGuard::new();
        seed_phase("s6", PhaseStatus::Planned);
        let err = phase_complete("s6").unwrap_err();
        assert!(err.to_string().contains("Planned"));
    }

    #[serial_test::serial]
    #[test]
    fn phase_abandon_from_running_sets_abandoned_and_abandoned_ts() {
        let _g = CrewGuard::new();
        seed_phase("s7", PhaseStatus::Running);
        let updated = phase_abandon("s7").unwrap();
        assert_eq!(updated.status, PhaseStatus::Abandoned);
        assert!(updated.abandoned_ts.is_some());
    }

    #[serial_test::serial]
    #[test]
    fn phase_abandon_from_complete_errors() {
        let _g = CrewGuard::new();
        seed_phase("s8", PhaseStatus::Complete);
        let err = phase_abandon("s8").unwrap_err();
        assert!(err.to_string().contains("Complete"));
    }

    #[serial_test::serial]
    #[test]
    fn phase_round_trip_records_durations() {
        let _g = CrewGuard::new();
        seed_phase("s9", PhaseStatus::Planned);

        let s1 = phase_start("s9").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let s2 = phase_complete("s9").unwrap();

        let started = s1.started_ts.expect("started_ts after start");
        let completed = s2.completed_ts.expect("completed_ts after complete");
        assert!(completed > started, "completed_ts ({completed}) > started_ts ({started})");
    }

    // ─── Mission state machine ────────────────────────────────────────

    #[serial_test::serial]
    #[test]
    fn mission_start_sets_active_and_started_ts() {
        let _g = CrewGuard::new();
        seed_mission("m1", MissionStatus::Active);
        let updated = mission_start("m1").unwrap();
        assert_eq!(updated.status, MissionStatus::Active);
        assert!(updated.started_ts.is_some());
    }

    #[serial_test::serial]
    #[test]
    fn mission_start_errors_when_already_started() {
        let _g = CrewGuard::new();
        seed_mission("m2", MissionStatus::Active);
        // First start sets the timestamp.
        let _ = mission_start("m2").unwrap();
        // Second start should error (already started — started_ts is set).
        let err = mission_start("m2").unwrap_err();
        assert!(err.to_string().contains("already Active"));
    }

    #[serial_test::serial]
    #[test]
    fn mission_start_from_paused_errors_with_resume_hint() {
        let _g = CrewGuard::new();
        seed_mission("m3", MissionStatus::Paused);
        let err = mission_start("m3").unwrap_err();
        assert!(err.to_string().contains("resume"));
    }

    #[serial_test::serial]
    #[test]
    fn mission_close_from_active_sets_closed_and_closed_ts() {
        let _g = CrewGuard::new();
        seed_mission("m4", MissionStatus::Active);
        let updated = mission_close("m4").unwrap();
        assert_eq!(updated.status, MissionStatus::Closed);
        assert!(updated.closed_ts.is_some());
    }

    #[serial_test::serial]
    #[test]
    fn mission_close_terminal_state_errors() {
        let _g = CrewGuard::new();
        seed_mission("m5", MissionStatus::Closed);
        let err = mission_close("m5").unwrap_err();
        assert!(err.to_string().contains("already Closed"));
    }

    #[serial_test::serial]
    #[test]
    fn mission_pause_resume_preserves_paused_ts() {
        let _g = CrewGuard::new();
        seed_mission("m6", MissionStatus::Active);

        let paused = mission_pause("m6").unwrap();
        assert_eq!(paused.status, MissionStatus::Paused);
        let original_paused_ts = paused.paused_ts.expect("paused_ts after pause");

        let resumed = mission_resume("m6").unwrap();
        assert_eq!(resumed.status, MissionStatus::Active);
        // Resume does NOT clear paused_ts — operator wants to see most-recent pause time.
        assert_eq!(
            resumed.paused_ts,
            Some(original_paused_ts),
            "resume preserves paused_ts; got {:?}, expected {:?}",
            resumed.paused_ts,
            paused.paused_ts,
        );
    }

    #[serial_test::serial]
    #[test]
    fn mission_resume_from_active_errors() {
        let _g = CrewGuard::new();
        seed_mission("m7", MissionStatus::Active);
        let err = mission_resume("m7").unwrap_err();
        assert!(err.to_string().contains("already Active"));
    }

    // ─── Error paths ──────────────────────────────────────────────────

    #[serial_test::serial]
    #[test]
    fn phase_start_on_missing_id_errors() {
        let _g = CrewGuard::new();
        let err = phase_start("does-not-exist").unwrap_err();
        assert!(err.to_string().contains("no phase with id"));
    }

    #[serial_test::serial]
    #[test]
    fn mission_start_on_missing_id_errors() {
        let _g = CrewGuard::new();
        let err = mission_start("does-not-exist").unwrap_err();
        assert!(err.to_string().contains("no mission with id"));
    }

    #[serial_test::serial]
    #[test]
    fn save_json_atomic_via_tmp_rename() {
        // After a save, the .tmp file should not be left behind.
        let _g = CrewGuard::new();
        seed_phase("s-atomic", PhaseStatus::Planned);
        let _ = phase_start("s-atomic").unwrap();
        let tmp_path = phase_path("test-mission", "s-atomic").with_extension("json.tmp");
        assert!(!tmp_path.exists(), "atomic save should rename, leaving no .tmp");
    }

    // ─── Sprint→Phase rename: existing real-data compat ────────────────

    /// End-to-end: a phase written under the legacy `sprints/` dir name
    /// (simulating an operator's real pre-rename mission on disk) is fully
    /// operable through the lifecycle verbs — `load_phase_by_id` finds it
    /// and `phase_start` writes the transition back to the SAME legacy
    /// dir (writes follow reads; no silent state split, no orphaned data).
    #[serial_test::serial]
    #[test]
    fn phase_lifecycle_operates_on_legacy_sprints_dir_data() {
        let _g = CrewGuard::new();
        let legacy_dir = crate::loader::missions_dir().join("legacy-mission").join("sprints");
        fs::create_dir_all(&legacy_dir).unwrap();
        let s = Phase {
            id: "legacy-phase".to_string(),
            mission_id: "legacy-mission".to_string(),
            description: "pre-rename phase".to_string(),
            status: PhaseStatus::Planned,
            created_ts: 1_700_000_000,
            started_ts: None,
            completed_ts: None,
            abandoned_ts: None,
            task_ids: Vec::new(),
        };
        save_json(&legacy_dir.join("legacy-phase.json"), &s).unwrap();

        let loaded = load_phase_by_id("legacy-phase").expect("legacy phase should be discoverable");
        assert_eq!(loaded.mission_id, "legacy-mission");

        let updated = phase_start("legacy-phase").expect("phase_start should operate on legacy data");
        assert_eq!(updated.status, PhaseStatus::Running);
        // The transition must have been written back into the SAME legacy
        // dir, not a freshly-created `phases/` dir — no silent state split.
        assert!(legacy_dir.join("legacy-phase.json").exists());
        assert!(!crate::loader::missions_dir().join("legacy-mission").join("phases").exists());
    }

    /// A mission.json written with the pre-rename `sprint_ids` wire key
    /// (instead of the canonical `phase_ids`) deserializes correctly via
    /// `#[serde(alias = "sprint_ids")]` — an operator's real existing
    /// mission JSON isn't silently emptied of its phase list.
    #[serial_test::serial]
    #[test]
    fn mission_json_with_legacy_sprint_ids_key_deserializes() {
        let _g = CrewGuard::new();
        let legacy_json = r#"{
            "id": "legacy-mission-2",
            "description": "pre-rename mission",
            "status": "active",
            "sprint_ids": ["s1", "s2"],
            "created_ts": 1700000000
        }"#;
        let m: Mission = serde_json::from_str(legacy_json).expect("legacy sprint_ids key must parse");
        assert_eq!(m.phase_ids, vec!["s1".to_string(), "s2".to_string()]);
    }

    // ─── add_phase_to_mission (#107) ───────────────────────────────────

    #[serial_test::serial]
    #[test]
    fn add_phase_creates_planned_phase_and_extends_mission() {
        let _g = CrewGuard::new();
        let mission = seed_mission("test-mission", MissionStatus::Active);
        assert!(mission.phase_ids.is_empty(), "starting state: empty phase list");

        let s = add_phase_to_mission(
            "test-mission",
            "new-phase",
            "discovered mid-flight",
            None,
        )
        .unwrap();

        assert_eq!(s.status, PhaseStatus::Planned);
        assert_eq!(s.mission_id, "test-mission");
        assert_eq!(s.description, "discovered mid-flight");
        assert!(s.started_ts.is_none());
        // Phase JSON exists on disk.
        assert!(phase_path("test-mission", "new-phase").exists());
        // Mission JSON's phase_ids was updated.
        let reloaded = load_mission("test-mission").unwrap();
        assert_eq!(reloaded.phase_ids, vec!["new-phase".to_string()]);
    }

    #[serial_test::serial]
    #[test]
    fn add_phase_is_idempotent_on_exact_match() {
        let _g = CrewGuard::new();
        seed_mission("m1", MissionStatus::Active);

        let first = add_phase_to_mission("m1", "s-once", "same desc", None).unwrap();
        let second = add_phase_to_mission("m1", "s-once", "same desc", None).unwrap();

        // Same created_ts on the second call — we returned the existing phase, not a fresh one.
        assert_eq!(first.created_ts, second.created_ts);
        // Mission's phase_ids contains the id once, not twice.
        let m = load_mission("m1").unwrap();
        assert_eq!(m.phase_ids.iter().filter(|s| *s == "s-once").count(), 1);
    }

    #[serial_test::serial]
    #[test]
    fn add_phase_errors_when_id_collides_across_missions() {
        let _g = CrewGuard::new();
        seed_mission("m-a", MissionStatus::Active);
        seed_mission("m-b", MissionStatus::Active);
        add_phase_to_mission("m-a", "shared", "first", None).unwrap();

        let err = add_phase_to_mission("m-b", "shared", "second", None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("already in use under mission `m-a`"), "got: {msg}");
    }

    #[serial_test::serial]
    #[test]
    fn add_phase_errors_when_same_id_has_different_description() {
        let _g = CrewGuard::new();
        seed_mission("m1", MissionStatus::Active);
        add_phase_to_mission("m1", "s1", "original", None).unwrap();

        let err = add_phase_to_mission("m1", "s1", "different", None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("different description"), "got: {msg}");
    }

    #[serial_test::serial]
    #[test]
    fn add_phase_errors_when_mission_missing() {
        let _g = CrewGuard::new();
        let err = add_phase_to_mission(
            "ghost-mission",
            "new-phase",
            "desc",
            None,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no mission") && msg.contains("ghost-mission"),
            "got: {msg}",
        );
    }

    /// (#1341) Phases are strictly linear now — no separate `depends_on`
    /// declaration to dangle-check; `resolve_insert_position` (called
    /// below, in the real add-phase path) already errors loud when
    /// `--after` names a phase id not in the mission's `phase_ids`, which
    /// is the equivalent "reference an unknown phase" failure mode.

    // ─── --after positioning (insert-in-middle) ─────────────────────────

    #[serial_test::serial]
    #[test]
    fn add_phase_with_after_inserts_in_middle_not_at_end() {
        let _g = CrewGuard::new();
        seed_mission("m1", MissionStatus::Active);
        add_phase_to_mission("m1", "alpha", "first", None).unwrap();
        add_phase_to_mission("m1", "beta", "second", None).unwrap();
        add_phase_to_mission("m1", "gamma", "third", None).unwrap();

        // Insert `delta` between `alpha` and `beta`.
        add_phase_to_mission("m1", "delta", "inserted", Some("alpha")).unwrap();

        let m = load_mission("m1").unwrap();
        assert_eq!(
            m.phase_ids,
            vec![
                "alpha".to_string(),
                "delta".to_string(),
                "beta".to_string(),
                "gamma".to_string(),
            ],
            "delta should land immediately after alpha, not at the end"
        );
    }

    #[serial_test::serial]
    #[test]
    fn add_phase_with_after_at_tail_inserts_after_last() {
        // Edge case: --after the last phase should append (not error).
        let _g = CrewGuard::new();
        seed_mission("m1", MissionStatus::Active);
        add_phase_to_mission("m1", "alpha", "first", None).unwrap();
        add_phase_to_mission("m1", "beta", "second", None).unwrap();

        add_phase_to_mission("m1", "gamma", "third", Some("beta")).unwrap();

        let m = load_mission("m1").unwrap();
        assert_eq!(m.phase_ids, vec!["alpha", "beta", "gamma"]);
    }

    #[serial_test::serial]
    #[test]
    fn add_phase_with_after_unknown_id_errors_loudly() {
        let _g = CrewGuard::new();
        seed_mission("m1", MissionStatus::Active);
        add_phase_to_mission("m1", "alpha", "first", None).unwrap();

        let err = add_phase_to_mission(
            "m1",
            "beta",
            "second",
            Some("nonexistent-id"),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("--after references phase `nonexistent-id`"),
            "got: {msg}"
        );
        // Mission was not mutated on error — alpha is still the only phase.
        let m = load_mission("m1").unwrap();
        assert_eq!(m.phase_ids, vec!["alpha".to_string()]);
        // The phase JSON should NOT have been left behind either.
        assert!(!phase_path("m1", "beta").exists(), "errored insert must not leave orphan phase");
    }

    #[serial_test::serial]
    #[test]
    fn add_phase_emits_phase_added_flow_record() {
        let _g = CrewGuard::new();
        seed_mission("m1", MissionStatus::Active);
        add_phase_to_mission("m1", "new-phase", "desc", None).unwrap();

        // Read the day's flow file from the temp DARKMUX_FLOWS_DIR.
        let flows_dir = env::var("DARKMUX_FLOWS_DIR").unwrap();
        let day = darkmux_flow::day_utc_now();
        let path = std::path::PathBuf::from(flows_dir).join(format!("{day}.jsonl"));
        let raw = std::fs::read_to_string(&path).expect("flow file should have been created");
        let found = raw.lines().any(|line| {
            line.contains("\"action\":\"phase added\"")
                && line.contains("\"handle\":\"new-phase\"")
                && line.contains("\"source\":\"mission_lifecycle\"")
        });
        assert!(found, "expected a `phase added` flow record, got:\n{raw}");
    }
}

#[cfg(test)]
mod path_helper_tests {
    use super::*;
    use serial_test::serial;

    fn with_test_root<F: FnOnce(&std::path::Path)>(f: F) {
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        std::env::set_var("DARKMUX_CREW_DIR", tmp.path());
        f(tmp.path());
        match prev {
            Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
            None => std::env::remove_var("DARKMUX_CREW_DIR"),
        }
    }

    #[test]
    #[serial]
    fn new_layout_resolvers() {
        with_test_root(|root| {
            assert_eq!(mission_dir("m"), root.join("missions/m"));
            assert_eq!(mission_path("m"), root.join("missions/m/mission.json"));
            assert_eq!(phases_dir("m"), root.join("missions/m/phases"));
            assert_eq!(phase_path("m", "s"), root.join("missions/m/phases/s.json"));
        });
    }

    #[test]
    #[serial]
    fn legacy_resolvers() {
        with_test_root(|root| {
            assert_eq!(legacy_mission_path("m"), root.join("missions/m.json"));
            assert_eq!(legacy_phase_path("s"), root.join("phases/s.json"));
            assert_eq!(legacy_missions_dir(), root.join("missions"));
            assert_eq!(legacy_phases_dir(), root.join("phases"));
        });
    }

    /// Sprint→Phase rename read-fallback (real-data compat): a mission dir
    /// with ONLY a legacy `sprints/` subdir (pre-rename on-disk layout) must
    /// still resolve `phases_dir` to it, so an operator's existing mission
    /// data survives the rename without a manual migration step.
    #[test]
    #[serial]
    fn phases_dir_falls_back_to_legacy_sprints_subdir_name() {
        with_test_root(|root| {
            let legacy_dir = root.join("missions/m/sprints");
            std::fs::create_dir_all(&legacy_dir).unwrap();
            assert_eq!(phases_dir("m"), legacy_dir);
            assert_eq!(phase_path("m", "s"), legacy_dir.join("s.json"));
        });
    }

    /// Once the canonical `phases/` subdir exists (even alongside a
    /// still-present `sprints/` one — e.g. mid-migration), canonical wins.
    #[test]
    #[serial]
    fn phases_dir_prefers_canonical_when_both_exist() {
        with_test_root(|root| {
            let legacy_dir = root.join("missions/m/sprints");
            let canonical_dir = root.join("missions/m/phases");
            std::fs::create_dir_all(&legacy_dir).unwrap();
            std::fs::create_dir_all(&canonical_dir).unwrap();
            assert_eq!(phases_dir("m"), canonical_dir);
        });
    }

    /// Brand-new mission (neither subdir exists yet) resolves to the
    /// canonical name so a fresh write creates the new-name layout.
    #[test]
    #[serial]
    fn phases_dir_defaults_to_canonical_when_neither_exists() {
        with_test_root(|root| {
            assert_eq!(phases_dir("m"), root.join("missions/m/phases"));
        });
    }

    #[test]
    #[serial]
    fn task_step_resolvers() {
        with_test_root(|root| {
            assert_eq!(tasks_dir("m", "s"), root.join("missions/m/tasks/s"));
            assert_eq!(task_path("m", "s", "t"), root.join("missions/m/tasks/s/t.json"));
            assert_eq!(steps_dir("m", "s"), root.join("missions/m/steps/s"));
            assert_eq!(step_path("m", "s", "st"), root.join("missions/m/steps/s/st.json"));
        });
    }
}

#[cfg(test)]
mod task_step_storage_tests {
    //! (#1230 Packet 2) Round-trip storage tests for the Task/Step
    //! per-mission-nested JSON layout — one file per Step, atomic-rename
    //! `save_json`, same conventions as Phase/Mission storage above.
    use super::*;
    use crate::types::{NodeStatus, Step, Task};
    use serial_test::serial;
    use tempfile::TempDir;

    struct CrewGuard {
        _tmp: TempDir,
        prev: Option<String>,
    }

    impl CrewGuard {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let prev = std::env::var("DARKMUX_CREW_DIR").ok();
            std::env::set_var("DARKMUX_CREW_DIR", tmp.path());
            Self { _tmp: tmp, prev }
        }
    }

    impl Drop for CrewGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }
    }

    fn task(id: &str, phase_id: &str) -> Task {
        Task {
            id: id.to_string(),
            phase_id: phase_id.to_string(),
            description: format!("task {id}"),
            step_ids: Vec::new(),
            depends_on: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        }
    }

    fn step(id: &str, task_id: &str) -> Step {
        Step {
            id: id.to_string(),
            task_id: task_id.to_string(),
            kind: "procedural.noop".to_string(),
            status: NodeStatus::Planned,
            config: serde_json::Value::Null,
            started_ts: None,
            completed_ts: None,
            output: None,
        }
    }

    #[test]
    #[serial]
    fn save_and_load_task_round_trips() {
        let _g = CrewGuard::new();
        let t = task("t1", "s1");
        save_task("m1", &t).unwrap();
        let loaded = load_task("m1", "s1", "t1").unwrap();
        assert_eq!(loaded.id, "t1");
        assert_eq!(loaded.phase_id, "s1");
        assert_eq!(loaded.description, "task t1");
    }

    #[test]
    #[serial]
    fn load_tasks_for_phase_missing_dir_is_empty_not_error() {
        let _g = CrewGuard::new();
        let tasks = load_tasks_for_phase("ghost-mission", "ghost-phase").unwrap();
        assert!(tasks.is_empty());
    }

    #[test]
    #[serial]
    fn load_tasks_for_phase_returns_every_task_in_deterministic_order() {
        let _g = CrewGuard::new();
        save_task("m1", &task("t-b", "s1")).unwrap();
        save_task("m1", &task("t-a", "s1")).unwrap();
        // A different phase's task must NOT show up in s1's listing.
        save_task("m1", &task("t-other", "s2")).unwrap();

        let tasks = load_tasks_for_phase("m1", "s1").unwrap();
        let ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["t-a", "t-b"], "sorted by filename, s2's task excluded");
    }

    #[test]
    #[serial]
    fn save_and_load_step_round_trips() {
        let _g = CrewGuard::new();
        let mut s = step("st1", "t1");
        s.status = NodeStatus::Complete;
        s.output = Some("done".to_string());
        save_step("m1", "s1", &s).unwrap();

        let loaded = load_step("m1", "s1", "st1").unwrap();
        assert_eq!(loaded.id, "st1");
        assert_eq!(loaded.status, NodeStatus::Complete);
        assert_eq!(loaded.output.as_deref(), Some("done"));
    }

    #[test]
    #[serial]
    fn load_steps_for_phase_missing_dir_is_empty_not_error() {
        let _g = CrewGuard::new();
        let steps = load_steps_for_phase("ghost-mission", "ghost-phase").unwrap();
        assert!(steps.is_empty());
    }

    #[test]
    #[serial]
    fn load_steps_for_phase_returns_every_step_across_tasks() {
        let _g = CrewGuard::new();
        save_step("m1", "s1", &step("st-b", "t1")).unwrap();
        save_step("m1", "s1", &step("st-a", "t2")).unwrap();
        save_step("m1", "s2", &step("st-other", "t3")).unwrap();

        let steps = load_steps_for_phase("m1", "s1").unwrap();
        let ids: Vec<&str> = steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["st-a", "st-b"], "sorted by filename, s2's step excluded");
    }

    #[test]
    #[serial]
    fn save_step_is_a_small_atomic_write_no_tmp_left_behind() {
        let _g = CrewGuard::new();
        let s = step("st1", "t1");
        save_step("m1", "s1", &s).unwrap();
        let tmp_path = step_path("m1", "s1", "st1").with_extension("json.tmp");
        assert!(!tmp_path.exists());
    }
}
