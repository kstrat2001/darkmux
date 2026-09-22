//! `darkmux lab` command handlers — extracted from `main.rs` (mechanical,
//! zero behavior change) alongside the `fleet_cli`/`cli` split. `LabCmd`
//! itself (the arg surface) lives in `cli.rs`; this module owns the
//! dispatch logic `cli::run` calls into, plus the `lab loop` (#986)
//! single-run bench that used to live directly below `cmd_lab` in
//! `main.rs`.

use anyhow::Result;

use crate::cli::{FixtureCmd, LabCmd, RunCmd, WorkloadCmd};
use crate::lab;
use crate::workloads;

pub(crate) fn cmd_lab(sub: LabCmd) -> Result<i32> {
    match sub {
        // (#1465) `lab workload list` — the retired flat `lab workloads` leaf,
        // now the sole member of the `workload` kind-family.
        LabCmd::Workload { sub } => match sub {
            WorkloadCmd::List => {
                // (#2553 cleanup) No empty-list branch: `list_available`
                // unconditionally inserts every `EMBEDDED_WORKLOADS` id, a
                // fixed non-empty compiled-in set, so `lab_workloads()` can
                // never return empty — the branch that used to print
                // "no workloads found — check templates/builtin/workloads/"
                // was dead code, and doubly so after this PR dropped the
                // cwd-relative `templates/builtin/workloads/` search that
                // path's own text referred to.
                for id in lab::run::lab_workloads() {
                    println!("{id}");
                }
                Ok(0)
            }
        },
        // (#1465) `lab run` takes EITHER a workload positional (dispatch) OR a
        // run sub-verb (list/inspect/compare — the retired flat `lab runs`/
        // `lab inspect`/`lab compare` leaves). `args_conflicts_with_subcommands`
        // guarantees the two forms never mix.
        LabCmd::Run {
            workload,
            profile,
            runs,
            profiles: crate::cli::ProfilesFileArg { profiles },
            quiet,
            sub,
        } => match sub {
            Some(run_sub) => cmd_lab_run_sub(run_sub),
            None => {
                let workload_id = workload.ok_or_else(|| {
                    anyhow::anyhow!(
                        "specify a workload to dispatch (`lab run <workload>`) or a run \
                         sub-verb (`lab run list` / `lab run inspect <id>` / \
                         `lab run compare <a> <b>`)"
                    )
                })?;
                // (#2262) `lab run` installed no signal handling at all — the
                // same gap #2131 closed for every `mission launch` launcher.
                // Without `arm()`, a caught SIGTERM/SIGINT/SIGHUP kills this
                // process via the OS default disposition: no unwind, no
                // `Drop`, the docker container (or curl child, for a
                // tool-less hosted role/profile) orphaned. `lab::run::
                // lab_run` already writes an explicit terminal
                // `lifecycle.json` on ANY dispatch `Err` (see
                // `RunLifecycle`'s own doc + the `lifecycle.finish_error`/
                // `finish_interrupted` calls in `run.rs`, #2462), and
                // `dispatch_internal.rs`'s own `DispatchBookendGuard` already
                // guarantees a `dispatch.error` liveness bookend — so the
                // only two things actually missing are: (1) install the
                // handlers so a signal becomes a flag instead of an outright
                // kill, and (2) something to notice that flag and kill the
                // blocked child. Armed ONCE, ahead of the whole (possibly
                // `--runs N`) dispatch loop below — `is_set()` never resets,
                // so one signal ends the whole invocation, matching every
                // other launcher's shape. See `spawn_reap_watchdog`'s own doc
                // for why the docker path is already self-killing and this
                // watchdog exists for the curl-only remote path.
                crate::launch_guard::arm();
                let _reap_watchdog = crate::launch_guard::spawn_reap_watchdog();
                let outcomes = match lab::run::lab_run(lab::run::RunOpts {
                    workload_id,
                    profile_name: profile,
                    runs,
                    config_path: profiles,
                    quiet,
                    loop_override: None,
                    inject_context: None,
                }) {
                    Ok(o) => o,
                    Err(e) => {
                        // (#2462) `lab_run`'s own terminal `lifecycle.json`
                        // write is already durable by the time it returns
                        // this `Err` (the RAII guard finalizes before
                        // propagating — see `run.rs`'s match arm). Matching
                        // `mission launch`'s own shape
                        // (`reap_and_exit_on_signal`'s doc), a run a signal
                        // actually ended now exits 130 instead of the
                        // default-error 1 — so a wrapper script can tell
                        // "the operator stopped this" from the exit code
                        // alone, the same way it already can for `mission
                        // launch`. A no-op (falls through to the ordinary
                        // `Err` return below) when no signal was ever
                        // observed.
                        //
                        // (#2462 review) `report_*`, NOT the bare
                        // `reap_and_exit_on_signal`: the force-exit runs
                        // before `main`'s own error printing, so a bare
                        // call exits 130 having discarded the interrupt
                        // message this change exists to produce. See that
                        // function's doc for the measurement, and for why
                        // this site keeps the hard exit where
                        // `radio_cli.rs` dropped it.
                        crate::launch_guard::report_reap_and_exit_on_signal(&e);
                        return Err(e);
                    }
                };
                if !quiet {
                    println!("\n{} run(s) complete:", outcomes.len());
                    for o in &outcomes {
                        println!("  {} — {}", o.run_id, o.notes.join(" | "));
                    }
                }
                // (#2494) Gate on BOTH the dispatch path and the workload's
                // own verify. `o.ok` alone meant `darkmux lab run <w> &&
                // echo PASS` printed PASS on a run whose tests failed — the
                // most machine-readable green available, and the one CI
                // keys on. `verify_passed == Some(false)` is a failure; a
                // `None` (nothing declared a verify) is not.
                let all_ok = outcomes
                    .iter()
                    .all(|o| o.ok && o.verify_passed != Some(false));
                Ok(if all_ok { 0 } else { 1 })
            }
        },
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
        } => {
            // (#2463) `lab eval` dispatches one internal-runtime call per
            // case (`darkmux_crew::dispatch`, the same `dispatch()`
            // primitive `darkmux dispatch` uses) in a plain loop with no
            // signal handling at all — the #2262 gap, unfixed here. Each
            // dispatch already gets its own `dispatch.error` bookend from
            // `DispatchBookendGuard`, and the docker path already
            // self-kills on a caught signal via the trajectory tailer's
            // `interrupt::is_set()` poll — so, same as `dispatch`/`lab
            // run`, the only two things missing are (1) installing the
            // handlers so SIGTERM/SIGINT/SIGHUP become a flag instead of
            // an outright kill, and (2) the watchdog that kills a
            // registered child for a role that resolves to the tool-less
            // remote `curl` path (no poll seam of its own). No new
            // finalize/envelope guard — a run killed mid-corpus loses only
            // the cases not yet scored, same as `Ctrl-C`-before-#2463 did;
            // `scores.json` still isn't written until the loop completes.
            crate::launch_guard::arm();
            let _reap_watchdog = crate::launch_guard::spawn_reap_watchdog();
            lab::review_bench::run_review_bench(lab::review_bench::ReviewBenchOpts {
                role,
                cases_dir: std::path::PathBuf::from(cases_dir),
                profile_name: profile,
                config_path: profiles,
                timeout_seconds: timeout,
                scores_out,
                mode: if dialectic {
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
                roster_profile,
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
        LabCmd::Characterize {
            workload,
            profile,
            profiles: crate::cli::ProfilesFileArg { profiles },
        } => {
            // (#2463) `characterize()` is a thin wrapper over `lab_run`
            // (single run) — the exact `lab_run` gap `LabCmd::Run` above
            // was already fixed for in #2262. Same fix, same reasoning:
            // `lab_run` already writes a terminal `lifecycle.json` on any
            // dispatch `Err`, so only the handlers + curl-path watchdog
            // are missing.
            crate::launch_guard::arm();
            let _reap_watchdog = crate::launch_guard::spawn_reap_watchdog();
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
            // (#2463) `tune()` is `lab_run` with `--runs N` — same gap,
            // same fix as `LabCmd::Run`/`LabCmd::Characterize` above.
            // Armed ONCE ahead of the whole multi-run loop (`lab_run`
            // itself loops over `runs`), matching `LabCmd::Run`'s own
            // placement.
            crate::launch_guard::arm();
            let _reap_watchdog = crate::launch_guard::spawn_reap_watchdog();
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
        // (#1465) `lab fixture list|register|unregister` — the retired flat
        // `lab fixtures`/`lab register`/`lab unregister` leaves folded into the
        // `fixture` kind-family.
        LabCmd::Fixture { sub } => match sub {
            FixtureCmd::List => {
                let msg = lab::fixture_cli::cmd_list()?;
                println!("{msg}");
                Ok(0)
            }
            FixtureCmd::Register {
                path,
                name,
                force,
                if_absent,
            } => {
                let msg = lab::fixture_cli::cmd_register(&path, name, force, if_absent)?;
                println!("{msg}");
                Ok(0)
            }
            FixtureCmd::Unregister { name } => {
                let msg = lab::fixture_cli::cmd_unregister(&name)?;
                println!("{msg}");
                Ok(0)
            }
        },
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

/// (#1465) The `lab run` sub-verbs — list/inspect/compare recorded runs.
/// Split out of `cmd_lab` when the flat `lab runs`/`lab inspect`/`lab compare`
/// leaves folded into the `run` kind-family; the handler bodies are the
/// pre-#1465 arm bodies verbatim, zero behavior change.
fn cmd_lab_run_sub(sub: RunCmd) -> Result<i32> {
    match sub {
        RunCmd::List { limit, all } => {
            let lim = if all { None } else { Some(limit) };
            let summaries = lab::list::list_runs(lim)?;
            print!(
                "{}",
                lab::list::format_table(&summaries, &darkmux_types::config_access::lab_dir())
            );
            Ok(0)
        }
        RunCmd::Inspect { run, summary } => {
            let report = lab::inspect::lab_inspect(&run)?;
            println!("run:         {}", report.run_id);
            println!("workload:    {}", report.workload_id);
            println!("wall:        {}s", report.walltime_ms / 1000);
            // (#2094 finding 7) Shown next to wall — a rested run's wall
            // clock must never be misread as a slow model. Milliseconds,
            // not truncated-to-integer-seconds: `rest_ms` is small enough
            // relative to typical dispatch walltimes that a seconds
            // display can round a real, knob-driven rest down to "0s" and
            // read as if no rest happened at all. `0` when the run
            // predates the feature or took no rests; not gated on
            // verify/mode outcome — a failed or Slow-classified run shows
            // its rest exactly the same as a clean Fast one whenever it's
            // known (populated straight off `report.rest_ms`, which
            // `CodingTaskProvider::inspect` fills best-effort regardless
            // of the run's own verdict).
            if report.rest_ms > 0 {
                println!("rest:        {}ms", report.rest_ms);
            }
            println!("turns:       {}", report.turns);
            println!("compactions: {}", report.compactions);
            // (#2494) The workload's OWN result, distinct from the dispatch
            // path's `ok`. Printed unconditionally when known so a failed
            // verify cannot be missed; the "not checked" case says so in
            // those words rather than being silently omitted, which would
            // read as a pass.
            match &report.verify {
                Some(v) if v.passed => println!("verify:      ok"),
                Some(v) if v.details.is_empty() => println!("verify:      FAILED"),
                Some(v) => println!("verify:      FAILED — {}", v.details),
                None => println!("verify:      not checked"),
            }
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
        RunCmd::Stats { runs, baseline, json } => {
            if runs.len() == 1 && baseline.is_empty() {
                let s = lab::stats::run_stats(&runs[0])?;
                if json.json {
                    println!("{}", serde_json::to_string_pretty(&s)?);
                } else {
                    render_stats(&s);
                }
                return Ok(0);
            }
            let cand = load_stats_set(&runs);
            let base = (!baseline.is_empty()).then(|| load_stats_set(&baseline));
            if json.json {
                let set_json = |set: &StatsSet| {
                    serde_json::json!({
                        "runs": set.runs,
                        "summary": lab::stats_set::summarize(&set.runs),
                        "errors": set.errors,
                    })
                };
                let mut out = set_json(&cand);
                if let Some(b) = &base {
                    out["baseline"] = set_json(b);
                }
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                render_stats_sets(&cand, base.as_ref());
            }
            // A run that could not be read is reported, and makes the exit
            // non-zero, so a script cannot mistake a partial set for a whole one.
            let errored = !cand.errors.is_empty() || base.as_ref().is_some_and(|b| !b.errors.is_empty());
            Ok(if errored { 1 } else { 0 })
        }
        RunCmd::Compare { run_a, run_b } => {
            let result = lab::compare::lab_compare(&run_a, &run_b)?;
            for n in &result.notes {
                println!("{n}");
            }
            Ok(0)
        }
    }
}

/// (#2855) The human read of a run's derived metrics.
///
/// Two rules the layout exists to enforce. **Rest is printed beside wall and
/// active, never alone**, so a rested run's wall clock cannot be misread as a
/// slow model. And **the caveats print last and unconditionally** — a figure
/// whose reconciliation check failed is still shown, because hiding it would
/// lose the evidence, but no run's numbers can be copied out of here without
/// the reasons they may not be quoted appearing in the same block.
fn render_stats(s: &darkmux_lab::lab::stats::RunStats) {
    let secs = |ms: u64| ms as f64 / 1000.0;
    println!("run:         {}", s.run);
    if let Some(m) = &s.model {
        println!("model:       {m}");
    }
    println!(
        "result:      {}{}",
        s.result.as_deref().unwrap_or("?"),
        s.verify.as_deref().map(|v| format!("   verify: {v}")).unwrap_or_default()
    );
    println!();

    println!(
        "time         wall {:.0}s   rest {:.0}s   active {:.0}s",
        secs(s.wall_ms),
        secs(s.rest_ms),
        secs(s.active_ms)
    );
    if s.rest_events > 0 {
        // The distinct delays, not a mean: more than one value means the
        // thermal ratchet doubled the delay mid-run.
        println!(
            "             {} rests, delays {:?}ms{}",
            s.rest_events,
            s.rest_delays_ms,
            if s.thermal_ratchet_fired { "  (ratchet fired)" } else { "" }
        );
    }
    println!(
        "work         {} turns   {} compactions   {} tool calls ({} failed)",
        s.turns, s.compactions, s.tool_calls_total, s.tool_calls_failed
    );
    println!(
        "output       {} completion tokens   {} reasoning chars   {} content chars",
        s.completion_tokens, s.reasoning_chars, s.content_chars
    );
    match (s.tok_per_s, s.billed_gen_fraction) {
        (Some(t), Some(f)) => println!(
            "throughput   {t} tok/s over {:.0}% of generation ({:.0}s of {:.0}s)",
            f * 100.0,
            secs(s.gen_ms_billed),
            secs(s.gen_ms_all)
        ),
        _ => println!("throughput   (no billed generation recorded)"),
    }

    // Both gates, always both, on their own lines. One line for "detection"
    // is what let a reader take the checkpoint gate's silence for the whole
    // answer.
    let g = &s.gates;
    let dp = darkmux_lab::lab::stats::TAIL_RATIO_DISPLAY_DP;
    let ratio = |r: Option<f64>| {
        r.map(|r| format!("   min ratio {:.*}", dp, r)).unwrap_or_default()
    };
    println!(
        "stream gate  {} observations   {} degenerate   {} aborts{}",
        g.stream.observations,
        g.stream.degenerate_turns.len(),
        g.stream.aborts,
        ratio(g.stream.min_tail_ratio)
    );
    println!(
        "checkpoint   {} observations   {} degenerate   {} cut{}{}",
        g.checkpoint.observations,
        g.checkpoint.degenerate_turns.len(),
        g.checkpoint.concluded_turns.len(),
        ratio(g.checkpoint.min_tail_ratio),
        g.checkpoint.policy.as_deref().map(|p| format!("   policy={p}")).unwrap_or_default()
    );

    if let (Some(gpu), Some(cpu), Some(pkg)) = (s.gpu_w_busy, s.cpu_w_busy, s.pkg_w_busy) {
        println!(
            "power        gpu {gpu} W   cpu {cpu} W   package {pkg} W   busy {}% of the run ({} samples)",
            s.gpu_duty_pct.unwrap_or(0.0),
            s.samples_busy + s.samples_idle
        );
        if let Some(j) = s.pkg_j_per_1k_tokens {
            println!(
                "energy       {j} J per 1k tokens{}",
                s.pkg_j_busy.map(|t| format!("   {:.1} kJ over the run", t / 1000.0)).unwrap_or_default()
            );
        }
        if !s.thermal_states_busy.is_empty() {
            let states: Vec<String> =
                s.thermal_states_busy.iter().map(|(k, v)| format!("{k} {v}")).collect();
            println!(
                "thermal      {}   cpu speed limit min {}%",
                states.join(", "),
                s.cpu_speed_limit_min.unwrap_or(100)
            );
        }
    }

    let caveats = s.unreconciled();
    if !caveats.is_empty() {
        println!();
        println!("not reconciled, so do not quote these figures without saying so:");
        for c in &caveats {
            println!("  - {c}");
        }
    }
}

/// Runs loaded for a set view, and the ones that could not be.
struct StatsSet {
    runs: Vec<darkmux_lab::lab::stats::RunStats>,
    /// `(run, error)`. Never silently dropped: a set missing a run it was
    /// asked for reads as a different arm.
    errors: Vec<(String, String)>,
}

fn load_stats_set(ids: &[String]) -> StatsSet {
    let mut set = StatsSet { runs: Vec::new(), errors: Vec::new() };
    for id in ids {
        match lab::stats::run_stats(id) {
            Ok(s) => set.runs.push(s),
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

/// `median (min–max)`, the only form a set figure is printed in.
fn fmt_range(r: Option<darkmux_lab::lab::stats_set::Range>, f: &dyn Fn(f64) -> String) -> String {
    match r {
        Some(r) if r.min == r.max => f(r.median),
        Some(r) => format!("{} ({}–{})", f(r.median), f(r.min), f(r.max)),
        None => "-".into(),
    }
}

/// (#2855) One row per run. Every row prints whatever its checks say, with
/// the failed ones as flags on the same line: dropping a row that did not
/// reconcile would be choosing the answer.
fn render_stats_table(set: &StatsSet) {
    use darkmux_lab::lab::stats_set::flags;
    let w = set.runs.iter().map(|s| s.run.len()).chain(set.errors.iter().map(|(r, _)| r.len())).max().unwrap_or(3).max(3);
    println!(
        "{:<w$}  {:>6} {:>7} {:>6} {:>5} {:>7} {:>6} {:>7} {:>4} {:>6} {:>5} {:>7}  flags",
        "run", "verify", "active", "rest", "turns", "tok/s", "billed", "tokens", "cuts", "pkgW", "duty", "J/1ktok"
    );
    for s in &set.runs {
        let f = flags(s);
        println!(
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
        println!("{r:<w$}  NOT READ: {e}");
    }
}

/// (#2855) The set view, and with a baseline, the comparison.
fn render_stats_sets(cand: &StatsSet, base: Option<&StatsSet>) {
    use darkmux_lab::lab::stats_set::{ratio, summarize, SetSummary};
    let c = summarize(&cand.runs);
    let b = base.map(|b| summarize(&b.runs));

    if let Some(bs) = base {
        println!("baseline");
        render_stats_table(bs);
        println!();
        println!("candidate");
    }
    render_stats_table(cand);
    println!();

    let secs = |v: f64| fmt_secs(v);
    let one = |v: f64| format!("{v:.1}");
    let pct = |v: f64| format!("{:.0}%", v * 100.0);
    let int = |v: f64| format!("{v:.0}");
    type Row<'a> = (&'a str, fn(&SetSummary) -> Option<darkmux_lab::lab::stats_set::Range>, &'a dyn Fn(f64) -> String);
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
            println!("set          {}   models: {}", outcome(&c), c.models.join(", "));
            for (name, get, f) in &rows {
                println!("  {name:<16} {}", fmt_range(get(&c), f));
            }
            println!(
                "  {:<16} {} runs with degeneracy, {} turns cut",
                "detection", c.runs_with_degeneracy, c.turns_cut
            );
            println!("cost per successful run");
            for (name, get, f) in &cost {
                println!("  {name:<16} {}", get(&c).map(f).unwrap_or_else(|| "-".into()));
            }
        }
        Some(b) => {
            let col = 26.max(b.models.join(", ").len() + 2);
            println!("{:<18} {:<col$} {:<col$} moved", "", "baseline", "candidate");
            println!("{:<18} {:<col$} {:<col$}", "outcome", outcome(b), outcome(&c));
            println!("{:<18} {:<col$} {:<col$}", "models", b.models.join(", "), c.models.join(", "));
            for (name, get, f) in &rows {
                let moved = ratio(get(&c).map(|r| r.median), get(b).map(|r| r.median))
                    .map(|x| format!("{x:.2}x"))
                    .unwrap_or_default();
                println!("{name:<18} {:<col$} {:<col$} {moved}", fmt_range(get(b), f), fmt_range(get(&c), f));
            }
            println!(
                "{:<18} {:<col$} {:<col$}",
                "degeneracy",
                format!("{} runs, {} cuts", b.runs_with_degeneracy, b.turns_cut),
                format!("{} runs, {} cuts", c.runs_with_degeneracy, c.turns_cut)
            );
            println!("cost per successful run (every run's cost, divided by the runs that passed)");
            for (name, get, f) in &cost {
                let show = |s: &SetSummary| get(s).map(f).unwrap_or_else(|| "-".into());
                let moved = ratio(get(&c), get(b)).map(|x| format!("{x:.2}x")).unwrap_or_default();
                println!("  {name:<16} {:<col$} {:<col$} {moved}", show(b), show(&c));
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
    let errors = cand.errors.len() + base.map_or(0, |b| b.errors.len());
    if errors > 0 {
        notes.push(format!("{errors} run(s) could not be read and are not in the figures above"));
    }
    if !notes.is_empty() {
        println!();
        println!("read before quoting:");
        for n in &notes {
            println!("  - {n}");
        }
        println!("  (flag meanings: `darkmux lab run stats <run>` explains a single run's failed checks)");
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

    // (#2463) `lab loop` calls `lab_run` (via `run_arm` below) either once
    // or twice (the `--ab` baseline/treatment pair) — same underlying gap
    // #2262 fixed for `LabCmd::Run`, unfixed here. Armed ONCE, ahead of
    // both the single-run and `--ab` two-run shapes, matching `LabCmd::
    // Run`'s own placement ahead of ITS (possibly `--runs N`) loop —
    // `lab_run` already writes a terminal `lifecycle.json` on any dispatch
    // `Err`, so the handlers + curl-path watchdog are the only things
    // missing.
    crate::launch_guard::arm();
    let _reap_watchdog = crate::launch_guard::spawn_reap_watchdog();

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
        let ctx = crate::coder_phase::injected_context_for_lab(
            args.inject_from_mission.as_deref(),
            &ws,
            args.profile.as_deref(),
            args.profiles.as_deref(),
        );
        if ctx.trim().is_empty() {
            anyhow::bail!(
                "--ab: nothing to inject — no authored lessons for this repo{}. \
                 Record a lesson (`darkmux memory lesson add`){}, then retry.",
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
