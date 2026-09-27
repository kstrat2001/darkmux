//! `darkmux lab` command handlers. `LabCmd` itself (the arg surface) lives in
//! `cli.rs`; this module owns the dispatch logic `cli::run` calls into. Each
//! `LabCmd` arm unpacks its arguments and hands them to one handler below;
//! `lab loop` (#986) lives in the `lab_loop` submodule.

mod lab_loop;

use anyhow::Result;

use crate::cli::{FixtureCmd, LabCmd, ProfilesFileArg, RunCmd, WorkloadCmd};
use crate::lab;
use crate::workloads;

pub(crate) fn cmd_lab(sub: LabCmd) -> Result<i32> {
    match sub {
        LabCmd::Workload { sub } => cmd_lab_workload(sub),
        LabCmd::Run { sub: Some(run_sub), .. } => cmd_lab_run_sub(run_sub),
        LabCmd::Run {
            workload,
            profile,
            runs,
            profiles: ProfilesFileArg { profiles },
            quiet,
            sub: None,
        } => cmd_lab_run_dispatch(workload, profile, runs, profiles, quiet),
        LabCmd::Eval {
            role,
            cases_dir,
            profile,
            profiles,
            timeout,
            scores_out,
            freeform,
            agentic,
            dialectic,
            workdirs,
            prosecutor_profile,
            defender_profile,
            judge_profile,
            roster_profile,
            exec_mode,
            k,
            bundler,
        } => cmd_lab_eval(lab::review_bench::ReviewBenchOpts {
            role,
            cases_dir: std::path::PathBuf::from(cases_dir),
            profile_name: profile,
            config_path: profiles,
            timeout_seconds: timeout,
            scores_out,
            mode: bench_mode(freeform, agentic, dialectic),
            workdirs,
            prosecutor_profile,
            defender_profile,
            judge_profile,
            roster_profile,
            exec_mode,
            k_override: k,
            bundler_cmd: bundler,
        }),
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
        } => lab_loop::cmd_lab_loop(lab_loop::LabLoopArgs {
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
        LabCmd::Characterize {
            workload,
            profile,
            profiles: ProfilesFileArg { profiles },
        } => cmd_lab_characterize(lab::characterize::CharacterizeOpts {
            workload,
            profile,
            config: profiles,
        }),
        LabCmd::Tune {
            workload,
            profile,
            runs,
            profiles: ProfilesFileArg { profiles },
        } => cmd_lab_tune(lab::tune::TuneOpts {
            workload,
            profile,
            runs,
            config: profiles,
        }),
        LabCmd::Fixture { sub } => cmd_lab_fixture(sub),
        LabCmd::Doctor => cmd_lab_doctor(),
    }
}

/// Run a lab verb that dispatches through `lab_run` (or `dispatch`, for
/// `lab eval`) under signal handling: the one exit path every such verb
/// shares.
///
/// Installs the SIGTERM/SIGINT/SIGHUP handlers and the reap watchdog for the
/// verb's lifetime. Without them a signal kills the process with no unwind,
/// orphaning the dispatch's docker container or `curl` child (#2262, #2463).
/// `lab_run` already writes a terminal `lifecycle.json` on any dispatch
/// `Err`, and the dispatch's own bookend guard emits `dispatch.error`, so
/// the handlers and the watchdog (which kills a registered `curl` child on
/// the tool-less hosted path, see `spawn_reap_watchdog`) are all a lab verb
/// adds. Armed once, ahead of the verb's whole run loop:
/// `interrupt::is_set()` never resets, so one signal ends the whole
/// invocation.
///
/// (#2462) A verb a signal ended exits 130, matching `mission launch`, so a
/// wrapper script can tell "the operator stopped this" from a failure by
/// exit code alone. `report_*`, not the bare `reap_and_exit_on_signal`: the
/// force-exit runs before `main` prints the error, so the bare call would
/// discard the interrupt message.
fn signal_aware<T>(verb: impl FnOnce() -> Result<T>) -> Result<T> {
    crate::launch_guard::arm();
    let _reap_watchdog = crate::launch_guard::spawn_reap_watchdog();
    verb().inspect_err(crate::launch_guard::report_reap_and_exit_on_signal)
}

/// (#1465) `lab workload list`. `list_available` always includes the fixed,
/// non-empty `EMBEDDED_WORKLOADS` set, so there is no empty-list case.
fn cmd_lab_workload(sub: WorkloadCmd) -> Result<i32> {
    match sub {
        WorkloadCmd::List => {
            for id in lab::run::lab_workloads() {
                println!("{id}");
            }
            Ok(0)
        }
    }
}

/// (#1465) `lab run <workload>`: dispatch a workload `runs` times. `lab run`
/// takes EITHER this positional OR a run sub-verb (`cmd_lab_run_sub`);
/// `args_conflicts_with_subcommands` keeps the two forms from mixing.
fn cmd_lab_run_dispatch(
    workload: Option<String>,
    profile: Option<String>,
    runs: u32,
    profiles: Option<String>,
    quiet: bool,
) -> Result<i32> {
    let workload_id = workload.ok_or_else(|| {
        anyhow::anyhow!(
            "specify a workload to dispatch (`lab run <workload>`) or a run \
             sub-verb (`lab run list` / `lab run inspect <id>` / \
             `lab run compare <a> <b>`)"
        )
    })?;
    let outcomes = signal_aware(|| {
        lab::run::lab_run(lab::run::RunOpts {
            workload_id,
            profile_name: profile,
            runs,
            config_path: profiles,
            quiet,
            loop_override: None,
            inject_context: None,
        })
    })?;
    if !quiet {
        println!("\n{}", lab::run::batch_summary(&outcomes));
        for o in &outcomes {
            println!("  {} — {}", o.run_id, o.notes.join(" | "));
        }
    }
    Ok(lab::run::exit_code(&outcomes))
}

/// `lab eval`'s condition flags, most specific first. clap already refuses
/// the conflicting combinations; the order here only decides which flag a
/// caller that bypasses clap would get.
fn bench_mode(freeform: bool, agentic: bool, dialectic: bool) -> lab::review_bench::BenchMode {
    use lab::review_bench::BenchMode;
    if dialectic {
        BenchMode::Dialectic
    } else if agentic {
        BenchMode::Agentic
    } else if freeform {
        BenchMode::FreeForm
    } else {
        BenchMode::Strict
    }
}

/// `lab eval`: one dispatch per labeled case. A run killed mid-corpus loses
/// only the cases not yet scored; `scores.json` is written when the loop
/// completes.
fn cmd_lab_eval(opts: lab::review_bench::ReviewBenchOpts) -> Result<i32> {
    signal_aware(|| lab::review_bench::run_review_bench(opts))?;
    Ok(0)
}

/// `lab characterize`: a single `lab_run`, reported.
fn cmd_lab_characterize(opts: lab::characterize::CharacterizeOpts) -> Result<i32> {
    let report = signal_aware(|| lab::characterize::characterize(&opts))?;
    lab::characterize::print_report(&report);
    Ok(lab::run::exit_code(&report.outcomes))
}

/// `lab tune`: `lab_run` with `--runs N`, reported as a distribution.
fn cmd_lab_tune(opts: lab::tune::TuneOpts) -> Result<i32> {
    let report = signal_aware(|| lab::tune::tune(&opts))?;
    lab::tune::print_report(&report);
    Ok(lab::run::exit_code(&report.outcomes))
}

/// (#1465, #491) `lab fixture list|register|unregister`.
fn cmd_lab_fixture(sub: FixtureCmd) -> Result<i32> {
    let msg = match sub {
        FixtureCmd::List => lab::fixture_cli::cmd_list()?,
        FixtureCmd::Register {
            path,
            name,
            force,
            if_absent,
        } => lab::fixture_cli::cmd_register(&path, name, force, if_absent)?,
        FixtureCmd::Unregister { name } => lab::fixture_cli::cmd_unregister(&name)?,
    };
    println!("{msg}");
    Ok(0)
}

/// `lab doctor`: warnings first, so actionable items aren't buried behind a
/// long list of passes (#498 QA).
fn cmd_lab_doctor() -> Result<i32> {
    let report = lab::doctor::lab_doctor()?;
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

/// (#1465) The `lab run` sub-verbs: list, inspect, stats and compare
/// recorded runs.
fn cmd_lab_run_sub(sub: RunCmd) -> Result<i32> {
    match sub {
        RunCmd::List { limit, all } => {
            let summaries = lab::list::list_runs((!all).then_some(limit))?;
            print!(
                "{}",
                lab::list::format_table(&summaries, &darkmux_types::config_access::lab_dir())
            );
            Ok(0)
        }
        RunCmd::Inspect { run, summary } => cmd_lab_run_inspect(&run, summary),
        RunCmd::Stats { runs, baseline, json } => cmd_lab_run_stats(&runs, &baseline, json.json),
        RunCmd::Compare { run_a, run_b } => {
            let result = lab::compare::lab_compare(&run_a, &run_b)?;
            for n in &result.notes {
                println!("{n}");
            }
            Ok(0)
        }
    }
}

/// `lab run inspect <run> [--summary]`.
fn cmd_lab_run_inspect(run: &str, summary: bool) -> Result<i32> {
    let report = lab::inspect::lab_inspect(run)?;
    print_inspection(&report);
    if summary {
        let run_dir = lab::inspect::resolve_run_path(run);
        print_compaction_summaries(&lab::inspect::read_compaction_summaries(&run_dir)?);
    }
    Ok(0)
}

fn print_inspection(report: &workloads::types::InspectionReport) {
    println!("run:         {}", report.run_id);
    println!("workload:    {}", report.workload_id);
    println!("wall:        {}s", report.walltime_ms / 1000);
    // (#2094 finding 7) Shown next to wall so a rested run's wall clock is
    // never misread as a slow model. Milliseconds, because a seconds display
    // can round a real rest down to "0s". `0` means the run predates the
    // field or took no rests.
    if report.rest_ms > 0 {
        println!("rest:        {}ms", report.rest_ms);
    }
    println!("turns:       {}", report.turns);
    println!("compactions: {}", report.compactions);
    println!("verify:      {}", verify_line(report.verify.as_ref()));
    if !report.tokens_before.is_empty() {
        let listed: Vec<String> = report.tokens_before.iter().map(|n| n.to_string()).collect();
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
}

/// (#2494) The workload's OWN result, distinct from the dispatch path's `ok`.
/// "not checked" is said in words: an omitted line would read as a pass.
fn verify_line(verify: Option<&workloads::types::VerifyReport>) -> String {
    match verify {
        Some(v) if v.passed => "ok".to_string(),
        Some(v) if v.details.is_empty() => "FAILED".to_string(),
        Some(v) => format!("FAILED — {}", v.details),
        None => "not checked".to_string(),
    }
}

fn print_compaction_summaries(summaries: &[lab::inspect::CompactionSummary]) {
    println!();
    if summaries.is_empty() {
        println!("compaction summaries: (none — no trajectory.jsonl recorded)");
        return;
    }
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

/// (#2855) `lab run stats`: one run alone prints the single-run view; a set,
/// or one run with a baseline, prints the set view.
fn cmd_lab_run_stats(runs: &[String], baseline: &[String], json: bool) -> Result<i32> {
    use lab::stats_render as render;
    if runs.len() == 1 && baseline.is_empty() {
        let s = lab::stats::run_stats(&runs[0])?;
        if json {
            println!("{}", serde_json::to_string_pretty(&s)?);
        } else {
            print!("{}", render::run_text(&s));
        }
        return Ok(0);
    }
    let cand = render::load_set(runs);
    let base = (!baseline.is_empty()).then(|| render::load_set(baseline));
    if json {
        println!("{}", serde_json::to_string_pretty(&render::sets_json(&cand, base.as_ref()))?);
    } else {
        print!("{}", render::sets_text(&cand, base.as_ref()));
    }
    Ok(render::exit_code(&cand, base.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lab::review_bench::BenchMode;

    #[test]
    fn bench_mode_picks_the_most_specific_condition_flag() {
        assert_eq!(bench_mode(false, false, false), BenchMode::Strict);
        assert_eq!(bench_mode(true, false, false), BenchMode::FreeForm);
        assert_eq!(bench_mode(false, true, false), BenchMode::Agentic);
        assert_eq!(bench_mode(false, false, true), BenchMode::Dialectic);
        assert_eq!(bench_mode(true, true, false), BenchMode::Agentic);
        assert_eq!(bench_mode(true, true, true), BenchMode::Dialectic);
    }

    #[test]
    fn verify_line_says_not_checked_rather_than_omitting_it() {
        use workloads::types::VerifyReport;
        let v = |passed, details: &str| VerifyReport { passed, details: details.to_string() };
        assert_eq!(verify_line(None), "not checked");
        assert_eq!(verify_line(Some(&v(true, "whatever"))), "ok");
        assert_eq!(verify_line(Some(&v(false, ""))), "FAILED");
        assert_eq!(verify_line(Some(&v(false, "missing ack"))), "FAILED — missing ack");
    }
}
