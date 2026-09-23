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
    if c.metrics_stale {
        f.push("STALE-METRICS");
    }
    if c.tokens_reconcile == Some(false) {
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
    if c.turns_match_trajectory == Some(false) || c.rest_matches_trajectory == Some(false) {
        f.push("COUNTS");
    }
    if !c.have_telemetry_samples {
        f.push("NO-TELEM");
    } else if !c.telemetry_covers_run {
        f.push("TELEM-GAP");
    }
    if !c.have_flow_records {
        f.push("NO-FLOW");
    }
    if c.policy_consistent == Some(false) {
        f.push("POLICY");
    }
    if !s.suspect_turns.is_empty() {
        f.push("CHARS");
    }
    if s.thermal_ratchet_fired {
        f.push("RATCHET");
    }
    if s.throttled_samples > 0 {
        f.push("THROTTLE");
    }
    // (#2833) verify came from before the write-the-tests work gate existed
    // for this fixture — the old, vacuous "verify command exited 0" signal,
    // not the gated one. Never reinterpreted; flagged so a mixed series
    // isn't silently compared across two definitions of success.
    if s.verify_ungated {
        f.push("UNGATED");
    }
    f
}

/// Run ids in `runs` whose `[started_at_unix_ms, +wall_ms]` window overlaps
/// another run's window IN THE SAME SET. `scan_flow_lines` matches host
/// telemetry by time window only, so two runs overlapping in time each claim
/// the WHOLE host's power — there is no way to apportion it between them, so
/// this only detects the condition; the caller withholds the figures it
/// would corrupt rather than dividing them up.
///
/// `metrics_stale` runs are excluded before windows are even built (review,
/// 2026-09-23): a STALE-METRICS run's window is `metrics.json`'s claim
/// about SOME run's clock, not necessarily this one's — using it to accuse
/// a genuine, clean run of overlapping is exactly what a byte-identical
/// stale copy did on disk. All 17 real OVERLAP hits before this fix were
/// stale copies or the clean owner they were copied from; zero were
/// independent concurrent runs.
pub fn overlapping(runs: &[RunStats]) -> std::collections::BTreeSet<String> {
    let mut windows: Vec<(&str, u64, u64)> = runs
        .iter()
        .filter(|s| !s.checks.metrics_stale)
        .filter_map(|s| s.started_at_unix_ms.map(|from| (s.run.as_str(), from, from.saturating_add(s.wall_ms))))
        .collect();
    windows.sort_by_key(|(_, from, _)| *from);
    let mut out = std::collections::BTreeSet::new();
    for i in 0..windows.len() {
        for j in (i + 1)..windows.len() {
            let (a_run, _, a_to) = windows[i];
            let (b_run, b_from, _) = windows[j];
            // Sorted by start: once a later run starts at or after `a`
            // ends, nothing further out can overlap `a` either.
            if b_from >= a_to {
                break;
            }
            out.insert(a_run.to_string());
            out.insert(b_run.to_string());
        }
    }
    out
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
    let overlap = overlapping(runs);
    let stale: std::collections::BTreeSet<&str> =
        runs.iter().filter(|s| s.checks.metrics_stale).map(|s| s.run.as_str()).collect();

    let mut models: Vec<String> = runs.iter().filter_map(|s| s.model.clone()).collect();
    models.sort();
    models.dedup();

    let r = |f: fn(&RunStats) -> Option<f64>| Range::of(runs.iter().filter_map(f));
    // (Review, 2026-09-23) A STALE-METRICS run's wall/rest/active time is
    // built on another run's clock, and a STALE-METRICS or OVERLAP run's
    // power/energy is either the same suspect window or the whole host's
    // draw claimed twice — either way a wrong number, not merely an
    // uncertain one, so these SET-level figures exclude the run rather than
    // averaging it in. This is narrower than the module's general "never
    // drop a run" rule: that rule is for CHECKS that leave the figure
    // itself untouched (TOKENS, UNBILLED, …); these two conditions mean the
    // figure is actively wrong. The per-run TABLE ROW still prints the
    // run's own number, flagged — only the aggregate excludes it.
    let r_excl = |excl: &std::collections::BTreeSet<&str>, f: fn(&RunStats) -> Option<f64>| {
        Range::of(runs.iter().filter(|s| !excl.contains(s.run.as_str())).filter_map(f))
    };

    // Cost per success. A numerator summed over only the runs that HAVE the
    // value is short by the ones that do not, and reads as cheaper; so each
    // figure is all-or-nothing across the set.
    let mut withheld = Vec::new();
    // `reason`, when given, replaces the generic fallback — used whenever
    // `total` is `None` BECAUSE of a run this fn's caller already excluded
    // (and already named in `withheld`), so the generic "not every run
    // recorded it" is never printed alongside a reason that contradicts it.
    let per_success = |total: Option<f64>, what: &str, reason: Option<&str>, withheld: &mut Vec<String>| -> Option<f64> {
        if passed == 0 {
            return None;
        }
        match total {
            Some(t) => Some(t / passed as f64),
            None => {
                withheld.push(format!("{what}: {}", reason.unwrap_or("not every run in the set recorded it")));
                None
            }
        }
    };
    // A figure built from `wall_ms`/`active_ms` for a STALE-METRICS run is
    // built on another run's clock; a figure built from host telemetry for
    // an OVERLAP run is the whole host's power, claimed twice. Neither can be
    // apportioned, so the run's contribution is excluded rather than summed
    // — which, like a run that never recorded the value, withholds the
    // WHOLE set's total instead of reading low.
    let sum_all_excluding = |excl: &std::collections::BTreeSet<&str>, f: fn(&RunStats) -> Option<f64>| -> Option<f64> {
        runs.iter()
            .map(|s| if excl.contains(s.run.as_str()) { None } else { f(s) })
            .try_fold(0.0, |acc, v| v.map(|v| acc + v))
    };
    if passed == 0 && n > 0 {
        withheld.push("no run in the set passed verify, so there is no success to cost".into());
    }
    // Both conditions can make a host-telemetry figure unreliable for the
    // same run; union them once rather than excluding twice.
    let energy_excl: std::collections::BTreeSet<&str> = stale.union(&overlap.iter().map(|s| s.as_str()).collect()).copied().collect();
    let stale_reason = (!stale.is_empty()).then(|| {
        format!("{} run(s) have a metrics.json that does not belong to them (STALE-METRICS)", stale.len())
    });
    let overlap_reason = (!overlap.is_empty()).then(|| {
        format!("{} run(s) have overlapping host-telemetry windows (OVERLAP) and cannot be apportioned", overlap.len())
    });
    // The energy reason is the CONCATENATION of whichever of the two
    // conditions applies — a run can be excluded from energy for both at
    // once, and each reason is independently true.
    let energy_reason: Option<String> = match (&stale_reason, &overlap_reason) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (Some(a), None) => Some(a.clone()),
        (None, Some(b)) => Some(b.clone()),
        (None, None) => None,
    };
    let cost_per_success = CostPerSuccess {
        active_ms: per_success(
            sum_all_excluding(&stale, |s| Some(s.active_ms as f64)),
            "active time",
            stale_reason.as_deref(),
            &mut withheld,
        ),
        gpu_busy_ms: per_success(
            sum_all_excluding(&energy_excl, |s| s.busy_ms.map(|v| v as f64)),
            "GPU busy time",
            energy_reason.as_deref(),
            &mut withheld,
        ),
        pkg_joules: per_success(
            sum_all_excluding(&energy_excl, |s| s.pkg_j_busy),
            "package energy",
            energy_reason.as_deref(),
            &mut withheld,
        ),
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
        active_ms: r_excl(&stale, |s| Some(s.active_ms as f64)),
        wall_ms: r_excl(&stale, |s| Some(s.wall_ms as f64)),
        rest_ms: r_excl(&stale, |s| Some(s.rest_ms as f64)),
        turns: r(|s| Some(s.turns as f64)),
        completion_tokens: r(|s| Some(s.completion_tokens as f64)),
        tok_per_s: r(|s| s.tok_per_s),
        billed_gen_fraction: r(|s| s.billed_gen_fraction),
        gpu_w_busy: r_excl(&energy_excl, |s| s.gpu_w_busy),
        pkg_w_busy: r_excl(&energy_excl, |s| s.pkg_w_busy),
        pkg_j_per_1k_tokens: r_excl(&energy_excl, |s| s.pkg_j_per_1k_tokens),
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
                let mut f = flags(s);
                // OVERLAP is a SET-level condition (needs another run's
                // window to exist at all), so it cannot live in `flags`,
                // which reads one run alone.
                if overlap.contains(&s.run) {
                    f.push("OVERLAP");
                }
                (!f.is_empty()).then(|| (s.run.clone(), f))
            })
            .collect(),
    }
}

/// A set figure for a terminal: `median (min–max)`, and `[n=k]` whenever it
/// rests on fewer runs than the set holds. A figure over one run must not
/// look like five identical runs, and a median over two must not sit beside
/// one over five as if they were the same claim (tok/s, for one, is absent
/// on a run with no billed stream).
pub fn fmt_range(r: Option<Range>, set_n: usize, f: &dyn Fn(f64) -> String) -> String {
    let Some(r) = r else { return "-".into() };
    let body = if r.min == r.max {
        f(r.median)
    } else {
        format!("{} ({}–{})", f(r.median), f(r.min), f(r.max))
    };
    if r.n < set_n { format!("{body} [n={}]", r.n) } else { body }
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
