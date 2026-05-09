//! `darkmux lab tune <workload> --runs N` — multi-run distribution
//! characterization with bimodal cluster detection.
//!
//! Wraps `lab_run` with multiple iterations, then computes:
//!   - mean wall clock + range (min/max)
//!   - fast cluster (mean + count) and slow cluster (mean + count)
//!   - slow rate (% of runs in the slow cluster)
//!
//! The bimodal split is what makes Article 2's claims interesting — naive
//! `mean ± stdev` collapses the fast/slow modes that are the real story.
//! See `~/.openclaw/PERFORMANCE.md` §1.4.3 (or LAB_NOTEBOOK.md §1960) for
//! the empirical motivation behind the bimodal model.

use crate::lab::run::{RunOpts, RunOutcome, lab_run};
use anyhow::Result;

#[derive(Debug, Clone)]
pub struct TuneOpts {
    pub workload: String,
    pub profile: Option<String>,
    pub runs: u32,
    pub config: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TuneReport {
    pub workload: String,
    pub profile: Option<String>,
    pub outcomes: Vec<RunOutcome>,
    pub stats: DistributionStats,
}

#[derive(Debug, Clone)]
pub struct DistributionStats {
    pub n: usize,
    pub min_seconds: u128,
    pub max_seconds: u128,
    pub mean_seconds: u128,
    pub fast_cluster: ClusterStats,
    pub slow_cluster: ClusterStats,
    pub slow_rate: f32,
}

#[derive(Debug, Clone)]
pub struct ClusterStats {
    pub count: usize,
    pub mean_seconds: Option<u128>,
    pub min_seconds: Option<u128>,
    pub max_seconds: Option<u128>,
}

pub fn tune(opts: &TuneOpts) -> Result<TuneReport> {
    let runs = opts.runs.max(1);
    let outcomes = lab_run(RunOpts {
        workload_id: opts.workload.clone(),
        profile_name: opts.profile.clone(),
        runs,
        config_path: opts.config.clone(),
        quiet: false,
    })?;
    let stats = compute_stats(&outcomes);
    Ok(TuneReport {
        workload: opts.workload.clone(),
        profile: opts.profile.clone(),
        outcomes,
        stats,
    })
}

/// Bimodal cluster detection. We call the boundary the **midpoint between
/// min and max** wall clock — runs at or below midpoint are "fast cluster",
/// above are "slow cluster". This is intentionally simple — it catches the
/// 200s/700s split without needing k-means or stats deps. Edge case: if all
/// runs are within 1.5× of each other, treat as a single cluster (no
/// meaningful bimodal signal).
pub fn compute_stats(outcomes: &[RunOutcome]) -> DistributionStats {
    let secs: Vec<u128> = outcomes
        .iter()
        .map(|o| o.duration_ms / 1000)
        .collect();
    let n = secs.len();
    if n == 0 {
        return DistributionStats {
            n: 0,
            min_seconds: 0,
            max_seconds: 0,
            mean_seconds: 0,
            fast_cluster: ClusterStats {
                count: 0,
                mean_seconds: None,
                min_seconds: None,
                max_seconds: None,
            },
            slow_cluster: ClusterStats {
                count: 0,
                mean_seconds: None,
                min_seconds: None,
                max_seconds: None,
            },
            slow_rate: 0.0,
        };
    }

    let min = *secs.iter().min().unwrap();
    let max = *secs.iter().max().unwrap();
    let sum: u128 = secs.iter().sum();
    let mean = sum / n as u128;

    // Decide whether bimodal split is meaningful: only if max is at least
    // 1.5× min AND we have ≥3 runs. Otherwise everything goes in fast.
    let bimodal = n >= 3 && max as f32 >= 1.5 * (min.max(1) as f32);

    let midpoint = if bimodal { (min + max) / 2 } else { u128::MAX };

    let mut fast: Vec<u128> = Vec::new();
    let mut slow: Vec<u128> = Vec::new();
    for s in &secs {
        if *s <= midpoint {
            fast.push(*s);
        } else {
            slow.push(*s);
        }
    }

    let slow_rate = (slow.len() as f32) / (n as f32);

    DistributionStats {
        n,
        min_seconds: min,
        max_seconds: max,
        mean_seconds: mean,
        fast_cluster: cluster_stats(&fast),
        slow_cluster: cluster_stats(&slow),
        slow_rate,
    }
}

fn cluster_stats(values: &[u128]) -> ClusterStats {
    if values.is_empty() {
        return ClusterStats {
            count: 0,
            mean_seconds: None,
            min_seconds: None,
            max_seconds: None,
        };
    }
    let sum: u128 = values.iter().sum();
    let mean = sum / values.len() as u128;
    let min = *values.iter().min().unwrap();
    let max = *values.iter().max().unwrap();
    ClusterStats {
        count: values.len(),
        mean_seconds: Some(mean),
        min_seconds: Some(min),
        max_seconds: Some(max),
    }
}

pub fn print_report(r: &TuneReport) {
    println!(
        "darkmux tune — workload `{}` profile `{}` × {} run(s)",
        r.workload,
        r.profile.as_deref().unwrap_or("(default)"),
        r.stats.n
    );
    println!();

    let s = &r.stats;
    if s.n == 0 {
        println!("(no runs completed)");
        return;
    }

    println!("┌─ wall clock");
    println!("│  range:  {}s – {}s", s.min_seconds, s.max_seconds);
    println!("│  mean:   {}s", s.mean_seconds);
    if s.slow_cluster.count == 0 {
        println!("│  cluster: single (variance < 1.5×, no meaningful bimodal split)");
    } else {
        let fc = &s.fast_cluster;
        let sc = &s.slow_cluster;
        println!(
            "│  fast cluster: n={} mean={}s range={}s–{}s",
            fc.count,
            fc.mean_seconds.unwrap_or(0),
            fc.min_seconds.unwrap_or(0),
            fc.max_seconds.unwrap_or(0)
        );
        println!(
            "│  slow cluster: n={} mean={}s range={}s–{}s",
            sc.count,
            sc.mean_seconds.unwrap_or(0),
            sc.min_seconds.unwrap_or(0),
            sc.max_seconds.unwrap_or(0)
        );
        println!("│  slow rate:   {:.0}%", s.slow_rate * 100.0);
    }
    println!("└─");
    println!();

    let dispatch_failures = r.outcomes.iter().filter(|o| !o.ok).count();
    let verify_failures = r
        .outcomes
        .iter()
        .filter(|o| matches!(o.verify_passed, Some(false)))
        .count();
    if dispatch_failures > 0 {
        println!(
            "⚠ {} of {} dispatches failed (runtime non-zero exit) — check `darkmux doctor` and \
             individual run dirs for the trace",
            dispatch_failures, s.n
        );
    }
    if verify_failures > 0 {
        println!(
            "⚠ {} of {} runs failed verify (dispatch ok, output didn't match expected)",
            verify_failures, s.n
        );
    }

    println!();
    println!("Next steps:");
    println!("  • `darkmux lab inspect <run-id>` for any individual run");
    if r.outcomes.len() >= 2 {
        let a = r.outcomes.first().map(|o| o.run_id.as_str()).unwrap_or("");
        let b = r.outcomes.last().map(|o| o.run_id.as_str()).unwrap_or("");
        println!("  • `darkmux lab compare {a} {b}` for a head-to-head diff");
    }
    if s.slow_cluster.count > 0 {
        println!("  • Slow cluster present — re-tune compaction knobs and re-run");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn outcome(secs: u64) -> RunOutcome {
        RunOutcome {
            run_id: format!("test-{secs}"),
            run_dir: PathBuf::from("/tmp"),
            ok: true,
            verify_passed: Some(true),
            duration_ms: (secs as u128) * 1000,
            notes: vec![],
        }
    }

    #[test]
    fn empty_outcomes_zeros() {
        let s = compute_stats(&[]);
        assert_eq!(s.n, 0);
        assert_eq!(s.fast_cluster.count, 0);
        assert_eq!(s.slow_cluster.count, 0);
    }

    #[test]
    fn single_run_no_cluster_split() {
        let s = compute_stats(&[outcome(8)]);
        assert_eq!(s.n, 1);
        assert_eq!(s.min_seconds, 8);
        assert_eq!(s.max_seconds, 8);
        assert_eq!(s.fast_cluster.count, 1);
        assert_eq!(s.slow_cluster.count, 0);
    }

    #[test]
    fn tight_distribution_no_bimodal_split() {
        let s = compute_stats(&[outcome(6), outcome(7), outcome(8), outcome(7)]);
        // Variance under 1.5× → no slow cluster
        assert_eq!(s.fast_cluster.count, 4);
        assert_eq!(s.slow_cluster.count, 0);
        assert_eq!(s.slow_rate, 0.0);
    }

    #[test]
    fn bimodal_distribution_splits_correctly() {
        // Article 2 reference shape: fast cluster ~220s, slow ~770s.
        let s = compute_stats(&[
            outcome(197),
            outcome(218),
            outcome(276),
            outcome(606),
            outcome(939),
        ]);
        assert!(s.fast_cluster.count >= 2);
        assert!(s.slow_cluster.count >= 1);
        assert_eq!(s.fast_cluster.count + s.slow_cluster.count, 5);
        // The fast cluster mean should be much smaller than slow
        let fc = s.fast_cluster.mean_seconds.unwrap();
        let sc = s.slow_cluster.mean_seconds.unwrap();
        assert!(sc > fc * 2);
    }

    #[test]
    fn slow_rate_is_fraction_of_total() {
        let s = compute_stats(&[
            outcome(200),
            outcome(220),
            outcome(900),
        ]);
        assert!((s.slow_rate - 1.0 / 3.0).abs() < 0.01);
    }

    #[test]
    fn n2_does_not_force_bimodal_split() {
        // With n=2 we don't have enough samples to claim bimodality.
        let s = compute_stats(&[outcome(200), outcome(800)]);
        // Should treat as single cluster (n<3 condition)
        assert_eq!(s.fast_cluster.count, 2);
        assert_eq!(s.slow_cluster.count, 0);
    }
}
