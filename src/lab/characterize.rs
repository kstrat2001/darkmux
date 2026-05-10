//! `darkmux lab characterize` — opinionated single-command "QA my Mac" entry.
//!
//! Wraps `lab_run` against the `quick-q` smoke workload (or whatever the
//! user names) on the active profile, then formats a single-screen verdict:
//! wall clock, verify status, classification, suggested next step.
//!
//! This is the MVP shape — the v1 "characterize" command will run a small
//! battery of workloads (smoke + bounded + open-ended) and produce a
//! distribution. For now this is a thin polish layer over a single dispatch
//! so a fresh user can answer *"does my Mac handle this?"* with one command.

use crate::lab::run::{RunOpts, RunOutcome, lab_run};
use anyhow::Result;

#[derive(Debug, Clone)]
pub struct CharacterizeOpts {
    pub workload: String,
    pub profile: Option<String>,
    pub config: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CharacterizeReport {
    pub workload: String,
    pub outcomes: Vec<RunOutcome>,
}

pub fn characterize(opts: &CharacterizeOpts) -> Result<CharacterizeReport> {
    let run_opts = RunOpts {
        workload_id: opts.workload.clone(),
        profile_name: opts.profile.clone(),
        runs: 1,
        config_path: opts.config.clone(),
        quiet: true,
        instrument: false,
    };
    let outcomes = lab_run(run_opts)?;
    Ok(CharacterizeReport {
        workload: opts.workload.clone(),
        outcomes,
    })
}

pub fn print_report(r: &CharacterizeReport) {
    println!("darkmux characterize — workload `{}`", r.workload);
    println!();
    for o in &r.outcomes {
        let status = if o.ok { "✓" } else { "✗" };
        println!("  {} {} — {}", status, o.run_id, format_seconds(o.duration_ms));
        for note in &o.notes {
            println!("      {note}");
        }
    }
    println!();
    let wall_secs: Vec<u128> = r.outcomes.iter().map(|o| o.duration_ms / 1000).collect();
    let any_dispatch_failed = r.outcomes.iter().any(|o| !o.ok);
    let any_verify_failed = r
        .outcomes
        .iter()
        .any(|o| matches!(o.verify_passed, Some(false)));

    if any_dispatch_failed {
        println!(
            "verdict: at least one dispatch failed — inspect `darkmux lab inspect <run-id>` \
             and check `darkmux doctor` for setup problems"
        );
    } else if any_verify_failed {
        let max = wall_secs.iter().max().copied().unwrap_or(0);
        let timing = classify_wall_clock(max);
        println!(
            "verdict: dispatch succeeded ({timing}) BUT the workload's verify check failed — \
             the model didn't produce the expected reply. This is normal for non-deterministic \
             single-turn smoke prompts; re-run a few times for distribution. For tighter \
             contracts, replace the keyword check with a coding-task workload (npm test etc.)"
        );
    } else if let Some(&max) = wall_secs.iter().max() {
        let label = classify_wall_clock(max);
        println!("verdict: {label}");
    }
    println!();
    println!("Next steps:");
    println!("  • `darkmux lab inspect <run-id>` for the per-run breakdown");
    if r.outcomes.len() == 1 {
        println!(
            "  • Re-run for distribution: `darkmux lab run {} --runs 5` then \
             `darkmux lab compare <a> <b>` for variance",
            r.workload
        );
    }
}

fn format_seconds(duration_ms: u128) -> String {
    let secs = duration_ms / 1000;
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}s", secs)
    }
}

/// Rough first-pass classifier for a single quick-q-shaped dispatch on
/// Apple Silicon. Bigger (multi-workload) characterization will replace
/// this with comparison to shipped baselines.
fn classify_wall_clock(secs: u128) -> &'static str {
    match secs {
        0..=10 => "fast — single-turn dispatch in expected range for any modern Apple Silicon",
        11..=30 => "ok — slightly slower than expected; check `darkmux doctor` if this is a fast machine",
        31..=120 => "slow — model may be loading from cold, or context is high. Re-run for warm-cache time",
        _ => "very slow — likely a setup issue (model not loaded, swap pressure, or wrong profile). Run `darkmux doctor`",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_seconds_under_a_minute() {
        assert_eq!(format_seconds(8_000), "8s");
        assert_eq!(format_seconds(0), "0s");
        assert_eq!(format_seconds(59_999), "59s");
    }

    #[test]
    fn format_seconds_minutes() {
        assert_eq!(format_seconds(60_000), "1m 0s");
        assert_eq!(format_seconds(125_000), "2m 5s");
        assert_eq!(format_seconds(3_600_000), "60m 0s");
    }

    #[test]
    fn classify_wall_clock_buckets() {
        assert!(classify_wall_clock(5).starts_with("fast"));
        assert!(classify_wall_clock(20).starts_with("ok"));
        assert!(classify_wall_clock(60).starts_with("slow"));
        assert!(classify_wall_clock(500).starts_with("very slow"));
    }
}
