//! darkmux — a lab and multiplexer for local LLM configurations.
//!
//! v0.2 in Rust. Ports the v0.1 TS prototype + the v0.2 lab foundation.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod agent_roles;
mod doctor;
mod eureka;
mod hardware;
mod heuristics;
mod init;
mod lab;
mod lms;
mod notebook;
mod optimize;
mod profiles;
mod providers;
mod runtime;
mod skills;
mod swap;
mod types;
mod workloads;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(name = "darkmux", version = VERSION, about = "Lab and multiplexer for local LLM configurations")]
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
    /// Manage agent-invokable skills bundled with darkmux.
    Skills {
        #[command(subcommand)]
        sub: SkillsCmd,
    },
    /// Notebook commands — agent-as-scribe for lab notebook entries.
    Notebook {
        #[command(subcommand)]
        sub: NotebookCmd,
    },
    /// Run pre-flight diagnostic checks. Verifies the local setup (profile
    /// registry, LMStudio, models, runtime, RAM, power) and reports
    /// pass/warn/fail with actionable hints. Exit 0 if no failures, else 1.
    Doctor,
    /// Scan the LMStudio model catalog for downloaded models that aren't yet
    /// covered by any profile. For each uncovered model, suggests a task class
    /// and rough memory impact. Run after downloading a new model in LMStudio
    /// to see whether you'd want to define a profile for it.
    Scan {
        #[arg(long, short = 'c')]
        config: Option<String>,
    },
    /// Profile management subcommands.
    Profile {
        #[command(subcommand)]
        sub: ProfileCmd,
    },
    /// Agent-role template subcommands. Browse + emit validated
    /// `systemPromptOverride` scaffolds for common roles (qa, scribe,
    /// engineer). Output is print-only — paste into your runtime config
    /// (`agents.list[]` in openclaw.json) yourself.
    Agent {
        #[command(subcommand)]
        sub: AgentCmd,
    },
    /// Optimize for your workload — guided wizard (Phase 1 scaffold).
    /// Composes scan, lab characterize/tune, heuristics, and eureka rules
    /// into an opinionated optimization loop.
    Optimize,
    /// One-command setup: install skills, optionally add session-start hook
    /// and CLAUDE.md integration so Claude Code knows about darkmux.
    Init {
        /// Add a SessionStart hook to ~/.claude/settings.json that runs
        /// `darkmux status` so Claude sees the current stack at session start.
        #[arg(long)]
        with_hook: bool,
        /// Append a darkmux integration section to the given CLAUDE.md.
        /// Use `~/.claude/CLAUDE.md` for global, or a project-relative path.
        #[arg(long)]
        with_claude_md: Option<std::path::PathBuf>,
        /// Overwrite existing skills / hook entries.
        #[arg(long, short = 'f')]
        force: bool,
        /// Show what would be installed without writing.
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum AgentCmd {
    /// List the role-template ids darkmux ships scaffolds for.
    ListTemplates,
    /// Emit a JSON snippet for `agents.list[]` for the given role.
    /// Print-only — paste into your runtime config yourself.
    Template {
        /// Role id (qa | scribe | engineer). Run `agent list-templates` to see what's available.
        role: String,
    },
}

#[derive(Subcommand)]
enum ProfileCmd {
    /// Generate a starter profile JSON for a model + task class. Output is
    /// printed to stdout — copy-paste into your `~/.darkmux/profiles.json`
    /// (or pipe into a file) and tune from there.
    Draft {
        /// Profile name to use as the JSON key (e.g. "phi-fast").
        name: String,
        /// LMStudio modelKey for the primary. Run `lms ls` to see ids.
        #[arg(long, short = 'm')]
        model: String,
        /// Task class: `fast` (single-turn), `mid` (balanced), `long` (deep agentic).
        #[arg(long, short = 't', default_value = "mid")]
        task_class: String,
        /// Required when the model isn't in `lms ls` (i.e., not yet
        /// downloaded). Format: "4B", "13B", "35B", etc. Without this flag,
        /// drafting an unknown model would silently produce a Tiny-bucket
        /// profile (32K, no compactor) regardless of the model's real size.
        #[arg(long)]
        params: Option<String>,
        /// Override max context length (in tokens). Useful when the model
        /// isn't in `lms ls` and you want a draft that doesn't cap at the
        /// 32K default. Pair with --params for tight heuristics.
        #[arg(long)]
        max_ctx: Option<u32>,
    },
}

#[derive(Subcommand)]
enum NotebookCmd {
    /// Draft a notebook entry from a recorded run via the active agent.
    Draft {
        run_id: String,
        /// Agent to dispatch the drafting prompt to (default: "main").
        #[arg(long, default_value = "main")]
        agent: String,
        /// Override the entry's filename slug (default derived from workload + run id).
        #[arg(long)]
        slug: Option<String>,
        /// Build the prompt and target filename without dispatching the agent.
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum SkillsCmd {
    /// Copy bundled skills into a Claude Code (or compatible) skills dir.
    Install {
        /// Target dir (default: ~/.claude/skills/darkmux/).
        #[arg(long)]
        target: Option<std::path::PathBuf>,
        /// Overwrite existing SKILL.md files.
        #[arg(long, short = 'f')]
        force: bool,
        /// Show what would be installed without writing.
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
    /// List currently-installed skills under the target dir.
    List {
        #[arg(long)]
        target: Option<std::path::PathBuf>,
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
        /// Capture cross-layer telemetry during the dispatch. Writes
        /// `instruments.jsonl` to the run dir with periodic samples of
        /// LMStudio state and gateway-process residency. Useful for
        /// "trust-but-verify" — confirming what the stack was actually
        /// doing, beyond the runtime's self-report. No root required.
        #[arg(long)]
        instrument: bool,
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
        Cmd::Swap { profile, config, dry_run, quiet } => cmd_swap(&profile, config.as_deref(), dry_run, quiet),
        Cmd::Status { config } => cmd_status(config.as_deref()),
        Cmd::Profiles { config } => cmd_profiles(config.as_deref()),
        Cmd::Lab { sub } => cmd_lab(sub),
        Cmd::Skills { sub } => cmd_skills(sub),
        Cmd::Notebook { sub } => cmd_notebook(sub),
        Cmd::Doctor => cmd_doctor(),
        Cmd::Scan { config } => cmd_scan(config.as_deref()),
        Cmd::Profile { sub } => cmd_profile(sub),
        Cmd::Agent { sub } => cmd_agent(sub),
        Cmd::Init {
            with_hook,
            with_claude_md,
            force,
            dry_run,
        } => cmd_init(with_hook, with_claude_md, force, dry_run),
        Cmd::Optimize => optimize::run(),
    }
}

fn cmd_notebook(sub: NotebookCmd) -> Result<i32> {
    match sub {
        NotebookCmd::Draft {
            run_id,
            agent,
            slug,
            dry_run,
        } => {
            let report = notebook::draft_entry(&notebook::DraftOptions {
                run_id,
                agent,
                slug,
                dry_run,
            })?;
            println!("source run: {}", report.run_dir.display());
            println!("entry path: {}", report.entry_path.display());
            println!("prompt chars: {}", report.prompt_chars);
            println!("reply chars:  {}", report.reply_chars);
            if dry_run {
                println!("[DRY RUN — nothing was written]");
            }
            Ok(0)
        }
    }
}

fn cmd_doctor() -> Result<i32> {
    let report = doctor::run();
    doctor::print_report(&report)?;
    Ok(match report.worst_status() {
        doctor::Status::Fail => 1,
        _ => 0,
    })
}

/// Surface LMStudio models not yet covered by any profile, with task-class
/// hints and a one-liner reason per model. Helps a user discover that a
/// freshly-downloaded model could be added to the registry.
fn cmd_scan(config: Option<&str>) -> Result<i32> {
    // Distinguish "no registry yet" (silent empty — fresh user) from
    // "registry exists but failed to parse / validate" (warn loudly so the
    // user knows their registry is broken — silent fallthrough would
    // misleadingly flag every loaded model as uncovered).
    let registry_loaded = match profiles::load_registry(config) {
        Ok(r) => Some(r),
        Err(e) => {
            let msg = e.to_string();
            // Heuristic: "not found" / "no profile registry" → first-run case;
            // anything else is a real load failure worth surfacing.
            if msg.contains("no profile registry") {
                None
            } else {
                eprintln!("warning: profile registry could not be loaded — {msg}");
                eprintln!("         continuing as if no profiles are defined.");
                None
            }
        }
    };
    let covered: std::collections::HashSet<String> = match registry_loaded.as_ref() {
        Some(r) => r
            .registry
            .profiles
            .values()
            .flat_map(|p| p.models.iter().map(|m| m.id.clone()))
            .collect(),
        None => std::collections::HashSet::new(),
    };

    let available = lms::list_available()?;
    let llms: Vec<&lms::ModelMeta> = available
        .iter()
        .filter(|m| m.model_type == "llm")
        .collect();

    let uncovered: Vec<&lms::ModelMeta> = llms
        .iter()
        .filter(|m| !covered.contains(&m.model_key))
        .copied()
        .collect();

    println!(
        "darkmux scan — {} model(s) in LMStudio, {} not yet in any profile",
        llms.len(),
        uncovered.len()
    );
    if uncovered.is_empty() {
        if !llms.is_empty() {
            println!();
            println!("All loaded LLMs are already covered. Nothing to suggest.");
        }
        return Ok(0);
    }

    // Pre-pass: detect derived-name collisions between uncovered models.
    // Two models with different publishers but the same base name (e.g.
    // unsloth/Qwen-7B and lmstudio-community/Qwen-7B) would each draft into
    // the same profile name and silently clobber each other in the registry.
    let mut name_collisions: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for m in &uncovered {
        let bucket = heuristics::classify_size_from_meta(m);
        let suggested_class = match bucket {
            heuristics::SizeBucket::Tiny => heuristics::TaskClass::Fast,
            heuristics::SizeBucket::Small => heuristics::TaskClass::Mid,
            heuristics::SizeBucket::Medium => heuristics::TaskClass::Long,
            heuristics::SizeBucket::Large => heuristics::TaskClass::Mid,
            heuristics::SizeBucket::Xl => heuristics::TaskClass::Fast,
        };
        let name = derive_profile_name(&m.model_key, suggested_class);
        *name_collisions.entry(name).or_insert(0) += 1;
    }

    println!();
    for m in &uncovered {
        let bucket = heuristics::classify_size_from_meta(m);
        let arch = heuristics::classify_architecture(m);
        let suggested_class = match bucket {
            heuristics::SizeBucket::Tiny => heuristics::TaskClass::Fast,
            heuristics::SizeBucket::Small => heuristics::TaskClass::Mid,
            heuristics::SizeBucket::Medium => heuristics::TaskClass::Long,
            heuristics::SizeBucket::Large => heuristics::TaskClass::Mid,
            heuristics::SizeBucket::Xl => heuristics::TaskClass::Fast,
        };
        let suggestion = heuristics::suggest_profile(m, suggested_class);
        let size_gb = (m.size_bytes as f64) / (1024.0 * 1024.0 * 1024.0);
        let display = if m.display_name.is_empty() {
            m.model_key.clone()
        } else {
            m.display_name.clone()
        };

        println!("• {display}");
        println!(
            "    id={}  params={}  arch={:?}  size={:.1}GB  maxCtx={}",
            m.model_key,
            m.params_string.as_deref().unwrap_or("?"),
            arch,
            size_gb,
            m.max_context_length.unwrap_or(0)
        );
        println!(
            "    suggested task class: `{}` (n_ctx={}, compactor={})",
            suggested_class.as_str(),
            suggestion.primary_n_ctx,
            suggestion
                .compactor
                .as_ref()
                .map(|c| format!("{} @ {}", c.model_id, c.n_ctx))
                .unwrap_or_else(|| "none".into())
        );
        if !m.trained_for_tool_use {
            println!(
                "    ⚠ NOT marked trainedForToolUse — agentic dispatch may be unreliable"
            );
        }
        let safe_name = derive_profile_name(&m.model_key, suggested_class);
        if name_collisions.get(&safe_name).copied().unwrap_or(0) > 1 {
            println!(
                "    ⚠ derived name `{safe_name}` collides with another uncovered model — \
                 customize the name when drafting (publisher prefix gets stripped)"
            );
        }
        println!(
            "    draft: `darkmux profile draft {safe_name} --model {} --task-class {}`",
            m.model_key,
            suggested_class.as_str()
        );
        println!();
    }
    Ok(0)
}

/// Compose a sensible default profile name from a model id + task class.
/// Strips publisher prefixes (e.g. `mlx-community/`), lowercases, replaces
/// underscores/spaces with dashes, drops anything that isn't alphanumeric +
/// dash + dot. Trims leading/trailing dashes; if the result starts with a
/// non-alphanumeric, prepends "model".
fn derive_profile_name(model_id: &str, task: heuristics::TaskClass) -> String {
    let last_segment = model_id.rsplit('/').next().unwrap_or(model_id);
    let cleaned: String = last_segment
        .chars()
        .map(|c| if c == '_' || c == ' ' { '-' } else { c.to_ascii_lowercase() })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    let safe_base = if trimmed.is_empty()
        || !trimmed.chars().next().map(|c| c.is_ascii_alphanumeric()).unwrap_or(false)
    {
        format!("model-{}", trimmed.trim_start_matches('-'))
    } else {
        trimmed
    };
    format!("{}-{}", safe_base, task.as_str())
}

/// True if the model id has a publisher prefix that gets stripped by
/// `derive_profile_name`. Reserved for future per-model warnings; the
/// scan currently catches collisions globally instead.
#[allow(dead_code)]
fn has_stripped_publisher(model_id: &str) -> bool {
    model_id.contains('/')
}

fn cmd_agent(sub: AgentCmd) -> Result<i32> {
    match sub {
        AgentCmd::ListTemplates => {
            let ids = agent_roles::list_role_ids();
            println!("darkmux ships {} role template(s):", ids.len());
            println!();
            for id in &ids {
                let t = agent_roles::load_role(id)?;
                println!("• {} ({})", t.role, t.runtime);
                println!("    {}", t.description);
                println!(
                    "    pairs with profile: {}, tools: {}",
                    t.recommended_profile,
                    t.recommended_tools.join(", ")
                );
                println!();
            }
            println!("Generate a template:  `darkmux agent template <role>`");
            Ok(0)
        }
        AgentCmd::Template { role } => {
            let template = agent_roles::load_role(&role)?;
            let snippet = agent_roles::snippet_for_agents_list(&template);
            println!("{}", serde_json::to_string_pretty(&snippet)?);
            eprintln!();
            eprintln!(
                "// Paste the above object into the `agents.list` array of your runtime config"
            );
            eprintln!(
                "// (e.g. ~/.openclaw/openclaw.json). Recommended profile: `{}`. Adjust",
                template.recommended_profile
            );
            eprintln!(
                "// `tools` to taste; the override text is the validated scaffold — tune the"
            );
            eprintln!("// task-specific framing for your codebase, but keep the structural blocks");
            eprintln!("// (Tool Call Style, Execution Bias) — they're the load-bearing parts.");
            Ok(0)
        }
    }
}

fn cmd_profile(sub: ProfileCmd) -> Result<i32> {
    match sub {
        ProfileCmd::Draft {
            name,
            model,
            task_class,
            params,
            max_ctx,
        } => {
            let task = heuristics::TaskClass::parse(&task_class).ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown task-class '{task_class}'. Try: fast | mid | long"
                )
            })?;
            // Try to find the model in `lms ls`. If not found, the user MUST
            // supply --params (otherwise we'd silently bucket the unknown
            // model as Tiny, producing a 32K no-compactor profile regardless
            // of its real size — a documented footgun).
            let available = lms::list_available().unwrap_or_default();
            let meta = match available.iter().find(|m| m.model_key == model).cloned() {
                Some(found) => {
                    if params.is_some() || max_ctx.is_some() {
                        eprintln!(
                            "note: model `{model}` is in `lms ls`; --params/--max-ctx overrides ignored."
                        );
                    }
                    found
                }
                None => {
                    let Some(params) = params.as_deref() else {
                        anyhow::bail!(
                            "model `{model}` not found in `lms ls` (not downloaded yet?). \
                             Re-run with `--params <NB>` (e.g. `--params 70B`) to draft a \
                             profile from explicit metadata, or download the model first \
                             so heuristics can read its size + max context length."
                        );
                    };
                    eprintln!(
                        "note: model `{model}` not found in `lms ls`; using --params={params}. \
                         Heuristics are tighter when the model is downloaded — re-run after \
                         download for the canonical draft."
                    );
                    lms::ModelMeta {
                        model_key: model.clone(),
                        display_name: model.clone(),
                        publisher: "".into(),
                        size_bytes: 0,
                        params_string: Some(params.to_string()),
                        architecture: None,
                        max_context_length: max_ctx,
                        trained_for_tool_use: true,
                        model_type: "llm".into(),
                    }
                }
            };

            let suggestion = heuristics::suggest_profile(&meta, task);
            let json = heuristics::suggestion_to_profile_json(
                &name,
                &model,
                &suggestion,
                None,
            );
            // Pretty-print
            println!("{}", serde_json::to_string_pretty(&json)?);
            eprintln!();
            eprintln!("// Copy the above into the `profiles` block of ~/.darkmux/profiles.json,");
            eprintln!("// then run `darkmux doctor` to verify the result.");
            Ok(0)
        }
    }
}

fn cmd_init(
    with_hook: bool,
    with_claude_md: Option<std::path::PathBuf>,
    force: bool,
    dry_run: bool,
) -> Result<i32> {
    let report = init::init(&init::InitOptions {
        with_hook,
        with_claude_md,
        force,
        dry_run,
    })?;
    if let Some(p) = report.profile_registry_path.as_ref() {
        if report.profile_registry_already_present {
            println!("profile registry: already present at {}", p.display());
        } else if report.profile_registry_created {
            println!(
                "profile registry: created at {} (edit it to point at your downloaded models — `lms ls`)",
                p.display()
            );
        }
    }
    println!("skills target: {}", report.skills_target.display());
    if !report.skills_installed.is_empty() {
        println!(
            "  installed ({}): {}",
            report.skills_installed.len(),
            report.skills_installed.join(", ")
        );
    }
    if !report.skills_overwritten.is_empty() {
        println!(
            "  overwritten ({}): {}",
            report.skills_overwritten.len(),
            report.skills_overwritten.join(", ")
        );
    }
    if !report.skills_skipped.is_empty() {
        println!(
            "  skipped ({}): {}",
            report.skills_skipped.len(),
            report.skills_skipped.join(", ")
        );
    }
    if let Some(p) = report.hook_added {
        if report.hook_already_present {
            println!("hook: already present in {}", p.display());
        } else {
            println!("hook: added to {}", p.display());
        }
    }
    if let Some(p) = report.claude_md_path {
        if report.claude_md_already_present {
            println!("CLAUDE.md: already integrated at {}", p.display());
        } else if report.claude_md_appended {
            println!("CLAUDE.md: integration section appended to {}", p.display());
        }
    }
    if dry_run {
        println!("[DRY RUN — nothing was written]");
    } else {
        println!();
        println!("Next steps:");
        if report.profile_registry_created {
            println!("  1. Edit ~/.darkmux/profiles.json to point at your downloaded models");
            println!("     (run `lms ls` to see what's available)");
            println!("  2. Run `darkmux doctor` to verify your setup");
            println!("  3. Run `darkmux lab characterize` to smoke-test your machine");
        } else {
            println!("  1. Run `darkmux doctor` to verify your setup");
            println!("  2. Run `darkmux lab characterize` to smoke-test your machine");
        }
    }
    Ok(0)
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
        println!(
            "  {:<40} ctx={:<8} {}",
            m.identifier, m.context, m.status
        );
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

fn cmd_skills(sub: SkillsCmd) -> Result<i32> {
    match sub {
        SkillsCmd::Install {
            target,
            force,
            dry_run,
        } => {
            let report = skills::install_skills(&skills::InstallOptions {
                target,
                force,
                dry_run,
            })?;
            println!("source: {}", report.source.display());
            println!("target: {}", report.target.display());
            if !report.installed.is_empty() {
                println!(
                    "installed ({}): {}",
                    report.installed.len(),
                    report.installed.join(", ")
                );
            }
            if !report.overwritten.is_empty() {
                println!(
                    "overwritten ({}): {}",
                    report.overwritten.len(),
                    report.overwritten.join(", ")
                );
            }
            if !report.skipped.is_empty() {
                println!(
                    "skipped (already exists, use --force to overwrite) ({}): {}",
                    report.skipped.len(),
                    report.skipped.join(", ")
                );
            }
            if dry_run {
                println!("[DRY RUN — nothing was written]");
            }
            Ok(0)
        }
        SkillsCmd::List { target } => {
            let listed = skills::list_installed_skills(target.as_deref())?;
            if listed.is_empty() {
                println!("(no skills installed)");
            } else {
                for n in listed {
                    println!("{n}");
                }
            }
            Ok(0)
        }
    }
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
            instrument,
        } => {
            let outcomes = lab::run::lab_run(lab::run::RunOpts {
                workload_id: workload,
                profile_name: profile,
                runs,
                config_path: config,
                quiet,
                instrument,
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
            // Telemetry summary, if `--instrument` was used on the run.
            // Detection is automatic: if instruments.jsonl is missing, this
            // is a no-op silently. Surfaced before the compaction summary
            // because operators usually want the cross-layer view first.
            let run_dir = lab::inspect::resolve_run_path(&run);
            if let Some(t) = lab::inspect::read_telemetry_summary(&run_dir)? {
                println!();
                println!("instruments:");
                println!("  elapsed:       {}s", t.elapsed_s);
                println!("  lms samples:   {}", t.lms_samples);
                println!("  proc samples:  {}", t.process_samples);
                if !t.model_identifiers_seen.is_empty() {
                    println!("  models seen:   {}", t.model_identifiers_seen.join(", "));
                }
                println!("  gw peak RSS:   {} MB", t.gateway_peak_rss_mb);
                println!("  gw mean CPU:   {:.1}%", t.gateway_mean_cpu);
                if !t.anomalies.is_empty() {
                    println!("  anomalies:");
                    for a in &t.anomalies {
                        println!("    ⚠ {a}");
                    }
                }
            }
            if summary {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_profile_name_strips_publisher_and_lowercases() {
        let n = derive_profile_name("nousresearch/hermes-4-70b", heuristics::TaskClass::Mid);
        assert_eq!(n, "hermes-4-70b-mid");
    }

    #[test]
    fn derive_profile_name_preserves_dot_in_version() {
        let n = derive_profile_name(
            "mlx-community/Qwen3-1.7B-MLX-MXFP4",
            heuristics::TaskClass::Fast,
        );
        assert_eq!(n, "qwen3-1.7b-mlx-mxfp4-fast");
    }

    #[test]
    fn derive_profile_name_collision_when_publishers_differ() {
        // Two different publishers, same base — derived names match (the
        // documented collision case warned about in cmd_scan).
        let a = derive_profile_name("unsloth/Qwen-7B", heuristics::TaskClass::Fast);
        let b = derive_profile_name("lmstudio-community/Qwen-7B", heuristics::TaskClass::Fast);
        assert_eq!(a, b);
    }

    #[test]
    fn derive_profile_name_handles_empty_id() {
        let n = derive_profile_name("", heuristics::TaskClass::Fast);
        assert!(
            n.starts_with("model-") || n.chars().next().map(|c| c.is_ascii_alphanumeric()).unwrap_or(false),
            "expected name to start with alphanumeric or 'model-', got: {n}"
        );
    }

    #[test]
    fn derive_profile_name_strips_garbage_chars() {
        let n = derive_profile_name("publisher/some@weird*name!", heuristics::TaskClass::Mid);
        assert_eq!(n, "someweirdname-mid");
    }

    #[test]
    fn has_stripped_publisher_true_for_pubprefixed() {
        assert!(has_stripped_publisher("nousresearch/hermes"));
        assert!(!has_stripped_publisher("hermes"));
    }
}
