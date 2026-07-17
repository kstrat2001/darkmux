//! `darkmux lab` command handlers — extracted from `main.rs` (mechanical,
//! zero behavior change) alongside the `fleet_cli`/`cli` split. `LabCmd`
//! itself (the arg surface) lives in `cli.rs`; this module owns the
//! dispatch logic `cli::run` calls into, plus the `lab loop` (#986)
//! single-run bench that used to live directly below `cmd_lab` in
//! `main.rs`.

use anyhow::Result;

use crate::cli::LabCmd;
use crate::lab;
use crate::workloads;

pub(crate) fn cmd_lab(sub: LabCmd) -> Result<i32> {
    match sub {
        LabCmd::Workloads => {
            let ids = lab::run::lab_workloads();
            if ids.is_empty() {
                println!("(no workloads found — check templates/builtin/workloads/ or .darkmux/workloads/)");
            } else {
                for id in ids {
                    println!("{id}");
                }
            }
            Ok(0)
        }
        LabCmd::Run {
            workload,
            profile,
            runs,
            profiles: crate::cli::ProfilesFileArg { profiles },
            quiet,
        } => {
            let outcomes = lab::run::lab_run(lab::run::RunOpts {
                workload_id: workload,
                profile_name: profile,
                runs,
                config_path: profiles,
                quiet,
                loop_override: None,
                inject_context: None,
            })?;
            if !quiet {
                println!("\n{} run(s) complete:", outcomes.len());
                for o in &outcomes {
                    println!("  {} — {}", o.run_id, o.notes.join(" | "));
                }
            }
            Ok(if outcomes.iter().all(|o| o.ok) { 0 } else { 1 })
        }
        LabCmd::ReviewBench {
            cases_dir,
            profile,
            profiles,
            timeout,
            scores_out,
            freeform,
            agentic,
            dialectic,
            funnel,
            workdirs,
            prosecutor_profile,
            defender_profile,
            judge_profile,
            crew,
            exec_mode,
            k,
            bundler,
        } => {
            lab::review_bench::run_review_bench(lab::review_bench::ReviewBenchOpts {
                cases_dir: std::path::PathBuf::from(cases_dir),
                profile_name: profile,
                config_path: profiles,
                timeout_seconds: timeout,
                scores_out,
                mode: if funnel {
                    lab::review_bench::BenchMode::Funnel
                } else if dialectic {
                    lab::review_bench::BenchMode::Dialectic
                } else if agentic {
                    lab::review_bench::BenchMode::Agentic
                } else if freeform {
                    lab::review_bench::BenchMode::FreeForm
                } else {
                    lab::review_bench::BenchMode::Strict
                },
                workdirs,
                prosecutor_profile,
                defender_profile,
                judge_profile,
                crew,
                exec_mode,
                k_override: k,
                bundler_cmd: bundler,
            })?;
            Ok(0)
        }
        LabCmd::Loop {
            workload,
            profile,
            profiles,
            max_turns,
            max_tokens,
            timeout,
            compact_threshold_tokens,
            compact_threshold_ratio,
            compact_strategy,
            bail_after_compactions,
            context_window,
            ab,
            inject_from_mission,
            json,
        } => cmd_lab_loop(LabLoopArgs {
            workload,
            profile,
            profiles,
            max_turns,
            max_tokens,
            timeout,
            compact_threshold_tokens,
            compact_threshold_ratio,
            compact_strategy,
            bail_after_compactions,
            context_window,
            ab,
            inject_from_mission,
            json,
        }),
        LabCmd::Runs { limit, all } => {
            let lim = if all { None } else { Some(limit) };
            let summaries = lab::list::list_runs(lim)?;
            print!("{}", lab::list::format_table(&summaries));
            Ok(0)
        }
        LabCmd::Inspect { run, summary } => {
            let report = lab::inspect::lab_inspect(&run)?;
            println!("run:         {}", report.run_id);
            println!("workload:    {}", report.workload_id);
            println!("wall:        {}s", report.walltime_ms / 1000);
            println!("turns:       {}", report.turns);
            println!("compactions: {}", report.compactions);
            if !report.tokens_before.is_empty() {
                let listed: Vec<String> =
                    report.tokens_before.iter().map(|n| n.to_string()).collect();
                println!("tokensBefore: {}", listed.join(", "));
            }
            if let Some(m) = report.mode {
                println!(
                    "mode:        {}",
                    match m {
                        workloads::types::RunMode::Fast => "fast",
                        workloads::types::RunMode::Slow => "slow",
                    }
                );
            }
            println!("notes:");
            for n in &report.notes {
                println!("  - {n}");
            }
            if summary {
                let run_dir = lab::inspect::resolve_run_path(&run);
                let summaries = lab::inspect::read_compaction_summaries(&run_dir)?;
                println!();
                if summaries.is_empty() {
                    println!("compaction summaries: (none — no trajectory.jsonl recorded)");
                } else {
                    println!("compaction summaries: {}", summaries.len());
                    for (i, s) in summaries.iter().enumerate() {
                        println!();
                        println!(
                            "─── summary {} of {} (turn {}, tokensBefore={}, {} chars) ───",
                            i + 1,
                            summaries.len(),
                            s.turn_index,
                            s.tokens_before,
                            s.summary_chars
                        );
                        println!("{}", s.summary_text);
                    }
                }
            }
            Ok(0)
        }
        LabCmd::Compare { run_a, run_b } => {
            let result = lab::compare::lab_compare(&run_a, &run_b)?;
            for n in &result.notes {
                println!("{n}");
            }
            Ok(0)
        }
        LabCmd::Characterize {
            workload,
            profile,
            profiles: crate::cli::ProfilesFileArg { profiles },
        } => {
            let report = lab::characterize::characterize(&lab::characterize::CharacterizeOpts {
                workload,
                profile,
                config: profiles,
            })?;
            lab::characterize::print_report(&report);
            Ok(if report.outcomes.iter().all(|o| o.ok) {
                0
            } else {
                1
            })
        }
        LabCmd::Tune {
            workload,
            profile,
            runs,
            profiles: crate::cli::ProfilesFileArg { profiles },
        } => {
            let report = lab::tune::tune(&lab::tune::TuneOpts {
                workload,
                profile,
                runs,
                config: profiles,
            })?;
            lab::tune::print_report(&report);
            Ok(if report.outcomes.iter().all(|o| o.ok) {
                0
            } else {
                1
            })
        }
        LabCmd::Register {
            path,
            name,
            force,
            if_absent,
        } => {
            let msg = lab::fixture_cli::cmd_register(&path, name, force, if_absent)?;
            println!("{msg}");
            Ok(0)
        }
        LabCmd::Unregister { name } => {
            let msg = lab::fixture_cli::cmd_unregister(&name)?;
            println!("{msg}");
            Ok(0)
        }
        LabCmd::Fixtures => {
            let msg = lab::fixture_cli::cmd_list()?;
            println!("{msg}");
            Ok(0)
        }
        LabCmd::Doctor => {
            let report = lab::doctor::lab_doctor()?;
            // Warnings first so actionable items don't get buried
            // behind a long list of passes when many fixtures are
            // registered. Reviewer suggestion (#498 QA).
            for w in &report.warnings {
                println!("[warn] {w}");
            }
            for p in &report.passes {
                println!("[ok]  {p}");
            }
            println!();
            println!(
                "{} pass, {} warn ({} fixture{} checked)",
                report.passes.len(),
                report.warnings.len(),
                report.fixture_count,
                if report.fixture_count == 1 { "" } else { "s" }
            );
            Ok(if report.has_warnings() { 1 } else { 0 })
        }
        // (#1426) `lab notebook draft|list` — the notebook family folded into
        // `lab` (the retired top-level `notebook` verb). The handler stays in
        // `main.rs` beside the other agent-as-scribe plumbing.
        LabCmd::Notebook { sub } => crate::cmd_notebook(sub),
    }
}

/// Flattened args for `darkmux lab loop` (#986). Kept as a struct so the
/// handler signature stays one parameter rather than a dozen.
struct LabLoopArgs {
    workload: String,
    profile: Option<String>,
    profiles: Option<String>,
    max_turns: Option<u32>,
    max_tokens: Option<u32>,
    timeout: Option<u64>,
    compact_threshold_tokens: Option<u32>,
    compact_threshold_ratio: Option<f32>,
    compact_strategy: Option<String>,
    bail_after_compactions: Option<u32>,
    context_window: Option<u32>,
    ab: bool,
    inject_from_mission: Option<String>,
    json: bool,
}

/// Parse the `--compact-strategy` flag into the typed enum. Accepts the two
/// strategies the runtime supports, kebab- or snake-cased.
fn parse_compact_strategy(raw: &str) -> Result<darkmux_types::CompactionStrategy> {
    match raw.trim().to_lowercase().replace('_', "-").as_str() {
        "narrative" => Ok(darkmux_types::CompactionStrategy::Narrative),
        "structured-slot" => Ok(darkmux_types::CompactionStrategy::StructuredSlot),
        other => anyhow::bail!(
            "unknown --compact-strategy `{other}` (expected `narrative` or `structured-slot`)"
        ),
    }
}

/// `darkmux lab loop` (#986) — single-run loop-engineering bench. Runs ONE
/// dispatch under the chosen harness config, then classifies how the loop
/// behaved via `lab::loop_report`.
///
/// Two loop-variation axes:
///   - **Caps** (`--max-turns` / `--max-tokens` / `--timeout`) resolve through
///     `config_access`'s live env tier, so we set the documented env override
///     for this dispatch. This process is single-shot (it exits right after),
///     so a process-wide `set_var` is the simplest honest mechanism — it's the
///     same tier an operator would `export` for one run.
///   - **Compaction** (`--compact-*` / `--bail-after-compactions` /
///     `--context-window`) overlays onto the profile-derived
///     `CompactionDispatchArgs` via the provider (`loop_override`).
fn cmd_lab_loop(args: LabLoopArgs) -> Result<i32> {
    use darkmux_lab::lab::loop_report::{analyze_run, LoopCompactionOverride};

    // ── build the compaction overlay (axis 2) ───────────────────────
    let strategy = match args.compact_strategy.as_deref() {
        Some(s) => Some(parse_compact_strategy(s)?),
        None => None,
    };
    // Validate the adaptive-trigger ratio upfront (parity with
    // --compact-strategy) — this is a trust-the-bench surface, so reject an
    // out-of-range value loudly rather than letting it flow into the runtime.
    if let Some(r) = args.compact_threshold_ratio {
        if !(0.1..=0.9).contains(&r) {
            anyhow::bail!(
                "--compact-threshold-ratio {r} is out of range (expected 0.1–0.9)"
            );
        }
    }
    let loop_override = LoopCompactionOverride {
        threshold_tokens: args.compact_threshold_tokens,
        threshold_ratio: args.compact_threshold_ratio,
        context_window: args.context_window,
        strategy,
        bail_after_compactions: args.bail_after_compactions,
    };

    // ── self-describing loop-config summary for the report ───────────
    let mut loop_config: Vec<String> = Vec::new();
    if let Some(p) = args.profile.as_deref() {
        loop_config.push(format!("profile={p}"));
    }
    if let Some(p) = args.profiles.as_deref() {
        loop_config.push(format!("profiles-file={p}"));
    }
    if let Some(n) = args.max_turns {
        loop_config.push(format!("max-turns={n}"));
    }
    if let Some(n) = args.max_tokens {
        loop_config.push(format!("max-tokens={n}"));
    }
    if let Some(n) = args.timeout {
        loop_config.push(format!("timeout={n}s"));
    }
    if let Some(n) = args.compact_threshold_tokens {
        loop_config.push(format!("compact-threshold-tokens={n}"));
    }
    if let Some(r) = args.compact_threshold_ratio {
        loop_config.push(format!("compact-threshold-ratio={r}"));
    }
    if let Some(s) = args.compact_strategy.as_deref() {
        loop_config.push(format!("compact-strategy={s}"));
    }
    if let Some(n) = args.bail_after_compactions {
        loop_config.push(format!("bail-after-compactions={n}"));
    }
    if let Some(n) = args.context_window {
        loop_config.push(format!("context-window={n}"));
    }
    if loop_config.is_empty() {
        loop_config.push("profile defaults (no overrides)".to_string());
    }

    // ── apply caps via the live env-override tier (axis 1) ───────────
    if let Some(n) = args.max_turns {
        std::env::set_var("DARKMUX_RUNTIME_MAX_TURNS", n.to_string());
    }
    if let Some(n) = args.max_tokens {
        std::env::set_var("DARKMUX_RUNTIME_MAX_TOKENS", n.to_string());
    }
    if let Some(n) = args.timeout {
        std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", n.to_string());
    }

    // ── run one arm of the bench (single dispatch + classify) ────────
    // Cloned inputs so the A/B path (#1004) can call it twice with only the
    // injected context varying. `inject` carries the engagement-context for the
    // "with" arm; `None` is the baseline. quiet in --json mode so stdout stays
    // pure JSON.
    use darkmux_lab::lab::loop_report::Verdict;
    let run_arm = |inject: Option<String>| -> Result<darkmux_lab::lab::loop_report::LoopReport> {
        let outcomes = darkmux_lab::lab::run::lab_run(darkmux_lab::lab::run::RunOpts {
            workload_id: args.workload.clone(),
            profile_name: args.profile.clone(),
            runs: 1,
            config_path: args.profiles.clone(),
            quiet: args.json,
            // When no compaction flag was set, pass `None` so the dispatch takes
            // the exact `lab run` compaction path (caps still apply via env).
            loop_override: if loop_override.is_empty() {
                None
            } else {
                Some(loop_override.clone())
            },
            inject_context: inject,
        })?;
        let outcome = outcomes
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("loop lab produced no run outcome"))?;
        analyze_run(
            &outcome.run_dir,
            &outcome.run_id,
            outcome.ok,
            outcome.verify_passed,
            outcome.duration_ms,
            loop_config.clone(),
        )
    };

    // Exit 0 when the loop achieved the task (productive or struggled-through);
    // non-zero when it failed or — critically — falsely passed while inert.
    let verdict_exit = |v: Verdict| match v {
        Verdict::Productive | Verdict::Struggled => 0,
        Verdict::InertFalsePass | Verdict::Failed => 1,
    };

    // ── (#1004) engagement-context A/B ───────────────────────────────
    if args.ab {
        let ws = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let ctx = crate::mission_run::injected_context_for_lab(
            args.inject_from_mission.as_deref(),
            &ws,
            args.profile.as_deref(),
            args.profiles.as_deref(),
        );
        if ctx.trim().is_empty() {
            anyhow::bail!(
                "--ab: nothing to inject — no authored lessons for this repo{}. \
                 Record a lesson (`darkmux lessons add`){}, then retry.",
                args.inject_from_mission
                    .as_deref()
                    .map(|m| format!(" and no detected cautions for mission `{m}`"))
                    .unwrap_or_default(),
                if args.inject_from_mission.is_some() {
                    ""
                } else {
                    " or pass --inject-from-mission <id> to add a mission's cautions"
                }
            );
        }
        let ctx_chars = ctx.len();
        if !args.json {
            eprintln!("… A/B: baseline run (WITHOUT engagement-context)");
        }
        let without = run_arm(None)?;
        if !args.json {
            eprintln!("… A/B: treatment run (WITH {ctx_chars} chars of engagement-context)");
        }
        let with = run_arm(Some(ctx))?;

        // Run ids are second-stamped; the two arms are sequential with a
        // multi-second dispatch between, so they normally differ. Guard the
        // (improbable) same-second collision — a shared run dir would merge
        // trajectories and confound the very comparison this feature makes —
        // so a confounded A/B is surfaced, never silently trusted (#44).
        if with.run_id == without.run_id {
            anyhow::bail!(
                "A/B run-id collision (`{}`): both arms wrote the same run dir, so the \
                 comparison would mix trajectories. Re-run `--ab` (the arms are sequential; \
                 a second-boundary collision won't recur).",
                with.run_id
            );
        }

        // Rank the verdicts so the shift is a single signed comparison.
        let rank = |v: Verdict| match v {
            Verdict::Productive => 3,
            Verdict::Struggled => 2,
            Verdict::InertFalsePass => 1,
            Verdict::Failed => 0,
        };
        let shift = match rank(with.verdict).cmp(&rank(without.verdict)) {
            std::cmp::Ordering::Greater => "improved",
            std::cmp::Ordering::Less => "regressed",
            std::cmp::Ordering::Equal => "no-change",
        };

        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ab": true,
                    "injected_context_chars": ctx_chars,
                    "verdict_shift": shift,
                    "without": without,
                    "with": with,
                }))?
            );
        } else {
            println!("\n── engagement-context A/B (#1004) ──");
            println!("  without context: {}", without.verdict.as_str());
            println!("  with context:    {} ({ctx_chars} chars injected)", with.verdict.as_str());
            println!("  verdict shift:   {shift}");
        }
        // The "with" arm is the configuration the operator would ship.
        return Ok(verdict_exit(with.verdict));
    }

    // ── single run (default) ─────────────────────────────────────────
    let report = run_arm(None)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        darkmux_lab::lab::loop_report::print_report(&report);
    }
    Ok(verdict_exit(report.verdict))
}
