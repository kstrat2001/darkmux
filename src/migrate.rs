//! `darkmux mission migrate` — flat → per-mission layout migration (#148 Task 8).
//!
//! Migrates mission and phase storage from the pre-#148 flat layout:
//!   `<crew_root>/missions/<id>.json`
//!   `<crew_root>/phases/<id>.json`
//!
//! …to the per-mission nested layout:
//!   `<crew_root>/missions/<id>/mission.json`
//!   `<crew_root>/missions/<id>/phases/<phase-id>.json`
//!
//! Operator UX:
//!   `darkmux mission migrate`         — dry-run preview (default)
//!   `darkmux mission migrate --apply` — actually moves files
//!   `darkmux mission migrate --apply` — idempotent; already-migrated is a no-op

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::crew::lifecycle;

/// A plan describing the set of moves needed to migrate from the flat layout
/// to the per-mission layout. Computed by [`plan_migration`] and executed by
/// [`apply_migration`]. The plan is safe to inspect before committing.
#[derive(Debug, Default)]
pub struct MigratePlan {
    /// `(old_flat_path, new_nested_path)` pairs — for missions.
    pub mission_moves: Vec<(PathBuf, PathBuf)>,
    /// `(old_flat_path, new_nested_path)` pairs — for phases.
    pub phase_moves: Vec<(PathBuf, PathBuf)>,
    /// Legacy phase files whose `mission_id` field doesn't match any mission
    /// on disk (flat or nested). Skipped during apply; operator must handle manually.
    pub orphan_phases: Vec<PathBuf>,
    /// (#1284 Packet 4a) Mission ids already on the per-mission nested
    /// layout (or landing there via `mission_moves` in THIS same plan) that
    /// have no `config-snapshot.json` yet — legacy hand-authored instances
    /// (pre-dating `mission launch`'s config-launched instance model) that
    /// need one synthesized. Sorted for deterministic output.
    pub config_snapshots_missing: Vec<String>,
}

impl MigratePlan {
    /// Returns `true` when there is nothing to migrate and nothing to warn about.
    pub fn is_empty(&self) -> bool {
        self.mission_moves.is_empty()
            && self.phase_moves.is_empty()
            && self.orphan_phases.is_empty()
            && self.config_snapshots_missing.is_empty()
    }
}

/// Compute the migration plan by scanning the legacy flat directories.
///
/// This is a read-only operation — no files are moved.
///
/// Algorithm:
/// 1. Walk `<crew_root>/missions/` for top-level `.json` files (flat missions).
///    Directories are skipped — they are already on the new layout.
/// 2. Build the set of known mission ids from both flat (pending migration)
///    and nested (already migrated) sources.
/// 3. Walk `<crew_root>/phases/` for `.json` files, read `mission_id` from
///    each, and classify as a move-candidate or orphan.
pub fn plan_migration() -> Result<MigratePlan> {
    let mut plan = MigratePlan::default();

    let legacy_missions = lifecycle::legacy_missions_dir();
    let legacy_phases = lifecycle::legacy_phases_dir();

    // ── Mission moves ──────────────────────────────────────────────────────
    // Walk <crew_root>/missions/ for .is_file() entries with .json extension.
    // Directories are already-migrated or hand-created subdirs — skip them.
    if legacy_missions.is_dir() {
        for entry in std::fs::read_dir(&legacy_missions)
            .with_context(|| format!("reading {}", legacy_missions.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let mission_id = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let target = lifecycle::mission_path(&mission_id);
            // If target already exists (operator partially migrated), skip
            // silently — idempotent behaviour.
            if target.is_file() {
                continue;
            }
            plan.mission_moves.push((path, target));
        }
    }

    // ── Build known mission id set ─────────────────────────────────────────
    // Includes both flat (pending) and nested (already migrated) missions so
    // we can correctly classify phases that reference already-migrated missions
    // as move-candidates rather than orphans.
    let mut known_missions: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    // Flat missions pending migration.
    for (src, _) in &plan.mission_moves {
        if let Some(id) = src.file_stem().and_then(|s| s.to_str()) {
            known_missions.insert(id.to_string());
        }
    }
    // Already-nested missions (already-migrated state).
    if legacy_missions.is_dir() {
        for entry in std::fs::read_dir(&legacy_missions)? {
            let entry = entry?;
            let p = entry.path();
            if p.is_dir() {
                if let Some(id) = p.file_name().and_then(|s| s.to_str()) {
                    if p.join("mission.json").is_file() {
                        known_missions.insert(id.to_string());
                    }
                }
            }
        }
    }

    // ── Phase moves ───────────────────────────────────────────────────────
    if legacy_phases.is_dir() {
        for entry in std::fs::read_dir(&legacy_phases)
            .with_context(|| format!("reading {}", legacy_phases.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }

            // Read the phase JSON to discover mission_id.
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let mission_id: String = match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(v) => match v.get("mission_id").and_then(|m| m.as_str()) {
                    Some(s) => s.to_string(),
                    None => {
                        // Phase without a mission_id field — treat as orphan.
                        plan.orphan_phases.push(path);
                        continue;
                    }
                },
                Err(_) => {
                    // Malformed JSON — treat as orphan; operator inspects manually.
                    plan.orphan_phases.push(path);
                    continue;
                }
            };

            if !known_missions.contains(&mission_id) {
                plan.orphan_phases.push(path);
                continue;
            }

            let phase_id = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let target = lifecycle::phase_path(&mission_id, &phase_id);
            if target.is_file() {
                continue; // already migrated — idempotent
            }
            plan.phase_moves.push((path, target));
        }
    }

    // ── Config-snapshot synthesis (#1284 Packet 4a) ────────────────────────
    // Every mission id already known (nested on disk, or landing there via
    // this same plan's mission_moves) that has no config-snapshot.json yet
    // — a legacy hand-authored instance minted before `mission launch`
    // existed. Checked here (read-only) so a dry-run preview shows the
    // synthesis work too; `apply_migration` does the actual write, AFTER
    // the file moves above land (a still-flat mission has no nested
    // mission.json/phases/ to read from yet).
    let mut config_snapshots_missing: Vec<String> = known_missions
        .into_iter()
        .filter(|id| !lifecycle::config_snapshot_path(id).is_file())
        .collect();
    config_snapshots_missing.sort();
    plan.config_snapshots_missing = config_snapshots_missing;

    Ok(plan)
}

/// Execute a migration plan, moving flat files to their nested destinations.
///
/// Idempotent: if a source file no longer exists when `apply` runs (e.g. re-run
/// after partial success), the move is skipped silently. Orphan phases are
/// never touched.
pub fn apply_migration(plan: &MigratePlan) -> Result<()> {
    for (src, dst) in &plan.mission_moves {
        if !src.is_file() {
            continue; // already moved (re-run case)
        }
        // (#907) Re-check the destination at apply time, mirroring the
        // plan-time guard — state can change between plan and apply, and
        // `fs::rename` would silently CLOBBER an existing dst.
        if dst.exists() {
            anyhow::bail!(
                "migration target {} already exists — refusing to overwrite (resolve the collision, then re-run)",
                dst.display()
            );
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::rename(src, dst)
            .with_context(|| format!("moving {} -> {}", src.display(), dst.display()))?;
    }
    for (src, dst) in &plan.phase_moves {
        if !src.is_file() {
            continue;
        }
        if dst.exists() {
            anyhow::bail!(
                "migration target {} already exists — refusing to overwrite (resolve the collision, then re-run)",
                dst.display()
            );
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::rename(src, dst)
            .with_context(|| format!("moving {} -> {}", src.display(), dst.display()))?;
    }

    // (#1284 Packet 4a) Synthesize config-snapshot.json for every legacy
    // instance the plan named — AFTER the moves above, so a mission that
    // was still flat at plan time now has a real nested mission.json +
    // phases/ to read from. Best-effort per mission: one mission's drifted
    // phase_ids (references a phase JSON that no longer exists) surfaces a
    // warning and is skipped, never aborting the rest of the batch —
    // re-running `mission migrate --apply` picks it up again once fixed
    // (idempotent: a mission that already got a snapshot this round, or on
    // a prior run, is a no-op here).
    for mission_id in &plan.config_snapshots_missing {
        if let Err(e) = synthesize_config_snapshot(mission_id) {
            eprintln!("mission migrate: config-snapshot synthesis warning for `{mission_id}`: {e:#}");
        }
    }

    Ok(())
}

/// Build a trivial, task-less [`darkmux_crew::mission_config::MissionConfig`]
/// from a legacy instance's OWN `mission.json` + `phases/<id>.json` files and
/// persist it as that mission's `config-snapshot.json` (#1284 Packet 4a).
/// Every phase becomes a freeform `PhaseConfig` (a hand-authored instance
/// never had a Task/Step graph to transcribe) — exactly the shape
/// `mission_launch::launch` already treats as "mint + start, leave phase
/// transitions operator-driven", so a migrated instance's snapshot describes
/// it honestly: a manual mission, not an automated one.
fn synthesize_config_snapshot(mission_id: &str) -> Result<()> {
    use darkmux_crew::mission_config::{MissionConfig, PhaseConfig, MISSION_CONFIG_SCHEMA};
    use darkmux_crew::types::{Mission, Phase};

    let mission_text = std::fs::read_to_string(lifecycle::mission_path(mission_id))
        .with_context(|| format!("reading mission.json for `{mission_id}`"))?;
    let mission: Mission = serde_json::from_str(&mission_text)
        .with_context(|| format!("parsing mission.json for `{mission_id}`"))?;

    let mut phases = Vec::with_capacity(mission.phase_ids.len());
    for phase_id in &mission.phase_ids {
        let phase_text = std::fs::read_to_string(lifecycle::phase_path(mission_id, phase_id))
            .with_context(|| format!("reading phase `{phase_id}` for mission `{mission_id}`"))?;
        let phase: Phase = serde_json::from_str(&phase_text)
            .with_context(|| format!("parsing phase `{phase_id}` for mission `{mission_id}`"))?;
        phases.push(PhaseConfig {
            id: phase.id,
            description: Some(phase.description),
            tasks: Vec::new(),
            extras: BTreeMap::new(),
        });
    }

    let config = MissionConfig {
        id: mission_id.to_string(),
        name: mission_id.to_string(),
        description: Some(mission.description),
        schema_version: Some(MISSION_CONFIG_SCHEMA.to_string()),
        inputs: Vec::new(),
        phases,
        extras: BTreeMap::new(),
    };
    lifecycle::save_config_snapshot(mission_id, &config)
}

/// Print a human-readable dry-run preview (or summary) of a [`MigratePlan`].
pub fn print_plan(plan: &MigratePlan) {
    if plan.is_empty() {
        println!("mission migrate: nothing to do — already on per-mission layout.");
        return;
    }
    if !plan.mission_moves.is_empty() {
        println!("Mission moves ({}):", plan.mission_moves.len());
        for (src, dst) in &plan.mission_moves {
            println!("  {}\n    -> {}", src.display(), dst.display());
        }
    }
    if !plan.phase_moves.is_empty() {
        println!("Phase moves ({}):", plan.phase_moves.len());
        for (src, dst) in &plan.phase_moves {
            println!("  {}\n    -> {}", src.display(), dst.display());
        }
    }
    if !plan.orphan_phases.is_empty() {
        println!(
            "Orphan phases ({} — mission_id has no matching mission JSON; SKIPPED):",
            plan.orphan_phases.len()
        );
        for path in &plan.orphan_phases {
            println!("  {}", path.display());
        }
        println!("  (Run with --apply to migrate everything else and leave orphans in place,");
        println!("  then manually move or rm orphan files yourself.)");
    }
    if !plan.config_snapshots_missing.is_empty() {
        println!(
            "Config snapshots to synthesize ({} — legacy instance(s) with no config-snapshot.json):",
            plan.config_snapshots_missing.len()
        );
        for id in &plan.config_snapshots_missing {
            println!("  {id}  (freeform — every phase becomes task-less)");
        }
    }
    println!(
        "\nSummary: {} mission(s), {} phase(s), {} orphan(s), {} config-snapshot(s) to synthesize.",
        plan.mission_moves.len(),
        plan.phase_moves.len(),
        plan.orphan_phases.len(),
        plan.config_snapshots_missing.len(),
    );
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
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

    fn write_flat_mission(root: &std::path::Path, id: &str) {
        let dir = root.join("missions");
        std::fs::create_dir_all(&dir).unwrap();
        let body = serde_json::json!({
            "id": id,
            "description": "test",
            "phase_ids": [],
            "created_ts": 1,
        });
        std::fs::write(
            dir.join(format!("{id}.json")),
            serde_json::to_string_pretty(&body).unwrap(),
        )
        .unwrap();
    }

    fn write_flat_phase(root: &std::path::Path, id: &str, mission_id: &str) {
        let dir = root.join("phases");
        std::fs::create_dir_all(&dir).unwrap();
        let body = serde_json::json!({
            "id": id,
            "mission_id": mission_id,
            "description": "test",
            "depends_on": [],
            "created_ts": 1,
        });
        std::fs::write(
            dir.join(format!("{id}.json")),
            serde_json::to_string_pretty(&body).unwrap(),
        )
        .unwrap();
    }

    #[test]
    #[serial]
    fn plan_empty_root_is_empty() {
        with_test_root(|_root| {
            let plan = plan_migration().unwrap();
            assert!(plan.is_empty());
        });
    }

    #[test]
    #[serial]
    fn plan_one_mission_one_phase() {
        with_test_root(|root| {
            write_flat_mission(root, "alpha");
            write_flat_phase(root, "s1", "alpha");
            let plan = plan_migration().unwrap();
            assert_eq!(plan.mission_moves.len(), 1);
            assert_eq!(plan.phase_moves.len(), 1);
            assert_eq!(plan.orphan_phases.len(), 0);
        });
    }

    #[test]
    #[serial]
    fn plan_orphan_phase_when_mission_missing() {
        with_test_root(|root| {
            write_flat_phase(root, "dangling", "ghost-mission");
            let plan = plan_migration().unwrap();
            assert_eq!(plan.mission_moves.len(), 0);
            assert_eq!(plan.phase_moves.len(), 0);
            assert_eq!(plan.orphan_phases.len(), 1);
        });
    }

    #[test]
    #[serial]
    fn apply_moves_files_and_re_apply_is_noop() {
        with_test_root(|root| {
            write_flat_mission(root, "alpha");
            write_flat_phase(root, "s1", "alpha");
            let plan = plan_migration().unwrap();
            apply_migration(&plan).unwrap();

            // Old paths gone.
            assert!(!root.join("missions/alpha.json").exists());
            assert!(!root.join("phases/s1.json").exists());
            // New paths exist.
            assert!(root.join("missions/alpha/mission.json").is_file());
            assert!(root.join("missions/alpha/phases/s1.json").is_file());

            // Re-plan: should be empty (idempotent).
            let plan2 = plan_migration().unwrap();
            assert!(plan2.is_empty(), "second plan should be empty (idempotent)");
        });
    }

    #[test]
    #[serial]
    fn plan_skips_already_migrated_missions() {
        with_test_root(|root| {
            // Operator has BOTH old flat AND new nested for the same mission.
            // Migrate should skip the flat one (target already exists).
            write_flat_mission(root, "alpha");
            // Manually create the nested layout too.
            let nested = root.join("missions/alpha");
            std::fs::create_dir_all(&nested).unwrap();
            std::fs::write(nested.join("mission.json"), "{}").unwrap();

            let plan = plan_migration().unwrap();
            assert_eq!(
                plan.mission_moves.len(),
                0,
                "should skip when target already exists"
            );
        });
    }

    // ── Config-snapshot synthesis (#1284 Packet 4a) ─────────────────────

    fn write_nested_mission(mission_id: &str, description: &str, phase_ids: &[&str]) {
        let mission = serde_json::json!({
            "id": mission_id,
            "description": description,
            "status": "active",
            "phase_ids": phase_ids,
            "created_ts": 1,
        });
        let path = lifecycle::mission_path(mission_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&mission).unwrap()).unwrap();
    }

    fn write_nested_phase(mission_id: &str, phase_id: &str, description: &str) {
        let phase = serde_json::json!({
            "id": phase_id,
            "mission_id": mission_id,
            "description": description,
            "status": "planned",
            "created_ts": 1,
        });
        let path = lifecycle::phase_path(mission_id, phase_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&phase).unwrap()).unwrap();
    }

    #[test]
    #[serial]
    fn plan_reports_missing_config_snapshot_for_an_already_nested_mission() {
        with_test_root(|_root| {
            write_nested_mission("legacy-1", "a legacy hand-authored mission", &["p1"]);
            write_nested_phase("legacy-1", "p1", "the only phase");

            let plan = plan_migration().unwrap();
            assert!(plan.config_snapshots_missing.contains(&"legacy-1".to_string()));
            assert!(!plan.is_empty(), "a missing config-snapshot alone must not read as empty");
        });
    }

    #[test]
    #[serial]
    fn apply_synthesizes_a_config_snapshot_with_the_expected_freeform_shape() {
        with_test_root(|_root| {
            write_nested_mission("legacy-2", "legacy mission body", &["p1", "p2"]);
            write_nested_phase("legacy-2", "p1", "first phase");
            write_nested_phase("legacy-2", "p2", "second phase");

            let plan = plan_migration().unwrap();
            assert!(plan.config_snapshots_missing.contains(&"legacy-2".to_string()));
            apply_migration(&plan).unwrap();

            let snapshot = lifecycle::load_config_snapshot("legacy-2")
                .expect("load_config_snapshot must succeed")
                .expect("a config-snapshot.json must now exist");
            assert_eq!(snapshot.id, "legacy-2");
            assert_eq!(snapshot.description.as_deref(), Some("legacy mission body"));
            assert_eq!(
                snapshot.schema_version.as_deref(),
                Some(darkmux_crew::mission_config::MISSION_CONFIG_SCHEMA)
            );
            assert_eq!(snapshot.phases.len(), 2);
            assert_eq!(snapshot.phases[0].id, "p1");
            assert_eq!(snapshot.phases[1].id, "p2");
            assert!(
                snapshot.phases.iter().all(|p| p.tasks.is_empty()),
                "a legacy hand-authored instance never had a Task/Step graph — every synthesized phase is task-less"
            );
            assert!(snapshot.is_valid(&[]), "the synthesized document must itself validate cleanly");
        });
    }

    #[test]
    #[serial]
    fn apply_config_snapshot_synthesis_is_idempotent() {
        with_test_root(|_root| {
            write_nested_mission("legacy-3", "idempotent check", &[]);

            let plan1 = plan_migration().unwrap();
            apply_migration(&plan1).unwrap();
            assert!(lifecycle::config_snapshot_path("legacy-3").is_file());

            // Second pass: nothing left to do.
            let plan2 = plan_migration().unwrap();
            assert!(
                plan2.config_snapshots_missing.is_empty(),
                "a mission with a snapshot already on disk must not be re-listed"
            );
            apply_migration(&plan2).unwrap(); // no-op; must not error
        });
    }

    #[test]
    #[serial]
    fn apply_migration_synthesizes_snapshots_for_missions_it_just_moved_from_flat() {
        // The flat->nested move and the snapshot synthesis compose in one
        // `--apply` pass — a mission that was still flat at plan time gets
        // BOTH its move and its snapshot in the same `apply_migration` call.
        with_test_root(|root| {
            write_flat_mission(root, "was-flat");
            write_flat_phase(root, "s1", "was-flat");

            let plan = plan_migration().unwrap();
            assert!(plan.config_snapshots_missing.contains(&"was-flat".to_string()));
            apply_migration(&plan).unwrap();

            assert!(root.join("missions/was-flat/mission.json").is_file());
            let snapshot = lifecycle::load_config_snapshot("was-flat").unwrap();
            assert!(snapshot.is_some(), "the just-migrated mission must have a synthesized snapshot");
        });
    }
}
