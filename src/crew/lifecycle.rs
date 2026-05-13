//! Sprint + Mission lifecycle transitions (#95).
//!
//! Sprint/Mission state lives in JSON files under `<crew_root>/sprints/<id>.json`
//! and `<crew_root>/missions/<id>.json` respectively. This module provides the
//! operator-facing verbs that transition status + timestamp those entities:
//!
//! - `sprint start <id>` / `sprint complete <id>` / `sprint abandon <id>`
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
//! Operator-sovereignty: nothing auto-transitions. A sprint stays `Running`
//! forever until the operator runs `complete` or `abandon`. The wall-clock
//! UI (consuming this data via Track B's API exposure) renders an unbounded
//! sweep on stale `Running` sprints — clearly-wrong-looking signals that
//! the operator forgot to close one. By design.
//!
//! State machines:
//!
//! Sprint:
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

use crate::crew::loader::{crew_root, load_missions, load_sprints};
use crate::crew::types::{Mission, MissionStatus, Sprint, SprintStatus};
use crate::flow::{self, Category, FlowRecord, Level, Stage, Tier};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

// ─── Paths ──────────────────────────────────────────────────────────────

fn sprint_path(sprint_id: &str) -> PathBuf {
    crew_root().join("sprints").join(format!("{sprint_id}.json"))
}

fn mission_path(mission_id: &str) -> PathBuf {
    crew_root().join("missions").join(format!("{mission_id}.json"))
}

// ─── Time ───────────────────────────────────────────────────────────────

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─── Atomic save ────────────────────────────────────────────────────────

fn save_json<T: serde::Serialize>(path: &std::path::Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(value)
        .with_context(|| format!("serializing to {}", path.display()))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, body + "\n")
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

// ─── Load-by-id ────────────────────────────────────────────────────────

fn load_sprint(id: &str) -> Result<Sprint> {
    let all = load_sprints()?;
    all.into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| anyhow::anyhow!("no sprint with id `{id}` found in {}", crew_root().join("sprints").display()))
}

fn load_mission(id: &str) -> Result<Mission> {
    let all = load_missions()?;
    all.into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| anyhow::anyhow!("no mission with id `{id}` found in {}", crew_root().join("missions").display()))
}

// ─── Flow record emission ──────────────────────────────────────────────

fn emit_sprint_transition_record(sprint_id: &str, mission_id: &str, action: &str) {
    let _ = flow::record(FlowRecord {
        ts: flow::ts_utc_now(),
        level: Level::Info,
        category: Category::Work,
        tier: Tier::Operator,
        stage: Stage::Scope,
        action: action.to_string(),
        handle: sprint_id.to_string(),
        sprint_id: Some(sprint_id.to_string()),
        session_id: Some(format!("mission:{mission_id}")),
        source: Some("sprint_lifecycle".to_string()),
        model: None,
    });
}

fn emit_mission_transition_record(mission_id: &str, action: &str) {
    let _ = flow::record(FlowRecord {
        ts: flow::ts_utc_now(),
        level: Level::Info,
        category: Category::Work,
        tier: Tier::Operator,
        stage: Stage::Scope,
        action: action.to_string(),
        handle: mission_id.to_string(),
        sprint_id: None,
        session_id: Some(format!("mission:{mission_id}")),
        source: Some("mission_lifecycle".to_string()),
        model: None,
    });
}

/// Distinct from `emit_sprint_transition_record` (which uses
/// `source: sprint_lifecycle` for status flips on an already-tracked
/// sprint). This one fires when the *mission's shape* changes — a
/// new sprint joining the plan — so the `source` field reflects that
/// the change came from the mission_lifecycle surface even though the
/// `handle` is the new sprint id.
fn emit_sprint_added_record(sprint_id: &str, mission_id: &str) {
    let _ = flow::record(FlowRecord {
        ts: flow::ts_utc_now(),
        level: Level::Info,
        category: Category::Work,
        tier: Tier::Operator,
        stage: Stage::Scope,
        action: "sprint added".to_string(),
        handle: sprint_id.to_string(),
        sprint_id: Some(sprint_id.to_string()),
        session_id: Some(format!("mission:{mission_id}")),
        source: Some("mission_lifecycle".to_string()),
        model: None,
    });
}

// ─── Sprint transitions ─────────────────────────────────────────────────

/// `sprint start <id>` — Planned/Abandoned → Running.
/// Sets `started_ts = now()`; clears `abandoned_ts` if it was set.
pub fn sprint_start(id: &str) -> Result<Sprint> {
    let mut sprint = load_sprint(id)?;
    match sprint.status {
        SprintStatus::Planned | SprintStatus::Abandoned => {}
        SprintStatus::Running => bail!("sprint `{id}` is already Running"),
        SprintStatus::Complete => bail!("sprint `{id}` is Complete (terminal) — create a new sprint instead"),
    }
    sprint.status = SprintStatus::Running;
    sprint.started_ts = Some(now_unix());
    sprint.abandoned_ts = None; // restart clears the prior abandonment
    save_json(&sprint_path(id), &sprint)?;
    emit_sprint_transition_record(id, &sprint.mission_id, "sprint start");
    Ok(sprint)
}

/// `sprint complete <id>` — Running → Complete (terminal).
pub fn sprint_complete(id: &str) -> Result<Sprint> {
    let mut sprint = load_sprint(id)?;
    match sprint.status {
        SprintStatus::Running => {}
        SprintStatus::Planned => bail!("sprint `{id}` is Planned — must `sprint start` first"),
        SprintStatus::Abandoned => bail!("sprint `{id}` is Abandoned — `sprint start` to restart, then complete"),
        SprintStatus::Complete => bail!("sprint `{id}` is already Complete"),
    }
    sprint.status = SprintStatus::Complete;
    sprint.completed_ts = Some(now_unix());
    save_json(&sprint_path(id), &sprint)?;
    emit_sprint_transition_record(id, &sprint.mission_id, "sprint complete");
    Ok(sprint)
}

/// `sprint abandon <id>` — Planned/Running → Abandoned.
/// Cannot abandon a `Complete` sprint (terminal in the other direction).
pub fn sprint_abandon(id: &str) -> Result<Sprint> {
    let mut sprint = load_sprint(id)?;
    match sprint.status {
        SprintStatus::Planned | SprintStatus::Running => {}
        SprintStatus::Abandoned => bail!("sprint `{id}` is already Abandoned"),
        SprintStatus::Complete => bail!("sprint `{id}` is Complete — can't abandon a finished sprint"),
    }
    sprint.status = SprintStatus::Abandoned;
    sprint.abandoned_ts = Some(now_unix());
    save_json(&sprint_path(id), &sprint)?;
    emit_sprint_transition_record(id, &sprint.mission_id, "sprint abandon");
    Ok(sprint)
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
pub fn mission_start(id: &str) -> Result<Mission> {
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
    emit_mission_transition_record(id, "mission start");
    Ok(mission)
}

/// `mission close <id>` — Active/Paused → Closed (terminal).
pub fn mission_close(id: &str) -> Result<Mission> {
    let mut mission = load_mission(id)?;
    match mission.status {
        MissionStatus::Active | MissionStatus::Paused => {}
        MissionStatus::Closed => bail!("mission `{id}` is already Closed"),
    }
    mission.status = MissionStatus::Closed;
    mission.closed_ts = Some(now_unix());
    save_json(&mission_path(id), &mission)?;
    emit_mission_transition_record(id, "mission close");
    Ok(mission)
}

/// `mission pause <id>` — Active → Paused. Updates `paused_ts` to now even
/// if a prior pause was recorded (operator gets the most-recent pause time).
pub fn mission_pause(id: &str) -> Result<Mission> {
    let mut mission = load_mission(id)?;
    match mission.status {
        MissionStatus::Active => {}
        MissionStatus::Paused => bail!("mission `{id}` is already Paused"),
        MissionStatus::Closed => bail!("mission `{id}` is Closed — can't pause a finished mission"),
    }
    mission.status = MissionStatus::Paused;
    mission.paused_ts = Some(now_unix());
    save_json(&mission_path(id), &mission)?;
    emit_mission_transition_record(id, "mission pause");
    Ok(mission)
}

/// `mission resume <id>` — Paused → Active. Does NOT clear `paused_ts` —
/// the operator may want to see how long the mission was paused.
pub fn mission_resume(id: &str) -> Result<Mission> {
    let mut mission = load_mission(id)?;
    match mission.status {
        MissionStatus::Paused => {}
        MissionStatus::Active => bail!("mission `{id}` is already Active"),
        MissionStatus::Closed => bail!("mission `{id}` is Closed — can't resume a finished mission"),
    }
    mission.status = MissionStatus::Active;
    save_json(&mission_path(id), &mission)?;
    emit_mission_transition_record(id, "mission resume");
    Ok(mission)
}

// ─── Mission scope growth ──────────────────────────────────────────────

/// `mission add-sprint` — operator-sovereign scope growth (#107).
///
/// Adds a new Sprint to an existing Mission mid-flight. The alternative
/// today is one of: (a) hand-edit the Mission JSON to append a sprint id
/// and hand-author a new Sprint JSON, (b) create a separate Mission that
/// loses the *"this composes with what we're already doing"* signal, or
/// (c) leave the discovery as a GH issue with no Mission/Sprint
/// representation. None reflect how engineering work actually evolves
/// during a sprint.
///
/// Behavior:
///   - Writes a new Sprint JSON at `<crew_root>/sprints/<sprint-id>.json`
///     with `status: Planned`, `mission_id`, `description`, `depends_on`,
///     `created_ts: now()`.
///   - Appends the sprint id to the Mission's `sprint_ids` array (atomic
///     write per the existing `save_json` semantics).
///   - Emits a `sprint added` flow record (tier=operator, source=
///     mission_lifecycle) so the addition is observable in the viewer.
///
/// Idempotent on EXACT match: re-adding a sprint with the same id +
/// mission + description is a no-op (no error). The sprint_ids array
/// gets the sprint appended if it had drifted off (defensive). Non-
/// matching descriptions error — we don't silently overwrite operator
/// content.
///
/// Errors loudly when:
///   - Mission doesn't exist.
///   - Sprint id is already in use under a *different* mission (would
///     break the unique-id-per-sprint invariant the loader relies on).
///   - Sprint id is in use under SAME mission but with different
///     description (operator probably meant a different id or wants to
///     explicitly edit; either way, don't paper over the conflict).
///   - Any `depends_on` id doesn't resolve to an existing sprint.
pub fn add_sprint_to_mission(
    mission_id: &str,
    sprint_id: &str,
    description: &str,
    depends_on: Vec<String>,
    after: Option<&str>,
) -> Result<Sprint> {
    let mut mission = load_mission(mission_id)?;

    let all_sprints = load_sprints()?;

    // Idempotency / collision check.
    if let Some(existing) = all_sprints.iter().find(|s| s.id == sprint_id) {
        if existing.mission_id != mission_id {
            bail!(
                "sprint id `{sprint_id}` already in use under mission `{}`; pick a fresh id",
                existing.mission_id
            );
        }
        if existing.description != description {
            bail!(
                "sprint id `{sprint_id}` already exists under this mission with a different description; \
                 edit the existing JSON or pick a fresh id"
            );
        }
        // Exact match — idempotent path. Defensive: ensure the mission's
        // sprint_ids still includes this id (operator may have hand-
        // edited the JSON and removed it). Idempotent re-adds do NOT
        // reposition; the operator has to remove the existing entry
        // first if they want a different position. That keeps re-runs
        // safe even when `--after` is non-default.
        let already_listed = mission.sprint_ids.iter().any(|s| s == sprint_id);
        if !already_listed {
            let pos = resolve_insert_position(&mission.sprint_ids, after)?;
            mission.sprint_ids.insert(pos, sprint_id.to_string());
            save_json(&mission_path(mission_id), &mission)?;
        }
        return Ok(existing.clone());
    }

    // depends_on dangling check — every referenced sprint must exist.
    for dep in &depends_on {
        if !all_sprints.iter().any(|s| s.id.as_str() == dep.as_str()) {
            bail!(
                "depends_on references unknown sprint `{}`; add it first or correct the id",
                dep
            );
        }
    }

    // Pre-validate the `--after` target BEFORE writing any state, so a
    // typo'd `--after` doesn't leave a sprint JSON on disk that's not
    // referenced from any mission. Resolve to the insertion index now,
    // then use it after the sprint JSON is written.
    let insert_pos = resolve_insert_position(&mission.sprint_ids, after)?;

    let sprint = Sprint {
        id: sprint_id.to_string(),
        mission_id: mission_id.to_string(),
        description: description.to_string(),
        status: SprintStatus::Planned,
        depends_on,
        created_ts: now_unix(),
        started_ts: None,
        completed_ts: None,
        abandoned_ts: None,
    };
    save_json(&sprint_path(sprint_id), &sprint)?;

    // Position into the mission's sprint_ids. Belt-and-suspenders: the
    // collision check above means we shouldn't see a duplicate here,
    // but the idempotent guard makes the operation safe to retry on
    // partial-failure paths.
    if !mission.sprint_ids.iter().any(|s| s == sprint_id) {
        mission.sprint_ids.insert(insert_pos, sprint_id.to_string());
        save_json(&mission_path(mission_id), &mission)?;
    }

    emit_sprint_added_record(sprint_id, mission_id);

    Ok(sprint)
}

/// Compute the index at which a new sprint should be inserted into
/// the mission's `sprint_ids` array. Pure — does NOT mutate the
/// vector. Pre-validates `--after` so callers can write the new
/// sprint JSON without risking an orphan record when the reference
/// is stale.
///
/// When `after` is `None`, returns `sprint_ids.len()` (append). When
/// `after` names a present sprint, returns the index immediately
/// after it. Errors when `after` names an absent sprint — silently
/// appending would obscure a typo or stale reference, which violates
/// operator sovereignty (don't substitute system judgment for
/// operator intent).
fn resolve_insert_position(sprint_ids: &[String], after: Option<&str>) -> Result<usize> {
    match after {
        None => Ok(sprint_ids.len()),
        Some(target) => {
            let pos = sprint_ids
                .iter()
                .position(|s| s == target)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "--after references sprint `{target}` which isn't in this mission's sprint_ids; \
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
    use crate::crew::types::{Mission, MissionStatus, Sprint, SprintStatus};
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

    fn seed_sprint(id: &str, status: SprintStatus) -> Sprint {
        let s = Sprint {
            id: id.to_string(),
            mission_id: "test-mission".to_string(),
            description: "test sprint".to_string(),
            status,
            depends_on: Vec::new(),
            created_ts: 1_700_000_000,
            started_ts: None,
            completed_ts: None,
            abandoned_ts: None,
        };
        save_json(&sprint_path(id), &s).unwrap();
        s
    }

    fn seed_mission(id: &str, status: MissionStatus) -> Mission {
        let m = Mission {
            id: id.to_string(),
            description: "test mission".to_string(),
            status,
            sprint_ids: Vec::new(),
            created_ts: 1_700_000_000,
            started_ts: None,
            closed_ts: None,
            paused_ts: None,
        };
        save_json(&mission_path(id), &m).unwrap();
        m
    }

    // ─── Sprint state machine ─────────────────────────────────────────

    #[serial_test::serial]
    #[test]
    fn sprint_start_from_planned_sets_running_and_started_ts() {
        let _g = CrewGuard::new();
        seed_sprint("s1", SprintStatus::Planned);

        let updated = sprint_start("s1").unwrap();

        assert_eq!(updated.status, SprintStatus::Running);
        assert!(updated.started_ts.is_some());
        assert!(updated.completed_ts.is_none());
        assert!(updated.abandoned_ts.is_none());
    }

    #[serial_test::serial]
    #[test]
    fn sprint_start_from_running_errors() {
        let _g = CrewGuard::new();
        seed_sprint("s2", SprintStatus::Running);
        let err = sprint_start("s2").unwrap_err();
        assert!(err.to_string().contains("already Running"));
    }

    #[serial_test::serial]
    #[test]
    fn sprint_start_from_complete_errors() {
        let _g = CrewGuard::new();
        seed_sprint("s3", SprintStatus::Complete);
        let err = sprint_start("s3").unwrap_err();
        assert!(err.to_string().contains("Complete"));
    }

    #[serial_test::serial]
    #[test]
    fn sprint_start_from_abandoned_clears_abandoned_ts() {
        let _g = CrewGuard::new();
        let mut s = seed_sprint("s4", SprintStatus::Abandoned);
        s.abandoned_ts = Some(1_700_000_500);
        save_json(&sprint_path("s4"), &s).unwrap();

        let updated = sprint_start("s4").unwrap();
        assert_eq!(updated.status, SprintStatus::Running);
        assert!(updated.started_ts.is_some());
        assert!(updated.abandoned_ts.is_none(), "restart clears abandoned_ts");
    }

    #[serial_test::serial]
    #[test]
    fn sprint_complete_from_running_sets_complete_and_completed_ts() {
        let _g = CrewGuard::new();
        let mut s = seed_sprint("s5", SprintStatus::Running);
        s.started_ts = Some(1_700_000_100);
        save_json(&sprint_path("s5"), &s).unwrap();

        let updated = sprint_complete("s5").unwrap();
        assert_eq!(updated.status, SprintStatus::Complete);
        assert!(updated.completed_ts.is_some());
        assert!(updated.completed_ts.unwrap() >= updated.started_ts.unwrap());
    }

    #[serial_test::serial]
    #[test]
    fn sprint_complete_from_planned_errors() {
        let _g = CrewGuard::new();
        seed_sprint("s6", SprintStatus::Planned);
        let err = sprint_complete("s6").unwrap_err();
        assert!(err.to_string().contains("Planned"));
    }

    #[serial_test::serial]
    #[test]
    fn sprint_abandon_from_running_sets_abandoned_and_abandoned_ts() {
        let _g = CrewGuard::new();
        seed_sprint("s7", SprintStatus::Running);
        let updated = sprint_abandon("s7").unwrap();
        assert_eq!(updated.status, SprintStatus::Abandoned);
        assert!(updated.abandoned_ts.is_some());
    }

    #[serial_test::serial]
    #[test]
    fn sprint_abandon_from_complete_errors() {
        let _g = CrewGuard::new();
        seed_sprint("s8", SprintStatus::Complete);
        let err = sprint_abandon("s8").unwrap_err();
        assert!(err.to_string().contains("Complete"));
    }

    #[serial_test::serial]
    #[test]
    fn sprint_round_trip_records_durations() {
        let _g = CrewGuard::new();
        seed_sprint("s9", SprintStatus::Planned);

        let s1 = sprint_start("s9").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let s2 = sprint_complete("s9").unwrap();

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
    fn sprint_start_on_missing_id_errors() {
        let _g = CrewGuard::new();
        let err = sprint_start("does-not-exist").unwrap_err();
        assert!(err.to_string().contains("no sprint with id"));
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
        seed_sprint("s-atomic", SprintStatus::Planned);
        let _ = sprint_start("s-atomic").unwrap();
        let tmp_path = sprint_path("s-atomic").with_extension("json.tmp");
        assert!(!tmp_path.exists(), "atomic save should rename, leaving no .tmp");
    }

    // ─── add_sprint_to_mission (#107) ───────────────────────────────────

    #[serial_test::serial]
    #[test]
    fn add_sprint_creates_planned_sprint_and_extends_mission() {
        let _g = CrewGuard::new();
        let mission = seed_mission("test-mission", MissionStatus::Active);
        assert!(mission.sprint_ids.is_empty(), "starting state: empty sprint list");

        let s = add_sprint_to_mission(
            "test-mission",
            "new-sprint",
            "discovered mid-flight",
            Vec::new(),
            None,
        )
        .unwrap();

        assert_eq!(s.status, SprintStatus::Planned);
        assert_eq!(s.mission_id, "test-mission");
        assert_eq!(s.description, "discovered mid-flight");
        assert!(s.started_ts.is_none());
        // Sprint JSON exists on disk.
        assert!(sprint_path("new-sprint").exists());
        // Mission JSON's sprint_ids was updated.
        let reloaded = load_mission("test-mission").unwrap();
        assert_eq!(reloaded.sprint_ids, vec!["new-sprint".to_string()]);
    }

    #[serial_test::serial]
    #[test]
    fn add_sprint_is_idempotent_on_exact_match() {
        let _g = CrewGuard::new();
        seed_mission("m1", MissionStatus::Active);

        let first = add_sprint_to_mission("m1", "s-once", "same desc", Vec::new(), None).unwrap();
        let second = add_sprint_to_mission("m1", "s-once", "same desc", Vec::new(), None).unwrap();

        // Same created_ts on the second call — we returned the existing sprint, not a fresh one.
        assert_eq!(first.created_ts, second.created_ts);
        // Mission's sprint_ids contains the id once, not twice.
        let m = load_mission("m1").unwrap();
        assert_eq!(m.sprint_ids.iter().filter(|s| *s == "s-once").count(), 1);
    }

    #[serial_test::serial]
    #[test]
    fn add_sprint_errors_when_id_collides_across_missions() {
        let _g = CrewGuard::new();
        seed_mission("m-a", MissionStatus::Active);
        seed_mission("m-b", MissionStatus::Active);
        add_sprint_to_mission("m-a", "shared", "first", Vec::new(), None).unwrap();

        let err = add_sprint_to_mission("m-b", "shared", "second", Vec::new(), None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("already in use under mission `m-a`"), "got: {msg}");
    }

    #[serial_test::serial]
    #[test]
    fn add_sprint_errors_when_same_id_has_different_description() {
        let _g = CrewGuard::new();
        seed_mission("m1", MissionStatus::Active);
        add_sprint_to_mission("m1", "s1", "original", Vec::new(), None).unwrap();

        let err = add_sprint_to_mission("m1", "s1", "different", Vec::new(), None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("different description"), "got: {msg}");
    }

    #[serial_test::serial]
    #[test]
    fn add_sprint_errors_when_mission_missing() {
        let _g = CrewGuard::new();
        let err = add_sprint_to_mission(
            "ghost-mission",
            "new-sprint",
            "desc",
            Vec::new(),
            None,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no mission") && msg.contains("ghost-mission"),
            "got: {msg}",
        );
    }

    #[serial_test::serial]
    #[test]
    fn add_sprint_errors_when_depends_on_dangles() {
        let _g = CrewGuard::new();
        seed_mission("m1", MissionStatus::Active);
        let err = add_sprint_to_mission(
            "m1",
            "new-sprint",
            "desc",
            vec!["nonexistent".to_string()],
            None,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("depends_on references unknown sprint `nonexistent`"),
            "got: {msg}",
        );
    }

    #[serial_test::serial]
    #[test]
    fn add_sprint_resolves_depends_on_against_existing_sprints() {
        let _g = CrewGuard::new();
        seed_mission("m1", MissionStatus::Active);
        seed_sprint("dep-sprint", SprintStatus::Planned);

        // Note: seed_sprint hard-codes mission_id="test-mission"; the dep
        // doesn't have to live in the same mission for the resolution
        // check (cross-mission deps are unusual but not invalid).
        let s = add_sprint_to_mission(
            "m1",
            "new-sprint",
            "desc",
            vec!["dep-sprint".to_string()],
            None,
        )
        .unwrap();
        assert_eq!(s.depends_on, vec!["dep-sprint".to_string()]);
    }

    // ─── --after positioning (insert-in-middle) ─────────────────────────

    #[serial_test::serial]
    #[test]
    fn add_sprint_with_after_inserts_in_middle_not_at_end() {
        let _g = CrewGuard::new();
        seed_mission("m1", MissionStatus::Active);
        add_sprint_to_mission("m1", "alpha", "first", Vec::new(), None).unwrap();
        add_sprint_to_mission("m1", "beta", "second", Vec::new(), None).unwrap();
        add_sprint_to_mission("m1", "gamma", "third", Vec::new(), None).unwrap();

        // Insert `delta` between `alpha` and `beta`.
        add_sprint_to_mission("m1", "delta", "inserted", Vec::new(), Some("alpha")).unwrap();

        let m = load_mission("m1").unwrap();
        assert_eq!(
            m.sprint_ids,
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
    fn add_sprint_with_after_at_tail_inserts_after_last() {
        // Edge case: --after the last sprint should append (not error).
        let _g = CrewGuard::new();
        seed_mission("m1", MissionStatus::Active);
        add_sprint_to_mission("m1", "alpha", "first", Vec::new(), None).unwrap();
        add_sprint_to_mission("m1", "beta", "second", Vec::new(), None).unwrap();

        add_sprint_to_mission("m1", "gamma", "third", Vec::new(), Some("beta")).unwrap();

        let m = load_mission("m1").unwrap();
        assert_eq!(m.sprint_ids, vec!["alpha", "beta", "gamma"]);
    }

    #[serial_test::serial]
    #[test]
    fn add_sprint_with_after_unknown_id_errors_loudly() {
        let _g = CrewGuard::new();
        seed_mission("m1", MissionStatus::Active);
        add_sprint_to_mission("m1", "alpha", "first", Vec::new(), None).unwrap();

        let err = add_sprint_to_mission(
            "m1",
            "beta",
            "second",
            Vec::new(),
            Some("nonexistent-id"),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("--after references sprint `nonexistent-id`"),
            "got: {msg}"
        );
        // Mission was not mutated on error — alpha is still the only sprint.
        let m = load_mission("m1").unwrap();
        assert_eq!(m.sprint_ids, vec!["alpha".to_string()]);
        // The sprint JSON should NOT have been left behind either.
        assert!(!sprint_path("beta").exists(), "errored insert must not leave orphan sprint");
    }

    #[serial_test::serial]
    #[test]
    fn add_sprint_emits_sprint_added_flow_record() {
        let _g = CrewGuard::new();
        seed_mission("m1", MissionStatus::Active);
        add_sprint_to_mission("m1", "new-sprint", "desc", Vec::new(), None).unwrap();

        // Read the day's flow file from the temp DARKMUX_FLOWS_DIR.
        let flows_dir = env::var("DARKMUX_FLOWS_DIR").unwrap();
        let day = crate::flow::day_utc_now();
        let path = std::path::PathBuf::from(flows_dir).join(format!("{day}.jsonl"));
        let raw = std::fs::read_to_string(&path).expect("flow file should have been created");
        let found = raw.lines().any(|line| {
            line.contains("\"action\":\"sprint added\"")
                && line.contains("\"handle\":\"new-sprint\"")
                && line.contains("\"source\":\"mission_lifecycle\"")
        });
        assert!(found, "expected a `sprint added` flow record, got:\n{raw}");
    }
}
