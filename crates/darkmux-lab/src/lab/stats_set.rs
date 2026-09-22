//! (#2855) A SET of runs, summarized the way a comparison has to read them.
//!
//! [`crate::lab::stats`] derives one run correctly. Comparing engines, models
//! or settings needs several runs per arm, and the summary is where the next
//! round of wrong numbers comes from:
//!
//! - **Ranges, not means.** A mean over n=5 hides the one run that went
//!   sideways, which is usually the finding. Every figure here is a median
//!   with its min and max.
//! - **Cost per SUCCESSFUL outcome.** Neither per-run throughput nor per-run
//!   energy says what a finished result costs, because a failed run still
//!   consumes the GPU. This reversed a headline once: the engine that was
//!   1.7x faster per token cost 1.85x the GPU time per finished run.
//! - **A total with a hole in it is not a total.** Cost per success sums every
//!   run's cost; if one run has no telemetry the sum is short by exactly that
//!   run, and it would read as cheaper. Such a figure is withheld, with the
//!   reason, rather than printed low.
//!
//! Every run stays in the set whatever its checks say, because dropping a
//! run that did not reconcile is its own way of choosing the answer. Instead
//! each run carries [`flags`], and the summary names the runs that raised
//! them.

use crate::lab::stats::RunStats;
use serde::Serialize;

/// Median with the spread around it. The spread is part of the claim.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Range {
    pub n: usize,
    pub median: f64,
    pub min: f64,
    pub max: f64,
}

impl Range {
    /// `None` when no run carried the value, rather than a range of zeros.
    pub fn of(values: impl IntoIterator<Item = f64>) -> Option<Range> {
        let mut v: Vec<f64> = values.into_iter().filter(|x| x.is_finite()).collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
        let n = v.len();
        let median = if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 };
        Some(Range { n, median, min: v[0], max: v[n - 1] })
    }
}

/// What a finished result cost, across the whole set.
///
/// Numerators sum over EVERY run, passed or not; the denominator is the runs
/// that passed. That asymmetry is the point.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CostPerSuccess {
    pub active_ms: Option<f64>,
    /// Wall time the GPU was busy, from each run's duty cycle.
    pub gpu_busy_ms: Option<f64>,
    pub pkg_joules: Option<f64>,
    /// Why a figure above is `None`, when it is.
    pub withheld: Vec<String>,
}

/// One set of runs — an arm of a comparison.
#[derive(Debug, Clone, Serialize)]
pub struct SetSummary {
    pub n: usize,
    pub passed: usize,
    pub failed: usize,
    /// No verify outcome recorded. Not counted as a pass or a failure.
    pub unverified: usize,
    /// Passes from runs whose runtime result was `error`. Verify passed, but
    /// the run did not finish normally, so the pass may say more about the
    /// fixture's starting state than about the run. Counted in `passed`, and
    /// named here so the reader decides.
    pub passed_with_runtime_error: usize,
    /// Models named by the runs. More than one means the set mixes arms.
    pub models: Vec<String>,

    pub active_ms: Option<Range>,
    pub wall_ms: Option<Range>,
    pub rest_ms: Option<Range>,
    pub turns: Option<Range>,
    pub completion_tokens: Option<Range>,
    pub tok_per_s: Option<Range>,
    /// The sample size each `tok_per_s` rests on. A low minimum means at
    /// least one run's rate was measured over a small billed subset.
    pub billed_gen_fraction: Option<Range>,
    pub gpu_w_busy: Option<Range>,
    pub pkg_w_busy: Option<Range>,
    pub pkg_j_per_1k_tokens: Option<Range>,

    /// Runs where either gate judged a turn degenerate.
    pub runs_with_degeneracy: usize,
    /// Turns actually ended by a gate, across the set: stream aborts plus
    /// checkpoint conclusions.
    pub turns_cut: usize,

    pub cost_per_success: CostPerSuccess,

    /// `(run, flags)` for every run that raised one.
    pub flagged: Vec<(String, Vec<&'static str>)>,
}

/// Short codes for a run's failed checks and notable conditions, for a table
/// column. Each maps to a [`crate::lab::stats::RunChecks`] field or a derived
/// condition; `RunStats::unreconciled` holds the long form.
pub fn flags(s: &RunStats) -> Vec<&'static str> {
    let c = &s.checks;
    let mut f = Vec::new();
    // The runtime's own terminal result. A run can pass verify without ever
    // doing the task: when a fixture's verify command is green on the
    // untouched tree, a run that errored in its first seconds records `pass`.
    // Flagged, never reclassified; the outcome is the fixture's to define.
    match s.result.as_deref() {
        Some("error") => f.push("RUNTIME-ERROR"),
        Some(r) if r.starts_with("escalation") => f.push("ESCALATED"),
        _ => {}
    }
    if !c.tokens_reconcile {
        f.push("TOKENS");
    }
    if !c.checkpoint_parse_consistent || !c.missing_required_events.is_empty() {
        f.push("PARSE");
    }
    if !c.verdict_matches_ratio {
        f.push("VERDICT");
    }
    if !c.all_streams_billed {
        f.push("UNBILLED");
    }
    if !c.frames_match_streams || !c.streams_terminated {
        f.push("STREAMS");
    }
    if !c.rest_within_wall {
        f.push("REST");
    }
    if !c.have_telemetry_samples {
        f.push("NO-TELEM");
    }
    if s.thermal_ratchet_fired {
        f.push("RATCHET");
    }
    if s.throttled_samples > 0 {
        f.push("THROTTLE");
    }
    f
}

fn verified(s: &RunStats) -> Option<bool> {
    match s.verify.as_deref() {
        Some("pass") => Some(true),
        Some("fail") => Some(false),
        _ => None,
    }
}

pub fn summarize(runs: &[RunStats]) -> SetSummary {
    let n = runs.len();
    let passed = runs.iter().filter(|s| verified(s) == Some(true)).count();
    let failed = runs.iter().filter(|s| verified(s) == Some(false)).count();

    let mut models: Vec<String> = runs.iter().filter_map(|s| s.model.clone()).collect();
    models.sort();
    models.dedup();

    let r = |f: fn(&RunStats) -> Option<f64>| Range::of(runs.iter().filter_map(f));

    // Cost per success. A numerator summed over only the runs that HAVE the
    // value is short by the ones that do not, and reads as cheaper; so each
    // figure is all-or-nothing across the set.
    let mut withheld = Vec::new();
    let per_success = |total: Option<f64>, what: &str, withheld: &mut Vec<String>| -> Option<f64> {
        if passed == 0 {
            return None;
        }
        match total {
            Some(t) => Some(t / passed as f64),
            None => {
                withheld.push(format!("{what}: not every run in the set recorded it"));
                None
            }
        }
    };
    let sum_all = |f: fn(&RunStats) -> Option<f64>| -> Option<f64> {
        runs.iter().map(f).try_fold(0.0, |acc, v| v.map(|v| acc + v))
    };
    if passed == 0 && n > 0 {
        withheld.push("no run in the set passed verify, so there is no success to cost".into());
    }
    let cost_per_success = CostPerSuccess {
        active_ms: per_success(sum_all(|s| Some(s.active_ms as f64)), "active time", &mut withheld),
        gpu_busy_ms: per_success(
            sum_all(|s| s.busy_ms.map(|v| v as f64)),
            "GPU busy time",
            &mut withheld,
        ),
        pkg_joules: per_success(sum_all(|s| s.pkg_j_busy), "package energy", &mut withheld),
        withheld,
    };

    SetSummary {
        n,
        passed,
        failed,
        unverified: n - passed - failed,
        passed_with_runtime_error: runs
            .iter()
            .filter(|s| verified(s) == Some(true) && s.result.as_deref() == Some("error"))
            .count(),
        models,
        active_ms: r(|s| Some(s.active_ms as f64)),
        wall_ms: r(|s| Some(s.wall_ms as f64)),
        rest_ms: r(|s| Some(s.rest_ms as f64)),
        turns: r(|s| Some(s.turns as f64)),
        completion_tokens: r(|s| Some(s.completion_tokens as f64)),
        tok_per_s: r(|s| s.tok_per_s),
        billed_gen_fraction: r(|s| s.billed_gen_fraction),
        gpu_w_busy: r(|s| s.gpu_w_busy),
        pkg_w_busy: r(|s| s.pkg_w_busy),
        pkg_j_per_1k_tokens: r(|s| s.pkg_j_per_1k_tokens),
        runs_with_degeneracy: runs
            .iter()
            .filter(|s| {
                !s.gates.stream.degenerate_turns.is_empty()
                    || !s.gates.checkpoint.degenerate_turns.is_empty()
            })
            .count(),
        turns_cut: runs
            .iter()
            .map(|s| s.gates.stream.aborts + s.gates.checkpoint.concluded_turns.len())
            .sum(),
        cost_per_success,
        flagged: runs
            .iter()
            .filter_map(|s| {
                let f = flags(s);
                (!f.is_empty()).then(|| (s.run.clone(), f))
            })
            .collect(),
    }
}

/// `candidate / baseline`, for a figure both sides have. `None` when either
/// side is missing or the baseline is zero — never an infinity.
pub fn ratio(candidate: Option<f64>, baseline: Option<f64>) -> Option<f64> {
    match (candidate, baseline) {
        (Some(c), Some(b)) if b != 0.0 => Some(c / b),
        _ => None,
    }
}

#[cfg(test)]
#[path = "stats_set_tests.rs"]
mod tests;
