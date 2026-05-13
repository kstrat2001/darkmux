//! darkmux — a lab and multiplexer for local LLM configurations.
//!
//! v0.2 in Rust. Ports the v0.1 TS prototype + the v0.2 lab foundation.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod agent_roles;
mod crew;
pub mod flow;
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
mod serve;
mod skills;
mod flow_cli;
mod role_cli;
mod sprint_cli;
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
    Doctor {
        /// Attempt to auto-apply known-safe fixes for failing or warning
        /// checks where a handler is registered (currently:
        /// `eureka: ctx-window-mismatch` realigns openclaw.json
        /// `contextWindow` values to match what `lms ps` reports). After
        /// the fixes run, doctor re-evaluates and prints the updated report.
        #[arg(long)]
        fix: bool,
    },
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
    /// Model lifecycle subcommands — operate on the darkmux-managed model
    /// group (anything in `lms ps` under the `darkmux:` namespace).
    /// User-loaded models (non-namespaced identifiers) are off-limits to
    /// these commands by design.
    Model {
        #[command(subcommand)]
        sub: ModelCmd,
    },
    /// Crew subcommands — dispatch a role for a single turn, or reconcile
    /// the openclaw agent registry with the on-disk crew manifests.
    Crew {
        #[command(subcommand)]
        sub: CrewCmd,
    },
    /// Role management — list and show role details from the SQLite index.
    Role {
        #[command(subcommand)]
        sub: RoleCmd,
    },
    /// Sprint planning — pre-dispatch budget oracle.
    /// `darkmux sprint estimate <spec.json>` computes token consumption +
    /// recommends a profile. `--narrate` adds a one-sentence operator-facing
    /// wrap from the 4B compactor.
    Sprint {
        #[command(subcommand)]
        sub: SprintCmd,
    },
    /// Agent-role template subcommands. Browse + emit validated
    /// `systemPromptOverride` scaffolds for common roles (qa, scribe,
    /// engineer). Output is print-only — paste into your runtime config
    /// (`agents.list[]` in openclaw.json) yourself.
    Agent {
        #[command(subcommand)]
        sub: AgentCmd,
    },
    /// Flow observability — record operator-facing flow events.
    Flow {
        #[command(subcommand)]
        sub: flow_cli::FlowCmd,
    },
    /// Start an HTTP daemon for flow record retrieval.
    Serve {
        /// Port to listen on (default: 8765).
        #[arg(long, default_value = "8765")]
        port: u16,
        /// Address to bind (default: 127.0.0.1).
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        /// Directory to serve flow records from (default: ~/.darkmux/flows/).
        #[arg(long = "flows-dir")]
        flows_dir: Option<std::path::PathBuf>,
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
enum CrewCmd {
    /// List every crew in the index.
    List,
    /// Show full details for a single crew.
    Show {
        /// Crew id to show.
        id: String,
    },
    /// Dispatch a single turn to the named role. Loads the role manifest +
    /// `.md` system prompt, verifies the corresponding `darkmux/<role-id>`
    /// openclaw agent exists and matches the manifest, then invokes
    /// `openclaw agent` with the assembled message.
    Dispatch {
        /// Role id (e.g. `code-reviewer`). Must have a manifest at
        /// `templates/builtin/crew/roles/<id>.json` (or under
        /// `~/.darkmux/crew/roles/`) AND a sibling `.md` prompt file.
        role: String,
        /// Message body for the dispatch.
        #[arg(long, short = 'm')]
        message: String,
        /// Optional delivery target in `<channel>:<target>` form
        /// (e.g. `discord:1500166601909993503`). When set, openclaw's
        /// reply is delivered to that channel in addition to being
        /// returned on stdout.
        #[arg(long)]
        deliver: Option<String>,
        /// Override the dispatch session id. Default: a fresh
        /// `crew-dispatch-<role>-<unix-micros>-<process-counter>` is
        /// generated per call, so consecutive dispatches don't share
        /// openclaw session state (which would otherwise pollute one
        /// task with another's context).
        #[arg(long)]
        session_id: Option<String>,
        /// Timeout in seconds (default: 600).
        #[arg(long, default_value = "600")]
        timeout: u32,
        /// Skip the pre-flight checks. Use only for debugging.
        #[arg(long, hide = true)]
        skip_preflight: bool,
    },
    /// Reconcile openclaw's `agents.list[]` with the crew role manifests.
    /// For every role with both a JSON manifest and a `.md` prompt, ensures
    /// a `darkmux/<role-id>` openclaw agent exists with the manifest's
    /// system prompt + tool palette. Idempotent.
    Sync {
        /// Skip the diff preview and write directly.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Show what would change without writing.
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
    /// SQLite-backed derived index over crew manifests (Phase B of #45).
    /// **Scaffold only** — `rebuild`/`status` return "not yet implemented".
    /// See `src/crew/index.rs` for the synthesized schema design awaiting
    /// implementation.
    Index {
        #[command(subcommand)]
        sub: CrewIndexCmd,
    },
}

#[derive(Subcommand)]
enum CrewIndexCmd {
    /// Rebuild the index from manifests on disk. (Not yet implemented.)
    Rebuild,
    /// Report index status: last-rebuild timestamp, source counts, drift.
    /// (Not yet implemented.)
    Status,
}

#[derive(Subcommand)]
enum RoleCmd {
    /// List every role in the index.
    List,
    /// Show full details for a single role.
    Show {
        /// Role id to show.
        id: String,
    },
}

#[derive(Subcommand)]
enum SprintCmd {
    /// Pre-dispatch budget oracle. Reads a workload-spec JSON, computes
    /// predicted token consumption across planned turns, picks the
    /// smallest adequate profile, emits structured JSON.
    Estimate {
        /// Path to the workload-spec JSON file.
        spec: std::path::PathBuf,
        /// Add a one-sentence operator-facing recommendation wrap from the
        /// 4B compactor (`darkmux:qwen3-4b-instruct-2507`). Adds ~500ms
        /// latency; gracefully degrades if the model isn't loaded.
        #[arg(long)]
        narrate: bool,
    },
    /// Run a code review on the current branch vs base.  Auto-detects
    /// target, computes `git diff`, dispatches the `code-reviewer` role,
    /// parses the QA-REVIEW-SIGNOFF block, and emits structured JSON.
    Review {
        /// Base branch to diff against. Defaults to `main`.
        #[arg(long)]
        base: Option<String>,
        /// Exit nonzero if any BLOCK-severity findings.
        #[arg(long)]
        require_clean: bool,
        /// Optional sprint identifier passed through to flow records.
        #[arg(long = "sprint-id")]
        sprint_id: Option<String>,
    },
}

#[derive(Subcommand)]
enum ModelCmd {
    /// Show models currently loaded in LMStudio, grouped by ownership:
    /// darkmux-managed (under the `darkmux:` namespace) vs user state
    /// (everything else). Read-only.
    Status,
    /// Eject all darkmux-managed model loads (anything in the `darkmux:`
    /// namespace). User-loaded models are never touched. Use this when
    /// you want to release darkmux's RAM footprint without affecting
    /// other tools using LMStudio.
    Eject {
        /// Show what would be ejected without actually unloading.
        #[arg(long, short = 'n')]
        dry_run: bool,
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
        /// Override the machine id (overrides DARKMUX_MACHINE_ID env var).
        #[arg(long)]
        machine: Option<String>,
    },
    /// List notebook entries (parsed from entry headers).
    ///
    /// Enumerates .md files in the notebook directory, reads each entry's
    /// `<!-- darkmux:notebook-entry: run=X machine=Y date=Z -->` header,
    /// and prints a summary table.  Optionally filter entries by machine.
    List {
        /// Only show entries from this machine (optional).
        #[arg(long)]
        machine: Option<String>,
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
        Cmd::Doctor { fix } => cmd_doctor(fix),
        Cmd::Scan { config } => cmd_scan(config.as_deref()),
        Cmd::Profile { sub } => cmd_profile(sub),
        Cmd::Model { sub } => cmd_model(sub),
        Cmd::Crew { sub } => cmd_crew(sub),
        Cmd::Role { sub } => cmd_role(sub),
        Cmd::Sprint { sub } => cmd_sprint(sub),
        Cmd::Agent { sub } => cmd_agent(sub),
        Cmd::Flow { sub } => {
            flow_cli::run(sub)?;
            Ok(0)
        }
        Cmd::Init {
            with_hook,
            with_claude_md,
            force,
            dry_run,
        } => cmd_init(with_hook, with_claude_md, force, dry_run),
        Cmd::Serve { port, bind, flows_dir } => {
            let flows_dir = flows_dir.unwrap_or_else(crate::flow::flows_dir);
            serve::run(port, bind, flows_dir)?;
            Ok(0)
        }
        Cmd::Optimize => optimize::run(),
    }
}

fn cmd_notebook(sub: NotebookCmd) -> Result<i32> {
    use crate::lab::paths;
    match sub {
        NotebookCmd::Draft {
            run_id,
            agent,
            slug,
            dry_run,
            machine,
        } => {
            let report = notebook::draft_entry(&notebook::DraftOptions {
                run_id,
                agent,
                slug,
                dry_run,
                machine_override: machine,
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
        NotebookCmd::List { machine } => {
            let paths = paths::resolve(paths::ResolveScope::Auto);
            if !paths.notebook.exists() {
                println!("no notebook directory found: {}", paths.notebook.display());
                return Ok(1);
            }
            let entries = notebook::list_entries(&paths.notebook, machine.as_deref())?;
            if entries.is_empty() {
                println!("no notebook entries found");
                return Ok(0);
            }
            // Column widths (dynamic based on longest value).
            let max_date: usize = entries.iter().map(|e| e.date.len()).max().unwrap_or(10);
            let max_machine: usize = entries.iter()
                .map(|e| e.machine.len()).max().unwrap_or(10);
            let max_run: usize = entries.iter()
                .map(|e| e.run.len()).max().unwrap_or(12);
            for entry in &entries {
                println!(
                    "{date:<width_date$}  {machine:<width_machine$}  {run:<width_run$}  {path}",
                    date = entry.date,
                    width_date = max_date.max(4),
                    machine = entry.machine,
                    width_machine = max_machine.max(8),
                    run = entry.run,
                    width_run = max_run.max(4),
                    path = entry.path.display(),
                );
            }
            Ok(0)
        }
    }
}

fn cmd_doctor(fix: bool) -> Result<i32> {
    let report = doctor::run();
    doctor::print_report(&report)?;

    // --fix path: attempt known-safe auto-fixes for failing/warning rules,
    // then re-run the full check set so the operator sees the post-fix
    // state. Without --fix, doctor is read-only — exits based on `report`.
    let final_report = if fix {
        let outcomes = doctor::try_fix(&report)?;
        if outcomes.is_empty() {
            println!();
            println!("--fix: no auto-fix available for any failing or warning check.");
            report
        } else {
            println!();
            println!("--fix: applied {} auto-fix(es):", outcomes.len());
            for o in &outcomes {
                let marker = if o.applied { "✓" } else { "·" };
                println!("  {marker} {} — {}", o.rule_id, o.message);
            }
            println!();
            println!("Re-running doctor…");
            println!();
            let report2 = doctor::run();
            doctor::print_report(&report2)?;
            report2
        }
    } else {
        report
    };

    Ok(match final_report.worst_status() {
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

fn cmd_role(sub: RoleCmd) -> Result<i32> {
    match sub {
        RoleCmd::List => role_cli::role_list(),
        RoleCmd::Show { id } => role_cli::role_show(&id),
    }
}

fn cmd_sprint(sub: SprintCmd) -> Result<i32> {
    match sub {
        SprintCmd::Estimate { spec, narrate } => sprint_cli::estimate(&spec, narrate),
        SprintCmd::Review { base, require_clean, sprint_id } => {
            let sid = sprint_id.as_deref();
            sprint_cli::sprint_review(base.as_deref(), require_clean, sid)
        }
    }
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

fn cmd_crew(sub: CrewCmd) -> Result<i32> {
    match sub {
        CrewCmd::List => crew::cli::crew_list(),
        CrewCmd::Show { id } => crew::cli::crew_show(&id),
        CrewCmd::Dispatch { role, message, deliver, session_id, timeout, skip_preflight } => {
            let opts = crew::dispatch::DispatchOpts {
                role_id: role,
                message,
                deliver,
                session_id,
                timeout_seconds: timeout,
                skip_preflight,
            };
            let result = crew::dispatch::dispatch(opts)?;
            // Announce the resolved session id on stderr so operators see
            // which session openclaw was pointed at — without polluting
            // the --json envelope on stdout that orchestrators parse.
            eprintln!("darkmux crew dispatch: session id `{}`", result.session_id);
            // Stream both stdout (openclaw's --json envelope) and stderr to
            // the caller — the orchestrator parses one or the other.
            print!("{}", result.stdout);
            if !result.stderr.is_empty() {
                eprint!("{}", result.stderr);
            }
            Ok(result.exit_code)
        }
        CrewCmd::Index { sub } => match sub {
            CrewIndexCmd::Rebuild => crew::index::rebuild().map(|_| 0),
            CrewIndexCmd::Status => crew::index::status().map(|_| 0),
        },
        CrewCmd::Sync { yes: _, dry_run } => {
            let opts = crew::dispatch::SyncOpts { dry_run };
            let result = crew::dispatch::sync(opts)?;
            let verbs = if dry_run {
                ("would add", "would update")
            } else {
                ("added", "updated")
            };
            let trail = if dry_run { " [DRY RUN]" } else { "" };
            println!(
                "crew sync{trail}: {add_v} {a}, {upd_v} {u}, unchanged {un}, skipped (no .md prompt) {sn}",
                add_v = verbs.0,
                upd_v = verbs.1,
                a = result.added.len(),
                u = result.updated.len(),
                un = result.unchanged.len(),
                sn = result.skipped_no_prompt.len(),
            );
            for id in &result.added {
                println!("  + {id}");
            }
            for id in &result.updated {
                println!("  ~ {id}");
            }
            for id in &result.skipped_no_prompt {
                println!("  · {id} (no .md prompt)");
            }
            Ok(0)
        }
    }
}

fn cmd_model(sub: ModelCmd) -> Result<i32> {
    match sub {
        ModelCmd::Status => cmd_model_status(),
        ModelCmd::Eject { dry_run } => cmd_model_eject(dry_run),
    }
}

fn cmd_model_status() -> Result<i32> {
    let loaded = lms::list_loaded()?;
    let (managed, user): (Vec<_>, Vec<_>) = loaded
        .iter()
        .partition(|m| swap::is_darkmux_owned(&m.identifier));
    println!(
        "darkmux-managed ({}):",
        managed.len()
    );
    if managed.is_empty() {
        println!("  (none — `darkmux swap <profile>` to load)");
    } else {
        for m in &managed {
            println!(
                "  {:<46} ctx={:<8} {}",
                m.identifier, m.context, m.size
            );
        }
    }
    println!();
    println!("user state ({}):", user.len());
    if user.is_empty() {
        println!("  (none — LMStudio is exclusively darkmux's right now)");
    } else {
        for m in &user {
            println!(
                "  {:<46} ctx={:<8} {}",
                m.identifier, m.context, m.size
            );
        }
        println!();
        println!("note: darkmux will never unload entries under `user state` — they're");
        println!("      yours. Use `lms unload <identifier>` to remove them manually.");
    }
    Ok(0)
}

fn cmd_model_eject(dry_run: bool) -> Result<i32> {
    let loaded = lms::list_loaded()?;
    let managed: Vec<_> = loaded
        .iter()
        .filter(|m| swap::is_darkmux_owned(&m.identifier))
        .collect();
    let user_count = loaded.len() - managed.len();
    if managed.is_empty() {
        println!("no darkmux-managed loads to eject");
        if user_count > 0 {
            println!(
                "({} user-loaded model(s) untouched — use `lms unload <identifier>` for those)",
                user_count
            );
        }
        return Ok(0);
    }
    for m in &managed {
        if dry_run {
            println!("would eject {} (ctx={})", m.identifier, m.context);
        } else {
            println!("eject {} (ctx={})", m.identifier, m.context);
            lms::unload(&m.identifier)?;
        }
    }
    let verb = if dry_run { "would eject" } else { "ejected" };
    let mut summary = format!("{verb} {} model(s)", managed.len());
    if user_count > 0 {
        summary.push_str(&format!(", respected {user_count} user-loaded model(s)"));
    }
    if dry_run {
        summary.push_str(" [DRY RUN]");
    }
    println!("{summary}");
    Ok(0)
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
    let result = swap::swap(
        profile,
        &loaded.registry,
        swap::SwapOpts { quiet, dry_run },
    )?;
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
