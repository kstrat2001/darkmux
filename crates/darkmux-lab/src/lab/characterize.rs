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

use crate::lab::run::{lab_run, RunOpts, RunOutcome};
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
        // characterize() is an internal utility operation — uses default
        // runtime (internal, post-Phase-D).
        // characterize() is not a loop-variation surface (#986).
        loop_override: None,
        // characterize() never injects engagement-context (#1004).
        inject_context: None,
    };
    let outcomes = lab_run(run_opts)?;
    Ok(CharacterizeReport {
        workload: opts.workload.clone(),
        outcomes,
    })
}

pub fn print_report(r: &CharacterizeReport) {
    print!("{}", render_report(r));
}

/// The text `lab characterize` prints.
pub(crate) fn render_report(r: &CharacterizeReport) -> String {
    let mut out = String::new();
    p!(out, "darkmux characterize — workload `{}`", r.workload);
    p!(out);
    for o in &r.outcomes {
        let status = if o.ok { "✓" } else { "✗" };
        p!(out, "  {} {} — {}", status, o.run_id, format_seconds(o.duration_ms));
        for note in &o.notes {
            p!(out, "      {note}");
        }
    }
    p!(out);
    if let Some(v) = verdict(&r.outcomes) {
        p!(out, "verdict: {v}");
    }
    p!(out);
    p!(out, "Next steps:");
    p!(out, "  • `darkmux run inspect <run-id>` for the per-run breakdown");
    if r.outcomes.len() == 1 {
        p!(
            out,
            "  • Re-run for distribution: `darkmux lab run {} --repeat 5` then \
             `darkmux run compare <a> <b>` for variance",
            r.workload
        );
    }
    out
}

/// The one-line verdict: a failed or errored dispatch dominates a failed
/// verify, which dominates the wall-clock read of the runs that completed.
/// `None` when there were no runs.
fn verdict(outcomes: &[RunOutcome]) -> Option<String> {
    if outcomes.is_empty() {
        return None;
    }
    if outcomes.iter().any(|o| !o.ok) {
        return Some(
            "at least one dispatch failed — inspect `darkmux run inspect <run-id>` \
             and check `darkmux doctor` for setup problems"
                .to_string(),
        );
    }
    let slowest = outcomes.iter().map(|o| o.duration_ms / 1000).max().unwrap_or(0);
    let timing = classify_wall_clock(slowest);
    Some(if outcomes.iter().any(RunOutcome::verify_failed) {
        format!(
            "dispatch succeeded ({timing}) BUT the workload's verify check failed — \
             the model didn't produce the expected reply. This is normal for non-deterministic \
             single-turn smoke prompts; re-run a few times for distribution. For tighter \
             contracts, replace the keyword check with a coding-task workload (npm test etc.)"
        )
    } else {
        timing.to_string()
    })
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

    fn outcome(id: &str, ok: bool, verify_passed: Option<bool>, secs: u128) -> RunOutcome {
        RunOutcome {
            run_id: id.into(),
            run_dir: std::path::PathBuf::new(),
            ok,
            verify_passed,
            duration_ms: secs * 1000,
            notes: vec!["provider=stub".into()],
            provider_error: None,
        }
    }

    fn report(outcomes: Vec<RunOutcome>) -> CharacterizeReport {
        CharacterizeReport { workload: "w".into(), outcomes }
    }

    /// The whole report for one passing run, byte for byte.
    #[test]
    fn a_passing_run_renders_the_full_report() {
        let text = render_report(&report(vec![outcome("r1", true, None, 8)]));
        assert_eq!(
            text,
            "darkmux characterize — workload `w`\n\n  ✓ r1 — 8s\n      provider=stub\n\n\
             verdict: fast — single-turn dispatch in expected range for any modern Apple Silicon\n\n\
             Next steps:\n  • `darkmux run inspect <run-id>` for the per-run breakdown\n\
             \x20 • Re-run for distribution: `darkmux lab run w --repeat 5` then \
             `darkmux run compare <a> <b>` for variance\n"
        );
    }

    /// A failed dispatch dominates a failed verify; a failed verify
    /// dominates the timing read; a verify nothing declared is not a failure.
    #[test]
    fn the_verdict_names_the_worst_outcome() {
        let v = |o: Vec<RunOutcome>| verdict(&o).unwrap();
        assert!(v(vec![outcome("a", false, Some(false), 1)]).starts_with("at least one dispatch failed"));
        let verify = v(vec![outcome("a", true, Some(true), 1), outcome("b", true, Some(false), 40)]);
        assert!(verify.starts_with("dispatch succeeded (slow — "), "{verify}");
        assert!(verify.contains("BUT the workload's verify check failed"), "{verify}");
        assert!(v(vec![outcome("a", true, None, 20)]).starts_with("ok — "));
        assert_eq!(verdict(&[]), None);
    }

    /// Several runs: a failed dispatch is marked, and there is no re-run
    /// hint and no verdict line when there are no runs.
    #[test]
    fn the_report_marks_failures_and_drops_the_rerun_hint_for_several_runs() {
        let text = render_report(&report(vec![outcome("a", false, None, 1), outcome("b", true, None, 1)]));
        assert!(text.contains("  ✗ a — 1s\n") && text.contains("  ✓ b — 1s\n"), "{text}");
        assert!(!text.contains("Re-run for distribution"), "{text}");
        let empty = render_report(&report(vec![]));
        assert!(!empty.contains("verdict:"), "{empty}");
    }

    /// `characterize` runs the workload once, quietly, through `lab_run`.
    #[test]
    #[serial_test::serial]
    fn characterize_runs_the_workload_once() {
        use crate::lab::run::run_tests::{script, Lab, Script};
        let lab = Lab::scripted(&["wchar"]);
        script(Script { ok: true, verify: Some(false), ..Default::default() });
        let r = characterize(&CharacterizeOpts {
            workload: "wchar".into(),
            profile: None,
            config: Some(lab.profiles.clone()),
        })
        .unwrap();
        assert_eq!(r.workload, "wchar");
        assert_eq!(r.outcomes.len(), 1);
        assert_eq!(crate::lab::run::exit_code(&r.outcomes), 1, "a failed verify fails characterize");
    }

    /// (#2986) A run whose provider errored is marked and names its error,
    /// and the verdict is a failed dispatch.
    #[test]
    fn an_errored_run_is_marked_with_its_error() {
        let mut errored = outcome("r2", false, None, 0);
        errored.provider_error = Some("boom".into());
        errored.notes = vec!["provider=stub".into(), "error: boom".into()];
        let text = render_report(&report(vec![errored]));
        assert!(text.contains("  ✗ r2 — 0s\n      provider=stub\n      error: boom\n"), "{text}");
        assert!(text.contains("verdict: at least one dispatch failed"), "{text}");
    }

    #[test]
    fn classify_wall_clock_buckets() {
        assert!(classify_wall_clock(5).starts_with("fast"));
        assert!(classify_wall_clock(20).starts_with("ok"));
        assert!(classify_wall_clock(60).starts_with("slow"));
        assert!(classify_wall_clock(500).starts_with("very slow"));
    }
}
