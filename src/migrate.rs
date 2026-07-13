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
}

impl MigratePlan {
    /// Returns `true` when there is nothing to migrate and nothing to warn about.
    pub fn is_empty(&self) -> bool {
        self.mission_moves.is_empty()
            && self.phase_moves.is_empty()
            && self.orphan_phases.is_empty()
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
    Ok(())
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
    println!(
        "\nSummary: {} mission(s), {} phase(s), {} orphan(s).",
        plan.mission_moves.len(),
        plan.phase_moves.len(),
        plan.orphan_phases.len(),
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
}
