//! (#2855) The text `darkmux lab run stats` prints, as pure functions.
//!
//! Rendering lives here, not in the CLI, so what an operator actually READS
//! is tested: CI's mutation job showed the whole renderer could be deleted
//! with every test still green, which left claims like "the caveats always
//! print beneath the figures" unchecked. The CLI prints what these return
//! and nothing else.

use serde_json::json;

/// Strip terminal control characters (`char::is_control`: C0 incl. ESC, and
/// C1) from a string before it is printed to a terminal. `model`, `result`,
/// `verify` and the checkpoint `policy` all ride the trajectory, and the
/// sandbox's `/darkmux-out` is model-writable — an untrusted string can
/// carry an escape sequence that repaints the terminal. JSON output needs no
/// such pass: `serde_json` already escapes control characters on write.
fn sanitize_for_terminal(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// `writeln!` into the output buffer. Writing to a `String` cannot fail.
macro_rules! p {
    ($o:expr) => {
        $o.push('\n')
    };
    ($o:expr, $($t:tt)*) => {{
        use std::fmt::Write as _;
        let _ = writeln!($o, $($t)*);
    }};
}

/// (#2855) The human read of a run's derived metrics.
///
/// Two rules the layout exists to enforce. **Rest is printed beside wall and
/// active, never alone**, so a rested run's wall clock cannot be misread as a
/// slow model. And **the caveats print last and unconditionally** — a figure
/// whose reconciliation check failed is still shown, because hiding it would
/// lose the evidence, but no run's numbers can be copied out of here without
/// the reasons they may not be quoted appearing in the same block.
pub fn run_text(s: &crate::lab::stats::RunStats) -> String {
    let mut out = String::new();
    let secs = |ms: u64| ms as f64 / 1000.0;
    p!(out, "run:         {}", s.run);
    if let Some(m) = &s.model {
        p!(out, "model:       {}", sanitize_for_terminal(m));
    }
    p!(out,
        "result:      {}{}",
        s.result.as_deref().map(sanitize_for_terminal).unwrap_or_else(|| "?".into()),
        s.verify.as_deref().map(|v| format!("   verify: {}", sanitize_for_terminal(v))).unwrap_or_default()
    );
    p!(out);

    p!(out, 
        "time         wall {:.0}s   rest {:.0}s   active {:.0}s",
        secs(s.wall_ms),
        secs(s.rest_ms),
        secs(s.active_ms)
    );
    if s.rest_events > 0 {
        // The distinct delays, not a mean: more than one value means the
        // thermal ratchet doubled the delay mid-run.
        p!(out, 
            "             {} rests, delays {:?}ms{}",
            s.rest_events,
            s.rest_delays_ms,
            if s.thermal_ratchet_fired { "  (ratchet fired)" } else { "" }
        );
    }
    p!(out, 
        "work         {} turns   {} compactions   {} tool calls ({} failed)",
        s.turns, s.compactions, s.tool_calls_total, s.tool_calls_failed
    );
    p!(out, 
        "output       {} completion tokens   {} reasoning chars   {} content chars",
        s.completion_tokens, s.reasoning_chars, s.content_chars
    );
    match (s.tok_per_s, s.billed_gen_fraction) {
        (Some(t), Some(f)) => p!(out, 
            "throughput   {t} tok/s over {:.0}% of generation ({:.0}s of {:.0}s)",
            f * 100.0,
            secs(s.gen_ms_billed),
            secs(s.gen_ms_all)
        ),
        _ => p!(out, "throughput   (no billed generation recorded)"),
    }

    // Both gates, always both, on their own lines. One line for "detection"
    // is what let a reader take the checkpoint gate's silence for the whole
    // answer.
    let g = &s.gates;
    let dp = crate::lab::stats::TAIL_RATIO_DISPLAY_DP;
    let ratio = |r: Option<f64>| {
        r.map(|r| format!("   min ratio {:.*}", dp, r)).unwrap_or_default()
    };
    p!(out, 
        "stream gate  {} observations   {} degenerate   {} aborts{}",
        g.stream.observations,
        g.stream.degenerate_turns.len(),
        g.stream.aborts,
        ratio(g.stream.min_tail_ratio)
    );
    p!(out, 
        "checkpoint   {} observations   {} degenerate   {} cut{}{}",
        g.checkpoint.observations,
        g.checkpoint.degenerate_turns.len(),
        g.checkpoint.concluded_turns.len(),
        ratio(g.checkpoint.min_tail_ratio),
        g.checkpoint.policy.as_deref().map(|p| format!("   policy={}", sanitize_for_terminal(p))).unwrap_or_default()
    );

    if let (Some(gpu), Some(cpu), Some(pkg)) = (s.gpu_w_busy, s.cpu_w_busy, s.pkg_w_busy) {
        p!(out, 
            "power        gpu {gpu} W   cpu {cpu} W   package {pkg} W   busy {}% of the run ({} samples)",
            s.gpu_duty_pct.unwrap_or(0.0),
            s.samples_busy + s.samples_idle
        );
        if let Some(j) = s.pkg_j_per_1k_tokens {
            p!(out, 
                "energy       {j} J per 1k tokens{}",
                s.pkg_j_busy.map(|t| format!("   {:.1} kJ while busy", t / 1000.0)).unwrap_or_default()
            );
        }
        if !s.thermal_states_busy.is_empty() {
            let states: Vec<String> =
                s.thermal_states_busy.iter().map(|(k, v)| format!("{k} {v}")).collect();
            p!(out, 
                "thermal      {}   cpu speed limit min {}%",
                states.join(", "),
                s.cpu_speed_limit_min.unwrap_or(100)
            );
        }
    }

    let caveats = s.unreconciled();
    if !caveats.is_empty() {
        p!(out);
        p!(out, "not reconciled, so do not quote these figures without saying so:");
        for c in &caveats {
            p!(out, "  - {c}");
        }
    }
    out
}

/// Runs loaded for a set view, and the ones that could not be.
pub struct StatsSet {
    pub runs: Vec<crate::lab::stats::RunStats>,
    /// `(run, error)`. Never silently dropped: a set missing a run it was
    /// asked for reads as a different arm.
    pub errors: Vec<(String, String)>,
    /// Runs named more than once (by id or by path): counted once. A notice,
    /// not an error, so it does not change the exit code.
    pub duplicates: Vec<String>,
}

pub fn load_set(ids: &[String]) -> StatsSet {
    let mut set = StatsSet { runs: Vec::new(), errors: Vec::new(), duplicates: Vec::new() };
    let mut seen = std::collections::BTreeSet::new();
    for id in ids {
        match crate::lab::stats::run_stats(id) {
            // Deduplicated on the run it RESOLVED to, not the argument text:
            // `run-a` and `~/.darkmux/runs/run-a` are the same run, and a run
            // counted twice skews every range and total in the set.
            Ok(s) => {
                if seen.insert(s.run.clone()) {
                    set.runs.push(s);
                } else {
                    set.duplicates.push(s.run);
                }
            }
            Err(e) => set.errors.push((id.clone(), format!("{e:#}"))),
        }
    }
    set
}

fn fmt_secs(ms: f64) -> String {
    format!("{:.0}s", ms / 1000.0)
}

fn fmt_opt(v: Option<f64>, dp: usize) -> String {
    v.map(|v| format!("{v:.dp$}")).unwrap_or_else(|| "-".into())
}

/// (#2855) One row per run. Every row prints whatever its checks say, with
/// the failed ones as flags on the same line: dropping a row that did not
/// reconcile would be choosing the answer.
fn table_text(out: &mut String, set: &StatsSet) {
    use crate::lab::stats_set::{flags, overlapping};
    let overlap = overlapping(&set.runs);
    let w = set.runs.iter().map(|s| s.run.len()).chain(set.errors.iter().map(|(r, _)| r.len())).max().unwrap_or(3).max(3);
    p!(out,
        "{:<w$}  {:>6} {:>7} {:>6} {:>5} {:>7} {:>6} {:>7} {:>4} {:>6} {:>5} {:>7}  flags",
        "run", "verify", "active", "rest", "turns", "tok/s", "billed", "tokens", "cuts", "pkgW", "duty", "J/1ktok"
    );
    for s in &set.runs {
        let mut f = flags(s);
        if overlap.contains(&s.run) {
            f.push("OVERLAP");
        }
        p!(out, 
            "{:<w$}  {:>6} {:>7} {:>6} {:>5} {:>7} {:>6} {:>7} {:>4} {:>6} {:>5} {:>7}  {}",
            s.run,
            s.verify.as_deref().unwrap_or("-"),
            fmt_secs(s.active_ms as f64),
            fmt_secs(s.rest_ms as f64),
            s.turns,
            fmt_opt(s.tok_per_s, 1),
            s.billed_gen_fraction.map(|f| format!("{:.0}%", f * 100.0)).unwrap_or_else(|| "-".into()),
            s.completion_tokens,
            s.gates.stream.aborts + s.gates.checkpoint.concluded_turns.len(),
            fmt_opt(s.pkg_w_busy, 1),
            s.gpu_duty_pct.map(|d| format!("{d:.0}%")).unwrap_or_else(|| "-".into()),
            fmt_opt(s.pkg_j_per_1k_tokens, 0),
            if f.is_empty() { "ok".to_string() } else { f.join(",") },
        );
    }
    for (r, e) in &set.errors {
        p!(out, "{r:<w$}  not counted: {e}");
    }
}

/// Run ids present in both arms of a comparison — `stats X --baseline X`
/// being the degenerate case, but any run named on both sides has the same
/// shape: every ratio touching it reads 1.00x because the two arms are
/// literally the same data, not because the change had no effect.
fn cross_arm_overlap(cand: &StatsSet, base: &StatsSet) -> Vec<String> {
    let base_ids: std::collections::BTreeSet<&str> = base.runs.iter().map(|s| s.run.as_str()).collect();
    cand.runs.iter().map(|s| s.run.as_str()).filter(|r| base_ids.contains(r)).map(str::to_string).collect()
}

/// (#2855) The set view, and with a baseline, the comparison.
pub fn sets_text(cand: &StatsSet, base: Option<&StatsSet>) -> String {
    let mut out = String::new();
    use crate::lab::stats_set::{fmt_range, ratio, summarize, SetSummary};
    let c = summarize(&cand.runs);
    let b = base.map(|b| summarize(&b.runs));

    if let Some(bs) = base {
        p!(out, "baseline");
        table_text(&mut out, bs);
        p!(out);
        p!(out, "candidate");
    }
    table_text(&mut out, cand);
    p!(out);

    let secs = |v: f64| fmt_secs(v);
    let one = |v: f64| format!("{v:.1}");
    let pct = |v: f64| format!("{:.0}%", v * 100.0);
    let int = |v: f64| format!("{v:.0}");
    type Row<'a> = (&'a str, fn(&SetSummary) -> Option<crate::lab::stats_set::Range>, &'a dyn Fn(f64) -> String);
    let rows: [Row; 9] = [
        ("active", |s| s.active_ms, &secs),
        ("rest", |s| s.rest_ms, &secs),
        ("turns", |s| s.turns, &int),
        ("tokens", |s| s.completion_tokens, &int),
        ("tok/s", |s| s.tok_per_s, &one),
        ("billed share", |s| s.billed_gen_fraction, &pct),
        ("gpu W busy", |s| s.gpu_w_busy, &one),
        ("package W busy", |s| s.pkg_w_busy, &one),
        ("J per 1k tokens", |s| s.pkg_j_per_1k_tokens, &int),
    ];
    let outcome = |s: &SetSummary| {
        format!("{} of {} passed", s.passed, s.n)
            + &if s.unverified > 0 { format!(", {} unverified", s.unverified) } else { String::new() }
    };
    type CostRow<'a> = (&'a str, fn(&SetSummary) -> Option<f64>, &'a dyn Fn(f64) -> String);
    let cost: [CostRow; 3] = [
        ("active", |s| s.cost_per_success.active_ms, &secs),
        ("GPU busy", |s| s.cost_per_success.gpu_busy_ms, &secs),
        ("energy", |s| s.cost_per_success.pkg_joules.map(|j| j / 1000.0), &|v| format!("{v:.1} kJ")),
    ];

    match &b {
        None => {
            p!(out, "set          {}   models: {}", outcome(&c), c.models.join(", "));
            for (name, get, f) in &rows {
                p!(out, "  {name:<16} {}", fmt_range(get(&c), c.n, f));
            }
            p!(out, 
                "  {:<16} {} runs with degeneracy, {} turns cut",
                "detection", c.runs_with_degeneracy, c.turns_cut
            );
            p!(out, "cost per successful run");
            for (name, get, f) in &cost {
                p!(out, "  {name:<16} {}", get(&c).map(f).unwrap_or_else(|| "-".into()));
            }
        }
        Some(b) => {
            let col = 26.max(b.models.join(", ").len() + 2);
            p!(out, "{:<18} {:<col$} {:<col$} moved", "", "baseline", "candidate");
            p!(out, "{:<18} {:<col$} {:<col$}", "outcome", outcome(b), outcome(&c));
            p!(out, "{:<18} {:<col$} {:<col$}", "models", b.models.join(", "), c.models.join(", "));
            for (name, get, f) in &rows {
                let moved = ratio(get(&c).map(|r| r.median), get(b).map(|r| r.median))
                    .map(|x| format!("{x:.2}x"))
                    .unwrap_or_default();
                p!(out, 
                    "{name:<18} {:<col$} {:<col$} {moved}",
                    fmt_range(get(b), b.n, f),
                    fmt_range(get(&c), c.n, f)
                );
            }
            p!(out, 
                "{:<18} {:<col$} {:<col$}",
                "degeneracy",
                format!("{} runs, {} cuts", b.runs_with_degeneracy, b.turns_cut),
                format!("{} runs, {} cuts", c.runs_with_degeneracy, c.turns_cut)
            );
            p!(out, "cost per successful run (every run's cost, divided by the runs that passed)");
            for (name, get, f) in &cost {
                let show = |s: &SetSummary| get(s).map(f).unwrap_or_else(|| "-".into());
                let moved = ratio(get(&c), get(b)).map(|x| format!("{x:.2}x")).unwrap_or_default();
                p!(out, "  {name:<16} {:<col$} {:<col$} {moved}", show(b), show(&c));
            }
        }
    }

    // Caveats last and unconditionally, as in the single-run view.
    let mut notes: Vec<String> = Vec::new();
    for (label, s) in b.iter().map(|s| ("baseline", s)).chain(std::iter::once(("candidate", &c))) {
        let label = if b.is_some() { format!("{label}: ") } else { String::new() };
        if s.passed_with_runtime_error > 0 {
            notes.push(format!(
                "{label}{} of the {} passes came from runs whose runtime result was `error`; \
                 if this fixture's verify is green on an untouched tree, those passes do not \
                 show the task was done, and cost per success is understated",
                s.passed_with_runtime_error, s.passed
            ));
        }
        if s.models.len() > 1 {
            notes.push(format!("{label}the set mixes {} models", s.models.len()));
        }
        for w in &s.cost_per_success.withheld {
            notes.push(format!("{label}cost per success withheld: {w}"));
        }
        for (run, f) in &s.flagged {
            notes.push(format!("{label}{run}: {}", f.join(", ")));
        }
    }
    for d in cand.duplicates.iter().chain(base.iter().flat_map(|b| b.duplicates.iter())) {
        notes.push(format!("{d} was listed more than once and is counted once"));
    }
    if let Some(b) = base {
        for run in cross_arm_overlap(cand, b) {
            notes.push(format!(
                "{run} is in both the candidate and the baseline; any comparison touching it is 1.00x by construction"
            ));
        }
    }
    let errors = cand.errors.len() + base.map_or(0, |b| b.errors.len());
    if errors > 0 {
        notes.push(format!("{errors} listed run(s) are not in the figures above; see their rows"));
    }
    if !notes.is_empty() {
        p!(out);
        p!(out, "read before quoting:");
        for n in &notes {
            p!(out, "  - {n}");
        }
        p!(out, "  (flag meanings: `darkmux lab run stats <run>` explains a single run's failed checks)");
    }
    out
}

/// The JSON `--json` prints for a set view: each set's runs, its summary, and
/// what could not be counted.
pub fn sets_json(cand: &StatsSet, base: Option<&StatsSet>) -> serde_json::Value {
    let one = |set: &StatsSet| {
        json!({
            "runs": set.runs,
            "summary": crate::lab::stats_set::summarize(&set.runs),
            "errors": set.errors,
            "duplicates": set.duplicates,
        })
    };
    let mut out = one(cand);
    if let Some(b) = base {
        out["baseline"] = one(b);
        out["cross_arm_overlap"] = json!(cross_arm_overlap(cand, b));
    }
    out
}

/// A run that could not be read makes the exit code non-zero, so a script
/// cannot mistake a partial set for a whole one. A run listed twice is a
/// notice, not a failure.
pub fn exit_code(cand: &StatsSet, base: Option<&StatsSet>) -> i32 {
    let errored = !cand.errors.is_empty() || base.is_some_and(|b| !b.errors.is_empty());
    if errored { 1 } else { 0 }
}

#[cfg(test)]
#[path = "stats_render_tests.rs"]
mod tests;
