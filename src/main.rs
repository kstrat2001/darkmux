//! darkmux — a multiplexer for local LLM configurations.
//!
//! v0.1: profile registry, status, swap. Persists a small set of named
//! "stacks" (combinations of LMStudio models loaded at specified context
//! lengths) and lets the user atomically switch between them.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod lms;
mod profiles;
mod runtime;
mod swap;
mod types;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(
    name = "darkmux",
    version = VERSION,
    about = "Multiplexer for local LLM configurations"
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
}

fn main() -> Result<()> {
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
