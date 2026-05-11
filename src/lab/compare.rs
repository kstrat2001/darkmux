//! `darkmux lab compare <run-A> <run-B>` — diff two runs.

use crate::lab::inspect::lab_inspect;
use crate::workloads::types::InspectionReport;
use anyhow::Result;

/// Result of `darkmux lab compare`. Fields are read by the CLI's print
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
    let mut notes = vec![
        format!("{} → {}", a.run_id, b.run_id),
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
    ];
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
