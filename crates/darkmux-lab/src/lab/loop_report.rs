//! `darkmux lab loop` report aggregator (#986).
//!
//! The loop lab is a **loop-engineering bench**: run one dispatch against a
//! fixed model + fixture under a chosen *harness* configuration (turn/token
//! caps, compaction knobs), then classify what the loop actually did. Where
//! `lab run` answers "did the dispatch succeed?", the loop lab answers the
//! loop-engineering question: **how did the loop behave — did it push through
//! cleanly, struggle (and get caught by the detectors), or go inert and
//! falsely report success?**
//!
//! This module is the analysis half. It reads a completed run's artifacts
//! (its trajectory, through the one fold: detector firings, tool calls,
//! turns, compactions; `manifest.json` sandbox hashes) plus the dispatch +
//! verify outcome, and produces a single [`Verdict`] over a small, explicit
//! axis. The classification rule lives in [`classify`] — pure, so it is the
//! unit-tested heart of the bench.
//!
//! The four verdicts map to the failure modes the loop-engineering research
//! and darkmux's own dogfood surfaced:
//!   - `Productive` — engaged, achieved the task, no pathological loop signals.
//!   - `Struggled`  — achieved the task, but a loop detector fired (cycle /
//!     reasoning-loop / repeated-failure / per-turn-cap / intra-turn-stall).
//!     The harness caught + bounded the struggle; the *config* survived it.
//!   - `InertFalsePass` — the model made zero tool calls (did not engage) yet
//!     verify reports pass. The dangerous false positive: a baseline that
//!     passes regardless makes "did nothing" look like success. (The phi-4
//!     probe case, 2026-06-21.)
//!   - `Failed` — the dispatch errored, the model did nothing and could not be
//!     confirmed to have passed, or it engaged but verify contradicts it.

use crate::lab::dispatch_end::DispatchEnd;
use anyhow::Result;
use darkmux_trajectory::DetectorCounts;
use serde::Serialize;
use std::path::Path;

/// Per-run-overridable compaction knobs (the loop-variation axis the trait
/// threads into the provider). Each `Some` overrides the value
/// `CompactionDispatchArgs::from_profile` derived; each `None` leaves the
/// profile's value in place. Caps (max-turns / max-tokens / inactivity
/// timeout) are NOT here — they resolve through `config_access`'s live env
/// tier, which `darkmux lab loop` sets directly for the dispatch.
#[derive(Debug, Clone, Default)]
pub struct LoopCompactionOverride {
    pub threshold_tokens: Option<u32>,
    pub threshold_ratio: Option<f32>,
    pub context_window: Option<u32>,
    pub strategy: Option<darkmux_types::CompactionStrategy>,
    pub bail_after_compactions: Option<u32>,
}

impl LoopCompactionOverride {
    /// True when no field is set — lets the provider skip the overlay
    /// entirely (and report "profile defaults" in the loop-config summary).
    pub fn is_empty(&self) -> bool {
        self.threshold_tokens.is_none()
            && self.threshold_ratio.is_none()
            && self.context_window.is_none()
            && self.strategy.is_none()
            && self.bail_after_compactions.is_none()
    }

    /// Apply the set fields onto an already-`from_profile`-derived args
    /// struct. Only `Some` fields override; the rest pass through unchanged.
    pub fn apply(&self, args: &mut darkmux_crew::dispatch::CompactionDispatchArgs) {
        if let Some(v) = self.threshold_tokens {
            args.threshold_tokens = Some(v);
        }
        if let Some(v) = self.threshold_ratio {
            args.threshold_ratio = Some(v);
        }
        if let Some(v) = self.context_window {
            args.context_window = Some(v);
        }
        if let Some(v) = self.strategy {
            args.strategy = Some(v);
        }
        if let Some(v) = self.bail_after_compactions {
            args.bail_after_compactions = Some(v);
        }
    }
}

/// Counts of each loop-detector firing in a run's trajectory. All are
/// "the harness noticed the model struggling" signals; `feedback_injected`
/// is reported for context but is NOT counted toward the struggle signal
/// (feedback is injected on normal nudges too, not only on pathology).
/// The detector firings that signal a STRUGGLE: every loop detector,
/// never the feedback injection that merely delivered one.
pub fn struggle_signal(d: &DetectorCounts) -> u32 {
    d.cycle
        .saturating_add(d.reasoning_loop)
        .saturating_add(d.intra_turn_stall)
        .saturating_add(d.repeated_failure)
        .saturating_add(d.per_turn_cap)
}

/// The struggle detectors that fired, as `name×count`.
pub fn fired_summary(d: &DetectorCounts) -> Vec<String> {
    [
        ("cycle", d.cycle),
        ("reasoning-loop", d.reasoning_loop),
        ("intra-turn-stall", d.intra_turn_stall),
        ("repeated-failure", d.repeated_failure),
        ("per-turn-cap", d.per_turn_cap),
    ]
    .into_iter()
    .filter(|(_, n)| *n > 0)
    .map(|(name, n)| format!("{name}×{n}"))
    .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    Productive,
    Struggled,
    InertFalsePass,
    Failed,
    /// (F2) The dispatch stopped on purpose and handed the work to a higher
    /// tier: unfinished by design, so neither a failure nor an achievement.
    Escalated,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Productive => "productive",
            Verdict::Struggled => "struggled",
            Verdict::InertFalsePass => "inert-false-pass",
            Verdict::Failed => "failed",
            Verdict::Escalated => "escalated",
        }
    }

    /// A short glyph for the human table.
    pub fn glyph(&self) -> &'static str {
        match self {
            Verdict::Productive => "✅",
            Verdict::Struggled => "⚠",
            Verdict::InertFalsePass => "🫥",
            Verdict::Failed => "❌",
            Verdict::Escalated => "↑",
        }
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Pure verdict rule — the heart of the bench, factored out so the
/// classification logic is unit-tested independently of artifact parsing.
///
/// Inputs:
///   - `dispatch_ok`: did the runtime exit cleanly (0) with a reply payload?
///   - `verify`: the workload's verify outcome (`None` when the workload
///     defines no verify command).
///   - `tool_calls`: count of `tool.completed` events. `0` means the model
///     made no tool calls at all — and since every edit goes through a tool,
///     zero tool calls means the sandbox is necessarily unchanged. This is
///     the primary "did the model engage?" signal.
///   - `struggle`: [`struggle_signal`].
///
/// Order matters — earlier branches dominate:
///   1. `!dispatch_ok` → `Failed` (nothing else is trustworthy).
///   2. `tool_calls == 0` (inert): `verify == Some(true)` → `InertFalsePass`
///      (verify passed on a baseline that passes regardless — the dangerous
///      false positive); otherwise `Failed` (did nothing, unconfirmed).
///   3. `verify == Some(false)` → `Failed` (engaged, but verify contradicts).
///   4. `struggle > 0` → `Struggled` (achieved it, but a detector fired).
///   5. otherwise → `Productive`.
pub fn classify(
    dispatch_ok: bool,
    verify: Option<bool>,
    tool_calls: u32,
    struggle: u32,
) -> Verdict {
    if !dispatch_ok {
        return Verdict::Failed;
    }
    if tool_calls == 0 {
        return match verify {
            Some(true) => Verdict::InertFalsePass,
            _ => Verdict::Failed,
        };
    }
    if verify == Some(false) {
        return Verdict::Failed;
    }
    if struggle > 0 {
        return Verdict::Struggled;
    }
    Verdict::Productive
}

/// The full loop-lab report for a single run.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct LoopReport {
    pub run_id: String,
    pub verdict: Verdict,
    /// Did the runtime exit cleanly with a reply payload?
    pub dispatch_ok: bool,
    /// (F2) The runtime's `escalation_*` reason when the dispatch escalated;
    /// absent otherwise. `dispatch_ok` is false then, but it is not an error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub escalation: Option<String>,
    /// Verify outcome (`None` when the workload defines no verify command).
    pub verify_passed: Option<bool>,
    /// `final_hash != baseline_hash` — whether the agent left the sandbox in
    /// a different state. `None` when one of the hashes is unrecorded (e.g. a
    /// self-contained workload with no baseline).
    pub sandbox_changed: Option<bool>,
    pub tool_calls: u32,
    pub turns: u32,
    pub compactions: u32,
    pub detectors: DetectorCounts,
    pub duration_ms: u128,
    /// Self-describing tag of the harness config this run used, so a report
    /// is interpretable on its own (e.g. `["profile=loop-phi4",
    /// "max-turns=20", "compact-threshold-tokens=8000"]`).
    pub loop_config: Vec<String>,
    /// Operator-facing explanation of *why* this verdict — the signals that
    /// drove the classification.
    pub notes: Vec<String>,
}

/// Analyze a completed run directory into a [`LoopReport`].
///
/// `dispatch_ok`, `verify_passed`, and `duration_ms` come from the run's
/// [`crate::lab::run::RunOutcome`] (the caller already has them); the rest is
/// read from the run dir's artifacts. Missing/partial artifacts degrade
/// gracefully — a run that hard-errored before writing a trajectory still
/// classifies (as `Failed`, via `dispatch_ok`), it just reports zero
/// detector counts.
pub fn analyze_run(
    run_dir: &Path,
    run_id: &str,
    end: &DispatchEnd,
    verify_passed: Option<bool>,
    duration_ms: u128,
    loop_config: Vec<String>,
) -> Result<LoopReport> {
    let dispatch_ok = end.ok();
    let fold = crate::lab::inspect::checked_run_trajectory(run_dir)?;
    let (detectors, tool_calls) = (fold.detectors, fold.tool_calls());
    let (turns, compactions) = (fold.turns(), fold.compactions());
    let sandbox_changed = read_sandbox_changed(run_dir);

    // An escalation exits non-zero but is not a failure: it gets its own
    // verdict before the exit-code-shaped `classify` could call it `Failed`.
    let verdict = if end.escalation().is_some() {
        Verdict::Escalated
    } else {
        classify(dispatch_ok, verify_passed, tool_calls, struggle_signal(&detectors))
    };

    let notes = build_notes(
        verdict,
        dispatch_ok,
        verify_passed,
        sandbox_changed,
        tool_calls,
        &detectors,
    );

    Ok(LoopReport {
        run_id: run_id.to_string(),
        verdict,
        dispatch_ok,
        escalation: end.escalation().map(str::to_string),
        verify_passed,
        sandbox_changed,
        tool_calls,
        turns,
        compactions,
        detectors,
        duration_ms,
        loop_config,
        notes,
    })
}

/// Why a `Failed` verdict failed, most fundamental cause first.
fn failed_note(dispatch_ok: bool, verify: Option<bool>, tool_calls: u32) -> String {
    match (dispatch_ok, tool_calls, verify) {
        (false, _, _) => "dispatch did not exit cleanly — runtime error or non-zero exit".into(),
        (true, 0, None) => {
            "model made 0 tool calls (inert), and the workload declares no verify to confirm success".into()
        }
        (true, 0, Some(_)) => "model made 0 tool calls (inert) and verify did not confirm success".into(),
        (true, _, _) => "model engaged but verify contradicts the result (verify failed)".into(),
    }
}

/// Build the operator-facing "why this verdict" notes.
fn build_notes(
    verdict: Verdict,
    dispatch_ok: bool,
    verify: Option<bool>,
    sandbox_changed: Option<bool>,
    tool_calls: u32,
    detectors: &DetectorCounts,
) -> Vec<String> {
    let mut notes = Vec::new();
    match verdict {
        Verdict::Failed => notes.push(failed_note(dispatch_ok, verify, tool_calls)),
        Verdict::InertFalsePass => {
            notes.push(
                "FALSE PASS: model made 0 tool calls (changed nothing) yet verify reports pass — \
                 the baseline passes regardless, so 'did nothing' looks like success"
                    .into(),
            );
        }
        Verdict::Struggled => {
            notes.push(format!(
                "task achieved, but the harness caught pathological loop signals: {}",
                fired_summary(detectors).join(", ")
            ));
        }
        Verdict::Productive => {
            notes.push("engaged, achieved the task, no pathological loop signals".into());
        }
        Verdict::Escalated => {
            notes.push(
                "dispatch escalated: the runtime stopped on purpose and handed the work to a \
                 higher tier (not a runtime error); the task is unfinished"
                    .into(),
            );
        }
    }
    if detectors.feedback_injected > 0 {
        notes.push(format!(
            "feedback injected ×{} (harness nudges; not a struggle signal on its own)",
            detectors.feedback_injected
        ));
    }
    if let Some(false) = sandbox_changed {
        if tool_calls > 0 {
            notes.push(
                "note: tool calls were made but the sandbox hash is unchanged \
                 (read-only investigation, or edits that net to no change)"
                    .into(),
            );
        }
    }
    let _ = verify; // referenced only via the verdict branches above
    notes
}

/// Compare `final_hash` (top-level) against `fixture.baseline_hash` (added by
/// the lab's manifest enrichment) to decide whether the sandbox changed.
/// `None` when either hash is absent (e.g. a self-contained workload with no
/// baseline, or a run whose final hash failed to compute).
fn read_sandbox_changed(run_dir: &Path) -> Option<bool> {
    let manifest = crate::lab::manifest::RunManifest::read_lenient(run_dir)?;
    Some(manifest.final_hash_value()? != manifest.baseline_hash()?)
}

/// Render a human-readable report to stdout.
pub fn print_report(report: &LoopReport) {
    println!();
    println!(
        "{} loop verdict: {}",
        report.verdict.glyph(),
        report.verdict.as_str().to_uppercase()
    );
    println!("  run:          {}", report.run_id);
    if !report.loop_config.is_empty() {
        println!("  loop config:  {}", report.loop_config.join(", "));
    }
    println!(
        "  dispatch:     {}",
        match (&report.escalation, report.dispatch_ok) {
            (Some(reason), _) => format!("escalated ({reason})"),
            (None, true) => "ok".to_string(),
            (None, false) => "error".to_string(),
        }
    );
    println!(
        "  verify:       {}",
        match report.verify_passed {
            Some(true) => "pass",
            Some(false) => "fail",
            None => "(no verify defined)",
        }
    );
    println!(
        "  sandbox:      {}",
        match report.sandbox_changed {
            Some(true) => "changed",
            Some(false) => "unchanged",
            None => "(no baseline hash)",
        }
    );
    println!("  tool calls:   {}", report.tool_calls);
    println!("  turns:        {}", report.turns);
    println!("  compactions:  {}", report.compactions);
    let fired = fired_summary(&report.detectors);
    println!(
        "  detectors:    {}",
        if fired.is_empty() {
            "none fired".to_string()
        } else {
            fired.join(", ")
        }
    );
    println!("  wall:         {}s", report.duration_ms / 1000);
    println!("  why:");
    for n in &report.notes {
        println!("    - {n}");
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    /// (review of #2986) An inert run whose workload declares no verify is
    /// failed for being inert, and the note says no verify was declared,
    /// not that verify "did not confirm" something it never checked.
    #[test]
    fn an_inert_run_names_whether_verify_was_declared() {
        let d = DetectorCounts::default();
        let none = build_notes(Verdict::Failed, true, None, None, 0, &d);
        assert_eq!(none[0], "model made 0 tool calls (inert), and the workload declares no verify to confirm success");
        let fail = build_notes(Verdict::Failed, true, Some(false), None, 0, &d);
        assert_eq!(fail[0], "model made 0 tool calls (inert) and verify did not confirm success");
    }

    // ─── classify: the four verdicts ────────────────────────────────

    /// (F2) An escalated dispatch exits non-zero but is its own verdict: not
    /// `Failed`, not "runtime error" in the notes, and the reason is carried.
    #[test]
    fn an_escalated_dispatch_is_its_own_verdict_not_failed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let end = DispatchEnd::Escalated { reason: "escalation_compaction_reread_loop".into() };
        let r = analyze_run(tmp.path(), "esc-run", &end, Some(false), 1_000, vec![]).unwrap();
        assert_eq!(r.verdict, Verdict::Escalated);
        assert_eq!(r.escalation.as_deref(), Some("escalation_compaction_reread_loop"));
        assert!(!r.dispatch_ok);
        assert!(r.notes.iter().all(|n| !n.contains("did not exit cleanly")), "{:?}", r.notes);
        let failed = analyze_run(tmp.path(), "f-run", &DispatchEnd::Failed, None, 1_000, vec![]).unwrap();
        assert_eq!(failed.verdict, Verdict::Failed);
        assert_eq!(failed.escalation, None);
    }

    #[test]
    fn classify_productive_clean_run() {
        // Engaged (tool calls), verify pass, no struggle.
        assert_eq!(classify(true, Some(true), 12, 0), Verdict::Productive);
    }

    #[test]
    fn classify_productive_no_verify_defined() {
        // No verify command → None. Engaged, no struggle → still productive.
        assert_eq!(classify(true, None, 5, 0), Verdict::Productive);
    }

    #[test]
    fn classify_struggled_when_detector_fired_but_succeeded() {
        // Achieved the task, but a loop detector fired along the way.
        assert_eq!(classify(true, Some(true), 30, 2), Verdict::Struggled);
    }

    #[test]
    fn classify_inert_false_pass_is_the_phi4_case() {
        // 0 tool calls (model didn't engage) yet verify passes because the
        // baseline passes regardless. The dangerous false positive.
        assert_eq!(classify(true, Some(true), 0, 0), Verdict::InertFalsePass);
    }

    #[test]
    fn classify_inert_with_no_verify_is_failed_not_false_pass() {
        // Did nothing, and verify can't confirm success → honest failure,
        // NOT a false pass (nothing falsely claimed to pass).
        assert_eq!(classify(true, None, 0, 0), Verdict::Failed);
    }

    #[test]
    fn classify_inert_with_failing_verify_is_failed() {
        // Did nothing and verify fails → failed.
        assert_eq!(classify(true, Some(false), 0, 0), Verdict::Failed);
    }

    #[test]
    fn classify_failed_when_dispatch_errored() {
        // Dispatch error dominates everything — even if a stale verify or
        // tool count looked OK, the run isn't trustworthy.
        assert_eq!(classify(false, Some(true), 10, 0), Verdict::Failed);
    }

    #[test]
    fn classify_failed_when_engaged_but_verify_fails() {
        // Made edits, but verify contradicts the result.
        assert_eq!(classify(true, Some(false), 8, 0), Verdict::Failed);
    }

    #[test]
    fn classify_dispatch_error_beats_struggle() {
        // Detectors fired AND dispatch errored → Failed wins (the bottom
        // line is the run can't be trusted), struggle is reported separately.
        assert_eq!(classify(false, Some(false), 4, 3), Verdict::Failed);
    }

    // ─── DetectorCounts ─────────────────────────────────────────────

    #[test]
    fn struggle_signal_excludes_feedback_injection() {
        let d = DetectorCounts {
            cycle: 1,
            reasoning_loop: 0,
            intra_turn_stall: 0,
            repeated_failure: 2,
            per_turn_cap: 0,
            feedback_injected: 5, // routine nudges — must NOT count as struggle
            ..Default::default()
        };
        assert_eq!(struggle_signal(&d), 3);
    }

    #[test]
    fn fired_summary_lists_only_nonzero_detectors() {
        let d = DetectorCounts {
            cycle: 2,
            reasoning_loop: 0,
            intra_turn_stall: 1,
            repeated_failure: 0,
            per_turn_cap: 0,
            feedback_injected: 0,
            ..Default::default()
        };
        assert_eq!(fired_summary(&d), vec!["cycle×2", "intra-turn-stall×1"]);
    }

    // ─── LoopCompactionOverride ─────────────────────────────────────

    #[test]
    fn override_apply_only_touches_set_fields() {
        use darkmux_crew::dispatch::CompactionDispatchArgs;
        let mut args = CompactionDispatchArgs {
            threshold_tokens: Some(50_000),
            compactor_model: None,
            compactor_context_window: None,
            threshold_ratio: Some(0.7),
            context_window: Some(100_000),
            strategy: None,
            bail_after_compactions: Some(8),
            custom_instructions: None,
        };
        let ov = LoopCompactionOverride {
            threshold_tokens: Some(8_000),
            strategy: Some(darkmux_types::CompactionStrategy::StructuredSlot),
            ..Default::default()
        };
        assert!(!ov.is_empty());
        ov.apply(&mut args);
        // Overridden:
        assert_eq!(args.threshold_tokens, Some(8_000));
        assert_eq!(args.strategy, Some(darkmux_types::CompactionStrategy::StructuredSlot));
        // Left alone (override field was None):
        assert_eq!(args.threshold_ratio, Some(0.7));
        assert_eq!(args.context_window, Some(100_000));
        assert_eq!(args.bail_after_compactions, Some(8));
    }

    #[test]
    fn empty_override_is_empty() {
        assert!(LoopCompactionOverride::default().is_empty());
    }

    // ─── analyze_run over synthetic artifacts ───────────────────────

    fn write_trajectory(dir: &Path, lines: &[&str]) {
        let mut f = fs::File::create(dir.join("trajectory.jsonl")).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    #[test]
    fn analyze_run_inert_false_pass_end_to_end() {
        // The phi-4 case: a trajectory with NO tool.completed events, a
        // manifest whose final_hash == baseline_hash (unchanged), and a
        // passing verify → InertFalsePass.
        let tmp = TempDir::new().unwrap();
        let run = tmp.path();
        write_trajectory(
            run,
            &[
                r#"{"type":"dispatch.start"}"#,
                r#"{"type":"model.completed","turn":0}"#,
                r#"{"type":"dispatch.complete"}"#,
            ],
        );
        fs::write(
            run.join("manifest.json"),
            r#"{"final_hash":"blake3:same","fixture":{"baseline_hash":"blake3:same"}}"#,
        )
        .unwrap();
        let report =
            analyze_run(run, "phi4-run", &DispatchEnd::Completed, Some(true), 37_000, vec!["profile=loop-phi4".into()])
                .unwrap();
        assert_eq!(report.verdict, Verdict::InertFalsePass);
        assert_eq!(report.tool_calls, 0);
        assert_eq!(report.sandbox_changed, Some(false));
        assert_eq!(report.turns, 1);
        assert!(report.notes.iter().any(|n| n.contains("FALSE PASS")));
    }

    #[test]
    fn analyze_run_struggled_counts_detectors() {
        // Engaged (two tool.completed), a cycle + a reasoning-loop fired,
        // verify passes → Struggled, with detector counts surfaced.
        let tmp = TempDir::new().unwrap();
        let run = tmp.path();
        write_trajectory(
            run,
            &[
                r#"{"type":"model.completed","seq":1}"#,
                r#"{"type":"tool.completed","ok":true}"#,
                r#"{"type":"dispatch.cycle.suspected"}"#,
                r#"{"type":"dispatch.feedback.injected"}"#,
                r#"{"type":"compaction","generation":1}"#,
                r#"{"type":"model.completed","seq":2}"#,
                r#"{"type":"dispatch.reasoning_loop.suspected"}"#,
                r#"{"type":"tool.completed","ok":true}"#,
                r#"{"type":"compaction","generation":2}"#,
                r#"{"type":"model.completed","seq":3}"#,
            ],
        );
        fs::write(
            run.join("manifest.json"),
            r#"{"final_hash":"blake3:after","fixture":{"baseline_hash":"blake3:before"}}"#,
        )
        .unwrap();
        // A stale metrics.json with other numbers: never read.
        fs::write(run.join("metrics.json"), r#"{"turns":18,"compactions":9}"#).unwrap();

        let report = analyze_run(run, "struggle-run", &DispatchEnd::Completed, Some(true), 240_000, vec![]).unwrap();
        assert_eq!(report.verdict, Verdict::Struggled);
        assert_eq!(report.tool_calls, 2);
        assert_eq!(report.detectors.cycle, 1);
        assert_eq!(report.detectors.reasoning_loop, 1);
        assert_eq!(report.detectors.feedback_injected, 1);
        assert_eq!(struggle_signal(&report.detectors), 2);
        assert_eq!(report.sandbox_changed, Some(true));
        assert_eq!(report.turns, 3, "from the trajectory, not the stale metrics.json");
        assert_eq!(report.compactions, 2);
    }

    #[test]
    fn analyze_run_productive_clean() {
        let tmp = TempDir::new().unwrap();
        let run = tmp.path();
        write_trajectory(
            run,
            &[
                r#"{"type":"tool.completed","ok":true}"#,
                r#"{"type":"tool.completed","ok":true}"#,
                r#"{"type":"tool.completed","ok":true}"#,
            ],
        );
        fs::write(
            run.join("manifest.json"),
            r#"{"final_hash":"blake3:after","fixture":{"baseline_hash":"blake3:before"}}"#,
        )
        .unwrap();
        let report = analyze_run(run, "clean-run", &DispatchEnd::Completed, Some(true), 60_000, vec![]).unwrap();
        assert_eq!(report.verdict, Verdict::Productive);
        assert_eq!(report.tool_calls, 3);
        assert!(fired_summary(&report.detectors).is_empty());
    }

    #[test]
    fn analyze_run_missing_trajectory_degrades_to_failed() {
        // A dispatch that hard-errored before writing a trajectory: no
        // trajectory file, no metrics, dispatch_ok=false → Failed, with
        // zeroed counts rather than an error.
        let tmp = TempDir::new().unwrap();
        let run = tmp.path();
        let report = analyze_run(run, "dead-run", &DispatchEnd::Failed, None, 1_000, vec![]).unwrap();
        assert_eq!(report.verdict, Verdict::Failed);
        assert_eq!(report.tool_calls, 0);
        assert_eq!(report.turns, 0);
        assert_eq!(report.sandbox_changed, None);
    }

    #[test]
    fn analyze_run_tolerates_malformed_trajectory_line() {
        // A truncated tail line (partial write / kill mid-flush) must not
        // sink the whole report — it's skipped, the rest is counted.
        let tmp = TempDir::new().unwrap();
        let run = tmp.path();
        write_trajectory(
            run,
            &[
                r#"{"type":"tool.completed","ok":true}"#,
                r#"{"type":"dispatch.cycle.susp"#, // truncated — invalid JSON
            ],
        );
        let report = analyze_run(run, "partial-run", &DispatchEnd::Completed, None, 5_000, vec![]).unwrap();
        assert_eq!(report.tool_calls, 1);
        assert_eq!(report.detectors.cycle, 0); // truncated line not counted
    }
}
