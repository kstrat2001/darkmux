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
mod cli_json;
use cli::*;

// SPIKE (#1388) — `darkmux acp`. See src/acp.rs module docs.
mod acp;
// The editor panel's generic `/mission list|launch|show` verbs (launch
// planning over the merged mission-config registry) and the in-process
// ephemeral graph runner `acp.rs`'s session/new and session/prompt
// handlers call into. Split out of acp.rs
// itself so the ACP wire-protocol plumbing and the registry/scheduler
// wiring stay independently readable.
mod acp_panel;

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
// keeps crate::fleet::* resolving for serve/phase_cli.
pub use darkmux_fleet as fleet;
// (#2265) `darkmux finding` — the write-once finding store's read verbs plus
// `sync`, the store's second producer after the live dispatch tailer.
mod finding_cli;
// (#2265) `darkmux mod` — the mod store's CLI producer plus its read verbs.
// A mod is a KIT: instructions plus data, opaque to darkmux.
mod mod_cli;
// `darkmux machine` roster-facing handlers — split out of main.rs alongside cli/lab_cli.
mod fleet_cli;
mod machine_list;
mod profile_remote;
mod fleet_defaults;
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
// crate::{lab,workloads,providers}::* resolving for main.
pub use darkmux_lab::lab;
// `darkmux lab` command handlers — split out of main.rs alongside cli/fleet_cli.
mod lab_cli;
mod config_cmd;
mod conventions;
mod mission_show;
mod mission_status;
mod retired_verbs;
mod run_list;
mod run_records;
mod mission_config_cli;
mod coder_phase;
// (#2112) Power-posture pre-flight — battery/Low-Power-Mode warnings + the
// serious/critical-thermal refusal, called once from each long-mission
// entry point (`mission_launch::launch`).
mod preflight;
// `darkmux mission launch crawl` (#1959 packet 2) — the crawl launcher,
// a dedicated launcher: it needs execution shape the generic
// mission_config::interpret + scheduler path has no seam for. See its own
// module doc.
mod mission_launch;
// (#2131) The shared SIGINT/SIGTERM/SIGHUP-aware finalize guard + child
// reaping used by the `mission launch` launchers (crawl,
// generic/coder-phase) — extracted from #2124/#2130's review-only
// `review_finalize_guard.rs`, which this replaces.
mod launch_guard;
pub use darkmux_lab::providers;
// (#1698 Packet A) The radio interpreter core (catalog compiler, closed-set
// router, frozen prompt assembly) — surface-neutral engine capability. See
// its own module doc for the surface-neutrality + two-seat receiver design.
mod radio;
// (#1698 Packet A) The `darkmux radio` CLI verb — thin execution wiring
// over `radio.rs`, reusing `acp_panel`'s ephemeral runner + routing plan.
mod radio_cli;
// (#1698 Packet B2) The radio interpreter's ANSWERING seat — grounding
// assembler + artifact shelf + the answering dispatch. See its own module
// doc.
mod radio_answer;
// (#2917) Is the instance radio would send to busy? Facts from `lms ps` and
// the residency-lease registry, checked BEFORE a request is sent.
mod radio_busy;
mod radio_index;
mod card_status;
mod role_cli;
// #515 — serve daemon extracted (final crate; deps doctor/eureka/fleet/crew/
// flow/profiles all crates). Re-export keeps crate::serve::* resolving for
// main + phase_cli.
pub use darkmux_serve as serve;
mod skills;
mod phase_cli;
// #463 workspace split (PR2) — profiles/ownership/lms extracted to the
// darkmux-profiles crate. These re-exports keep crate::{profiles,
// ownership,lms}::* paths resolving. (2.0, #1405: the
// `runtime` module — the legacy openclaw config-file patcher — was removed.)
pub use darkmux_profiles::lms;
pub use darkmux_profiles::profiles;
pub use darkmux_profiles::ownership;
// #463 workspace split — types extracted to the darkmux-types crate. The
// re-export keeps all existing `crate::types::*` paths resolving unchanged.
pub use darkmux_types as types;
// #463 — workdir lifted into darkmux-types (leaf util shared by crew + fleet,
// so crew can use it without a binary-resident edge). Re-export keeps
// crate::workdir::* resolving for fleet + other binary modules.
pub use darkmux_types::workdir;
pub use darkmux_lab::workloads;

/// A test's session: an ad-hoc `coder` dispatch `nonce` in mission `m-test`.
#[cfg(test)]
pub(crate) fn test_session(nonce: &str) -> darkmux_types::session_id::SessionId {
    darkmux_types::session_id::SessionId::adhoc(test_run(), "coder", nonce)
}

/// A test's mission run.
#[cfg(test)]
pub(crate) fn test_run() -> darkmux_types::session_id::RunId {
    darkmux_types::session_id::RunId::mission("m-test").expect("a literal run id is never empty")
}

fn main() -> Result<()> {
    providers::register_builtins()?;
    let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let words: Vec<String> = argv.iter().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    if let Some(refusal) = retired_verbs::refusal(&words) {
        eprintln!("error: {refusal}");
        std::process::exit(2);
    }
    let cli = Cli::parse_from(argv);
    refuse_retired_env(&cli.command)?;
    let code = run(cli.command)?;
    std::process::exit(code);
}

/// The ONE check of a retired or renamed setting's env var. Every command but
/// `doctor` (which reports it) and `config` (which fixes it) refuses to start
/// while one whose loss would change behavior is set, and warns once about
/// one that nothing reads (and about a leftover `config.json` key at its old
/// default); `--help` / `--version` never reach `run`.
fn refuse_retired_env(cmd: &Cmd) -> Result<()> {
    if matches!(cmd, Cmd::Doctor { .. } | Cmd::Config { .. }) {
        return Ok(());
    }
    for leftover in darkmux_types::config_access::check_retired_env()? {
        eprintln!("warning: {}", leftover.line);
    }
    // A retired `config.json` key at its old default warns the same way and
    // starts everything (#3057); a value ignoring which would change
    // something is refused by the preflight instead.
    for leftover in darkmux_types::user_files::config_json_warnings() {
        eprintln!("warning: {leftover}");
    }
    Ok(())
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
            finding,
            mod_key,
            profile,
            name,
            timeout,
            workdir,
            workspace_read_only,
            skip_preflight,
            json,
            no_wait,
            image,
            max_completion_tokens,
            resume_from,
        } => cmd_dispatch(DispatchInvocation {
            role,
            message,
            message_from_file,
            finding,
            mod_key,
            profile,
            name,
            timeout,
            workdir,
            workspace_read_only,
            skip_preflight,
            json,
            no_wait,
            image,
            max_completion_tokens,
            resume_from,
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
        Cmd::Finding { sub } => cmd_finding(sub),
        Cmd::Mod { sub } => cmd_mod(sub),
        Cmd::Mission { sub } => cmd_mission(sub),
        Cmd::Run { sub } => run_records::cmd_run(sub),
        Cmd::Flow { sub } => {
            flow_cli::run(sub)?;
            Ok(0)
        }
        Cmd::Config { sub } => config_cmd::run(sub),
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
            // (#2765) `--port`/`--bind` still win outright; unset now falls
            // through to `env(DARKMUX_SERVE_*) > config.serve.* > built-in`
            // — the SAME resolution every client-side daemon probe reads, so
            // the two halves cannot land on different ports the way they
            // could when the port lived only in this command line.
            let (port, bind) = serve::resolve_listen_addr(port, bind);
            let flows_dir = flows_dir.unwrap_or_else(crate::flow::flows_dir);
            // (#1585) `--lab-dir` > `DARKMUX_LAB_DIR` > `config.dirs.lab` >
            // `~/.darkmux/lab`. The flag still wins; the tiers beneath it are
            // new, and `Some(...)` is now unconditional.
            //
            // This REPLACES #1247's opt-in ("no config tier and no built-in
            // default — the lab lens only ever reads a directory the operator
            // explicitly named"). That was right while lab was a separate
            // side-lens: unset meant "not using it." #1508 made lab one of the
            // three sources feeding `/runs`, at which point unset stopped
            // meaning that and started meaning "a source of your primary run
            // view is silently missing" — measured: 247 lab runs on disk, zero
            // visible anywhere. Optionality is part of a subsystem's contract,
            // and promoting it into a shared read-model changes that contract.
            let lab_dir = Some(lab_dir.unwrap_or_else(darkmux_types::config_access::lab_dir));
            serve::run(port, bind, flows_dir, lab_dir)?;
            Ok(0)
        }
        // SPIKE (#1388) — see src/acp.rs module docs.
        Cmd::Acp => acp::run(),
        // (#1698 Packet A) — see src/radio.rs / src/radio_cli.rs module docs.
        Cmd::Radio { text, dry_run } => radio_cli::run(&text, dry_run),
    }
}

fn cmd_lessons(sub: LessonCmd) -> Result<i32> {
    match sub {
        LessonCmd::Add { title, body, file, global } => lessons_add(&title, &body, file.as_deref(), global),
        LessonCmd::List { json: cli::JsonFlagPlain { json } } => lessons_list(json),
        LessonCmd::Edit { id, title, body, file, clear_file, global } => {
            lessons_edit(id, title.as_deref(), body.as_deref(), file.as_deref(), clear_file, global)
        }
        LessonCmd::Remove { id, global } => lessons_remove(id, global),
        LessonCmd::Export { global } => lessons_export(global),
        LessonCmd::Import { file, global } => lessons_import(file, global),
        LessonCmd::Recall { term, file, json: cli::JsonFlagPlain { json } } => {
            lessons_recall(term.as_deref(), file.as_deref(), json)
        }
    }
}

fn lessons_add(title: &str, body: &str, file: Option<&str>, global: bool) -> Result<i32> {
    use darkmux_crew::lessons;
        let (path, tier) = if global {
            (lessons::global_db_path(), "global")
        } else {
            (lessons::repo_db_path(), "repo")
        };
        let conn = lessons::open_at(&path)?;
        lessons::add(&conn, title, body, file, None)?;
        println!(
            "{}",
            darkmux_types::style::success(&format!("recorded lesson ({tier}): {title}"))
        );
        println!("{}", darkmux_types::style::dim(&format!("  {}", path.display())));
        Ok(0)
}

fn lessons_list(json: bool) -> Result<i32> {
    use darkmux_crew::lessons;
        let repo_path = lessons::repo_db_path();
        let global_path = lessons::global_db_path();
        // Strict: a store this build cannot read (one a newer darkmux wrote)
        // is an error here, never "no lessons recorded yet".
        let repo = lessons::load_entries(&repo_path)?;
        // When `$DARKMUX_HOME` collapses both tiers to one root the paths are
        // identical — read once, don't double-display the same entries.
        let global = if global_path == repo_path {
            Vec::new()
        } else {
            lessons::load_entries(&global_path)?
        };

        if json {
            cli_json::emit(&cli_json::LessonTiers { repo, global })?;
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

fn lessons_edit(
    id: i64,
    title: Option<&str>,
    body: Option<&str>,
    file: Option<&str>,
    clear_file: bool,
    global: bool,
) -> Result<i32> {
    use darkmux_crew::lessons;
        let (path, tier) = lessons_tier(global);
        // tri-state: --clear-file wins (Some(None)); else --file (Some(Some));
        // else leave unchanged (None).
        let file_update: Option<Option<&str>> = if clear_file {
            Some(None)
        } else {
            file.map(Some)
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
            title,
            body,
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

fn lessons_remove(id: i64, global: bool) -> Result<i32> {
    use darkmux_crew::lessons;
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

fn lessons_export(global: bool) -> Result<i32> {
    use darkmux_crew::lessons;
        let (path, _) = lessons_tier(global);
        // export reads the store; if absent, emit an empty envelope rather
        // than creating the db (a read must not write). Build the envelope
        // through `LessonsExport` so the wire shape is single-sourced with
        // `import_json` — the two can't drift.
        let env = lessons::LessonsExport {
            schema_version: lessons::LESSONS_SCHEMA_VERSION,
            lessons: lessons::load_entries(&path)?,
        };
        cli_json::emit(&env)?;
        Ok(0)
}

fn lessons_import(file: Option<std::path::PathBuf>, global: bool) -> Result<i32> {
    use darkmux_crew::lessons;
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

fn lessons_recall(term: Option<&str>, file: Option<&str>, json: bool) -> Result<i32> {
    use darkmux_crew::lessons;
        let repo_path = lessons::repo_db_path();
        let global_path = lessons::global_db_path();
        let recall_tier = |path: &std::path::Path| -> Result<Vec<lessons::Lesson>> {
            if !path.exists() {
                return Ok(Vec::new());
            }
            lessons::recall(&lessons::open_at(path)?, term, file)
        };
        let repo = recall_tier(&repo_path)?;
        let global = if global_path == repo_path {
            Vec::new()
        } else {
            recall_tier(&global_path)?
        };
        if json {
            cli_json::emit(&cli_json::LessonTiers { repo, global })?;
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

/// The phase sessions `memory correction list --mission <mid>` scans: the
/// coder runs of the mission's own phases.
fn correction_phase_sessions(mid: &str) -> Result<crew::corrections::PhaseSessions> {
    fleet::validate_identifier("mission", mid)?;
    let missions = crew::loader::load_missions()?;
    let m = missions
        .iter()
        .find(|m| m.id == mid)
        .ok_or_else(|| anyhow::anyhow!("mission `{mid}` not found (check `darkmux mission status`)"))?;
    Ok(crew::corrections::PhaseSessions::new(mid, m.phase_ids.iter().cloned()))
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
            execution,
            days,
            json: cli::JsonFlagPlain { json },
        } => {
            // Resolve the scope. A mission resolves to its phases' coder
            // runs (the same scope the brief uses — an exact phase match,
            // never a prefix, which would bleed a sibling mission whose id
            // is a hyphen-extension, #849).
            let phases = mission.as_deref().map(correction_phase_sessions).transpose()?;
            let scope = match (&phases, &execution) {
                (Some(p), _) => crew::corrections::Scope::Phases(p),
                (None, Some(id)) => crew::corrections::Scope::Execution(id),
                (None, None) => crew::corrections::Scope::All,
            };

            let found = crew::corrections::scan(days, scope);

            if json {
                cli_json::emit(&cli_json::CorrectionList { corrections: found })?;
                return Ok(0);
            }
            if found.is_empty() {
                let scoped = match (&mission, &execution) {
                    (Some(m), _) => format!(" for mission `{m}`"),
                    (None, Some(s)) => format!(" for role execution `{s}`"),
                    (None, None) => String::new(),
                };
                println!(
                    "{}",
                    darkmux_types::style::dim(&format!(
                        "no adjudication corrections recorded{scoped} in the last {days} day(s) \
                         — your reviewer records them with darkmux flow note --execution <id> \
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
                    darkmux_types::style::dim(&format!("[{}]", correction_origin(c)))
                );
                println!("    {}", c.text);
            }
            Ok(0)
        }
    }
}

/// What `memory correction list` names a correction as being about: its role
/// execution, or, for an old record that names none, that plainly.
fn correction_origin(correction: &crew::corrections::Correction) -> String {
    correction
        .execution_id
        .as_ref()
        .map_or_else(|| "no execution recorded".to_string(), |id| id.to_string())
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
    let maintainer_only: Vec<String> = skills::MAINTAINER_ONLY_SKILLS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    report.checks.push(doctor::check_installed_skills_freshness(
        &skill_targets,
        &embedded_skills,
        &maintainer_only,
    ));

    // (#2312, #2430) The mission-config check needs the kinds' declared ports,
    // which only the root crate can assemble: the coder-phase kinds live here.
    // The catalog is the one `mission launch` validates against.
    match mission_launch::kind_catalog() {
        Ok(catalog) => report.checks.push(doctor::check_mission_config_registry(&catalog)),
        Err(e) => report.checks.push(doctor::Check {
            name: "mission config registry".into(),
            status: doctor::Status::Fail,
            message: format!("could not build the step-kind registry: {e:#}"),
            hint: None,
        }),
    }

    // (#2924) Fleet-roster rows: a loopback address no peer can use, and an
    // entry not named by its machine's machine_id. Appended here because they
    // read the roster (darkmux-fleet), which the doctor crate does not depend
    // on — it evaluates, this layer gathers.
    report.checks.extend(fleet_cli::roster_doctor_checks());
    // (#2916) Fleet work submission: token, identity provider, listener,
    // allow-list, and the retired Redis queue if it is still there.
    report.checks.extend(fleet_cli::fleet_submission_doctor_checks());
    // (#3022) The fleet hub and the defaults it hands out, read from the same
    // fleet view `machine list` prints.
    report.checks.extend(fleet_defaults::doctor_checks());

    // (#1177) Opt-in live endpoint probes append to the same report so they
    // share the verdict/exit-code path — a failed probe exits 1 like any
    // failed check.
    let probe_checks = if probe {
        doctor::probe_unmanaged_endpoints()
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

/// (#2265) The mod family — record how something could change, and read what
/// was recorded. The kit is stored and printed verbatim; darkmux never opens
/// one.
fn cmd_mod(sub: cli::ModCmd) -> Result<i32> {
    match sub {
        cli::ModCmd::Create {
            by,
            r#for,
            kit,
            kit_kind,
            attach,
            allow_missing_finding,
            json: cli::JsonFlag { json },
        } => mod_cli::create(
            &by,
            &r#for,
            kit.as_deref(),
            &attach,
            kit_kind.as_deref(),
            allow_missing_finding,
            json,
        ),
        cli::ModCmd::List { r#for, mission, json: cli::JsonFlag { json } } => {
            mod_cli::list(r#for.as_deref(), mission.as_deref(), json)
        }
        cli::ModCmd::Show { key, json: cli::JsonFlag { json } } => mod_cli::show(&key, json),
    }
}

/// (#2265) The finding family — read the store, and replay the flow stream
/// into it. Read-only over operator state; `sync` writes only records that are
/// missing, and never overwrites one that exists.
fn cmd_finding(sub: cli::FindingCmd) -> Result<i32> {
    match sub {
        cli::FindingCmd::List { mission, execution, rule, json: cli::JsonFlag { json } } => {
            finding_cli::list(mission.as_deref(), execution.as_ref(), rule.as_deref(), json)
        }
        cli::FindingCmd::Show { key, json: cli::JsonFlag { json } } => {
            finding_cli::show(&key, json)
        }
        cli::FindingCmd::Sync { since, json: cli::JsonFlag { json } } => {
            finding_cli::sync(since.as_deref(), json)
        }
    }
}

fn cmd_mission(sub: MissionCmd) -> Result<i32> {
    match sub {
        MissionCmd::Status { json, limit, all, named } => mission_status::run(json, limit, all, named),
        MissionCmd::Show { id, json } => mission_show::run(&id, json),
        MissionCmd::Debrief { id, json } => coder_phase::debrief(&id, json),
        MissionCmd::Finalize { id, reasoning } => {
            coder_phase::finalize(&id, reasoning.as_deref())
        }
        MissionCmd::Launch { config_id, input, params, timeout, dry_run, force } => {
            // (#1959) `--dry-run` reaches every launch path (crawl,
            // review, generic step-graph) the SAME way any other input
            // does — a synthetic `--param dry_run=true` appended here,
            // never a separate function parameter threaded through three
            // different launcher signatures. (#2112) `--force` rides the
            // same mechanism for the power-posture pre-flight refusal.
            let mut params = params;
            if dry_run {
                params.push("dry_run=true".to_string());
            }
            if force {
                params.push("force=true".to_string());
            }
            mission_launch::launch(&config_id, input.as_deref(), &params, timeout)
        }
        MissionCmd::Abort {
            mission_id,
            phase,
        } => coder_phase::abort(&mission_id, phase.as_deref()),
        MissionCmd::Config { sub } => mission_config_cli::run(sub),
    }
}

/// (#1426) Owned fields of the top-level `dispatch` verb — a plain carrier so
/// the handler takes ONE argument (clippy `too_many_arguments`) rather than
/// the fifteen the clap variant unpacks into.
struct DispatchInvocation {
    role: String,
    message: Option<String>,
    message_from_file: Option<std::path::PathBuf>,
    finding: Vec<String>,
    mod_key: Vec<String>,
    profile: Option<String>,
    name: Option<String>,
    timeout: Option<u32>,
    workdir: Option<std::path::PathBuf>,
    workspace_read_only: bool,
    skip_preflight: bool,
    json: bool,
    no_wait: bool,
    image: Option<String>,
    max_completion_tokens: Option<u32>,
    resume_from: Option<std::path::PathBuf>,
}

/// (F6) The placeholder a `--resume-from` dispatch carries when no message is
/// given. The runtime replaces its `messages` with the checkpoint history on
/// resume, so no message ever reaches the model, explicit or default; this text
/// only fills the host's required prompt file and keeps the dispatch from
/// reading stdin.
const RESUME_DEFAULT_MESSAGE: &str = "Continue the interrupted dispatch from its checkpoint.";

/// The stderr note for a message given alongside `--resume-from`: a resume
/// continues from the checkpoint, so the message is dropped, not delivered.
fn resume_message_note(resuming: bool, message_given: bool) -> Option<&'static str> {
    (resuming && message_given).then_some(
        "darkmux dispatch: the message is ignored on --resume-from; the dispatch continues from the checkpoint.",
    )
}

/// The dispatch message in precedence order. `message` and `message_from_file`
/// arrive as clap parsed them (mutually exclusive), so `role` is only for the
/// usage guidance. A `resuming` dispatch with no message of its own takes
/// [`RESUME_DEFAULT_MESSAGE`] instead of reading stdin.
fn resolve_dispatch_message(
    role: &str,
    message: Option<String>,
    message_from_file: Option<std::path::PathBuf>,
    resuming: bool,
) -> Result<String> {
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
    Ok(match (message, message_from_file) {
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
        (None, None) if resuming => RESUME_DEFAULT_MESSAGE.to_string(),
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
    })
}

/// The `--finding` / `--mod` refs of a dispatch, each checked against its
/// store, refused when the dispatch routes to another machine (#2295).
fn checked_brief_refs(
    finding: &[String],
    mod_key: &[String],
    machine: Option<&str>,
) -> Result<Vec<darkmux_crew::brief_refs::BriefRef>> {
    // (#2295) `--finding <key>` and `--mod <key>` (both repeatable): each
    // named record's stored content is appended to the brief VERBATIM, after
    // the operator's own message. A key that addresses no stored record is
    // refused BEFORE any dispatch setup — dispatching with a silently missing
    // block would send the role to work on a record it never saw.
    //
    // Findings first, then mods, each in the order given: clap collects the
    // two flags into two lists, so their interleaving is not recoverable, and
    // an order that is stated is better than one that looks meaningful and
    // is not. A caller that needs a different order sets `brief_refs` on the
    // step config directly — that, not the flags, is the list's home.
    let brief_refs: Vec<darkmux_crew::brief_refs::BriefRef> = finding
        .iter()
        .map(darkmux_crew::brief_refs::BriefRef::finding)
        .chain(mod_key.iter().map(darkmux_crew::brief_refs::BriefRef::mod_))
        .collect();
    //
    // (#2295 review, CRITICAL 1) The CLI CHECKS but does not append. The block
    // is rendered once, in `DispatchInternalStepKind` — the point every
    // producer of a `brief_refs` step config goes through, so a mission graph
    // that sets the field gets the same brief this verb does. Checking here
    // anyway is what keeps a typo cheap: it refuses before the ack gate and
    // before any routing or container work, which the step kind (one layer
    // down) could not do as early.
    let brief_refs = darkmux_crew::brief_refs::check_all(
        &brief_refs,
        &darkmux_crew::brief_refs::StoreDirs::resolved(),
    )?;
    // (#2295 review, CRITICAL 1) A cross-machine dispatch is published as a
    // `WorkJob`, which has no field for these refs, and the peer's store is
    // its own — so the remote step kind would resolve nothing and dispatch a
    // brief with no block. Refuse instead: before this change the CLI appended
    // the text into `message`, which made the gap invisible. Adding the field
    // to `WorkJob` is a coordinated wire break (see the FLOW 1.36.0 entry) and
    // is the real fix.
    if !brief_refs.is_empty() {
        if let Some(target) = machine {
            let local = darkmux_flow::resolve_machine_id();
            if matches!(
                crew::dispatch::routing_decision(Some(target), local.as_deref()),
                crew::dispatch::RoutingDecision::Remote { .. }
            ) {
                anyhow::bail!(
                    "--finding / --mod cannot be routed to another machine yet: a submitted \
                     job carries no record refs, and {target}'s own finding / \
                     mod stores are its own. Run it on this machine (a profile without \
                     `@{target}`), or paste the record's content into the message."
                );
            }
        }
    }
    Ok(brief_refs)
}

/// (#1426) `darkmux dispatch <role> [MESSAGE]` — the task-grain execution
/// entry, promoted from the retired `dispatch`. The plumbing is unchanged
/// (`fleet::dispatch_routed`); only the message source is reshaped: the message
/// is a positional argument, falls back to stdin when omitted, and can still be
/// read from a file via `--message-from-file`.
fn cmd_dispatch(inv: DispatchInvocation) -> Result<i32> {
    // (#2947) Bad enum config refuses first: before the message is read
    // from stdin, before brief refs resolve, and before the crew-of-one
    // mission is minted. `--skip-preflight` does not waive it: that flag
    // skips the Docker/daemon probe, and a bad config value is not a probe
    // result that could be stale or wrong.
    darkmux_crew::user_files::preflight(darkmux_types::config_enum::Scope::Dispatch)?;
    let DispatchInvocation {
        role,
        message,
        message_from_file,
        finding,
        mod_key,
        profile,
        name,
        timeout,
        workdir,
        workspace_read_only,
        skip_preflight,
        json,
        no_wait,
        image,
        max_completion_tokens,
        resume_from,
    } = inv;
    // (#2916 stage 2) The machine comes from the profile address: parsed
    // here, before the message is read, so a malformed address is refused
    // first. `dispatch_routed_via` splits it for the dispatch itself.
    let machine = profile
        .as_deref()
        .map(darkmux_types::profile_address::ProfileAddress::parse)
        .transpose()
        .map_err(|e| anyhow::anyhow!("darkmux dispatch: {e}"))?
        .and_then(|a| a.machine);
    if let Some(note) = resume_message_note(resume_from.is_some(), message.is_some() || message_from_file.is_some()) {
        eprintln!("{note}");
    }
    let message = resolve_dispatch_message(&role, message, message_from_file, resume_from.is_some())?;
    let brief_refs = checked_brief_refs(&finding, &mod_key, machine.as_deref())?;
    let opts = crew::dispatch::DispatchOpts {
        finding_sites: None,
        // (#2914) Work never runs on the utility model.
        allow_utility_model: false,
        remote_origin: None,
        live_channel: true,
        // (#2774 review F2) Operator-settable now, so a checkpoint written
        // under a read-only mount (every crawl unit) can actually be
        // resumed — the resume gate refuses an origin-read-only checkpoint
        // resumed read-write, and tier 4's own resume hint emits this flag.
        workspace_read_only,
        record_context: None,
        // The crew-of-one run this dispatch is, minted here so the route
        // record, a fleet submission and the local run all carry it;
        // `--name` names the dispatch within it.
        session: crew::dispatch_as_crew_of_one::dispatch_session(&role, name),
        role_id: role,
        message,
        brief_refs,
        // (#2480) `timeout_seconds` bounds ONLY the tool-less single-call
        // paths (remote/hosted dispatch, the RADIO single-shot path) —
        // unchanged behavior, same default as before the CLI flag became
        // `Option`. `timeout_override_seconds` (below) is the NEW knob:
        // the container-agentic path's per-invocation inactivity-budget
        // override, which is what #2480 was actually filed against.
        timeout_seconds: timeout.unwrap_or(600),
        skip_preflight,
        json,
        workdir,
        // A CLI dispatch is its own crew-of-one run: it names no phase of
        // another mission.
        phase_id: None,
        // (#2916 stage 2) Set from the `profile@machine` address by
        // `dispatch_routed_via`, never from a flag.
        machine: None,
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
        step_id: None, // (#1483) set on the graph-step path only
        system_prompt_override: None,
        // (#2114 follow-up) `--resume-from <dir>` — the trigger for a
        // checkpoint resume. `dispatch_internal::dispatch` verifies +
        // stages the checkpoint; `None` (the default) preserves the
        // fresh-start behavior.
        resume_from,
        // (#2153) No CLI surface for naming an exact out dir — a bare
        // `dispatch` always gets a fresh tempdir. Only the crawl launcher
        // sets this today.
        host_out: None,
        max_turns_override: None,
        // (#2480) The CLI's only wiring site for this field — every other
        // `DispatchOpts` construction in the workspace passes `None`.
        timeout_override_seconds: timeout,
    };
    // (#2262) A bare `dispatch` installed no signal handling at all — the
    // same gap #2131 closed for every `mission launch` launcher, never
    // closed here. Without `arm()`, a caught SIGTERM/SIGINT/SIGHUP kills
    // this process via the OS default disposition: no unwind, no `Drop`,
    // the docker container (or curl child, for a tool-less hosted role)
    // orphaned. `dispatch_as_crew_of_one` below already mints a real
    // (cardinality-one) mission and reconciles it to a terminal status on
    // ANY `Err`/`NodeStatus::Error` outcome (see its own `finalize`/
    // `reconcile_on_error`) — the SAME `finalize_mission` a `mission
    // launch` run reaches — and `dispatch_internal.rs`'s own
    // `DispatchBookendGuard` already guarantees a `dispatch.error`
    // liveness bookend on every exit from the dispatch call itself. So
    // the only two things actually missing here are: (1) install the
    // handlers so a signal becomes a flag instead of an outright kill,
    // and (2) something to notice that flag and kill the blocked child —
    // the docker path already self-kills via its own trajectory-tailer
    // poll of `interrupt::is_set()` once the flag is live, but the
    // tool-less remote/hosted `curl` path has no poll seam of its own
    // (see `spawn_reap_watchdog`'s own doc). No new finalize/envelope
    // guard is added here — one would be redundant with the crew-of-one
    // machinery this call already goes through.
    crate::launch_guard::arm();
    let _reap_watchdog = crate::launch_guard::spawn_reap_watchdog();

    // (#1509) Route the LOCAL half of `dispatch_routed`'s routing decision
    // through the engine as a crew of one (a full Mission -> Phase ->
    // Task(role) -> Step graph at cardinality one, run through the SAME
    // `run_step_graph` scheduler missions use) instead of the raw
    // `crew::dispatch::dispatch` primitive — see
    // `darkmux_crew::dispatch_as_crew_of_one`'s module doc for the full
    // rationale (closes the #1487 residency-lease bypass a raw `dispatch`
    // fell through). `profile@machine` routing (and every other caller of
    // `fleet::dispatch_routed`) is untouched.
    let result = match fleet::dispatch_routed_via(opts, crew::dispatch_as_crew_of_one::dispatch_as_crew_of_one) {
        Ok(r) => r,
        Err(e) => {
            // (#2462) `dispatch_as_crew_of_one::dispatch` already finalizes
            // the crew-of-one mission (`finalize`/`reconcile_on_error`) to a
            // terminal status BEFORE returning this `Err` — the terminal
            // record is durable. Matching `mission launch`'s own shape
            // (`reap_and_exit_on_signal`'s doc), a dispatch a signal
            // actually ended now exits 130 instead of the default-error 1,
            // so a wrapper script can tell "the operator stopped this" from
            // the exit code alone, the same way it already can for `mission
            // launch`. A no-op (falls through to the ordinary `Err` return
            // below) when no signal was ever observed.
            //
            // (#2462 review) `report_*`, NOT the bare
            // `reap_and_exit_on_signal`: the force-exit runs before `main`'s
            // own error printing, so a bare call exits 130 having discarded
            // the very interrupt message this change exists to produce. See
            // that function's doc for the measurement, and for why this site
            // keeps the hard exit where `radio_cli.rs` dropped it.
            crate::launch_guard::report_reap_and_exit_on_signal(&e);
            return Err(e);
        }
    };
    // (#1955) A `--json` caller receives stdout AND stderr as one payload and
    // pays context for both. On a run that SUCCEEDED the prose is now pure
    // overhead: the envelope carries the checkpoint tally, the detections and
    // the host peaks, so twelve repetitions of `per-call budget reached` cost
    // tokens to receive and say strictly less than `checkpoints.total`.
    //
    // Only on success. A failing dispatch's stderr is the diagnosis — the
    // docker error, the model load failure, the escalation reason — and
    // suppressing it to save context would trade a few hundred tokens for the
    // ability to tell what went wrong. Quiet is for the boring case.
    //
    // stderr stays exactly as it is for a human at a terminal, which is the
    // consumer it was written for and serves well.
    let quiet = json && result.exit_code == 0;
    if !quiet {
        // Announce the resolved execution id on stderr so operators can
        // correlate this dispatch with the flow stream — without polluting the
        // --json envelope on stdout that orchestrators parse.
        if let Some(line) = execution_id_line(&result) {
            eprintln!("{line}");
        }
    }
    print!("{}", result.stdout);
    if !quiet && !result.stderr.is_empty() {
        eprint!("{}", result.stderr);
    }
    Ok(result.exit_code)
}

/// The line `dispatch` prints naming the role execution it ran: its
/// `exec-...` id, the one `--execution` takes. A result no local execution
/// produced (a job routed to another machine) names none.
fn execution_id_line(result: &crew::dispatch::DispatchResult) -> Option<String> {
    result.execution.as_ref().map(|execution| format!("darkmux dispatch: execution id `{execution}`"))
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
        Some(MachineCmd::List { json }) => fleet_cli::cmd_machine_list(json),
        Some(MachineCmd::Add {
            id,
            address,
            description,
            allow_loopback,
        }) => fleet_cli::cmd_machine_add(&id, &address, description.as_deref(), allow_loopback),
        Some(MachineCmd::Remove { id }) => fleet_cli::cmd_machine_remove(&id),
        Some(MachineCmd::Trust { name, node, profiles, roles, images, workspace }) => {
            fleet_cli::cmd_machine_trust(&name, node.as_deref(), &profiles, &roles, images.as_deref(), workspace)
        }
        Some(MachineCmd::Untrust { name }) => fleet_cli::cmd_machine_untrust(&name),
    }
}

/// `darkmux machine resources` (#1286, renamed from `model ledger` in #1426)
/// — the no-viewer twin of the machine lens: one bounded gather (lms metadata
/// and kernel counters, zero model dispatches), rendered as a table or emitted
/// as JSON. With a roster `id`, reads that peer's resources over its serve
/// daemon and prints the daemon's own `/machine/resources` response.
fn cmd_machine_resources(id: Option<&str>, json: bool) -> Result<i32> {
    if let Some(id) = id {
        // Remote read — fetch the peer's live /machine/resources payload.
        let value = fleet_cli::fetch_peer_json(id, "/machine/resources")?;
        print!("{}", fleet_cli::peer_resources_view(id, value, json)?);
        return Ok(0);
    }
    let ledger = darkmux_profiles::model_ledger::gather();
    if json {
        cli_json::emit(&ledger)?;
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
/// A peer's resident list, rendered like a local one.
fn machine_status_remote(id: &str, json: bool) -> Result<i32> {
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
    // refuse, naming the cause (the same refusal `cmd_machine_resources`
    // makes), never a fabricated-empty render.
    let models: Vec<types::LoadedModel> = match value
        .get("models")
        .map(|m| serde_json::from_value(m.clone()))
    {
        Some(Ok(models)) => models,
        _ => anyhow::bail!(
            "machine `{id}` answered with a resident list this darkmux does not read ({})",
            fleet_cli::peer_reports(&value)
        ),
    };
    if lms_unreachable {
        if json {
            cli_json::emit(&cli_json::MachineStatusOutput::lms_unreachable(Some(id.to_string())))?;
        } else {
            eprintln!(
                "machine `{id}`: the peer's daemon could not reach LMStudio (`lms ps` \
                 failed there) — residents UNKNOWN, not empty. Check LMStudio + the \
                 `lms` CLI on `{id}`."
            );
        }
        return Ok(2);
    }
    let code = render_residents(&models, None, None, json, Some(id))?;
    if !json {
        println!();
        println!("{}", peer_utility_line(id));
    }
    Ok(code)
}

/// The utility model line for a peer, from the `/machine/specs` it serves (the
/// same `utility_model` its card carries). A peer whose specs cannot be read,
/// or an older one that does not state the field, is "not reported", never
/// "none bound" and never "resident".
fn peer_utility_line(id: &str) -> String {
    let stated = fleet_cli::fetch_peer_json(id, "/machine/specs").ok().and_then(|specs| specs.get("utility_model").cloned());
    match stated {
        Some(v) => card_status::utility_line(serde_json::from_value::<darkmux_serve::wire::UtilityModel>(v).ok().as_ref()),
        None => format!("utility model: not reported by `{id}`; job: not shown here"),
    }
}

fn cmd_machine_status(id: Option<&str>, config: Option<&str>, json: bool) -> Result<i32> {
    if let Some(id) = id {
        return machine_status_remote(id, json);
    }
    // (#2774 round-9 review C1) The LOCAL twin of the remote branch's
    // `lms_unreachable` handling just above.
    //
    // `list_loaded` distinguishes "nothing is loaded" from "I could not
    // tell" (#2774 round-9 MF3), and a bare `?` here turned the second
    // into an anyhow chain on stderr with rc 1 and — measured, in `--json`
    // mode — ZERO bytes on stdout, where a script reading the IDENTICAL
    // condition from a PEER gets a parseable object and rc 2. One host's
    // answer to "can you see LMStudio" should not depend on which side of
    // the fleet is asking. Same shape, same exit code, both branches.
    let loaded = match lms::list_loaded() {
        Ok(loaded) => loaded,
        Err(e) => {
            if json {
                cli_json::emit(&cli_json::MachineStatusOutput::lms_unreachable(
                    darkmux_types::config_access::machine_id(),
                ))?;
            } else {
                eprintln!(
                    "this machine: could not query LMStudio (`lms ps` failed here) — residents \
                     UNKNOWN, not empty. Check LMStudio + the `lms` CLI. ({e:#})"
                );
            }
            return Ok(2);
        }
    };
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
    let code = render_residents(&loaded, matches.as_deref(), registry_path.as_deref(), json, None)?;
    if !json {
        println!();
        println!("{}", card_status::utility_line(darkmux_serve::local_utility_model(&loaded).as_ref()));
    }
    Ok(code)
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
        .partition(|m| ownership::is_darkmux_owned(&m.identifier));
    if json {
        // (#907) machine-readable parity, grouped by ownership.
        cli_json::emit(&cli_json::MachineStatusOutput {
            machine_id: remote_id.map(str::to_string).or_else(darkmux_types::config_access::machine_id),
            lms_unreachable: false,
            managed,
            user_state: user,
            matching_profiles: matches,
            registry: registry_path,
        })?;
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
    // (#2774 tier 5) `ownership::eject_all_managed` is the one unloader now —
    // the thermal breaker's tier-5 hard-stop calls the SAME function
    // rather than a second copy of this filter+unload loop. One
    // `lms::list_loaded()` call total: `summary.user_loaded_count` already
    // carries what the old "nothing to eject" message needed, so there is
    // no separate peek to keep in sync with it.
    let summary = ownership::eject_all_managed(dry_run)?;
    // (#2774 review C1) A stuck resident no longer aborts the sweep, so
    // report what did NOT come out alongside what did — and exit non-zero,
    // since "eject" did not fully happen.
    let had_failures = !summary.failed.is_empty();
    for f in &summary.failed {
        eprintln!("darkmux: ⚠ could not eject {} — {}", f.identifier, f.error);
    }
    if summary.ejected.is_empty() && !had_failures {
        println!("no darkmux-managed loads to eject");
        if summary.user_loaded_count > 0 {
            println!(
                "({} user-loaded model(s) untouched — use `lms unload <identifier>` for those)",
                summary.user_loaded_count
            );
        }
        return Ok(0);
    }
    for m in &summary.ejected {
        if dry_run {
            println!("would eject {} (ctx={})", m.identifier, m.context);
        } else {
            println!("eject {} (ctx={})", m.identifier, m.context);
        }
    }
    let verb = if dry_run { "would eject" } else { "ejected" };
    let mut line = format!("{verb} {} model(s)", summary.ejected.len());
    if summary.user_loaded_count > 0 {
        line.push_str(&format!(", respected {} user-loaded model(s)", summary.user_loaded_count));
    }
    if had_failures {
        line.push_str(&format!(", {} FAILED to eject", summary.failed.len()));
    }
    if dry_run {
        line.push_str(" [DRY RUN]");
    }
    println!("{line}");
    Ok(i32::from(had_failures))
}

fn cmd_profile(sub: ProfileCmd) -> Result<i32> {
    match sub {
        // (#1426) `profile list` / `profile scan` — the retired top-level
        // `profiles` / `scan` verbs folded into the profile family.
        ProfileCmd::List {
            profiles: cli::ProfilesFileArg { profiles },
            json: cli::JsonFlag { json },
            machine,
            remote,
        } => {
            // (None from `run`: the named machine is this one, so the plain
            // local list answers.)
            let target = profile_remote::Target::from_flags(machine.as_deref(), remote);
            match target.map(|t| profile_remote::run(t, json)).transpose()?.flatten() {
                Some(code) => Ok(code),
                None => cmd_profiles(profiles.as_deref(), json),
            }
        }
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
            cli_json::emit(&heuristics::draft_profile(&name, &model, &suggestion))?;
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
            println!("profile registry: created at {}", p.display());
        }
        // (#2038) The worker model: filled from LM Studio, or say why not.
        if let Some(id) = report.worker_model_filled.as_deref() {
            println!("worker model: `{id}` (LM Studio has it; every worker profile now names it, edit {} to change)", p.display());
        } else if let Some(reason) = report.worker_model_unfilled_reason.as_deref() {
            println!("worker model: not set. {reason}");
        }
        if let Some(id) = report.utility_model_filled.as_deref() {
            println!("utility model: `{id}` (the registry named one LM Studio does not have; this is the closest downloaded match, edit {} to change)", p.display());
        } else if let Some(reason) = report.utility_model_unfilled_reason.as_deref() {
            println!("utility model: not verified. {reason}");
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
    if !report.skills_protected.is_empty() {
        // (#1927) These darkmux-* skills differ from the bundled copy and
        // have no recorded provenance (or a broken one) — could be an older
        // bundled version, could be an operator edit; refresh alone can't
        // tell, so it declines rather than guess. --force overwrites anyway.
        //
        // `init` is where the decision is made, so `init` is where the whole
        // decision has to be legible: WHAT was kept, WHY it was kept, and the
        // exact command that takes the new version. Naming the "installed by
        // an older darkmux, so it has no stamp at all" case explicitly is the
        // load-bearing part — on the first run after upgrading, that case is
        // every skill on the machine, and an operator who does not know it is
        // expected reads this list as darkmux refusing to work.
        println!(
            "  kept as-is, not refreshed ({}): {}",
            report.skills_protected.len(),
            report.skills_protected.join(", ")
        );
        for line in [
            "    why: darkmux refreshes a skill only when it can prove it wrote the installed copy",
            "         itself, by matching a provenance stamp it records at install time. These",
            "         differ from the bundled copy and carry no stamp, or carry one that no longer",
            "         matches.",
            "    a skill installed by a darkmux older than this one has no stamp at all, so the",
            "         first run after an upgrade lists every skill here even if you never edited one.",
            "    to take this binary's version: `darkmux init --force` — overwrites ALL of the above",
            "         (any edit of yours included) and records provenance, so later runs refresh",
            "         silently.",
            "    to refresh just ONE of them, keeping the rest: delete that skill's directory under",
            "         the target above and re-run `darkmux init` — it reinstalls the missing one and",
            "         leaves the others exactly as they are. `--force` is all-or-nothing; this is not.",
            "    to keep an edit: do nothing. `darkmux doctor` keeps flagging these as differing",
            "         from the bundled copy; that is the reminder, not an error.",
        ] {
            println!("{line}");
        }
    }
    if !report.skills_force_overwrote_modified.is_empty() {
        // (#1927) --force is the explicit "yes, discard it" escape hatch —
        // say plainly which of the "overwritten" names above it applied to.
        //
        // Wording matters here (#1839, describe don't adjudicate). darkmux
        // does NOT know these carried an operator edit; it knows only that it
        // could not prove otherwise, which is also true of every skill an
        // older darkmux installed. Claiming "discarded your changes" for a
        // set that is usually "16 skills you never touched" is a verdict the
        // evidence does not support — and an alarming one.
        println!(
            "  --force overwrote without proof they were unmodified ({}): {}",
            report.skills_force_overwrote_modified.len(),
            report.skills_force_overwrote_modified.join(", ")
        );
        println!(
            "    any local edit in those is gone. darkmux had no provenance stamp for them (or a"
        );
        println!(
            "    stale one), so it could not tell an edit from a copy an older darkmux installed."
        );
    }
    if !report.skills_pruned.is_empty() {
        // (#1449) Retired darkmux-* skills removed from the install target so an
        // upgraded machine stops teaching dead verbs.
        let verb = if dry_run { "would prune" } else { "pruned" };
        println!(
            "  {} ({}): {}",
            verb,
            report.skills_pruned.len(),
            report.skills_pruned.join(", ")
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
        // (#2053) The runtime image is pulled from GHCR on the first dispatch;
        // telling a new user to `docker build` it was the guide's own
        // prerequisites table contradicted by init's last lines.
        let mut n = 0;
        let mut step = |text: &str| {
            n += 1;
            println!("  {n}. {text}");
        };
        if report.profile_registry_created && report.worker_model_filled.is_none() {
            step("Edit ~/.darkmux/profiles.json to point at a downloaded model (`lms ls` lists them)");
        }
        step("First answer, no Docker needed: `darkmux radio \"do you have a brain?\"`");
        step("Check the setup: `darkmux doctor`");
        step("With Docker running, a first dispatch: `darkmux dispatch code-reviewer \"What do you do?\"` (the runtime image is pulled on first use)");
    }
    Ok(0)
}

fn profile_matches(profile: &types::Profile, loaded: &[types::LoadedModel]) -> bool {
    // (#1282) Endpoint-bearing (remote) models are served by their provider —
    // they never appear in `lms ps` — so the comparison covers LOCAL models
    // only. A hybrid profile (local + endpoint) therefore matches on its
    // local half.
    //
    // Pure-endpoint semantics: zero local models required ⇒ the profile
    // matches exactly when NOTHING is loaded locally, so `darkmux machine
    // status` reports that state as the profile it is rather than "matches
    // no registered profile". Any local load means the state isn't this
    // profile's.
    let local: Vec<&types::ProfileModel> =
        profile.models.iter().filter(|m| m.is_managed()).collect();
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
        cli_json::emit(&cli_json::ProfileList {
            registry_path: loaded.path.display().to_string(),
            registry: &loaded.registry,
        })?;
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
            let ctx = model_ctx_label(m, &loaded.registry);
            println!("  - {} {} @ {}", darkmux_types::style::dim(&format!("{:<10}", marker)), m.id, ctx);
        }
    }
    // (#2902 step 4) The endpoints profiles name by id: what darkmux does
    // there and where requests go (host only, never the path or a credential).
    if !loaded.registry.endpoints.is_empty() {
        println!("\n{}", darkmux_types::style::accent("endpoints"));
        for (id, ep) in &loaded.registry.endpoints {
            let what = match ep.kind() {
                Ok(darkmux_types::EndpointKind::Managed(_)) => "managed (lmstudio)".to_string(),
                Ok(darkmux_types::EndpointKind::Unmanaged) => {
                    format!("unmanaged @ {}", ep.host().unwrap_or_else(|| "?".to_string()))
                }
                Err(e) => format!("unusable: {e}"),
            };
            println!("  - {id}: {what}");
        }
    }
    Ok(0)
}

/// `profile list`'s `@ …` for one model: its window, or (#2902) its
/// endpoint by id, saying WHY an id cannot be used: its entry (or the whole
/// `endpoints` value) is quarantined, it is defined but unusable, or it is
/// not defined at all.
fn model_ctx_label(m: &types::ProfileModel, registry: &darkmux_types::ProfileRegistry) -> String {
    use darkmux_types::QuarantinedEntryKind;
    if let Some(n) = m.n_ctx {
        return format!("ctx {n}");
    }
    if m.is_managed() {
        return "ctx unset".to_string();
    }
    let Some(id) = m.endpoint.as_ref().and_then(|e| e.named_id()) else {
        return "endpoint".to_string();
    };
    if m.endpoint_kind().is_ok() {
        return format!("endpoint `{id}`");
    }
    let quarantined = registry
        .quarantined
        .iter()
        .any(|q| q.kind == QuarantinedEntryKind::Endpoint && (q.name == id || q.name == "endpoints"));
    if quarantined {
        format!("endpoint `{id}` (quarantined: its `endpoints` entry failed to parse; see `darkmux doctor`)")
    } else if registry.endpoints.contains_key(id) {
        format!("endpoint `{id}` (unusable; see `darkmux doctor`)")
    } else {
        format!("endpoint `{id}` (not defined in `endpoints`)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_correction_shows_its_execution_or_says_it_has_none() {
        let id = darkmux_types::execution_id::ExecutionId::mint();
        let correction = |execution_id| crew::corrections::Correction { ts: "t".into(), execution_id, text: "x".into() };
        assert_eq!(correction_origin(&correction(Some(id.clone()))), id.as_str());
        assert_eq!(correction_origin(&correction(None)), "no execution recorded");
    }

    /// (F6) A resume with no message continues the checkpoint instead of
    /// reading an empty stdin; an explicit message still wins.
    #[test]
    fn a_message_given_with_a_resume_is_reported_as_ignored() {
        assert!(resume_message_note(true, true).is_some_and(|n| n.contains("ignored")));
        assert_eq!(resume_message_note(true, false), None);
        assert_eq!(resume_message_note(false, true), None);
    }

    #[test]
    fn a_resume_without_a_message_defaults_it_and_an_explicit_message_wins() {
        let defaulted = resolve_dispatch_message("coder", None, None, true).unwrap();
        assert_eq!(defaulted, RESUME_DEFAULT_MESSAGE);
        let given = resolve_dispatch_message("coder", Some("do x".into()), None, true).unwrap();
        assert_eq!(given, "do x");
    }

    /// The id `dispatch` prints is the execution's, not its session's, and
    /// is one every `--execution` flag takes.
    #[test]
    fn dispatch_prints_the_execution_id_that_every_execution_flag_takes() {
        let execution = darkmux_types::execution_id::ExecutionId::mint();
        let mut result = crew::dispatch::DispatchResult {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            session_id: test_session("s"),
            execution: Some(execution.clone()),
            out_dir: None,
            trajectory: None,
        };
        let line = execution_id_line(&result).expect("a local dispatch names its execution");
        assert!(line.contains(execution.as_str()), "{line}");
        assert!(!line.contains(&result.session_id.wire()), "the session id is not the execution id: {line}");
        let printed = line.split('`').nth(1).unwrap();
        assert_eq!(flow_cli::parse_execution_arg(printed).unwrap(), execution);

        result.execution = None;
        assert_eq!(execution_id_line(&result), None, "a routed job has no local execution to name");
    }

    /// (#2902 re-review C2) `profile list` tells a quarantined endpoint
    /// entry from an undefined id and from a defined-but-unusable one.
    #[test]
    fn profile_list_says_why_an_endpoint_id_cannot_be_used() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("profiles.json");
        std::fs::write(
            &p,
            r#"{"profiles":{"p":{"models":[
                    {"id":"a","endpoint":"bad"},{"id":"b","endpoint":"ghost"},
                    {"id":"c","endpoint":"newer"},{"id":"d","endpoint":"ok"}]}},
                "endpoints":{"bad":{"url":5},"newer":{"managed":"machine"},"ok":{"url":"https://h.example/v1"}}}"#,
        )
        .unwrap();
        let reg = profiles::load_registry(p.to_str()).unwrap().registry;
        let label = |i: usize| model_ctx_label(&reg.profiles["p"].models[i], &reg);
        assert!(label(0).contains("quarantined"), "{}", label(0));
        assert!(label(1).contains("not defined"), "{}", label(1));
        assert!(label(2).contains("unusable"), "{}", label(2));
        assert_eq!(label(3), "endpoint `ok`");
    }

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
            queued: None,
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
