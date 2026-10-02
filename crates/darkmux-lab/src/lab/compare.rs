//! `darkmux run compare <run-A> <run-B>` — diff two runs.

use crate::lab::inspect::{lab_inspect, resolve_run_path};
use crate::workloads::types::InspectionReport;
use anyhow::Result;

/// Result of `darkmux run compare`. Fields are read by the CLI's print
/// path (formatted in main.rs); the dead-code lint doesn't see that
/// because the formatter accesses via `{:?}`/`Debug` on the whole
/// struct. Allowing keeps the public-API shape stable for downstream
/// tools that may consume fields individually.
#[allow(dead_code)]
#[derive(Debug)]
pub struct CompareResult {
    pub a: InspectionReport,
    pub b: InspectionReport,
    pub delta_walltime_ms: i128,
    pub delta_walltime_pct: f64,
    pub delta_turns: i32,
    pub delta_compactions: i32,
    pub notes: Vec<String>,
}

pub fn lab_compare(run_a: &str, run_b: &str) -> Result<CompareResult> {
    let a = lab_inspect(run_a)?;
    let b = lab_inspect(run_b)?;
    let d_wall_ms = b.walltime_ms as i128 - a.walltime_ms as i128;
    let pct = if a.walltime_ms > 0 {
        (d_wall_ms as f64 / a.walltime_ms as f64) * 100.0
    } else {
        0.0
    };
    let mut notes = vec![format!("{} → {}", a.run_id, b.run_id)];
    notes.extend(differences(
        (&a.workload_id, &b.workload_id),
        (run_profile(run_a).as_deref(), run_profile(run_b).as_deref()),
    ));
    notes.extend([
        format!(
            "wall: {}s → {}s ({}{}s, {}{:.1}%)",
            a.walltime_ms / 1000,
            b.walltime_ms / 1000,
            if d_wall_ms >= 0 { "+" } else { "" },
            d_wall_ms / 1000,
            if pct >= 0.0 { "+" } else { "" },
            pct
        ),
        format!("turns: {} → {}", a.turns, b.turns),
        format!("compactions: {} → {}", a.compactions, b.compactions),
    ]);
    if a.mode.is_some() || b.mode.is_some() {
        notes.push(format!(
            "mode: {:?} → {:?}",
            a.mode, b.mode
        ));
    }
    Ok(CompareResult {
        delta_walltime_ms: d_wall_ms,
        delta_walltime_pct: pct,
        delta_turns: b.turns as i32 - a.turns as i32,
        delta_compactions: b.compactions as i32 - a.compactions as i32,
        a,
        b,
        notes,
    })
}

/// (F3) The profile a run's manifest records as requested, when it records one.
fn run_profile(run: &str) -> Option<String> {
    crate::lab::manifest::RunManifest::read_lenient(&resolve_run_path(run))?.profile
}

/// (F3) What differs between the two runs besides the thing under test: a
/// delta across different workloads or profiles is not a like-for-like
/// comparison, and the reader is told before the numbers. A profile missing
/// from either manifest is unknown, not a difference.
fn differences(workloads: (&str, &str), profiles: (Option<&str>, Option<&str>)) -> Vec<String> {
    let mut out = Vec::new();
    if workloads.0 != workloads.1 {
        out.push(format!(
            "note: different workloads ({} vs {}), so the deltas below are not like-for-like",
            workloads.0, workloads.1
        ));
    }
    if let (Some(a), Some(b)) = profiles {
        if a != b {
            out.push(format!(
                "note: different profiles ({a} vs {b}), so the deltas below mix the profile's effect with the run's"
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn differences_names_a_workload_and_a_profile_change() {
        let d = differences(("refresh-rotation", "pepper-grinder"), (Some("deep"), Some("small")));
        assert_eq!(d.len(), 2, "{d:?}");
        assert!(d[0].contains("refresh-rotation vs pepper-grinder"), "{}", d[0]);
        assert!(d[1].contains("deep vs small"), "{}", d[1]);
    }

    #[test]
    fn differences_is_quiet_for_like_for_like_and_for_an_unrecorded_profile() {
        assert!(differences(("w", "w"), (Some("p"), Some("p"))).is_empty());
        assert!(differences(("w", "w"), (None, Some("p"))).is_empty());
    }

    #[test]
    fn compare_errors_when_run_dirs_missing() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        // Both missing manifest.json -> inspect errors -> compare errors.
        let err = lab_compare(a.to_str().unwrap(), b.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("no run manifest"));
    }
}
