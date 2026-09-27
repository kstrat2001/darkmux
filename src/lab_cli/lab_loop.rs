//! `darkmux lab loop` (#986): the single-run loop-engineering bench. Runs ONE
//! dispatch under the chosen harness config, then classifies how the loop
//! behaved via `lab::loop_report`. With `--ab` (#1004) it runs the workload
//! twice, without and with the engagement context, and reports the shift.
//!
//! Two loop-variation axes. [`LabLoopArgs::flags`] maps every flag to its
//! report label and every cap to its env var; [`LabLoopArgs::compaction_overlay`]
//! maps the compaction flags to their typed overlay.
//!   - **Caps** (`--max-turns` / `--max-tokens` / `--timeout`) resolve through
//!     `config_access`'s live env tier, so the documented env override is set
//!     for this dispatch. This process is single-shot (it exits right after),
//!     so a process-wide `set_var` is the simplest honest mechanism: it is
//!     the same tier an operator would `export` for one run.
//!   - **Compaction** (`--compact-*` / `--bail-after-compactions` /
//!     `--context-window`) overlays onto the profile-derived
//!     `CompactionDispatchArgs` via the provider (`loop_override`).

use anyhow::Result;
use darkmux_lab::lab::loop_report::{self, LoopCompactionOverride, LoopReport, Verdict};

/// Flattened args for `darkmux lab loop`, one field per CLI flag.
pub(super) struct LabLoopArgs {
    pub(super) workload: String,
    pub(super) profile: Option<String>,
    pub(super) profiles: Option<String>,
    pub(super) max_turns: Option<u32>,
    pub(super) max_tokens: Option<u32>,
    pub(super) timeout: Option<u64>,
    pub(super) compact_threshold_tokens: Option<u32>,
    pub(super) compact_threshold_ratio: Option<f32>,
    pub(super) compact_strategy: Option<String>,
    pub(super) bail_after_compactions: Option<u32>,
    pub(super) context_window: Option<u32>,
    pub(super) ab: bool,
    pub(super) inject_from_mission: Option<String>,
    pub(super) json: bool,
}

/// One harness flag as the report names it: the `loop_config` label, the
/// value as given (`None` = flag not given), the unit the label appends, and,
/// for a cap, the env var the runtime reads the bare value from.
struct LoopFlag {
    label: &'static str,
    value: Option<String>,
    unit: &'static str,
    cap_env: Option<&'static str>,
}

/// Everything one `lab loop` invocation needs besides the workload: the
/// compaction overlay, the self-describing config summary, and the env caps.
struct LoopPlan {
    overlay: LoopCompactionOverride,
    loop_config: Vec<String>,
    caps: Vec<(&'static str, String)>,
}

impl LabLoopArgs {
    /// Every profile and harness flag, in the order the report lists them.
    /// The single place a flag is mapped to its label and, for a cap, to its
    /// env var; the typed compaction values go through
    /// [`Self::compaction_overlay`].
    fn flags(&self) -> [LoopFlag; 10] {
        fn flag(label: &'static str, value: Option<String>) -> LoopFlag {
            LoopFlag { label, value, unit: "", cap_env: None }
        }
        fn cap(label: &'static str, value: Option<String>, unit: &'static str, env: &'static str) -> LoopFlag {
            LoopFlag { label, value, unit, cap_env: Some(env) }
        }
        fn s<T: ToString>(v: Option<T>) -> Option<String> {
            v.map(|v| v.to_string())
        }
        [
            flag("profile", self.profile.clone()),
            flag("profiles-file", self.profiles.clone()),
            cap("max-turns", s(self.max_turns), "", "DARKMUX_RUNTIME_MAX_TURNS"),
            cap("max-tokens", s(self.max_tokens), "", "DARKMUX_RUNTIME_MAX_TOKENS"),
            cap("timeout", s(self.timeout), "s", "DARKMUX_INACTIVITY_TIMEOUT_SECONDS"),
            flag("compact-threshold-tokens", s(self.compact_threshold_tokens)),
            flag("compact-threshold-ratio", s(self.compact_threshold_ratio)),
            flag("compact-strategy", self.compact_strategy.clone()),
            flag("bail-after-compactions", s(self.bail_after_compactions)),
            flag("context-window", s(self.context_window)),
        ]
    }

    /// The compaction overlay (axis 2). Parses `--compact-strategy` and
    /// range-checks `--compact-threshold-ratio`: this is a trust-the-bench
    /// surface, so a bad value is refused loudly before any dispatch.
    fn compaction_overlay(&self) -> Result<LoopCompactionOverride> {
        let strategy = self.compact_strategy.as_deref().map(parse_compact_strategy).transpose()?;
        if let Some(r) = self.compact_threshold_ratio {
            anyhow::ensure!(
                (0.1..=0.9).contains(&r),
                "--compact-threshold-ratio {r} is out of range (expected 0.1–0.9)"
            );
        }
        Ok(LoopCompactionOverride {
            threshold_tokens: self.compact_threshold_tokens,
            threshold_ratio: self.compact_threshold_ratio,
            context_window: self.context_window,
            strategy,
            bail_after_compactions: self.bail_after_compactions,
        })
    }

    fn plan(&self) -> Result<LoopPlan> {
        let overlay = self.compaction_overlay()?;
        let given: Vec<LoopFlag> = self.flags().into_iter().filter(|f| f.value.is_some()).collect();
        let mut loop_config: Vec<String> = given
            .iter()
            .map(|f| format!("{}={}{}", f.label, f.value.as_deref().unwrap_or_default(), f.unit))
            .collect();
        if loop_config.is_empty() {
            loop_config.push("profile defaults (no overrides)".to_string());
        }
        let caps = given
            .into_iter()
            .filter_map(|f| Some((f.cap_env?, f.value?)))
            .collect();
        Ok(LoopPlan { overlay, loop_config, caps })
    }
}

/// Parse the `--compact-strategy` flag into the typed enum. Accepts the two
/// strategies the runtime supports, kebab- or snake-cased, any case.
fn parse_compact_strategy(raw: &str) -> Result<darkmux_types::CompactionStrategy> {
    match raw.trim().to_lowercase().replace('_', "-").as_str() {
        "narrative" => Ok(darkmux_types::CompactionStrategy::Narrative),
        "structured-slot" => Ok(darkmux_types::CompactionStrategy::StructuredSlot),
        other => anyhow::bail!(
            "unknown --compact-strategy `{other}` (expected `narrative` or `structured-slot`)"
        ),
    }
}

pub(super) fn cmd_lab_loop(args: LabLoopArgs) -> Result<i32> {
    // Armed once, ahead of both the single-run and the two-run `--ab` shape.
    let _reap_watchdog = super::arm_signal_handling();
    let plan = args.plan()?;
    apply_caps(&plan.caps);
    if args.ab {
        return run_ab_compare(&args, &plan);
    }
    let report = run_arm(&args, &plan, None)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        loop_report::print_report(&report);
    }
    Ok(verdict_exit(report.verdict))
}

/// Set each cap on the live env-override tier (axis 1) for this process's
/// dispatches.
fn apply_caps(caps: &[(&'static str, String)]) {
    for (env, value) in caps {
        std::env::set_var(env, value);
    }
}

/// One arm of the bench: a single dispatch, classified. `inject` carries the
/// engagement context for the A/B treatment arm; `None` is the baseline.
/// Quiet in `--json` mode so stdout stays pure JSON.
fn run_arm(args: &LabLoopArgs, plan: &LoopPlan, inject: Option<String>) -> Result<LoopReport> {
    let outcomes = darkmux_lab::lab::run::lab_run(darkmux_lab::lab::run::RunOpts {
        workload_id: args.workload.clone(),
        profile_name: args.profile.clone(),
        runs: 1,
        config_path: args.profiles.clone(),
        quiet: args.json,
        // No compaction flag: `None`, so the dispatch takes the exact `lab
        // run` compaction path (caps still apply via env).
        loop_override: (!plan.overlay.is_empty()).then(|| plan.overlay.clone()),
        inject_context: inject,
    })?;
    let outcome = outcomes
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("loop lab produced no run outcome"))?;
    loop_report::analyze_run(
        &outcome.run_dir,
        &outcome.run_id,
        outcome.ok,
        outcome.verify_passed,
        outcome.duration_ms,
        plan.loop_config.clone(),
    )
}

/// Exit 0 when the loop achieved the task (productive or struggled through);
/// 1 when it failed or, critically, falsely passed while inert.
fn verdict_exit(v: Verdict) -> i32 {
    match v {
        Verdict::Productive | Verdict::Struggled => 0,
        Verdict::InertFalsePass | Verdict::Failed => 1,
    }
}

/// (#1004) Run the baseline and treatment arms and report the shift; the
/// exit code is [`ab_exit`]'s.
fn run_ab_compare(args: &LabLoopArgs, plan: &LoopPlan) -> Result<i32> {
    let ctx = ab_context(args)?;
    let ctx_chars = ctx.len();
    if !args.json {
        eprintln!("… A/B: baseline run (WITHOUT engagement-context)");
    }
    let without = run_arm(args, plan, None)?;
    if !args.json {
        eprintln!("… A/B: treatment run (WITH {ctx_chars} chars of engagement-context)");
    }
    let with = run_arm(args, plan, Some(ctx))?;
    // Run ids are second-stamped; the arms are sequential with a dispatch
    // between, so they normally differ. A shared run dir would merge
    // trajectories and confound the very comparison this makes, so a
    // collision is surfaced, never silently trusted (#44).
    anyhow::ensure!(
        with.run_id != without.run_id,
        "A/B run-id collision (`{}`): both arms wrote the same run dir, so the \
         comparison would mix trajectories. Re-run `--ab` (the arms are sequential; \
         a second-boundary collision won't recur).",
        with.run_id
    );
    print!("{}", render_ab(args.json, ctx_chars, &without, &with)?);
    Ok(ab_exit(&without, &with))
}

/// The `--ab` exit code follows the WITH arm: that is the configuration the
/// operator would ship. The baseline arm never decides it.
fn ab_exit(_without: &LoopReport, with: &LoopReport) -> i32 {
    verdict_exit(with.verdict)
}

/// The engagement context the treatment arm injects: the repo's authored
/// lessons, plus a mission's detected cautions with `--inject-from-mission`.
/// Budgeted for the model that reads it, which the workload's dispatch role
/// picks (#2902). Refuses when there is nothing to inject.
fn ab_context(args: &LabLoopArgs) -> Result<String> {
    let ws = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let role = darkmux_lab::lab::run::workload_dispatch_role(&args.workload);
    let mission = args.inject_from_mission.as_deref();
    let ctx = crate::coder_phase::injected_context_for_lab(
        mission,
        &ws,
        role.as_deref(),
        args.profile.as_deref(),
        args.profiles.as_deref(),
    );
    if ctx.trim().is_empty() {
        let (no_cautions, remedy) = match mission {
            Some(m) => (format!(" and no detected cautions for mission `{m}`"), ""),
            None => (
                String::new(),
                " or pass --inject-from-mission <id> to add a mission's cautions",
            ),
        };
        anyhow::bail!(
            "--ab: nothing to inject — no authored lessons for this repo{no_cautions}. \
             Record a lesson (`darkmux memory lesson add`){remedy}, then retry."
        );
    }
    Ok(ctx)
}

/// How the verdict moved from the baseline arm to the treatment arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerdictShift {
    Improved,
    Regressed,
    NoChange,
}

impl VerdictShift {
    fn between(without: Verdict, with: Verdict) -> Self {
        match verdict_rank(with).cmp(&verdict_rank(without)) {
            std::cmp::Ordering::Greater => VerdictShift::Improved,
            std::cmp::Ordering::Less => VerdictShift::Regressed,
            std::cmp::Ordering::Equal => VerdictShift::NoChange,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            VerdictShift::Improved => "improved",
            VerdictShift::Regressed => "regressed",
            VerdictShift::NoChange => "no-change",
        }
    }
}

/// Verdicts ranked best to worst, so a shift is one signed comparison.
fn verdict_rank(v: Verdict) -> u8 {
    match v {
        Verdict::Productive => 3,
        Verdict::Struggled => 2,
        Verdict::InertFalsePass => 1,
        Verdict::Failed => 0,
    }
}

/// The A/B result as printed: one JSON object in `--json` mode, else the
/// human block.
fn render_ab(json: bool, ctx_chars: usize, without: &LoopReport, with: &LoopReport) -> Result<String> {
    let shift = VerdictShift::between(without.verdict, with.verdict).as_str();
    if json {
        let v = serde_json::json!({
            "ab": true,
            "injected_context_chars": ctx_chars,
            "verdict_shift": shift,
            "without": without,
            "with": with,
        });
        return Ok(format!("{}\n", serde_json::to_string_pretty(&v)?));
    }
    Ok(format!(
        "\n── engagement-context A/B (#1004) ──\n\
         \x20 without context: {}\n\
         \x20 with context:    {} ({ctx_chars} chars injected)\n\
         \x20 verdict shift:   {shift}\n",
        without.verdict.as_str(),
        with.verdict.as_str(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare(workload: &str) -> LabLoopArgs {
        LabLoopArgs {
            workload: workload.to_string(),
            profile: None,
            profiles: None,
            max_turns: None,
            max_tokens: None,
            timeout: None,
            compact_threshold_tokens: None,
            compact_threshold_ratio: None,
            compact_strategy: None,
            bail_after_compactions: None,
            context_window: None,
            ab: false,
            inject_from_mission: None,
            json: false,
        }
    }

    fn every_flag() -> LabLoopArgs {
        LabLoopArgs {
            profile: Some("p".into()),
            profiles: Some("/r.json".into()),
            max_turns: Some(7),
            max_tokens: Some(900),
            timeout: Some(33),
            compact_threshold_tokens: Some(4000),
            compact_threshold_ratio: Some(0.5),
            compact_strategy: Some("Structured_Slot".into()),
            bail_after_compactions: Some(2),
            context_window: Some(16000),
            ..bare("w")
        }
    }

    #[test]
    fn every_compaction_flag_reaches_the_overlay() {
        let o = every_flag().plan().unwrap().overlay;
        assert_eq!(o.threshold_tokens, Some(4000));
        assert_eq!(o.threshold_ratio, Some(0.5));
        assert_eq!(o.context_window, Some(16000));
        assert_eq!(o.strategy, Some(darkmux_types::CompactionStrategy::StructuredSlot));
        assert_eq!(o.bail_after_compactions, Some(2));
        assert!(bare("w").plan().unwrap().overlay.is_empty());
    }

    /// Each compaction flag on its own makes the overlay non-empty, so the
    /// dispatch takes the override path.
    #[test]
    fn each_compaction_flag_alone_makes_the_overlay_non_empty() {
        let alone = [
            LabLoopArgs { compact_threshold_tokens: Some(1), ..bare("w") },
            LabLoopArgs { compact_threshold_ratio: Some(0.2), ..bare("w") },
            LabLoopArgs { compact_strategy: Some("narrative".into()), ..bare("w") },
            LabLoopArgs { bail_after_compactions: Some(1), ..bare("w") },
            LabLoopArgs { context_window: Some(1), ..bare("w") },
        ];
        for args in alone {
            assert!(!args.plan().unwrap().overlay.is_empty(), "{:?}", args.plan().unwrap().loop_config);
        }
        // Caps are not compaction: they leave the overlay empty.
        let caps = LabLoopArgs { max_turns: Some(1), max_tokens: Some(1), timeout: Some(1), ..bare("w") };
        assert!(caps.plan().unwrap().overlay.is_empty());
    }

    #[test]
    fn every_cap_reaches_its_env_var_as_a_bare_number() {
        let caps = every_flag().plan().unwrap().caps;
        assert_eq!(
            caps,
            vec![
                ("DARKMUX_RUNTIME_MAX_TURNS", "7".to_string()),
                ("DARKMUX_RUNTIME_MAX_TOKENS", "900".to_string()),
                ("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", "33".to_string()),
            ]
        );
        assert!(bare("w").plan().unwrap().caps.is_empty());
    }

    /// `cmd_lab_loop` applies the caps to the process env the runtime's
    /// `config_access` reads before its first dispatch: an unknown workload
    /// fails the dispatch after the caps are already set.
    #[test]
    #[serial_test::serial]
    fn cmd_lab_loop_sets_each_cap_on_the_process_env_before_dispatch() {
        let vars = [
            "DARKMUX_RUNTIME_MAX_TURNS",
            "DARKMUX_RUNTIME_MAX_TOKENS",
            "DARKMUX_INACTIVITY_TIMEOUT_SECONDS",
        ];
        let prev: Vec<_> = vars.iter().map(|v| std::env::var_os(v)).collect();
        let result = cmd_lab_loop(LabLoopArgs {
            workload: "no-such-wl".into(),
            profile: None,
            profiles: None,
            ..every_flag()
        });
        let got: Vec<_> = vars.iter().map(|v| std::env::var(v).ok()).collect();
        for (v, p) in vars.iter().zip(prev) {
            match p {
                Some(p) => std::env::set_var(v, p),
                None => std::env::remove_var(v),
            }
        }
        let err = result.err().expect("an unknown workload must fail the dispatch").to_string();
        assert!(err.contains("no-such-wl"), "{err}");
        assert_eq!(got, [Some("7".into()), Some("900".into()), Some("33".into())]);
    }

    #[test]
    fn every_flag_is_labeled_in_report_order() {
        assert_eq!(
            every_flag().plan().unwrap().loop_config,
            [
                "profile=p",
                "profiles-file=/r.json",
                "max-turns=7",
                "max-tokens=900",
                "timeout=33s",
                "compact-threshold-tokens=4000",
                "compact-threshold-ratio=0.5",
                "compact-strategy=Structured_Slot",
                "bail-after-compactions=2",
                "context-window=16000",
            ]
        );
        assert_eq!(bare("w").plan().unwrap().loop_config, ["profile defaults (no overrides)"]);
    }

    #[test]
    fn bad_compaction_values_are_refused_strategy_first() {
        let both = LabLoopArgs {
            compact_strategy: Some("bogus".into()),
            compact_threshold_ratio: Some(5.0),
            ..bare("w")
        };
        let err = both.plan().err().unwrap().to_string();
        assert!(err.contains("unknown --compact-strategy `bogus`"), "{err}");
        for (r, ok) in [(0.09, false), (0.1, true), (0.9, true), (0.91, false)] {
            let args = LabLoopArgs { compact_threshold_ratio: Some(r), ..bare("w") };
            assert_eq!(args.plan().is_ok(), ok, "{r}");
        }
    }

    #[test]
    fn verdict_exit_passes_only_an_achieved_task() {
        assert_eq!(verdict_exit(Verdict::Productive), 0);
        assert_eq!(verdict_exit(Verdict::Struggled), 0);
        assert_eq!(verdict_exit(Verdict::InertFalsePass), 1);
        assert_eq!(verdict_exit(Verdict::Failed), 1);
    }

    #[test]
    fn ab_exit_follows_the_with_arm_in_both_directions() {
        let (failed, productive) = (report("a", Verdict::Failed), report("b", Verdict::Productive));
        assert_eq!(ab_exit(&failed, &productive), 0, "Failed -> Productive ships a passing config");
        assert_eq!(ab_exit(&productive, &failed), 1, "Productive -> Failed ships a failing one");
    }

    #[test]
    fn verdict_shift_is_signed_by_rank() {
        use Verdict::*;
        let all = [Failed, InertFalsePass, Struggled, Productive];
        for (i, &a) in all.iter().enumerate() {
            for (j, &b) in all.iter().enumerate() {
                let want = match j.cmp(&i) {
                    std::cmp::Ordering::Greater => VerdictShift::Improved,
                    std::cmp::Ordering::Less => VerdictShift::Regressed,
                    std::cmp::Ordering::Equal => VerdictShift::NoChange,
                };
                assert_eq!(VerdictShift::between(a, b), want, "{a} -> {b}");
            }
        }
    }

    fn report(run_id: &str, verdict: Verdict) -> LoopReport {
        LoopReport {
            run_id: run_id.to_string(),
            verdict,
            dispatch_ok: true,
            verify_passed: None,
            sandbox_changed: None,
            tool_calls: 0,
            turns: 0,
            compactions: 0,
            detectors: Default::default(),
            duration_ms: 0,
            loop_config: vec!["max-turns=4".to_string()],
            notes: vec![],
        }
    }

    #[test]
    fn render_ab_human_block() {
        let text =
            render_ab(false, 42, &report("a", Verdict::Failed), &report("b", Verdict::Struggled)).unwrap();
        assert_eq!(
            text,
            "\n── engagement-context A/B (#1004) ──\n  without context: failed\n  \
             with context:    struggled (42 chars injected)\n  verdict shift:   improved\n"
        );
    }

    #[test]
    fn render_ab_json_object() {
        let without = report("a", Verdict::Productive);
        let with = report("b", Verdict::Failed);
        let text = render_ab(true, 7, &without, &with).unwrap();
        assert!(text.ends_with("}\n"), "{text}");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "ab": true,
                "injected_context_chars": 7,
                "verdict_shift": "regressed",
                "without": serde_json::to_value(&without).unwrap(),
                "with": serde_json::to_value(&with).unwrap(),
            })
        );
    }
}
