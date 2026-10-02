//! `darkmux lab` command handlers. `LabCmd` itself (the arg surface) lives in
//! `cli.rs`; this module owns the dispatch logic `cli::run` calls into. Each
//! `LabCmd` arm unpacks its arguments and hands them to one handler below;
//! `lab loop` (#986) lives in the `lab_loop` submodule.

pub(crate) mod lab_loop;

use anyhow::Result;

use crate::cli::{FixtureCmd, LabCmd, ProfilesFileArg, WorkloadCmd};
use crate::lab;

pub(crate) fn cmd_lab(sub: LabCmd) -> Result<i32> {
    match sub {
        LabCmd::Workload { sub } => cmd_lab_workload(sub),
        LabCmd::Run {
            workload,
            profile,
            repeat,
            profiles: ProfilesFileArg { profiles },
            quiet,
        } => cmd_lab_run_dispatch(workload, profile, repeat, profiles, quiet),
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
            repeat,
            profiles: ProfilesFileArg { profiles },
        } => cmd_lab_tune(lab::tune::TuneOpts {
            workload,
            profile,
            runs: repeat,
            config: profiles,
        }),
        LabCmd::Fixture { sub } => cmd_lab_fixture(sub),
        LabCmd::Doctor => cmd_lab_doctor(),
    }
}

/// Run a lab verb that dispatches through `lab_run` under signal handling: the one exit path every such verb
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
            println!("  {}: {}", o.run_id, o.notes.join(" | "));
        }
    }
    Ok(lab::run::exit_code(&outcomes))
}

/// `lab characterize`: a single `lab_run`, reported.
fn cmd_lab_characterize(opts: lab::characterize::CharacterizeOpts) -> Result<i32> {
    let report = signal_aware(|| lab::characterize::characterize(&opts))?;
    lab::characterize::print_report(&report);
    Ok(lab::run::exit_code(&report.outcomes))
}

/// `lab tune`: `lab_run` with `--repeat N`, reported as a distribution.
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
