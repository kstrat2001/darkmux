//! darkmux — a lab and multiplexer for local LLM configurations.
//!
//! v0.2 in Rust. Ports the v0.1 TS prototype + the v0.2 lab foundation.

use anyhow::{Context, Result};
use clap::Parser;

// The clap command tree (Cli/Cmd + every subcommand's arg struct) — split
// out of main.rs (mechanical extraction, zero behavior change) alongside
// fleet_cli/lab_cli. Glob-imported so every `FooCmd` variant type main.rs's
// handlers already reference by bare name keeps resolving unchanged.
mod cli;
use cli::*;

// #463 workspace split — crew extracted to its own crate (the velocity-debt
// target: touching dispatch_internal.rs now rebuilds only darkmux-crew + the
// binary stub). Re-export keeps crate::crew::* resolving for the binary +
// fleet.
pub use darkmux_crew as crew;
// #515 — doctor extracted (all deps now crates; resolves the plan's open
// sub-decision). Re-export keeps crate::doctor::* resolving for serve.
pub use darkmux_doctor as doctor;
// #515 Tier B — eureka rules engine extracted. Re-export keeps crate::eureka::*
// resolving for doctor/serve/lab.
pub use darkmux_eureka as eureka;
// #515 — fleet extracted (deps crew/flow/types all crates now). Re-export
// keeps crate::fleet::* resolving for serve/phase_cli/notebook/mission_propose.
pub use darkmux_fleet as fleet;
// `darkmux machine` roster-facing handlers — split out of main.rs alongside cli/lab_cli.
mod fleet_cli;
// #463 workspace split — flow extracted to the darkmux-flow crate. The
// re-export keeps all existing `crate::flow::*` paths resolving unchanged.
pub use darkmux_flow as flow;
mod flow_cli;
// #515 — zero-edge leaf extracted to darkmux-hardware. Re-export keeps
// crate::hardware::* resolving for heuristics/eureka/recommendations/doctor/etc.
pub use darkmux_hardware as hardware;
// #515 Tier B — per-tier heuristics extracted. Re-export keeps
// crate::heuristics::* resolving for recommendations/doctor.
pub use darkmux_heuristics as heuristics;
mod init;
// #515 — lab harness extracted (lab + workloads + providers). Re-exports keep
// crate::{lab,workloads,providers}::* resolving for main + notebook.
pub use darkmux_lab::lab;
// `darkmux lab` command handlers — split out of main.rs alongside cli/fleet_cli.
mod lab_cli;
mod migrate;
mod config_cmd;
mod conventions;
mod mission_propose;
mod mission_status;
mod coder_phase;
mod mission_launch;
mod mission_launch_review;
mod notebook;
mod pr_review;
pub use darkmux_lab::providers;
mod role_cli;
// #515 — serve daemon extracted (final crate; deps doctor/eureka/fleet/crew/
// flow/profiles all crates). Re-export keeps crate::serve::* resolving for
// main + phase_cli.
pub use darkmux_serve as serve;
mod skills;
mod phase_cli;
// #463 workspace split (PR2) — profiles/swap/lms extracted to the
// darkmux-profiles crate. These re-exports keep every existing
// crate::{profiles,swap,lms}::* path resolving unchanged. (2.0, #1405: the
// `runtime` module — the legacy openclaw config-file patcher — was removed.)
pub use darkmux_profiles::lms;
pub use darkmux_profiles::profiles;
pub use darkmux_profiles::swap;
// #463 workspace split — types extracted to the darkmux-types crate. The
// re-export keeps all existing `crate::types::*` paths resolving unchanged.
pub use darkmux_types as types;
// #463 — workdir lifted into darkmux-types (leaf util shared by crew + fleet,
// so crew can use it without a binary-resident edge). Re-export keeps
// crate::workdir::* resolving for fleet + other binary modules.
pub use darkmux_types::workdir;
pub use darkmux_lab::workloads;

fn main() -> Result<()> {
    providers::register_builtins()?;
    let cli = Cli::parse();
    let code = run(cli.command)?;
    std::process::exit(code);
}

fn run(cmd: Cmd) -> Result<i32> {
    match cmd {
        Cmd::Lab { sub } => lab_cli::cmd_lab(sub),
        // (#1426) `dispatch` promoted to a top-level verb — the task-grain
        // execution entry. Relocated out of the `crew` family (`dispatch`
        // retired) with the message reshaped to a positional + stdin source.
        Cmd::Dispatch {
            role,
            message,
            message_from_file,
            profile,
            session_id,
            timeout,
            workdir,
            phase_id,
            skip_preflight,
            json,
            machine,
            no_wait,
            image,
            max_completion_tokens,
        } => cmd_dispatch(DispatchInvocation {
            role,
            message,
            message_from_file,
            profile,
            session_id,
            timeout,
            workdir,
            phase_id,
            skip_preflight,
            json,
            machine,
            no_wait,
            image,
            max_completion_tokens,
        }),
        Cmd::Doctor { verbose, probe } => cmd_doctor(verbose, probe),
        Cmd::Profile { sub } => cmd_profile(sub),
        // (#1426) Bare `machine` routes to `machine status` (no id) — one
        // code path, no separate overview render.
        Cmd::Machine { sub } => cmd_machine(sub),
        // (#1426, decision 17) One verb per KIND of thing darkmux knows.
        Cmd::Memory { sub } => match sub {
            cli::MemoryCmd::Lesson { sub } => cmd_lessons(sub),
            cli::MemoryCmd::Correction { sub } => cmd_correction(sub),
        },
        Cmd::Role { sub } => cmd_role(sub),
        Cmd::Phase { sub } => cmd_phase(sub),
        Cmd::Mission { sub } => cmd_mission(sub),
        Cmd::Flow { sub } => {
            flow_cli::run(sub)?;
            Ok(0)
        }
        Cmd::Config { sub } => {
            config_cmd::run(sub)?;
            Ok(0)
        }
        Cmd::Init {
            with_hook,
            with_claude_md,
            with_agents_md,
            force,
            dry_run,
        } => cmd_init(with_hook, with_claude_md, with_agents_md, force, dry_run),
        Cmd::Serve {
            port,
            bind,
            flows_dir,
            lab_dir,
        } => {
            let flows_dir = flows_dir.unwrap_or_else(crate::flow::flows_dir);
            // (#1247 Part 3) `--lab-dir` > `DARKMUX_LAB_DIR` > unset. No
            // config.json tier and no built-in default — see the flag's doc
            // comment: the lab lens only ever reads a directory the operator
            // explicitly named.
            let lab_dir = lab_dir.or_else(|| std::env::var("DARKMUX_LAB_DIR").ok().map(std::path::PathBuf::from));
            serve::run(port, bind, flows_dir, lab_dir)?;
            Ok(0)
        }
    }
}

fn cmd_lessons(sub: LessonsCmd) -> Result<i32> {
    use darkmux_crew::lessons;
    match sub {
        LessonsCmd::Add {
            title,
            body,
            file,
            global,
        } => {
            let (path, tier) = if global {
                (lessons::global_db_path(), "global")
            } else {
                (lessons::repo_db_path(), "repo")
            };
            let conn = lessons::open_at(&path)?;
            lessons::add(&conn, &title, &body, file.as_deref(), None)?;
            println!(
                "{}",
                darkmux_types::style::success(&format!("recorded lesson ({tier}): {title}"))
            );
            println!("{}", darkmux_types::style::dim(&format!("  {}", path.display())));
            Ok(0)
        }
        LessonsCmd::List {
            json: cli::JsonFlagPlain { json },
        } => {
            let repo_path = lessons::repo_db_path();
            let global_path = lessons::global_db_path();
            let repo = lessons::load_entries_best_effort(&repo_path);
            // When `$DARKMUX_HOME` collapses both tiers to one root the paths are
            // identical — read once, don't double-display the same entries.
            let global = if global_path == repo_path {
                Vec::new()
            } else {
                lessons::load_entries_best_effort(&global_path)
            };

            if json {
                let out = serde_json::json!({ "repo": repo, "global": global });
                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(0);
            }
            if repo.is_empty() && global.is_empty() {
                println!(
                    "{}",
                    darkmux_types::style::dim(
                        "no lessons recorded yet — darkmux memory lesson add --title <t> --body <b>"
                    )
                );
                return Ok(0);
            }
            print_lessons_tier("repo (this engagement)", &repo);
            print_lessons_tier("global (all engagements)", &global);
            Ok(0)
        }
        LessonsCmd::Edit {
            id,
            title,
            body,
            file,
            clear_file,
            global,
        } => {
            let (path, tier) = lessons_tier(global);
            // tri-state: --clear-file wins (Some(None)); else --file (Some(Some));
            // else leave unchanged (None).
            let file_update: Option<Option<&str>> = if clear_file {
                Some(None)
            } else {
                file.as_deref().map(Some)
            };
            if title.is_none() && body.is_none() && file_update.is_none() {
                eprintln!(
                    "{}",
                    darkmux_types::style::error(
                        "nothing to edit — pass at least one of --title / --body / --file / --clear-file"
                    )
                );
                return Ok(2);
            }
            let conn = lessons::open_at(&path)?;
            let changed = lessons::edit(
                &conn,
                id,
                title.as_deref(),
                body.as_deref(),
                file_update,
                None,
            )?;
            if changed {
                println!(
                    "{}",
                    darkmux_types::style::success(&format!("edited lesson #{id} ({tier})"))
                );
                Ok(0)
            } else {
                eprintln!(
                    "{}",
                    darkmux_types::style::error(&format!(
                        "no lesson #{id} in the {tier} store (ids are per-tier — try --global?)"
                    ))
                );
                Ok(1)
            }
        }
        LessonsCmd::Remove { id, global } => {
            let (path, tier) = lessons_tier(global);
            let conn = lessons::open_at(&path)?;
            if lessons::remove(&conn, id)? {
                println!(
                    "{}",
                    darkmux_types::style::success(&format!("removed lesson #{id} ({tier})"))
                );
                Ok(0)
            } else {
                eprintln!(
                    "{}",
                    darkmux_types::style::error(&format!(
                        "no lesson #{id} in the {tier} store (ids are per-tier — try --global?)"
                    ))
                );
                Ok(1)
            }
        }
        LessonsCmd::Export { global } => {
            let (path, _) = lessons_tier(global);
            // export reads the store; if absent, emit an empty envelope rather
            // than creating the db (a read must not write). Build the envelope
            // through `LessonsExport` so the wire shape is single-sourced with
            // `import_json` — the two can't drift.
            let env = lessons::LessonsExport {
                schema_version: lessons::LESSONS_SCHEMA_VERSION,
                lessons: lessons::load_entries_best_effort(&path),
            };
            println!("{}", serde_json::to_string_pretty(&env)?);
            Ok(0)
        }
        LessonsCmd::Import { file, global } => {
            let (path, tier) = lessons_tier(global);
            let data = match file {
                Some(p) => std::fs::read_to_string(&p)
                    .with_context(|| format!("reading {}", p.display()))?,
                None => {
                    use std::io::Read;
                    let mut buf = String::new();
                    std::io::stdin()
                        .read_to_string(&mut buf)
                        .context("reading lessons import from stdin")?;
                    buf
                }
            };
            let mut conn = lessons::open_at(&path)?;
            let stats = lessons::import_json(&mut conn, &data)?;
            println!(
                "{}",
                darkmux_types::style::success(&format!(
                    "imported into {tier}: {} inserted, {} updated",
                    stats.inserted, stats.updated
                ))
            );
            Ok(0)
        }
        LessonsCmd::Recall {
            term,
            file,
            json: cli::JsonFlagPlain { json },
        } => {
            let repo_path = lessons::repo_db_path();
            let global_path = lessons::global_db_path();
            let recall_tier = |path: &std::path::Path| -> Vec<lessons::Lesson> {
                if !path.exists() {
                    return Vec::new();
                }
                lessons::open_at(path)
                    .and_then(|conn| lessons::recall(&conn, term.as_deref(), file.as_deref()))
                    .unwrap_or_default()
            };
            let repo = recall_tier(&repo_path);
            let global = if global_path == repo_path {
                Vec::new()
            } else {
                recall_tier(&global_path)
            };
            if json {
                let out = serde_json::json!({ "repo": repo, "global": global });
                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(0);
            }
            if repo.is_empty() && global.is_empty() {
                println!("{}", darkmux_types::style::dim("no lessons match"));
                return Ok(0);
            }
            print_lessons_tier("repo (this engagement)", &repo);
            print_lessons_tier("global (all engagements)", &global);
            Ok(0)
        }
    }
}

/// Resolve the `(db path, label)` for a lessons tier — repo (this engagement)
/// by default, user-global when `--global`. Shared by the mutating verbs.
fn lessons_tier(global: bool) -> (std::path::PathBuf, &'static str) {
    use darkmux_crew::lessons;
    if global {
        (lessons::global_db_path(), "global")
    } else {
        (lessons::repo_db_path(), "repo")
    }
}

/// (#1426, decision 17) `memory correction list` — the first verb #849's
/// persisted adjudication corrections have ever had. Read-only: corrections are
/// recorded by the review path as flow notes, never authored here.
///
/// Reads through `crew::corrections::scan`, the SAME definition the coder-brief
/// injection reads, so `--mission` shows precisely the set that mission's next
/// brief would carry — the verb can't drift from the behavior it reports on.
fn cmd_correction(sub: CorrectionCmd) -> Result<i32> {
    match sub {
        CorrectionCmd::List {
            mission,
            session,
            days,
            json: cli::JsonFlagPlain { json },
        } => {
            // Resolve the scope. `None` = every session in the window; a
            // mission resolves to its EXACT dispatch session ids (the same
            // construction the brief uses — never a prefix, which would bleed a
            // sibling mission whose id is a hyphen-extension, #849).
            let scope: Option<std::collections::HashSet<String>> = match (&mission, &session) {
                (Some(mid), _) => {
                    fleet::validate_identifier("mission", mid)?;
                    let missions = crew::loader::load_missions()?;
                    let m = missions.iter().find(|m| &m.id == mid).ok_or_else(|| {
                        anyhow::anyhow!(
                            "mission `{mid}` not found (check `darkmux mission status`)"
                        )
                    })?;
                    Some(
                        m.phase_ids
                            .iter()
                            .map(|pid| darkmux_types::session_id::mission_run(mid, pid))
                            .collect(),
                    )
                }
                (None, Some(sid)) => Some(std::iter::once(sid.clone()).collect()),
                (None, None) => None,
            };

            let found = crew::corrections::scan(days, scope.as_ref());

            if json {
                println!("{}", serde_json::to_string_pretty(&found)?);
                return Ok(0);
            }
            if found.is_empty() {
                let scoped = match (&mission, &session) {
                    (Some(m), _) => format!(" for mission `{m}`"),
                    (None, Some(s)) => format!(" for session `{s}`"),
                    (None, None) => String::new(),
                };
                println!(
                    "{}",
                    darkmux_types::style::dim(&format!(
                        "no adjudication corrections recorded{scoped} in the last {days} day(s) \
                         — your reviewer records them with darkmux flow note --session-id <sid> \
                         --text \"<verdict · what you overrode · why>\" --source adjudication"
                    ))
                );
                return Ok(0);
            }
            println!(
                "{}",
                darkmux_types::style::header(&format!(
                    "adjudication corrections (last {days} day(s))"
                ))
            );
            for c in &found {
                println!(
                    "  {} {}",
                    darkmux_types::style::accent(&c.ts),
                    darkmux_types::style::dim(&format!("[{}]", c.session_id))
                );
                println!("    {}", c.text);
            }
            Ok(0)
        }
    }
}

fn print_lessons_tier(label: &str, entries: &[darkmux_crew::lessons::Lesson]) {
    if entries.is_empty() {
        return;
    }
    println!("{}", darkmux_types::style::header(label));
    for e in entries {
        let scope = e
            .file
            .as_deref()
            .map(|f| format!(" [{f}]"))
            .unwrap_or_default();
        println!(
            "  {}{}",
            darkmux_types::style::accent(&e.title),
            darkmux_types::style::dim(&scope)
        );
        println!("    {}", e.body);
    }
}

// (#1426) `pub(crate)` so `lab_cli::cmd_lab` can dispatch `lab notebook …`
// — the notebook family folded into `lab` (the retired top-level `notebook`
// verb).
pub(crate) fn cmd_notebook(sub: NotebookCmd) -> Result<i32> {
    match sub {
        NotebookCmd::Draft {
            run_id,
            role,
            slug,
            dry_run,
            machine,
        } => {
            let report = notebook::draft_entry(&notebook::DraftOptions {
                run_id,
                role,
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
            // env > config.dirs.notebook > <root>/notebook (#661 Slice 3).
            let notebook_dir = darkmux_types::config_access::notebook_dir();
            if !notebook_dir.exists() {
                // (#895) An absent notebook dir is "nothing to list", not an
                // error — a fresh user, or `notebook list && …` chaining,
                // must not see a false failure. Mirrors the empty-dir case
                // below (also Ok(0)).
                println!("no notebook directory yet: {}", notebook_dir.display());
                return Ok(0);
            }
            let entries = notebook::list_entries(&notebook_dir, machine.as_deref())?;
            if entries.is_empty() {
                println!("no notebook entries found");
                return Ok(0);
            }
            // Column widths (dynamic based on longest value).
            let max_date: usize = entries.iter().map(|e| e.date.len()).max().unwrap_or(10);
            let max_machine: usize = entries.iter().map(|e| e.machine.len()).max().unwrap_or(10);
            let max_run: usize = entries.iter().map(|e| e.run.len()).max().unwrap_or(12);
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

fn cmd_doctor(verbose: bool, probe: bool) -> Result<i32> {
    let mut report = doctor::run();

    // (#1426) Installed darkmux-* skills freshness. The check lives in the
    // doctor crate as a pure evaluator; the embedded reference set and install
    // targets are supplied here from the root crate that owns the `include_str!`
    // skill embed (the doctor crate can't depend on this binary crate). Appended
    // after `run()` — the same shape as the endpoint probes below, but taking an
    // input rather than reading it for itself.
    let embedded_skills: Vec<doctor::EmbeddedSkill> = skills::embedded_skills()
        .iter()
        .map(|(name, content)| doctor::EmbeddedSkill {
            name: (*name).to_string(),
            content: (*content).to_string(),
        })
        .collect();
    let skill_targets = skills::install_target_dirs().unwrap_or_default();
    report.checks.push(doctor::check_installed_skills_freshness(
        &skill_targets,
        &embedded_skills,
    ));

    // (#1177) Opt-in live endpoint probes append to the same report so they
    // share the verdict/exit-code path — a failed probe exits 1 like any
    // failed check.
    let probe_checks = if probe {
        doctor::probe_remote_endpoints()
    } else {
        Vec::new()
    };
    report.checks.extend(probe_checks);
    doctor::print_report(&report, verbose)?;

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
    let llms: Vec<&lms::ModelMeta> = available.iter().filter(|m| m.model_type == "llm").collect();

    let uncovered: Vec<&lms::ModelMeta> = llms
        .iter()
        .filter(|m| !covered.contains(&m.model_key))
        .copied()
        .collect();

    println!(
        "{}",
        darkmux_types::style::header(&format!(
            "darkmux profile scan — {} model(s) in LMStudio, {} not yet in any profile",
            llms.len(),
            uncovered.len()
        ))
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

        let icon = if m.trained_for_tool_use {
            darkmux_types::style::success("✓")
        } else {
            darkmux_types::style::warn("⚠")
        };
        println!("{} {}", icon, darkmux_types::style::accent(&display));
        println!("    {}",
            darkmux_types::style::dim(&format!(
                "id={}  params={}  arch={:?}  size={:.1}GB  maxCtx={}",
                m.model_key,
                m.params_string.as_deref().unwrap_or("?"),
                arch,
                size_gb,
                m.max_context_length.unwrap_or(0)
            ))
        );
        println!(
            "    suggested task class: `{}` (n_ctx={}, compactor={})",
            darkmux_types::style::accent(suggested_class.as_str()),
            suggestion.primary_n_ctx,
            suggestion
                .compactor
                .as_ref()
                .map(|c| format!("{} @ {}", c.model_id, c.n_ctx))
                .unwrap_or_else(|| "none".into())
        );
        if !m.trained_for_tool_use {
            println!("    {}", darkmux_types::style::warn(
                "⚠ NOT marked trainedForToolUse — agentic dispatch may be unreliable"
            ));
        }
        let safe_name = derive_profile_name(&m.model_key, suggested_class);
        if name_collisions.get(&safe_name).copied().unwrap_or(0) > 1 {
            println!(
                "    {}",
                darkmux_types::style::warn(&format!(
                    "⚠ derived name `{safe_name}` collides with another uncovered model — \
                     customize the name when drafting (publisher prefix gets stripped)"
                ))
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
        .map(|c| {
            if c == '_' || c == ' ' {
                '-'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    let safe_base = if trimmed.is_empty()
        || !trimmed
            .chars()
            .next()
            .map(|c| c.is_ascii_alphanumeric())
            .unwrap_or(false)
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
        RoleCmd::List {
            json: cli::JsonFlag { json },
        } => role_cli::role_list(json),
        RoleCmd::Show {
            id,
            json: cli::JsonFlag { json },
        } => role_cli::role_show(&id, json),
    }
}

fn cmd_phase(sub: PhaseCmd) -> Result<i32> {
    match sub {
        PhaseCmd::Estimate { spec, narrate } => phase_cli::estimate(&spec, narrate),
        PhaseCmd::Review {
            base,
            require_clean,
            phase_id,
        } => {
            let sid = phase_id.as_deref();
            phase_cli::phase_review(base.as_deref(), require_clean, sid)
        }
        PhaseCmd::Start { id } => {
            let s = crew::lifecycle::phase_start(&id)?;
            println!(
                "phase `{}` → Running  started_ts={}",
                s.id,
                s.started_ts.unwrap_or(0)
            );
            Ok(0)
        }
        PhaseCmd::Complete { id } => {
            let s = crew::lifecycle::phase_complete(&id)?;
            let started = s.started_ts.unwrap_or(0);
            let completed = s.completed_ts.unwrap_or(0);
            let dur = completed.saturating_sub(started);
            println!(
                "phase `{}` → Complete  duration={}s  completed_ts={}",
                s.id, dur, completed
            );
            Ok(0)
        }
        PhaseCmd::Abandon { id } => {
            let s = crew::lifecycle::phase_abandon(&id)?;
            println!(
                "phase `{}` → Abandoned  abandoned_ts={}",
                s.id,
                s.abandoned_ts.unwrap_or(0)
            );
            Ok(0)
        }
    }
}

fn cmd_mission(sub: MissionCmd) -> Result<i32> {
    match sub {
        MissionCmd::Status { json } => mission_status::run(json),
        MissionCmd::Debrief { id, json } => coder_phase::debrief(&id, json),
        MissionCmd::Start { id, reasoning } => {
            let m = crew::lifecycle::mission_start_with_reasoning(&id, reasoning.as_deref())?;
            println!(
                "mission `{}` → Active  started_ts={}",
                m.id,
                m.started_ts.unwrap_or(0)
            );
            Ok(0)
        }
        MissionCmd::Close { id, reasoning } => {
            let m = crew::lifecycle::mission_close_with_reasoning(&id, reasoning.as_deref())?;
            let started = m.started_ts.unwrap_or(0);
            let closed = m.closed_ts.unwrap_or(0);
            let dur = closed.saturating_sub(started);
            println!(
                "mission `{}` → Closed  duration={}s  closed_ts={}",
                m.id, dur, closed
            );
            // (#1000) The mission's done — prompt the debrief ceremony so its
            // transient signal (cautions + corrections) becomes durable lessons
            // for the next crew. Emits Stage::Debrief; never blocks the close.
            coder_phase::nudge_mission_debrief(&m.id);
            Ok(0)
        }
        MissionCmd::Pause { id, reasoning } => {
            let m = crew::lifecycle::mission_pause_with_reasoning(&id, reasoning.as_deref())?;
            println!(
                "mission `{}` → Paused  paused_ts={}",
                m.id,
                m.paused_ts.unwrap_or(0)
            );
            Ok(0)
        }
        MissionCmd::Resume { id, reasoning } => {
            let m = crew::lifecycle::mission_resume_with_reasoning(&id, reasoning.as_deref())?;
            println!(
                "mission `{}` → Active  (paused_ts preserved: {})",
                m.id,
                m.paused_ts.unwrap_or(0)
            );
            Ok(0)
        }
        MissionCmd::Propose {
            from_stdin,
            from_file,
            yes,
            start,
            ticket,
        } => mission_propose::propose(from_stdin, from_file.as_deref(), yes, start, ticket.as_deref()),
        MissionCmd::Launch { config_id, input, params, timeout } => {
            mission_launch::launch(&config_id, input.as_deref(), &params, timeout)
        }
        MissionCmd::AddPhase {
            mission_id,
            phase_id,
            description,
            after,
            reasoning,
        } => {
            let s = crew::lifecycle::add_phase_to_mission_with_reasoning(
                &mission_id,
                &phase_id,
                &description,
                after.as_deref(),
                reasoning.as_deref(),
            )?;
            let position = match after.as_deref() {
                Some(a) => format!(" (after `{a}`)"),
                None => String::new(),
            };
            println!(
                "mission `{}` ← added phase `{}`{}",
                mission_id, s.id, position
            );
            Ok(0)
        }
        MissionCmd::Migrate { apply } => {
            let plan = migrate::plan_migration()?;
            migrate::print_plan(&plan);
            if !apply {
                if !plan.is_empty() {
                    println!("\nRe-run with --apply to commit.");
                }
                return Ok(0);
            }
            let synthesized = migrate::apply_migration(&plan)?;
            if !plan.is_empty() {
                println!(
                    "\nmigrate: applied {} move(s), synthesized {} of {} config-snapshot(s).",
                    plan.mission_moves.len() + plan.phase_moves.len(),
                    synthesized,
                    plan.config_snapshots_missing.len()
                );
            }
            Ok(0)
        }
        MissionCmd::Dispatch {
            mission_id,
            role,
            machine,
            timeout,
            no_wait,
        } => cmd_mission_dispatch(&mission_id, &role, machine.as_deref(), timeout, !no_wait),
        MissionCmd::Abort {
            mission_id,
            phase,
        } => coder_phase::abort(&mission_id, phase.as_deref()),
        MissionCmd::Ship {
            mission_id,
            phase,
            base,
            wait_ci,
            merge,
        } => coder_phase::ship(&mission_id, phase.as_deref(), &base, wait_ci, merge),
    }
}

fn cmd_mission_dispatch(
    mission_id: &str,
    role_id: &str,
    machine: Option<&str>,
    timeout_seconds: u32,
    wait: bool,
) -> Result<i32> {
    use crew::loader::{load_missions, load_roles, load_phases};

    // 0. CLI-boundary charset validation (Wave-E.5 #255 — security-
    //    auditor MEDIUM from PR-D.1 review). `mission_id` flows into
    //    the session_id format string + WorkJob payload + audit chain
    //    + future "look up by mission" filters; charset enforcement
    //    at the boundary protects all current AND future use of the
    //    value. Rejects path-traversal, special chars, over-long ids.
    fleet::validate_identifier("mission_id", mission_id)?;
    fleet::validate_identifier("role_id", role_id)?;
    if let Some(m) = machine {
        fleet::validate_identifier("--machine", m)?;
    }

    // 1. Validate the mission exists.
    let missions = load_missions()?;
    let mission = missions
        .iter()
        .find(|m| m.id == mission_id)
        .ok_or_else(|| {
            anyhow::anyhow!(
            "mission `{mission_id}` not found. Run `darkmux mission propose` then `darkmux mission launch <config-id>` first, or check the id."
        )
        })?;
    if !matches!(mission.status, crew::types::MissionStatus::Active) {
        eprintln!(
            "darkmux mission dispatch: warning — mission `{mission_id}` status is {:?}, not Active. \
             Proceeding anyway (operator-explicit override).",
            mission.status
        );
    }

    // 2. Confirm the role exists before fanning out phases. After #590
    //    there is no tier requirement — phases fan onto the single
    //    global `darkmux:work` stream and the first available runner
    //    claims each one.
    let roles = load_roles()?;
    if !roles.iter().any(|r| r.id == role_id) {
        anyhow::bail!("role `{role_id}` not found");
    }

    // 3. Find the next runnable phase in THIS mission. (#1341) Phases are
    //    strictly linear — ordered purely by position in
    //    `Mission.phase_ids`, no `depends_on` of their own — so "ready"
    //    is a linear scan: the FIRST phase in that order whose status is
    //    `Planned` AND every phase before it is `Complete`. Replaces the
    //    historical `crew::scheduler::is_ready`/`PhaseNode` graph check
    //    (itself a replacement for the even older flat
    //    `depends_on.is_empty()` filter) — a strictly-linear Phase list
    //    has at most ONE next-runnable phase per mission, so `initial`
    //    below holds 0 or 1 entries, never a real "fan-out" across
    //    parallel phases (that capability lives at the Task level now,
    //    within `mission run`'s own graph — see `types::Phase`'s doc).
    //    `Running` phases are naturally excluded (not `Planned`). Wave-E.3
    //    state-machine gate unchanged: the filtered phase goes through
    //    `lifecycle::phase_start` BEFORE publish, flipping Planned →
    //    Running, so a second `mission dispatch` invocation finds nothing
    //    dispatchable and bails with exit 2.
    let phases = load_phases()?;
    let phase_by_id: std::collections::BTreeMap<String, &crew::types::Phase> =
        phases.iter().map(|p| (p.id.clone(), p)).collect();
    let mut initial: Vec<&crew::types::Phase> = Vec::new();
    let mut all_prior_complete = true;
    for phase_id in &mission.phase_ids {
        let Some(phase) = phase_by_id.get(phase_id) else { continue };
        if all_prior_complete && phase.status == crew::types::PhaseStatus::Planned {
            initial.push(phase);
            break;
        }
        if phase.status != crew::types::PhaseStatus::Complete {
            all_prior_complete = false;
        }
    }

    if initial.is_empty() {
        eprintln!(
            "darkmux mission dispatch: no runnable phase in mission `{mission_id}` — either \
             every phase is already Running/Complete/Abandoned, or the next Planned phase is \
             blocked on an incomplete predecessor. Nothing to fan out. (Running phases from a \
             previous dispatch must be `darkmux phase complete` or `phase abandon` before \
             they're eligible again.)"
        );
        return Ok(2);
    }

    // 3b. Flip each filtered phase Planned → Running BEFORE publishing.
    //     If a phase flipped between the filter and this call (unlikely
    //     in single-operator scenarios but possible under racing CLIs),
    //     `phase_start` bails on already-Running; skip and warn.
    let mut started: Vec<&crew::types::Phase> = Vec::with_capacity(initial.len());
    for phase in &initial {
        match crew::lifecycle::phase_start(&phase.id) {
            Ok(_) => started.push(*phase),
            Err(e) => {
                eprintln!(
                    "darkmux mission dispatch: skipping phase `{}` — phase_start failed: {e:#}",
                    phase.id
                );
            }
        }
    }
    if started.is_empty() {
        eprintln!(
            "darkmux mission dispatch: no phases survived phase_start (all were \
             already Running/Complete). Nothing to fan out."
        );
        return Ok(2);
    }

    // 4. Redis required for cross-machine fan-out. env(DARKMUX_REDIS_URL) >
    //    config-assembled (#661 Slice 5).
    let raw_url = flow::redis_url().ok_or_else(|| anyhow::anyhow!(
        "mission dispatch requires Redis (DARKMUX_REDIS_URL or config.redis.enabled) — the fleet work queue lives on Redis."
    ))?;
    let client = redis::Client::open(raw_url.expose_for_probe())
        .with_context(|| format!("opening Redis client {raw_url} for mission dispatch"))?;

    // 5. Build + pre-validate all WorkJobs BEFORE publishing any
    //    (HIGH-2 from review). All-or-nothing semantics: if any phase
    //    would trip validate() (oversize description, etc.), the
    //    operator finds out before ANY orphan job lands on Redis.
    //    Loop-index suffix on session_id defeats microsecond collisions
    //    under sub-microsecond loop iterations (review M-session-id).
    let local_machine = flow::resolve_machine_id();
    let local_orchestrator = flow::resolve_orchestrator();
    let dispatch_micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    let mut jobs: Vec<(String, String, fleet::WorkJob)> = Vec::new(); // (phase_id, session_id, job)
    for (idx, phase) in started.iter().enumerate() {
        let session_id =
            darkmux_types::session_id::mission_phase_dispatch(mission_id, &phase.id, dispatch_micros, idx);
        let job = fleet::build_work_job(
            machine.map(String::from),
            role_id.to_string(),
            phase.description.clone(),
            session_id.clone(),
            None,
            Some(phase.id.clone()),
            None, // image (#703 Slice 4) — mission dispatch uses the runner's default
            timeout_seconds,
            local_machine.clone(),
            local_orchestrator.clone(),
        );
        // Pre-validate. Surfaces oversize/charset failures BEFORE any
        // partial publish lands on the queue.
        job.validate()
            .with_context(|| format!("pre-publish validation failed for phase `{}`", phase.id))?;
        jobs.push((phase.id.clone(), session_id, job));
    }

    // 6. Publish. Capture sessions for wait-aggregation. If a mid-loop
    //    publish fails (Redis network blip), the operator gets the
    //    list of already-published (phase_id, session_id, work_id)
    //    triples on stderr so they can dedup / clean up via flow tail.
    eprintln!(
        "darkmux mission dispatch: mission={mission_id} role={role_id} \
         phases={} target_machine={}",
        jobs.len(),
        machine.unwrap_or("<any>")
    );
    let mut sessions: Vec<(String, String, String)> = Vec::new(); // (phase_id, session_id, work_id)
    for (phase_id, session_id, job) in &jobs {
        match fleet::publish_job(&client, job) {
            Ok(work_id) => {
                eprintln!("  phase={phase_id} work_id={work_id} session={session_id}");
                sessions.push((phase_id.clone(), session_id.clone(), work_id));
            }
            Err(e) => {
                eprintln!(
                    "\ndarkmux mission dispatch: ERROR — publish failed for phase `{phase_id}` \
                     after {} successful publishes. Already-published jobs are in flight on runners:",
                    sessions.len()
                );
                for (sid, sess, wid) in &sessions {
                    eprintln!("  ORPHAN phase={sid} session={sess} work_id={wid}");
                }
                eprintln!(
                    "Tail each via `darkmux flow tail --session <id>` OR \
                     XRANGE darkmux:flow against the published session_ids to track completion. \
                     Do NOT re-run mission dispatch without checking — re-publish would \
                     double-fire the orphans."
                );
                return Err(e)
                    .with_context(|| format!("publishing phase `{phase_id}` as WorkJob"));
            }
        }
    }

    if !wait {
        println!(
            "Published {} phase job(s); operator polls for completion via flow stream.",
            sessions.len()
        );
        for (phase_id, session_id, work_id) in &sessions {
            println!("  phase={phase_id} session_id={session_id} work_id={work_id}");
        }
        return Ok(0);
    }

    // 6. Wait for each completion. Sequential polling is correct (XRANGE
    //    full-scan returns ALL records; finding session A doesn't preclude
    //    finding session B in the same pass). Net wall-clock is bounded by
    //    the slowest phase's completion.
    let wait_timeout = std::time::Duration::from_secs((timeout_seconds as u64).saturating_add(60));
    eprintln!(
        "\n{}",
        worst_case_wait_banner(sessions.len(), timeout_seconds, wait_timeout.as_secs())
    );
    let mut completed: usize = 0;
    let mut failures: usize = 0;
    let mission_start = std::time::Instant::now();
    let mut sum_phase_wall_ms: u64 = 0;
    for (phase_id, session_id, _work_id) in &sessions {
        match fleet::wait_for_completion(&raw_url, session_id, wait_timeout) {
            Ok(c) => {
                completed += 1;
                if c.result_class != "ok" {
                    failures += 1;
                }
                if let Some(ms) = c.wall_ms {
                    sum_phase_wall_ms += ms;
                }
                eprintln!(
                    "  ✓ phase={phase_id} result={} wall_ms={:?}",
                    c.result_class, c.wall_ms
                );
            }
            Err(e) => {
                failures += 1;
                eprintln!("  ✗ phase={phase_id} wait error: {e:#}");
            }
        }
    }
    let mission_wall_ms = mission_start.elapsed().as_millis() as u64;

    // Empirical parallelism check (#246 Q3 risk #3): if total wall-clock
    // is meaningfully less than sum of phase wall-clocks, dispatches
    // ran in parallel. Otherwise they were serial under the hood.
    println!(
        "\nmission dispatch: completed={completed}/{} failures={failures} \
         wall_ms={mission_wall_ms} sum_phase_wall_ms={sum_phase_wall_ms}",
        sessions.len()
    );
    match speedup_verdict(sum_phase_wall_ms, mission_wall_ms, sessions.len()) {
        SpeedupVerdict::ParallelConfirmed { speedup } => println!(
            "  → wall-clock indicates parallel execution: {speedup:.2}× speedup vs the \
             sum of per-phase wall_ms (runner self-reported; not authenticated)."
        ),
        SpeedupVerdict::SeriallySuspect { speedup } => println!(
            "  ⚠ wall_ms ≈ sum of phase wall_ms ({speedup:.2}×) — phases may have run \
             serially. Check fleet roster + runner reachability."
        ),
        SpeedupVerdict::Inconclusive => {}
    }

    if failures > 0 {
        Ok(1)
    } else {
        Ok(0)
    }
}

/// Render the operator-facing "waiting for N completion(s)" banner with
/// the worst-case wall-clock bound named up front (Wave-E.9 #255). The
/// wait loop is sequential-per-phase, so worst case is
/// `N × (per_phase_timeout + slack)`. Surfacing this lets the operator
/// decide whether to SIGINT before the second per-phase timeout if the
/// first phase hangs — closes the PR-D.1 review MEDIUM where the
/// unbounded total wait could quietly run hours.
pub fn worst_case_wait_banner(
    n_sessions: usize,
    per_phase_timeout_seconds: u32,
    wait_timeout_seconds: u64,
) -> String {
    let worst_case_secs = (n_sessions as u64).saturating_mul(wait_timeout_seconds);
    format!(
        "darkmux mission dispatch: waiting for {n_sessions} completion(s) \
         (per-phase timeout {per_phase_timeout_seconds}s + 60s slack; \
         worst-case total wall ≈ {worst_case_secs}s = {worst_case_min}min). \
         SIGINT cleanly aborts.",
        worst_case_min = worst_case_secs / 60,
    )
}

/// Minimum speedup ratio (sum_phase_wall_ms / mission_wall_ms) at which
/// the mission-dispatch summary asserts "parallel execution." Below this,
/// the metric is reported with a serially-suspect warning OR nothing
/// (n=1 case). 1.5 is a conservative threshold for 2-machine fleets —
/// noise and per-phase setup overhead can push a truly-parallel run
/// below 2.0× speedup. Adjust upward if false-positives appear.
const PARALLELISM_CONFIRMED_THRESHOLD: f64 = 1.5;

/// Verdict from the empirical parallelism metric computed at the end of
/// `mission dispatch --wait`. Extracted as a pure function (#255 Wave-E.4)
/// so the math + thresholding are unit-testable independent of the rest
/// of the dispatch handler.
#[derive(Debug, PartialEq)]
pub enum SpeedupVerdict {
    /// Wall-clock indicates parallel execution: speedup ≥
    /// `PARALLELISM_CONFIRMED_THRESHOLD` AND more than one phase
    /// completed. Caller renders an operator-facing
    /// "parallel execution: Nx speedup" line.
    ParallelConfirmed { speedup: f64 },
    /// Wall-clock ≈ sum-of-phases (`speedup < threshold`) with multiple
    /// phases — phases may have run serially under the hood. Caller
    /// renders the operator-warning line pointing at fleet roster +
    /// runner reachability.
    SeriallySuspect { speedup: f64 },
    /// Insufficient data to assert parallel vs serial: either zero
    /// phases completed (`sum_phase_wall_ms == 0`) OR exactly one
    /// phase (parallelism is undefined for n=1). Caller stays silent.
    Inconclusive,
}

/// Pure-function speedup verdict computation. Inputs are the metric
/// summaries collected during `mission dispatch --wait`:
///
/// - `sum_phase_wall_ms` — sum of `wall_ms` from each
///   `dispatch.complete` flow record. Runner self-reported.
/// - `mission_wall_ms` — wall time from `mission dispatch` invocation
///   to the last completion seen, measured by the publisher.
/// - `n_phases` — number of phases dispatched (sessions.len()).
pub fn speedup_verdict(
    sum_phase_wall_ms: u64,
    mission_wall_ms: u64,
    n_phases: usize,
) -> SpeedupVerdict {
    if sum_phase_wall_ms == 0 || n_phases == 0 {
        return SpeedupVerdict::Inconclusive;
    }
    // Avoid divide-by-zero on instantaneous missions; the `.max(1.0)`
    // floor doesn't materially change any non-degenerate case.
    let speedup = (sum_phase_wall_ms as f64) / (mission_wall_ms as f64).max(1.0);
    if n_phases > 1 && speedup >= PARALLELISM_CONFIRMED_THRESHOLD {
        SpeedupVerdict::ParallelConfirmed { speedup }
    } else if n_phases > 1 {
        SpeedupVerdict::SeriallySuspect { speedup }
    } else {
        // n == 1: parallelism is undefined for a single phase. Stay
        // silent even if the math says speedup >= threshold (which can
        // only happen via clock skew or wall_ms misreporting).
        SpeedupVerdict::Inconclusive
    }
}

/// (#1426) Owned fields of the top-level `dispatch` verb — a plain carrier so
/// the handler takes ONE argument (clippy `too_many_arguments`) rather than
/// the fifteen the clap variant unpacks into.
struct DispatchInvocation {
    role: String,
    message: Option<String>,
    message_from_file: Option<std::path::PathBuf>,
    profile: Option<String>,
    session_id: Option<String>,
    timeout: u32,
    workdir: Option<std::path::PathBuf>,
    phase_id: Option<String>,
    skip_preflight: bool,
    json: bool,
    machine: Option<String>,
    no_wait: bool,
    image: Option<String>,
    max_completion_tokens: Option<u32>,
}

/// (#1426) `darkmux dispatch <role> [MESSAGE]` — the task-grain execution
/// entry, promoted from the retired `dispatch`. The plumbing is unchanged
/// (`fleet::dispatch_routed`); only the message source is reshaped: the message
/// is a positional argument, falls back to stdin when omitted, and can still be
/// read from a file via `--message-from-file`.
fn cmd_dispatch(inv: DispatchInvocation) -> Result<i32> {
    let DispatchInvocation {
        role,
        message,
        message_from_file,
        profile,
        session_id,
        timeout,
        workdir,
        phase_id,
        skip_preflight,
        json,
        machine,
        no_wait,
        image,
        max_completion_tokens,
    } = inv;
    // (#1426) Resolve the message in precedence order: positional MESSAGE >
    // `--message-from-file` > stdin. clap makes the positional and the file
    // flag mutually exclusive, so at most one of the first two is present.
    // When neither is given, read stdin to EOF byte-faithfully (no trim on the
    // delivered message) so pipe composition works:
    // `git diff | darkmux dispatch pr-reviewer`. A terminal stdin with no
    // piped input would block forever waiting for EOF, so refuse loudly with
    // usage guidance instead of hanging. An empty or whitespace-only source
    // (empty `git diff`, a stray `echo |`, a blank brief file) likewise bails
    // loudly rather than burning a container run on a blank brief — the trim
    // is only the emptiness CHECK; real content is passed through unmodified.
    let message = match (message, message_from_file) {
        (Some(m), _) => m,
        (None, Some(path)) => {
            let m = std::fs::read_to_string(&path)
                .with_context(|| format!("reading --message-from-file {}", path.display()))?;
            if m.trim().is_empty() {
                anyhow::bail!(
                    "--message-from-file {} is empty (or whitespace-only) — refusing to \
                     dispatch a blank brief. Write the message into the file, or pass it \
                     as the positional MESSAGE argument.",
                    path.display()
                );
            }
            m
        }
        (None, None) => {
            use std::io::{IsTerminal, Read};
            if std::io::stdin().is_terminal() {
                anyhow::bail!(
                    "no message given. Pass it as the positional MESSAGE argument \
                     (`darkmux dispatch {role} \"<message>\"`), pipe it on stdin \
                     (`git diff | darkmux dispatch {role}`), or use \
                     `--message-from-file <path>`. For a message that begins with \
                     `-`, use the `--` separator: `darkmux dispatch {role} -- <message>`."
                );
            }
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading the dispatch message from stdin")?;
            if buf.trim().is_empty() {
                anyhow::bail!(
                    "stdin was empty — the pipe produced no message (empty git diff?). \
                     Pass a positional MESSAGE, pipe non-empty content, or use \
                     --message-from-file."
                );
            }
            buf
        }
    };
    let opts = crew::dispatch::DispatchOpts {
        role_id: role,
        message,
        session_id,
        timeout_seconds: timeout,
        skip_preflight,
        json,
        workdir,
        phase_id,
        machine,
        wait: !no_wait,
        // A bare `dispatch` carries no profile-derived compaction config here;
        // the internal dispatch fills the runtime-required context window from
        // the resolved `default_profile` (#632 — the runtime has no built-in
        // context-window default), so a `default()` is safe. Lab + phase paths
        // derive the full compaction config from the profile up front.
        compaction: crew::dispatch::CompactionDispatchArgs::default(),
        // (#1054) `--profile <name>` selects a named profile from the machine's
        // registry; when omitted (None) or undefined on this machine, model +
        // context-window resolution fall back to the registry's
        // `default_profile`. The lab path passes its own resolved name.
        profile_name: profile,
        // (#984) No --profiles-file here; dispatch resolves env > default.
        config_path: None,
        // (#1199) force_container stays programmatic (bench-only); the
        // completion cap is operator-facing for long single-shot outputs
        // (e.g. many-finding hosted reviews).
        force_container: false,
        max_completion_tokens,
        // (#703) operator-selected dispatch image; darkmux injects its runtime
        // binary into it when it's not the default.
        image,
        // Mock-model harness (v1): no CLI surface yet (deliberately — see
        // darkmux's CLAUDE.md doctrine on shipping the underlying mechanism
        // before the CLI verb). `None` on every operator-facing dispatch.
        model_base_url_override: None,
    };
    let result = fleet::dispatch_routed(opts)?;
    // Announce the resolved session id on stderr so operators can correlate
    // this dispatch with the flow stream — without polluting the --json
    // envelope on stdout that orchestrators parse.
    eprintln!("darkmux dispatch: session id `{}`", result.session_id);
    print!("{}", result.stdout);
    if !result.stderr.is_empty() {
        eprint!("{}", result.stderr);
    }
    Ok(result.exit_code)
}

/// (#1426) The `machine` family — this host's AI state. Bare `machine` (and
/// `machine status`) route here; reads may target a roster peer over its
/// serve daemon, but mutations (`machine eject`) stay local by construction.
fn cmd_machine(sub: Option<MachineCmd>) -> Result<i32> {
    match sub {
        // Bare `darkmux machine` — the at-a-glance health view IS status.
        None => cmd_machine_status(None, None, false),
        Some(MachineCmd::Status {
            id,
            profiles: cli::ProfilesFileArg { profiles },
            json: cli::JsonFlag { json },
        }) => cmd_machine_status(id.as_deref(), profiles.as_deref(), json),
        Some(MachineCmd::Resources { id, json }) => cmd_machine_resources(id.as_deref(), json),
        Some(MachineCmd::Eject { dry_run }) => cmd_model_eject(dry_run),
        Some(MachineCmd::List { json, deep }) => fleet_cli::cmd_machine_list(json, deep),
        Some(MachineCmd::Add {
            id,
            address,
            description,
        }) => fleet_cli::cmd_machine_add(&id, &address, description.as_deref()),
        Some(MachineCmd::Remove { id }) => fleet_cli::cmd_machine_remove(&id),
    }
}

/// `darkmux machine resources` (#1286, renamed from `model ledger` in #1426)
/// — the no-viewer twin of the machine lens: one bounded gather (lms metadata
/// and kernel counters, zero model dispatches), rendered as a table or emitted
/// as the same JSON shape the serve daemon's /machine/resources returns. With
/// a roster `id`, reads that peer's resources over its serve daemon.
fn cmd_machine_resources(id: Option<&str>, json: bool) -> Result<i32> {
    if let Some(id) = id {
        // Remote read — fetch the peer's live /machine/resources payload.
        let value = fleet_cli::fetch_peer_json(id, "/machine/resources")?;
        if json {
            println!("{}", serde_json::to_string_pretty(&value)?);
            return Ok(0);
        }
        // Deserialize into the same ledger shape and render it, so a remote
        // read reads like a local one. Fall back to raw JSON if the peer's
        // shape doesn't parse (older/newer daemon).
        match serde_json::from_value::<darkmux_profiles::model_ledger::ModelLedger>(value.clone()) {
            Ok(ledger) => print!("{}", darkmux_profiles::model_ledger::render_human(&ledger)),
            Err(_) => println!("{}", serde_json::to_string_pretty(&value)?),
        }
        return Ok(0);
    }
    let ledger = darkmux_profiles::model_ledger::gather();
    if json {
        println!("{}", serde_json::to_string_pretty(&ledger)?);
    } else {
        print!("{}", darkmux_profiles::model_ledger::render_human(&ledger));
    }
    Ok(0)
}

/// `darkmux machine status [id]` (#1426) — residents grouped by ownership
/// PLUS which registered profile(s) the loaded set matches (absorbs the
/// retired top-level `status` verb's unique dimension). With a roster `id`,
/// fetches THAT peer's residents over its serve daemon; the profile-match
/// column is local-only (it reads this host's registry), so it is omitted for
/// a remote read.
fn cmd_machine_status(id: Option<&str>, config: Option<&str>, json: bool) -> Result<i32> {
    if let Some(id) = id {
        // Remote read: the peer's /machine/status returns a flat resident list;
        // partition by ownership here so a remote read renders like a local
        // one. No profile-match — that reads this host's registry.
        let value = fleet_cli::fetch_peer_json(id, "/machine/status")?;
        // (#1426 gate fix) A degraded peer must NOT render as healthy-empty:
        // `lms_unreachable: true` means the peer's daemon could not query
        // LMStudio — its residents are UNKNOWN, not zero. Surface it loudly
        // and exit 2 instead of printing an all-clear empty view.
        let lms_unreachable = value
            .get("lms_unreachable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // Shape mismatch (an older/newer daemon whose payload doesn't parse):
        // fall back to a raw JSON print — the same fallback
        // `cmd_machine_resources` uses — never a fabricated-empty render.
        let models: Vec<types::LoadedModel> = match value
            .get("models")
            .map(|m| serde_json::from_value(m.clone()))
        {
            Some(Ok(models)) => models,
            _ => {
                println!("{}", serde_json::to_string_pretty(&value)?);
                return Ok(0);
            }
        };
        if lms_unreachable {
            if json {
                let out = serde_json::json!({
                    "machine_id": id,
                    "lms_unreachable": true,
                    "managed": [],
                    "user_state": [],
                });
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                eprintln!(
                    "machine `{id}`: the peer's daemon could not reach LMStudio (`lms ps` \
                     failed there) — residents UNKNOWN, not empty. Check LMStudio + the \
                     `lms` CLI on `{id}`."
                );
            }
            return Ok(2);
        }
        return render_residents(&models, None, None, json, Some(id));
    }
    let loaded = lms::list_loaded()?;
    // Which registered profile(s) does the loaded set match? (The retired
    // top-level `status` verb's one unique dimension.)
    let (matches, registry_path): (Option<Vec<String>>, Option<String>) =
        match profiles::load_registry(config) {
            Ok(loaded_reg) => (
                Some(
                    loaded_reg
                        .registry
                        .profiles
                        .iter()
                        .filter(|(_, p)| profile_matches(p, &loaded))
                        .map(|(k, _)| k.clone())
                        .collect(),
                ),
                Some(loaded_reg.path.display().to_string()),
            ),
            // (#1426 gate fix) An EXPLICIT --profiles-file that doesn't load
            // errors loudly (the retired `status` verb's behavior) — an
            // operator-named path is never silently swallowed. Only the
            // no-arg default degrades gracefully (fresh machine, no registry
            // yet: still show residents, without the profile-match line).
            Err(e) if config.is_some() => {
                return Err(e.context("reading --profiles-file for the profile-match column"))
            }
            Err(_) => (None, None),
        };
    render_residents(&loaded, matches.as_deref(), registry_path.as_deref(), json, None)
}

/// Shared renderer for `machine status` (local + remote): residents grouped
/// by ownership, and — for a local read — the registry provenance +
/// matching-profile lines. (#1426)
fn render_residents(
    loaded: &[types::LoadedModel],
    matches: Option<&[String]>,
    registry_path: Option<&str>,
    json: bool,
    remote_id: Option<&str>,
) -> Result<i32> {
    let (managed, user): (Vec<_>, Vec<_>) = loaded
        .iter()
        .partition(|m| swap::is_darkmux_owned(&m.identifier));
    if json {
        // (#907) machine-readable parity, grouped by ownership.
        let mut out = serde_json::json!({
            "managed": managed,
            "user_state": user,
        });
        if let Some(m) = matches {
            out["matching_profiles"] = serde_json::json!(m);
        }
        if let Some(p) = registry_path {
            out["registry"] = serde_json::json!(p);
        }
        if let Some(id) = remote_id {
            out["machine_id"] = serde_json::json!(id);
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }
    if let Some(id) = remote_id {
        println!("{}", darkmux_types::style::header(&format!("machine `{id}` (remote):")));
        println!();
    }
    // Registry provenance (absorbed from the retired `status` verb) — the
    // operator never has to wonder which registry the match line read.
    if let Some(p) = registry_path {
        println!("registry: {p}");
        println!();
    }
    println!("{}", darkmux_types::style::header(&format!("darkmux-managed ({}):", managed.len())));
    if managed.is_empty() {
        println!("  (none — a dispatch loads what its staffing needs)");
    } else {
        for m in &managed {
            println!(
                "  {} ctx={:<8} {:<10} {}",
                darkmux_types::style::accent(&format!("{:<46}", m.identifier)),
                m.context,
                m.size,
                darkmux_types::style::dim(&m.status)
            );
        }
    }
    println!();
    println!("{}", darkmux_types::style::header(&format!("user state ({}):", user.len())));
    if user.is_empty() {
        println!("  (none — LMStudio is exclusively darkmux's right now)");
    } else {
        for m in &user {
            println!(
                "  {} ctx={:<8} {:<10} {}",
                darkmux_types::style::dim(&format!("{:<46}", m.identifier)),
                m.context,
                m.size,
                darkmux_types::style::dim(&m.status)
            );
        }
        println!();
        println!("note: darkmux will never unload entries under `user state` — they're");
        println!("      yours. Use `lms unload <identifier>` to remove them manually.");
    }
    // The matching-profile line (absorbed from the retired `status` verb).
    // Local reads only — a remote peer's residents can't be matched against
    // this host's registry.
    if let Some(matches) = matches {
        println!();
        if matches.is_empty() {
            println!("matches no registered profile");
        } else {
            let listed: Vec<&str> = matches.iter().map(|s| s.as_str()).collect();
            println!("matches profile(s): {}", listed.join(", "));
        }
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
        // (#1426) `profile list` / `profile scan` — the retired top-level
        // `profiles` / `scan` verbs folded into the profile family.
        ProfileCmd::List {
            profiles: cli::ProfilesFileArg { profiles },
            json: cli::JsonFlag { json },
        } => cmd_profiles(profiles.as_deref(), json),
        ProfileCmd::Scan {
            profiles: cli::ProfilesFileArg { profiles },
        } => cmd_scan(profiles.as_deref()),
        ProfileCmd::Draft {
            name,
            model,
            task_class,
            params,
            max_ctx,
        } => {
            let task = heuristics::TaskClass::parse(&task_class).ok_or_else(|| {
                anyhow::anyhow!("unknown task-class '{task_class}'. Try: fast | mid | long")
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
            let json = heuristics::suggestion_to_profile_json(&name, &model, &suggestion, None);
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
    with_agents_md: Option<std::path::PathBuf>,
    force: bool,
    dry_run: bool,
) -> Result<i32> {
    let report = init::init(&init::InitOptions {
        with_hook,
        with_claude_md,
        with_agents_md,
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
    if let Some(p) = report.config_path.as_ref() {
        if report.config_already_present {
            println!("config: already present at {}", p.display());
        } else if report.config_created {
            println!(
                "config: created at {} (machine_id seeded — edit to set Redis, dirs, runtime knobs)",
                p.display()
            );
        }
    }
    println!(
        "skills targets: {}",
        report
            .skills_targets
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
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
    if let Some(p) = report.agents_md_path {
        if report.agents_md_already_present {
            println!("AGENTS.md: already integrated at {}", p.display());
        } else if report.agents_md_appended {
            println!("AGENTS.md: integration section appended to {}", p.display());
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
            println!(
                "  2. Build the internal-runtime image (one-time, ~50 MB, from the darkmux repo root):"
            );
            println!("       docker build -t darkmux-runtime:latest runtime/");
            println!("  3. Run `darkmux doctor` to verify your setup");
            println!("  4. Run `darkmux lab characterize` to smoke-test your machine");
        } else {
            println!(
                "  1. Build the internal-runtime image if you haven't (one-time, ~50 MB, from the darkmux repo root):"
            );
            println!("       docker build -t darkmux-runtime:latest runtime/");
            println!("  2. Run `darkmux doctor` to verify your setup");
            println!("  3. Run `darkmux lab characterize` to smoke-test your machine");
        }
    }
    Ok(0)
}

fn profile_matches(profile: &types::Profile, loaded: &[types::LoadedModel]) -> bool {
    // (#1282) Endpoint-bearing (remote) models are served by their provider —
    // they never appear in `lms ps` — so the comparison covers LOCAL models
    // only, mirroring `swap::desired_loads`' skip. A hybrid profile (local +
    // endpoint) that swap just loaded therefore matches on its local half.
    //
    // Pure-endpoint semantics: zero local models required ⇒ the profile
    // matches exactly when NOTHING is loaded locally. That's what a swap to
    // it produces (everything darkmux-owned unloaded), so `darkmux machine status`
    // reports that state as the profile it is rather than "matches no
    // registered profile". Any local load means the state isn't this
    // profile's.
    let local: Vec<&types::ProfileModel> =
        profile.models.iter().filter(|m| !m.is_remote()).collect();
    if local.len() != loaded.len() {
        return false;
    }
    for m in local {
        let ident = m.identifier.clone().unwrap_or_else(|| m.id.clone());
        let Some(cur) = loaded.iter().find(|x| x.identifier == ident) else {
            return false;
        };
        // A LOCAL model with no declared `n_ctx` (a resolution error surfaced
        // by swap/dispatch/doctor) can never assert a matching loaded context.
        if m.n_ctx != Some(cur.context as u32) {
            return false;
        }
    }
    true
}

fn cmd_profiles(config: Option<&str>, json: bool) -> Result<i32> {
    let loaded = profiles::load_registry(config)?;
    if json {
        // (#907) Serialize the registry directly — `default_profile` + the
        // full profile map, the lowest-surprise machine-readable shape.
        let out = serde_json::json!({
            "registry_path": loaded.path.display().to_string(),
            "registry": loaded.registry,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }
    println!("{}", darkmux_types::style::header(&format!("registry: {}", loaded.path.display())));
    for (name, profile) in &loaded.registry.profiles {
        let default_marker = if loaded.registry.default_profile.as_deref() == Some(name) {
            &format!(" {}", darkmux_types::style::success("(default)"))
        } else {
            ""
        };
        println!("\n{}{}", darkmux_types::style::accent(name), default_marker);
        if let Some(desc) = profile.description.as_deref() {
            println!("  {}", darkmux_types::style::dim(desc));
        }
        // (#590) Models no longer carry a role; mark the default model
        // (default_model, or first model) instead.
        let default_id = profile.default_model_id();
        for m in &profile.models {
            let marker = if Some(m.id.as_str()) == default_id {
                "default"
            } else {
                ""
            };
            // (#1282) `n_ctx` is optional (endpoint-bearing models have no
            // local context to declare) — show what the entry actually says.
            let ctx = match m.n_ctx {
                Some(n) => format!("ctx {n}"),
                None if m.is_remote() => "endpoint".to_string(),
                None => "ctx unset".to_string(),
            };
            println!("  - {} {} @ {}", darkmux_types::style::dim(&format!("{:<10}", marker)), m.id, ctx);
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── profile_matches (#1282) ─────────────────────────────────────

    fn local_model(id: &str, n_ctx: u32) -> types::ProfileModel {
        types::ProfileModel {
            id: id.to_string(),
            n_ctx: Some(n_ctx),
            ..Default::default()
        }
    }

    fn remote_model(id: &str, n_ctx: Option<u32>) -> types::ProfileModel {
        types::ProfileModel {
            id: id.to_string(),
            n_ctx,
            endpoint: Some(types::ModelEndpoint {
                url: Some("https://example.azure.com/openai".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn profile_of(models: Vec<types::ProfileModel>) -> types::Profile {
        types::Profile {
            models,
            ..Default::default()
        }
    }

    fn loaded_model(identifier: &str, context: u64) -> types::LoadedModel {
        types::LoadedModel {
            identifier: identifier.to_string(),
            model: identifier.to_string(),
            status: "loaded".to_string(),
            size: "1 GB".to_string(),
            context,
        }
    }

    #[test]
    fn profile_matches_skips_endpoint_models_in_hybrid_profile() {
        // (#1282) A hybrid profile (one local + one endpoint model) that swap
        // just loaded: only the local half appears in `lms ps`, and that must
        // count as a match — pre-fix the length check demanded BOTH.
        let profile = profile_of(vec![
            local_model("worker", 32000),
            remote_model("gpt-remote", None),
        ]);
        let loaded = vec![loaded_model("worker", 32000)];
        assert!(profile_matches(&profile, &loaded));
        // A remote model with a DECLARED n_ctx ceiling is still skipped —
        // it's never locally loaded regardless.
        let profile = profile_of(vec![
            local_model("worker", 32000),
            remote_model("gpt-remote", Some(100000)),
        ]);
        assert!(profile_matches(&profile, &loaded));
    }

    #[test]
    fn profile_matches_pure_endpoint_profile_matches_empty_loaded_state() {
        // (#1282) Zero local models required ⇒ match exactly when nothing is
        // loaded locally (what a swap to this profile produces); any local
        // load means the state isn't this profile's.
        let profile = profile_of(vec![remote_model("gpt-remote", None)]);
        assert!(profile_matches(&profile, &[]));
        assert!(!profile_matches(&profile, &[loaded_model("worker", 32000)]));
    }

    #[test]
    fn profile_matches_still_requires_local_models_present_at_declared_ctx() {
        let profile = profile_of(vec![
            local_model("worker", 32000),
            remote_model("gpt-remote", None),
        ]);
        // Local model absent → no match.
        assert!(!profile_matches(&profile, &[]));
        // Local model loaded at the wrong context → no match.
        assert!(!profile_matches(&profile, &[loaded_model("worker", 4096)]));
    }

    // ─── worst_case_wait_banner (Wave-E.9 #255) ──────────────────────

    #[test]
    fn worst_case_wait_banner_names_total_bound() {
        let s = worst_case_wait_banner(3, 600, 660);
        assert!(s.contains("3 completion(s)"));
        assert!(s.contains("worst-case total wall ≈ 1980s"));
        assert!(s.contains("33min")); // 1980 / 60
        assert!(s.contains("SIGINT"));
    }

    #[test]
    fn worst_case_wait_banner_handles_single_phase() {
        let s = worst_case_wait_banner(1, 60, 120);
        assert!(s.contains("1 completion(s)"));
        assert!(s.contains("worst-case total wall ≈ 120s"));
        assert!(s.contains("2min"));
    }

    #[test]
    fn worst_case_wait_banner_handles_zero_sessions_gracefully() {
        // Defensive: should NOT panic on degenerate inputs even if the
        // caller's flow normally guards against this.
        let s = worst_case_wait_banner(0, 600, 660);
        assert!(s.contains("0 completion(s)"));
        assert!(s.contains("worst-case total wall ≈ 0s"));
    }

    #[test]
    fn worst_case_wait_banner_uses_saturating_arithmetic() {
        // u64 overflow check: large N × large wait_timeout shouldn't panic.
        let s = worst_case_wait_banner(u64::MAX as usize, 3600, 3660);
        assert!(s.contains("worst-case"));
    }

    // ─── speedup_verdict (Wave-E.4 #255) ──────────────────────────────

    #[test]
    fn speedup_verdict_confirms_parallel_when_speedup_above_threshold() {
        // 2 phases, each 5000ms, mission wall 5000ms → speedup = 2.0
        let v = speedup_verdict(10_000, 5_000, 2);
        match v {
            SpeedupVerdict::ParallelConfirmed { speedup } => {
                assert!((speedup - 2.0).abs() < 0.01, "speedup ≈ 2.0; got {speedup}");
            }
            other => panic!("expected ParallelConfirmed; got {other:?}"),
        }
    }

    #[test]
    fn speedup_verdict_warns_serial_when_speedup_below_threshold() {
        // 2 phases, sum 10000ms, mission wall 8500ms → speedup ≈ 1.18 (< 1.5)
        let v = speedup_verdict(10_000, 8_500, 2);
        match v {
            SpeedupVerdict::SeriallySuspect { speedup } => {
                assert!(
                    (speedup - 1.18).abs() < 0.05,
                    "speedup ≈ 1.18; got {speedup}"
                );
            }
            other => panic!("expected SeriallySuspect; got {other:?}"),
        }
    }

    #[test]
    fn speedup_verdict_inconclusive_when_no_phases_completed() {
        assert_eq!(
            speedup_verdict(0, 100, 2),
            SpeedupVerdict::Inconclusive,
            "zero sum (e.g. all dispatch errors) → Inconclusive"
        );
    }

    #[test]
    fn speedup_verdict_inconclusive_when_single_phase() {
        // Parallelism is undefined for a single phase — stay silent
        // even if the math would otherwise say "confirmed".
        let v = speedup_verdict(10_000, 5_000, 1);
        assert_eq!(v, SpeedupVerdict::Inconclusive);
    }

    #[test]
    fn speedup_verdict_inconclusive_when_zero_sessions() {
        let v = speedup_verdict(10_000, 5_000, 0);
        assert_eq!(v, SpeedupVerdict::Inconclusive);
    }

    #[test]
    fn speedup_verdict_handles_zero_wall_ms_safely() {
        // Instantaneous mission (clock granularity). Math floor at 1ms
        // prevents divide-by-zero; verdict is still computed.
        let v = speedup_verdict(5_000, 0, 2);
        match v {
            SpeedupVerdict::ParallelConfirmed { speedup } => {
                assert!(speedup >= PARALLELISM_CONFIRMED_THRESHOLD);
            }
            other => panic!("expected ParallelConfirmed (degenerate); got {other:?}"),
        }
    }

    #[test]
    fn speedup_verdict_threshold_boundary_exact_match_confirms() {
        // 1.5× exactly → ParallelConfirmed (boundary inclusive). Sum=1500, wall=1000.
        let v = speedup_verdict(1_500, 1_000, 2);
        assert!(matches!(v, SpeedupVerdict::ParallelConfirmed { .. }));
    }

    #[test]
    fn speedup_verdict_threshold_boundary_just_below_warns() {
        // 1.49× → SeriallySuspect (just below the inclusive threshold).
        let v = speedup_verdict(1_490, 1_000, 2);
        assert!(matches!(v, SpeedupVerdict::SeriallySuspect { .. }));
    }

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
            n.starts_with("model-")
                || n.chars()
                    .next()
                    .map(|c| c.is_ascii_alphanumeric())
                    .unwrap_or(false),
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
