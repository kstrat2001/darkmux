//! darkmux — a multiplexer + lab for local LLM configurations.
//!
//! v0.2 lab: workload-driven dispatches against a configured profile, with
//! per-run telemetry (turns, compactions, mode classification, wall-clock)
//! captured under `.darkmux/runs/<run-id>/`.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod lab;
mod lms;
mod profiles;
mod providers;
mod runtime;
mod swap;
mod types;
mod workloads;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(
    name = "darkmux",
    version = VERSION,
    about = "Multiplexer + lab for local LLM configurations"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Swap LMStudio + runtime config to a profile.
    Swap {
        profile: String,
        #[arg(long, short = 'c')]
        config: Option<String>,
        #[arg(long, short = 'n')]
        dry_run: bool,
        #[arg(long, short = 'q')]
        quiet: bool,
    },
    /// Show what's loaded and which profile (if any) it matches.
    Status {
        #[arg(long, short = 'c')]
        config: Option<String>,
    },
    /// List profiles in the registry.
    Profiles {
        #[arg(long, short = 'c')]
        config: Option<String>,
    },
    /// Lab subcommands.
    Lab {
        #[command(subcommand)]
        sub: LabCmd,
    },
}

#[derive(Subcommand)]
enum LabCmd {
    /// List available workloads.
    Workloads,
    /// Run a workload (one or more times).
    Run {
        workload: String,
        #[arg(long, short = 'p')]
        profile: Option<String>,
        #[arg(long, short = 'n', default_value = "1")]
        runs: u32,
        #[arg(long, short = 'c')]
        config: Option<String>,
        #[arg(long, short = 'q')]
        quiet: bool,
    },
    /// List recent runs (most recent first).
    Runs {
        /// Show at most N runs (default: 5).
        #[arg(long, short = 'l', default_value = "5")]
        limit: usize,
        /// Show all runs (overrides --limit).
        #[arg(long, short = 'a')]
        all: bool,
    },
    /// Inspect a previously-recorded run.
    Inspect {
        run: String,
        /// Also dump the full compaction summary text(s) the compactor model
        /// wrote during this run (read from trajectory.jsonl). Useful for
        /// methodology validation — confirming the compactor is producing
        /// substantive summaries rather than degenerate / empty output.
        #[arg(long)]
        summary: bool,
    },
    /// Compare two runs.
    Compare { run_a: String, run_b: String },
    /// Run an opinionated single-command characterization of the local setup.
    /// Dispatches a single workload (default `quick-q`) on the active profile
    /// and returns a one-screen verdict — wall clock, verify outcome, hint at
    /// next steps. The "QA my Mac" entry point.
    Characterize {
        /// Workload to dispatch (default: quick-q smoke prompt).
        #[arg(default_value = "quick-q")]
        workload: String,
        #[arg(long, short = 'p')]
        profile: Option<String>,
        #[arg(long, short = 'c')]
        config: Option<String>,
    },
    /// Multi-run distribution characterization with bimodal cluster detection.
    /// Run a workload N times on a profile, then report fast cluster / slow
    /// cluster / slow-rate. The bimodal model captures the variance shape of
    /// long-agentic dispatches better than a naive mean ± stdev would.
    Tune {
        workload: String,
        #[arg(long, short = 'p')]
        profile: Option<String>,
        /// Number of dispatches (default 6 — enough for a meaningful bimodal
        /// signal without burning hours on Apple Silicon).
        #[arg(long, short = 'n', default_value = "6")]
        runs: u32,
        #[arg(long, short = 'c')]
        config: Option<String>,
    },
}

fn main() -> Result<()> {
    providers::register_builtins()?;
    let cli = Cli::parse();
    let code = run(cli.command)?;
    std::process::exit(code);
}

fn run(cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Swap {
            profile,
            config,
            dry_run,
            quiet,
        } => cmd_swap(&profile, config.as_deref(), dry_run, quiet),
        Cmd::Status { config } => cmd_status(config.as_deref()),
        Cmd::Profiles { config } => cmd_profiles(config.as_deref()),
        Cmd::Lab { sub } => cmd_lab(sub),
    }
}

fn cmd_swap(profile_name: &str, config: Option<&str>, dry_run: bool, quiet: bool) -> Result<i32> {
    let loaded = profiles::load_registry(config)?;
    let profile = profiles::get_profile(&loaded.registry, profile_name)?;
    if !quiet {
        println!(
            "darkmux: swapping to \"{profile_name}\" (registry: {})",
            loaded.path.display()
        );
    }
    let result = swap::swap(profile, &loaded.registry, swap::SwapOpts { quiet, dry_run })?;
    if !quiet {
        let mut bits = vec![
            format!("done in {}ms", result.walltime_ms),
            format!("unloaded {}", result.unloaded.len()),
            format!("loaded {}", result.loaded.len()),
        ];
        if result.runtime_modified {
            bits.push("runtime patched".to_string());
        }
        if result.hooks_ran > 0 {
            bits.push(format!("{} hook(s)", result.hooks_ran));
        }
        if dry_run {
            bits.push("[DRY RUN]".to_string());
        }
        println!("{}", bits.join(" — "));
    }
    Ok(0)
}

fn cmd_status(config: Option<&str>) -> Result<i32> {
    let loaded = profiles::load_registry(config)?;
    let models = lms::list_loaded()?;
    println!("registry: {}", loaded.path.display());
    println!("loaded models ({}):", models.len());
    for m in &models {
        println!("  {:<40} ctx={:<8} {}", m.identifier, m.context, m.status);
    }
    let matches: Vec<&String> = loaded
        .registry
        .profiles
        .iter()
        .filter(|(_, p)| profile_matches(p, &models))
        .map(|(k, _)| k)
        .collect();
    if matches.is_empty() {
        println!("matches no registered profile");
    } else {
        let listed: Vec<&str> = matches.iter().map(|s| s.as_str()).collect();
        println!("matches profile(s): {}", listed.join(", "));
    }
    Ok(0)
}

fn profile_matches(profile: &types::Profile, loaded: &[types::LoadedModel]) -> bool {
    if profile.models.len() != loaded.len() {
        return false;
    }
    for m in &profile.models {
        let ident = m.identifier.clone().unwrap_or_else(|| m.id.clone());
        let Some(cur) = loaded.iter().find(|x| x.identifier == ident) else {
            return false;
        };
        if cur.context as u32 != m.n_ctx {
            return false;
        }
    }
    true
}

fn cmd_profiles(config: Option<&str>) -> Result<i32> {
    let loaded = profiles::load_registry(config)?;
    println!("registry: {}", loaded.path.display());
    for (name, profile) in &loaded.registry.profiles {
        let default_marker = if loaded.registry.default_profile.as_deref() == Some(name) {
            " (default)"
        } else {
            ""
        };
        println!("\n{}{}", name, default_marker);
        if let Some(desc) = profile.description.as_deref() {
            println!("  {desc}");
        }
        for m in &profile.models {
            let role = format!("{:?}", m.role).to_lowercase();
            println!("  - {:<10} {} @ ctx {}", role, m.id, m.n_ctx);
        }
    }
    Ok(0)
}

fn cmd_lab(sub: LabCmd) -> Result<i32> {
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
            config,
            quiet,
        } => {
            let outcomes = lab::run::lab_run(lab::run::RunOpts {
                workload_id: workload,
                profile_name: profile,
                runs,
                config_path: config,
                quiet,
            })?;
            if !quiet {
                println!("\n{} run(s) complete:", outcomes.len());
                for o in &outcomes {
                    println!("  {} — {}", o.run_id, o.notes.join(" | "));
                }
            }
            Ok(if outcomes.iter().all(|o| o.ok) { 0 } else { 1 })
        }
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
                let listed: Vec<String> = report
                    .tokens_before
                    .iter()
                    .map(|n| n.to_string())
                    .collect();
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
            config,
        } => {
            let report = lab::characterize::characterize(&lab::characterize::CharacterizeOpts {
                workload,
                profile,
                config,
            })?;
            lab::characterize::print_report(&report);
            Ok(if report.outcomes.iter().all(|o| o.ok) { 0 } else { 1 })
        }
        LabCmd::Tune {
            workload,
            profile,
            runs,
            config,
        } => {
            let report = lab::tune::tune(&lab::tune::TuneOpts {
                workload,
                profile,
                runs,
                config,
            })?;
            lab::tune::print_report(&report);
            Ok(if report.outcomes.iter().all(|o| o.ok) { 0 } else { 1 })
        }
    }
}
