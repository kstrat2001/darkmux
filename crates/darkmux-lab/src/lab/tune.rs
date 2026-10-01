//! `darkmux lab tune <workload> --repeat N` — multi-run distribution
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

use crate::lab::dispatch_end::DispatchEnd;
use crate::lab::run::{lab_run, RunOpts, RunOutcome};
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
    /// (#2848) Wall clock SUMMED across the N runs -- the execution time
    /// spent on the tasks. The campaign question a blocked run asks is "how
    /// long did the whole set take", and no other field answers it:
    /// reconstructing it as `mean * n` loses the integer-division
    /// remainder.
    ///
    /// Each run's own clock, so in-run rest (thermal duty-cycle pauses,
    /// turn delay) is INSIDE this figure deliberately. A throttled engine
    /// taking longer on run 5 is the signal, not noise to be subtracted.
    pub total_seconds: u128,
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
        // tune() runs the workload through the default runtime
        // (internal, post-Phase-D).
        // tune() varies model behavior across N runs, not loop config (#986).
        loop_override: None,
        inject_context: None, // tune() never injects engagement-context (#1004)
    })?;
    let stats = compute_stats(&outcomes);
    Ok(TuneReport {
        workload: opts.workload.clone(),
        profile: opts.profile.clone(),
        outcomes,
        stats,
    })
}

/// Distribution stats over the runs that COMPLETED (#2986): an errored run
/// has no wall clock worth measuring, and is named in the report instead.
///
/// Bimodal cluster detection. We call the boundary the **midpoint between
/// min and max** wall clock — runs at or below midpoint are "fast cluster",
/// above are "slow cluster". This is intentionally simple — it catches the
/// 200s/700s split without needing k-means or stats deps. Edge case: if all
/// runs are within 1.5× of each other, treat as a single cluster (no
/// meaningful bimodal signal).
pub(crate) fn compute_stats(outcomes: &[RunOutcome]) -> DistributionStats {
    let secs: Vec<u128> = outcomes.iter().filter(|o| o.completed()).map(|o| o.duration_ms / 1000).collect();
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
            total_seconds: 0,
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
        total_seconds: sum,
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
    print!("{}", render_report(r));
}

/// The text `lab tune` prints.
pub(crate) fn render_report(r: &TuneReport) -> String {
    let mut out = String::new();
    p!(
        out,
        "darkmux tune — workload `{}` profile `{}` × {} run(s), {} completed",
        r.workload,
        r.profile.as_deref().unwrap_or("(default)"),
        r.outcomes.len(),
        r.stats.n
    );
    p!(out);
    let s = &r.stats;
    if s.n == 0 {
        p!(out, "(no runs completed)");
        render_errored(&mut out, &r.outcomes);
        return out;
    }
    render_wall_clock(&mut out, s);
    p!(out);
    render_failures(&mut out, &r.outcomes, s.n);
    render_errored(&mut out, &r.outcomes);
    p!(out);
    p!(out, "Next steps:");
    p!(out, "  • `darkmux run inspect <run-id>` for any individual run");
    let completed: Vec<_> = r.outcomes.iter().filter(|o| o.completed()).collect();
    if let [first, .., last] = completed.as_slice() {
        p!(out, "  • `darkmux run compare {} {}` for a head-to-head diff", first.run_id, last.run_id);
    }
    if s.slow_cluster.count > 0 {
        p!(out, "  • Slow cluster present — re-tune compaction knobs and re-run");
    }
    out
}

fn render_wall_clock(out: &mut String, s: &DistributionStats) {
    p!(out, "┌─ wall clock");
    p!(out, "│  range:  {}s – {}s", s.min_seconds, s.max_seconds);
    p!(out, "│  mean:   {}s", s.mean_seconds);
    p!(out, "│  total:  {}s across {} run(s)", s.total_seconds, s.n);
    if s.slow_cluster.count == 0 {
        p!(out, "│  cluster: single (variance < 1.5×, no meaningful bimodal split)");
    } else {
        p!(out, "│  fast cluster: {}", cluster_line(&s.fast_cluster));
        p!(out, "│  slow cluster: {}", cluster_line(&s.slow_cluster));
        p!(out, "│  slow rate:   {:.0}%", s.slow_rate * 100.0);
    }
    p!(out, "└─");
}

fn cluster_line(c: &ClusterStats) -> String {
    format!(
        "n={} mean={}s range={}s–{}s",
        c.count,
        c.mean_seconds.unwrap_or(0),
        c.min_seconds.unwrap_or(0),
        c.max_seconds.unwrap_or(0)
    )
}

/// (#2986) Each run whose provider errored before completing, by name.
fn render_errored(out: &mut String, outcomes: &[RunOutcome]) {
    for o in outcomes {
        if let Some(e) = &o.provider_error {
            p!(out, "✗ {} errored before completing: {e}", o.run_id);
        }
    }
}

/// Failures among the `n` runs that completed.
fn render_failures(out: &mut String, outcomes: &[RunOutcome], n: usize) {
    let dispatch_failures =
        outcomes.iter().filter(|o| o.completed() && o.end() == DispatchEnd::Failed).count();
    let escalations = outcomes.iter().filter(|o| o.escalation.is_some()).count();
    // An escalated run's verify fails because the work never finished, not
    // because the output missed: the escalation line below names it.
    let verify_failures = outcomes.iter().filter(|o| o.verify_failed() && o.escalation.is_none()).count();
    if dispatch_failures > 0 {
        p!(
            out,
            "⚠ {dispatch_failures} of {n} dispatches failed (runtime non-zero exit) — check \
             `darkmux doctor` and individual run dirs for the trace"
        );
    }
    if escalations > 0 {
        p!(
            out,
            "↑ {escalations} of {n} dispatches escalated (the runtime handed the work to a higher tier; \
             not an error)"
        );
    }
    if verify_failures > 0 {
        p!(out, "⚠ {verify_failures} of {n} runs failed verify (dispatch ok, output didn't match expected)");
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
            provider_error: None,
            escalation: None,
        }
    }

    fn tune_report(outcomes: Vec<RunOutcome>) -> TuneReport {
        let stats = compute_stats(&outcomes);
        TuneReport { workload: "w".into(), profile: None, outcomes, stats }
    }

    /// (F2) An escalated dispatch is not "runtime non-zero exit": it is counted
    /// on its own line as an escalation, and only a real failure reads failed.
    #[test]
    fn an_escalated_run_is_not_counted_as_a_failed_dispatch() {
        let mut esc = outcome(8);
        esc.ok = false;
        esc.verify_passed = Some(false);
        esc.escalation = Some("escalation_compaction_reread_loop".into());
        let mut out = String::new();
        render_failures(&mut out, &[esc.clone(), outcome(9)], 2);
        assert!(!out.contains("failed") && !out.contains("non-zero"), "{out}");
        assert!(out.contains("1 of 2 dispatches escalated"), "{out}");
        let mut failed = outcome(7);
        failed.ok = false;
        let mut both = String::new();
        render_failures(&mut both, &[esc, failed], 2);
        assert!(both.contains("1 of 2 dispatches failed (runtime non-zero exit)"), "{both}");
        assert!(both.contains("1 of 2 dispatches escalated"), "{both}");
    }

    /// The whole report for a tight two-run set, byte for byte.
    #[test]
    fn a_single_cluster_set_renders_the_full_report() {
        let text = render_report(&tune_report(vec![outcome(8), outcome(9)]));
        assert_eq!(
            text,
            "darkmux tune — workload `w` profile `(default)` × 2 run(s), 2 completed\n\n\
             ┌─ wall clock\n│  range:  8s – 9s\n│  mean:   8s\n│  total:  17s across 2 run(s)\n\
             │  cluster: single (variance < 1.5×, no meaningful bimodal split)\n└─\n\n\n\
             Next steps:\n  • `darkmux run inspect <run-id>` for any individual run\n\
             \x20 • `darkmux run compare test-8 test-9` for a head-to-head diff\n"
        );
    }

    /// A bimodal set prints both clusters and the slow-cluster hint; failed
    /// dispatches and failed verifies are each counted; a verify nothing
    /// declared is not.
    #[test]
    fn a_bimodal_set_with_failures_renders_clusters_and_counts() {
        let mut runs = vec![outcome(200), outcome(220), outcome(230), outcome(900)];
        runs[0].ok = false;
        runs[1].verify_passed = Some(false);
        runs[2].verify_passed = None;
        runs[3].verify_passed = Some(false);
        let text = render_report(&tune_report(runs));
        assert!(text.contains("│  fast cluster: n=3 mean=216s range=200s–230s\n"), "{text}");
        assert!(text.contains("│  slow cluster: n=1 mean=900s range=900s–900s\n"), "{text}");
        assert!(text.contains("│  slow rate:   25%\n"), "{text}");
        assert!(text.contains("⚠ 1 of 4 dispatches failed"), "{text}");
        assert!(text.contains("⚠ 2 of 4 runs failed verify"), "{text}");
        assert!(text.contains("Slow cluster present"), "{text}");
    }

    /// No runs: the header and the empty marker only; one run: no compare hint.
    #[test]
    fn empty_and_single_run_reports() {
        let empty = render_report(&tune_report(vec![]));
        assert_eq!(empty, "darkmux tune — workload `w` profile `(default)` × 0 run(s), 0 completed\n\n(no runs completed)\n");
        let one = render_report(&tune_report(vec![outcome(5)]));
        assert!(!one.contains("run compare"), "{one}");
        assert!(!one.contains("⚠"), "{one}");
    }

    /// `tune` runs the workload N times through `lab_run`.
    #[test]
    #[serial_test::serial]
    fn tune_runs_the_workload_n_times() {
        use crate::lab::run::run_tests::{script, Lab, Script};
        let lab = Lab::scripted(&["wtune"]);
        script(Script { ok: true, verify: Some(true), ..Default::default() });
        let r = tune(&TuneOpts { workload: "wtune".into(), profile: None, runs: 2, config: Some(lab.profiles.clone()) })
            .unwrap();
        assert_eq!(r.outcomes.len(), 2);
        assert_eq!(r.stats.n, 2);
        assert_eq!(crate::lab::run::exit_code(&r.outcomes), 0);
    }

    /// (#2986) A provider error on run 2 of 4 fails that run only: the
    /// batch keeps going, all four outcomes come back, the errored run is
    /// named, the stats cover the three that completed, and the verb still
    /// exits 1.
    #[test]
    #[serial_test::serial]
    fn a_failed_run_mid_batch_keeps_every_other_outcome() {
        use crate::lab::run::run_tests::{script, Lab, Script};
        let lab = Lab::scripted(&["wbatch"]);
        script(Script { ok: true, verify: Some(true), run_err_on_call: Some(2), ..Default::default() });
        let r = tune(&TuneOpts { workload: "wbatch".into(), profile: None, runs: 4, config: Some(lab.profiles.clone()) })
            .unwrap();
        assert_eq!(r.outcomes.len(), 4);
        let failed: Vec<_> = r.outcomes.iter().filter(|o| o.provider_error.is_some()).collect();
        assert_eq!(failed.len(), 1);
        assert!(failed[0].run_id.ends_with("-2"), "{}", failed[0].run_id);
        assert!(!failed[0].ok);
        assert_eq!(failed[0].provider_error.as_deref(), Some("scripted failure on run 2"));
        assert_eq!(crate::lab::run::exit_code(&r.outcomes), 1);
        assert_eq!(r.stats.n, 3, "stats cover the completed runs");
        let text = render_report(&r);
        assert!(text.starts_with("darkmux tune — workload `wbatch` profile `(default)` × 4 run(s), 3 completed\n"), "{text}");
        assert!(
            text.contains(&format!("✗ {} errored before completing: scripted failure on run 2\n", failed[0].run_id)),
            "{text}"
        );
        assert!(!text.contains("dispatches failed"), "an errored run is not a completed failed dispatch: {text}");
    }

    /// (#2986) When every run errored there are no stats, and each errored
    /// run is still named.
    #[test]
    fn a_batch_where_every_run_errored_names_each_one() {
        let mut o = outcome(0);
        o.ok = false;
        o.provider_error = Some("boom".into());
        let text = render_report(&tune_report(vec![o]));
        assert_eq!(
            text,
            "darkmux tune — workload `w` profile `(default)` × 1 run(s), 0 completed\n\n(no runs completed)\n\
             ✗ test-0 errored before completing: boom\n"
        );
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

    /// (#2848) The block total is the SUM, and it is not reconstructible
    /// from the other reported fields. These five values were chosen so
    /// `mean * n` DISAGREES with the true total: 2396/5 = 479 by integer
    /// division, and 479 * 5 = 2395, one second short. A campaign total
    /// that silently drifts from the runs it summarizes is worse than no
    /// total, so this pins the sum itself rather than a derivation of it.
    #[test]
    fn total_seconds_is_the_sum_not_mean_times_n() {
        let s = compute_stats(&[outcome(490), outcome(793), outcome(375), outcome(674), outcome(64)]);
        assert_eq!(s.total_seconds, 2396, "total must be the plain sum of every run");
        assert_eq!(s.n, 5);
        assert_eq!(s.mean_seconds, 479, "integer division floors here, which is the point");
        assert_ne!(
            s.mean_seconds * s.n as u128,
            s.total_seconds,
            "if these ever agree this test has stopped proving anything -- pick inputs whose mean does not divide evenly"
        );
    }

    /// (#2848) In-run rest is INSIDE the figure by design: the total is each
    /// run's own wall clock, so an engine that throttles on later runs shows
    /// up as a larger total rather than having the pause subtracted away.
    #[test]
    fn total_seconds_counts_a_throttled_run_at_full_length() {
        let steady =
            compute_stats(&[outcome(300), outcome(300), outcome(300), outcome(300), outcome(300)]);
        let throttled =
            compute_stats(&[outcome(300), outcome(300), outcome(300), outcome(300), outcome(600)]);
        assert_eq!(steady.total_seconds, 1500);
        assert_eq!(
            throttled.total_seconds, 1800,
            "the run that took twice as long must add its full duration, pause included"
        );
    }

    #[test]
    fn slow_rate_is_fraction_of_total() {
        let s = compute_stats(&[outcome(200), outcome(220), outcome(900)]);
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
