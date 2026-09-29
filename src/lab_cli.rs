//! `darkmux lab` command handlers. `LabCmd` itself (the arg surface) lives in
//! `cli.rs`; this module owns the dispatch logic `cli::run` calls into. Each
//! `LabCmd` arm unpacks its arguments and hands them to one handler below;
//! `lab loop` (#986) lives in the `lab_loop` submodule.

mod lab_loop;

use anyhow::Result;

use crate::cli::{FixtureCmd, LabCmd, ProfilesFileArg, WorkloadCmd};
use crate::lab;

/// Whether a lab verb reads or writes run records under the lab dir, and so
/// must not run while the pre-4.0 runs are still un-moved. The workload
/// catalog, the fixture registry and `lab doctor` (a fixture health check)
/// never touch it.
fn touches_lab_dir(sub: &LabCmd) -> bool {
    match sub {
        LabCmd::Workload { .. } | LabCmd::Fixture { .. } | LabCmd::Doctor => false,
        LabCmd::Run { .. }
        | LabCmd::Eval { .. }
        | LabCmd::Loop { .. }
        | LabCmd::Characterize { .. }
        | LabCmd::Tune { .. } => true,
    }
}

pub(crate) fn cmd_lab(sub: LabCmd) -> Result<i32> {
    if touches_lab_dir(&sub) {
        darkmux_types::config_access::require_current_lab_dir()?;
    }
    match sub {
        LabCmd::Workload { sub } => cmd_lab_workload(sub),
        LabCmd::Run {
            workload,
            profile,
            runs,
            profiles: ProfilesFileArg { profiles },
            quiet,
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

/// `lab run <workload>`: dispatch a workload `runs` times. The launcher only;
/// reading recorded runs is `darkmux run`.
fn cmd_lab_run_dispatch(
    workload: String,
    profile: Option<String>,
    runs: u32,
    profiles: Option<String>,
    quiet: bool,
) -> Result<i32> {
    let outcomes = signal_aware(|| {
        lab::run::lab_run(lab::run::RunOpts {
            workload_id: workload,
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

#[cfg(test)]
mod tests {
    use super::*;
    use lab::review_bench::BenchMode;

    /// `lab doctor` is the fixture-registry health check and never touches
    /// the lab dir, so a pending move must not make it refuse.
    #[serial_test::serial]
    #[test]
    fn lab_doctor_runs_while_a_lab_dir_move_is_pending() {
        let home = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join("runs").join("quick-q-1")).unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", home.path()) };
        let pending = darkmux_types::config_access::require_current_lab_dir();
        let doctor = cmd_lab(LabCmd::Doctor);
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
        assert!(pending.is_err(), "premise: the move is pending in the scratch home");
        assert!(doctor.is_ok(), "lab doctor must not be gated: {:?}", doctor.err());
    }

    #[test]
    fn only_verbs_that_read_or_write_run_records_are_gated_on_the_lab_dir() {
        let characterize = LabCmd::Characterize {
            workload: "quick-q".into(),
            profile: None,
            profiles: ProfilesFileArg { profiles: None },
        };
        assert!(touches_lab_dir(&characterize), "a dispatching verb writes run records");
        assert!(!touches_lab_dir(&LabCmd::Doctor), "fixture health check never touches the lab dir");
    }

    #[test]
    fn bench_mode_picks_the_most_specific_condition_flag() {
        assert_eq!(bench_mode(false, false, false), BenchMode::Strict);
        assert_eq!(bench_mode(true, false, false), BenchMode::FreeForm);
        assert_eq!(bench_mode(false, true, false), BenchMode::Agentic);
        assert_eq!(bench_mode(false, false, true), BenchMode::Dialectic);
        assert_eq!(bench_mode(true, true, false), BenchMode::Agentic);
        assert_eq!(bench_mode(true, true, true), BenchMode::Dialectic);
    }
}
