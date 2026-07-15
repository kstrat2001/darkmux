//! `darkmux mission launch <config-id>` — mints a mission INSTANCE from a
//! named mission CONFIG (#1284 Packet 4a, the instance-model collapse).
//!
//! Per the epic's locked arc design (#1284): "config-launched becomes the
//! only instance-creation path... instance = resolved-config snapshot +
//! runtime state." This module is that path. It:
//!
//!   1. Resolves `<config-id>` through `mission_config::load` (user →
//!      on-disk → embedded) and validates it loud (contract 7 — semantic
//!      validation is a separate, explicit CONSUMPTION-time pass, never
//!      folded into the lenient-on-read `load`).
//!   2. Collects the config's declared `inputs` from `--input <file.json>`
//!      / `--param key=value` (params win), bailing loud with a
//!      copy-pasteable example when a required input is missing.
//!   3. Mints a mission instance: `mission.json`, one `phases/<id>.json`
//!      per declared phase, AND a `config-snapshot.json` — the
//!      fully-resolved config frozen alongside the instance so a later
//!      edit/delete of the source config never orphans a running
//!      instance's own record of what it ran (mirrors the review
//!      pipeline's crew-staffing-snapshot precedent).
//!   4. Interprets the graph (`mission_config::interpret`) and, when the
//!      graph is one this packet's launcher knows how to EXECUTE, runs it
//!      through the real scheduler and finalizes via `MissionEnvelope`.
//!
//! **Scope boundary (read before extending).** This packet wires exactly
//! ONE mission type all the way through to real dispatch: `coder-phase`,
//! reusing `mission_run.rs`'s own `MissionWorktreeStepKind` /
//! `MissionCoderStepKind` / `MissionVerifyStepKind` Tier 3 kinds verbatim
//! (elevated to `pub(crate)` for this module — see their doc comments) so
//! the `mission.worktree`/`mission.coder`/`mission.verify` flow-record
//! shape and `darkmux-serve`'s `/diff` contract stay byte-identical
//! whether a mission was minted by `mission run` or `mission launch`.
//! `darkmux mission run` is UNCHANGED and still the primary path for an
//! operator working an EXISTING hand-authored (or `mission propose`-drafted
//! pre-Packet-4a) mission one phase at a time; `mission launch` is the new,
//! ADDITIONAL config-driven path, proven out here.
//!
//! `review` (the 3-phase PR-review config) is NOT executable through this
//! verb yet — its `review.*` Tier 3 kinds need crew-staffing resolution
//! (`crew_staffing`, `judge_concurrency` — see `templates/builtin/
//! mission-configs/review.json`'s own `inputs` doc) that only
//! `crates/darkmux-lab/src/lab/review.rs::build_review_graph` currently
//! knows how to do. In practice a `mission launch review` attempt bails at
//! the missing-required-input gate (step 2 above) before anything is
//! minted — no special-casing needed, the generic gate already produces the
//! honest failure. A config whose graph references a step kind this
//! launcher can't construct AT ALL (step 4's `executable` check below) gets
//! a GUARDED punt instead: the instance is still minted (so its Task/Step
//! records show the intended graph shape for inspection), but nothing is
//! dispatched and `launch` returns exit code `2`. Wiring `review`'s
//! crew-staffing resolution through this generic verb is named Packet 4b
//! work in the epic, not forced here.

use crate::crew;
use crate::fleet;
use crate::flow;
use crate::mission_run;
use anyhow::{anyhow, bail, Context, Result};
use crew::mission_config::{self, FindingSeverity, LaunchParams, MissionConfig, TaskOverride};
use crew::types::{Mission, MissionStatus, NodeStatus, Phase, PhaseStatus};
use darkmux_types::style;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// The three Tier 3 step kinds `mission_run.rs` defines for `coder-phase`'s
/// graph (#1352) — the only Tier 3 kinds this launcher knows how to
/// construct in Packet 4a. See the module doc's scope boundary.
const CODER_PHASE_TIER3_KINDS: &[&str] = &["mission.worktree", "mission.coder", "mission.verify"];

/// `darkmux mission launch <config-id>` entry point. Returns the process
/// exit code:
///   `0` — freeform mint (no executable graph) OR a Clean/Degraded run.
///   `1` — an Error/Degenerate run (the graph executed but produced no
///         usable result — the instance is still minted + finalized).
///   `2` — the graph was minted but NOT executed because it references
///         step kind(s) this packet's launcher can't construct yet.
pub fn launch(
    config_id: &str,
    input_file: Option<&Path>,
    params: &[String],
    timeout_seconds: u32,
) -> Result<i32> {
    fleet::validate_identifier("config_id", config_id)?;

    let loaded = mission_config::load(config_id).with_context(|| {
        format!(
            "loading mission config \"{config_id}\" — note: a user-tier copy \
             (~/.darkmux/mission-configs/{config_id}.json) or an on-disk template overrides \
             an embedded built-in; the failing file is named above if one was found"
        )
    })?;
    let config = &loaded.config;

    // (contract 7) Semantic validation is a SEPARATE, explicit pass — this
    // IS the consumption point. `known_step_kinds` is everything this
    // launcher can actually run today (Tier 1 builtins + the coder-phase
    // Tier 3 set); anything else warns rather than errors (#1284 Packet 1's
    // own rule — a step kind this call site doesn't recognize isn't
    // necessarily wrong, just not yet reachable through THIS launcher).
    let tier1_ids = crew::step_kinds::StepKindRegistry::with_builtins().ids();
    let mut known_kinds: Vec<&str> = tier1_ids.iter().map(String::as_str).collect();
    known_kinds.extend(CODER_PHASE_TIER3_KINDS.iter().copied());
    let findings = config.validate(&known_kinds);
    let errors: Vec<_> = findings.iter().filter(|f| f.severity == FindingSeverity::Error).collect();
    if !errors.is_empty() {
        let msg = errors.iter().map(|f| f.to_string()).collect::<Vec<_>>().join("\n");
        bail!("mission launch: config \"{config_id}\" failed validation:\n{msg}");
    }
    for f in findings.iter().filter(|f| f.severity == FindingSeverity::Warning) {
        eprintln!("{}", style::warn(&f.to_string()));
    }

    println!(
        "{}",
        style::header(&format!(
            "▶ mission launch — {} ({} tier)",
            config_id,
            loaded.source
        ))
    );

    let collected = collect_inputs(input_file, params)?;
    let missing = missing_required_inputs(config, &collected);
    if !missing.is_empty() {
        bail!("{}", missing_inputs_message(config, &missing));
    }

    // Instance id: `<config-id>-<disambiguator>`, where the disambiguator
    // is derived from the OPERATOR-SUPPLIED inputs (never `mission_id`
    // itself, which the launcher supplies below — hashing it would be
    // circular). Same params → same instance id → the reuse/reopen path
    // below fires (idempotent relaunch); different params → a distinct
    // instance. No `--mission-id` flag needed.
    let mission_id = derive_mission_id(config_id, &collected)?;
    let mut collected = collected;
    if config.inputs.iter().any(|i| i.name == "mission_id") {
        collected.insert("mission_id".to_string(), serde_json::Value::String(mission_id.clone()));
    }

    let real_phase_ids = ensure_mission_and_phases(&mission_id, config)?;

    // config-snapshot.json — ALWAYS written (fresh mint or relaunch
    // overwrite), regardless of whether the graph turns out executable.
    crew::lifecycle::save_config_snapshot(&mission_id, config)
        .context("persisting config-snapshot.json")?;

    let params = build_launch_params(config, &real_phase_ids, &collected);
    let (tasks, mut steps) =
        mission_config::interpret(config, &params).context("interpreting mission config graph")?;

    for task in &tasks {
        if let Err(e) = crew::lifecycle::save_task(&mission_id, task) {
            eprintln!("{}", style::dim(&format!("mission launch: task persist warning: {e:#}")));
        }
    }

    if tasks.is_empty() {
        // Freeform/manual mission (every phase has zero tasks) — mint +
        // start, leave every phase transition operator-driven.
        println!(
            "{}",
            style::success(&format!(
                "✓ mission `{mission_id}` minted from config \"{config_id}\" — {} freeform phase(s)",
                config.phases.len()
            ))
        );
        println!("  {}", style::dim("no automated graph; drive phases by hand:"));
        for phase in &config.phases {
            println!(
                "    darkmux phase start {}   {}",
                real_phase_ids[&phase.id],
                style::dim(&format!("— {}", phase.description.as_deref().unwrap_or(&phase.id)))
            );
        }
        return Ok(0);
    }

    let all_known: Vec<&str> = tier1_ids.iter().map(String::as_str).chain(CODER_PHASE_TIER3_KINDS.iter().copied()).collect();
    let executable = steps.values().all(|s| all_known.contains(&s.kind.as_str()));
    if !executable {
        for task in &tasks {
            for step_id in &task.step_ids {
                if let Some(step) = steps.get(step_id) {
                    let _ = crew::lifecycle::save_step(&mission_id, &task.phase_id, step);
                }
            }
        }
        let unknown: Vec<&str> = steps
            .values()
            .map(|s| s.kind.as_str())
            .filter(|k| !all_known.contains(k))
            .collect();
        println!(
            "{}",
            style::warn(&format!(
                "⚠ mission `{mission_id}` minted from config \"{config_id}\", but its graph \
                 references step kind(s) this launcher can't construct yet: {}. Nothing was \
                 dispatched — Task/Step records show the intended shape for inspection. This \
                 config needs Packet 4b's remaining launcher plumbing before `mission launch` \
                 can run it end to end.",
                unknown.join(", ")
            ))
        );
        return Ok(2);
    }

    // Real execution — build the registry (Tier 1 always, plus the
    // coder-phase Tier 3 kinds when the graph actually uses them), start
    // every real phase that has tasks, run the scheduler, then finalize.
    let registry = crew::step_kinds::StepKindRegistry::with_builtins();
    let uses_coder_phase_kinds = steps.values().any(|s| CODER_PHASE_TIER3_KINDS.contains(&s.kind.as_str()));
    if uses_coder_phase_kinds {
        register_coder_phase_kinds(
            &registry,
            &mission_id,
            config,
            &real_phase_ids,
            &collected,
            timeout_seconds,
        )?;
    }

    for phase in &config.phases {
        let real_id = &real_phase_ids[&phase.id];
        if !tasks.iter().any(|t| &t.phase_id == real_id) {
            continue;
        }
        if let Err(e) = crew::lifecycle::phase_start(real_id) {
            eprintln!(
                "{}",
                style::dim(&format!(
                    "mission launch: phase_start({real_id}) failed: {e:#} — continuing; state \
                     can be reconciled with `darkmux phase` verbs."
                ))
            );
        }
    }

    let tasks_by_id: BTreeMap<String, crew::types::Task> =
        tasks.iter().map(|t| (t.id.clone(), t.clone())).collect();
    let facts = crew::step_kinds::Facts::default();
    let est = crew::step_kinds::FixedEstimator::default();
    crew::scheduler::run_step_graph(
        &mut steps,
        &tasks_by_id,
        &registry,
        &facts,
        &est,
        1,
        &crew::concurrent_dispatch::lms_host_factory,
        &mut |record| {
            let _ = flow::record(record);
        },
    )?;

    for task in &tasks {
        for step_id in &task.step_ids {
            if let Some(step) = steps.get(step_id) {
                if let Err(e) = crew::lifecycle::save_step(&mission_id, &task.phase_id, step) {
                    eprintln!("{}", style::dim(&format!("mission launch: step persist warning: {e:#}")));
                }
            }
        }
    }

    let envelope = build_envelope(&mission_id, config, &real_phase_ids, &tasks, &steps);
    let status = envelope.status;
    crew::envelope::finalize_mission(&envelope);

    print_run_summary(&mission_id, &steps);

    use crew::envelope::MissionOutcomeStatus;
    Ok(match status {
        MissionOutcomeStatus::Clean | MissionOutcomeStatus::Degraded => 0,
        MissionOutcomeStatus::Degenerate | MissionOutcomeStatus::Error => 1,
    })
}

/// Parse `--input <file.json>` (a flat object) and `--param key=value`
/// (repeatable; wins over the file) into one collected-inputs map.
fn collect_inputs(
    input_file: Option<&Path>,
    params: &[String],
) -> Result<BTreeMap<String, serde_json::Value>> {
    let mut collected: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    if let Some(path) = input_file {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading --input file {}", path.display()))?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("parsing --input file {} as JSON", path.display()))?;
        let obj = value.as_object().ok_or_else(|| {
            anyhow!(
                "--input file {} must contain a JSON object mapping input name -> value",
                path.display()
            )
        })?;
        for (k, v) in obj {
            collected.insert(k.clone(), v.clone());
        }
    }
    for raw in params {
        let (k, v) = raw
            .split_once('=')
            .ok_or_else(|| anyhow!("--param `{raw}` must be in `key=value` form"))?;
        collected.insert(k.to_string(), serde_json::Value::String(v.to_string()));
    }
    Ok(collected)
}

/// Declared inputs (per [`MissionConfig::inputs`]) still missing from
/// `collected`, excluding `mission_id` (auto-supplied by the launcher — see
/// `launch`'s doc). Optional inputs (`required == Some(false)`) never
/// count as missing.
fn missing_required_inputs<'a>(
    config: &'a MissionConfig,
    collected: &BTreeMap<String, serde_json::Value>,
) -> Vec<&'a mission_config::MissionInput> {
    config
        .inputs
        .iter()
        .filter(|i| i.name != "mission_id")
        .filter(|i| i.required != Some(false))
        .filter(|i| !collected.contains_key(&i.name))
        .collect()
}

fn missing_inputs_message(config: &MissionConfig, missing: &[&mission_config::MissionInput]) -> String {
    let mut msg = format!("mission launch: config \"{}\" is missing required input(s):\n", config.id);
    for i in missing {
        msg.push_str(&format!("  - {}", i.name));
        if let Some(d) = &i.description {
            msg.push_str(&format!(": {d}"));
        }
        msg.push('\n');
    }
    msg.push_str("\nExample --input file:\n");
    let mut obj = serde_json::Map::new();
    for i in &config.inputs {
        if i.name == "mission_id" {
            continue; // launcher-supplied — never asked of the operator
        }
        obj.insert(i.name.clone(), serde_json::Value::String(format!("<{}>", i.name)));
    }
    msg.push_str(&serde_json::to_string_pretty(&serde_json::Value::Object(obj)).unwrap_or_default());
    msg.push_str("\n\nOr pass each as --param:\n  ");
    msg.push_str(
        &config
            .inputs
            .iter()
            .filter(|i| i.name != "mission_id")
            .map(|i| format!("--param {}=<{}>", i.name, i.name))
            .collect::<Vec<_>>()
            .join(" "),
    );
    msg
}

/// `<config-id>-<10-hex-char digest>` — the digest is over the CANONICAL
/// (BTreeMap-sorted) JSON of `collected`, so the SAME operator-supplied
/// inputs always derive the SAME instance id (idempotent relaunch) while
/// different inputs derive a distinct one (a genuinely new instance). No
/// CLI flag needed — see the module doc.
fn derive_mission_id(config_id: &str, collected: &BTreeMap<String, serde_json::Value>) -> Result<String> {
    let canon = serde_json::to_string(collected).context("serializing collected inputs for id derivation")?;
    let digest = blake3::hash(canon.as_bytes());
    let hex = digest.to_hex();
    let short = &hex.as_str()[..10];
    let id = format!("{config_id}-{short}");
    fleet::validate_identifier("mission_id", &id)?;
    Ok(id)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Mint (or, on relaunch, reopen-if-terminal reuse) the Mission + one Phase
/// per declared phase. Mirrors `src/pr_review.rs::build_mission_for_review`'s
/// exact reuse pattern (#1372): a fresh mission is created Active with every
/// phase Planned and started via the real `mission_start_with_reasoning`
/// lifecycle verb (so the standard "mission start" flow record lands); an
/// EXISTING mission (same derived id → same prior launch) is reopened if the
/// prior run left it Closed, and any Abandoned phase is restarted — never
/// re-created from scratch, so a relaunch's Task/Step overwrites land on the
/// SAME instance rather than silently forking a duplicate.
///
/// Returns the doc phase id → real (composed) phase id map every subsequent
/// step needs.
fn ensure_mission_and_phases(mission_id: &str, config: &MissionConfig) -> Result<BTreeMap<String, String>> {
    let real_phase_ids: BTreeMap<String, String> = config
        .phases
        .iter()
        .map(|p| (p.id.clone(), format!("{mission_id}-{}", p.id)))
        .collect();

    let mission_path = crew::lifecycle::mission_path(mission_id);
    if mission_path.exists() {
        if let Ok(text) = std::fs::read_to_string(&mission_path) {
            if let Ok(mission) = serde_json::from_str::<Mission>(&text) {
                if mission.status == MissionStatus::Closed {
                    crew::lifecycle::mission_reopen_with_reasoning(
                        mission_id,
                        Some(&format!("relaunch of config `{}`", config.id)),
                    )?;
                }
            }
        }
        for real_id in real_phase_ids.values() {
            let phase_path = crew::lifecycle::phase_path(mission_id, real_id);
            if let Ok(text) = std::fs::read_to_string(&phase_path) {
                if let Ok(phase) = serde_json::from_str::<Phase>(&text) {
                    if phase.status == PhaseStatus::Abandoned {
                        let _ = crew::lifecycle::phase_start(real_id);
                    }
                }
            }
        }
        return Ok(real_phase_ids);
    }

    let now = now_unix();
    let mission = Mission {
        id: mission_id.to_string(),
        description: config.description.clone().unwrap_or_else(|| config.name.clone()),
        status: MissionStatus::Active,
        phase_ids: config.phases.iter().map(|p| real_phase_ids[&p.id].clone()).collect(),
        created_ts: now,
        started_ts: None,
        closed_ts: None,
        paused_ts: None,
        source_input: None,
        ticket: None,
    };
    crew::lifecycle::save_mission(&mission).context("persisting mission.json")?;

    for phase in &config.phases {
        let real_id = &real_phase_ids[&phase.id];
        let p = Phase {
            id: real_id.clone(),
            mission_id: mission_id.to_string(),
            description: phase.description.clone().unwrap_or_default(),
            status: PhaseStatus::Planned,
            created_ts: now,
            started_ts: None,
            completed_ts: None,
            abandoned_ts: None,
            task_ids: Vec::new(),
        };
        crew::lifecycle::save_phase(&p).with_context(|| format!("persisting phase {real_id}"))?;
    }

    crew::lifecycle::mission_start_with_reasoning(
        mission_id,
        Some(&format!("launched from config `{}`", config.id)),
    )
    .context("starting the newly-minted mission")?;

    Ok(real_phase_ids)
}

/// Build the [`LaunchParams`] `mission_config::interpret` needs: every
/// phase's composed real id (generic, always safe), plus — ONLY for the
/// `coder-phase` task ids `default_phase_graph` itself already hardcodes
/// (`build-coder`/`build-verify`, see that function's own doc) — the
/// role/workdir/image/description overrides `collected` supplies. A config
/// that isn't `coder-phase`-shaped gets no task_overrides here (pure
/// pass-through of its own document defaults); Packet 4b is where a
/// genuinely generic per-config override mapping would need to be invented,
/// per the module doc's scope boundary.
fn build_launch_params(
    config: &MissionConfig,
    real_phase_ids: &BTreeMap<String, String>,
    collected: &BTreeMap<String, serde_json::Value>,
) -> LaunchParams {
    let mut task_overrides = BTreeMap::new();
    if config.id == "coder-phase" {
        let role = collected.get("role").and_then(|v| v.as_str());
        let image = collected.get("image").and_then(|v| v.as_str());
        let workdir = collected.get("workdir").and_then(|v| v.as_str()).map(std::path::PathBuf::from);
        if let Some(role) = role {
            task_overrides.insert(
                "build-coder".to_string(),
                TaskOverride {
                    role_id: Some(role.to_string()),
                    workdir: workdir.clone(),
                    image: image.map(String::from),
                    description: Some(format!("dispatch `{role}` into the worktree")),
                    ..Default::default()
                },
            );
        } else if workdir.is_some() || image.is_some() {
            task_overrides.insert(
                "build-coder".to_string(),
                TaskOverride { workdir: workdir.clone(), image: image.map(String::from), ..Default::default() },
            );
        }
        if let Some(workdir) = workdir {
            task_overrides.insert("build-verify".to_string(), TaskOverride { workdir: Some(workdir), ..Default::default() });
        }
    }

    LaunchParams {
        phase_ids: real_phase_ids.clone(),
        task_overrides,
        step_config_overrides: BTreeMap::new(),
        expansions: BTreeMap::new(),
    }
}

/// Register `mission_run.rs`'s three `coder-phase` Tier 3 kinds against
/// `registry`, using the operator-collected `workdir`/`branch`/`base`/
/// `role`/`image` inputs plus a launcher-resolved `repo_root`. Bails loud
/// (naming the missing input) rather than constructing a kind with an
/// empty path if the graph needs these kinds but the operator didn't
/// supply them.
fn register_coder_phase_kinds(
    registry: &crew::step_kinds::StepKindRegistry,
    mission_id: &str,
    config: &MissionConfig,
    real_phase_ids: &BTreeMap<String, String>,
    collected: &BTreeMap<String, serde_json::Value>,
    timeout_seconds: u32,
) -> Result<()> {
    let require = |name: &str| -> Result<String> {
        collected
            .get(name)
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| {
                anyhow!(
                    "mission launch: config `{}` uses the coder-phase step kinds but no \
                     `{name}` input was supplied",
                    config.id
                )
            })
    };
    let workdir = std::path::PathBuf::from(require("workdir")?);
    let branch = require("branch")?;
    let base = require("base")?;
    let role = collected
        .get("role")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| "coder".to_string());
    let image = collected.get("image").and_then(|v| v.as_str()).map(String::from);

    // The `coder-phase` config has exactly one phase; find its real id.
    let phase_doc_id = config
        .phases
        .iter()
        .find(|p| p.tasks.iter().any(|t| t.steps.iter().any(|s| CODER_PHASE_TIER3_KINDS.contains(&s.kind.as_str()))))
        .map(|p| p.id.clone())
        .ok_or_else(|| anyhow!("mission launch: internal error — no phase in `{}` declares a coder-phase step", config.id))?;
    let real_phase_id = real_phase_ids[&phase_doc_id].clone();
    let session_id = format!("mission-launch-{mission_id}-{real_phase_id}");

    let mission = load_mission_for_brief(mission_id)?;
    let phase = load_phase_for_brief(mission_id, &real_phase_id)?;

    let repo_root = mission_run::repo_root()?;
    registry
        .register(Arc::new(mission_run::MissionWorktreeStepKind {
            repo_root,
            wt_path: workdir.clone(),
            branch: branch.clone(),
            base: base.clone(),
            mission_id: mission_id.to_string(),
            phase_id: real_phase_id.clone(),
            session_id: session_id.clone(),
            role: role.clone(),
        }))
        .map_err(|e| anyhow!("registering mission.worktree: {e}"))?;

    let opts = crew::dispatch::DispatchOpts {
        role_id: role.clone(),
        message: mission_run::coder_brief(&phase, &mission, &[], &[], &[]),
        deliver: None,
        session_id: Some(session_id.clone()),
        timeout_seconds,
        skip_preflight: false,
        json: true,
        watch_paths: Vec::new(),
        workdir: Some(workdir.clone()),
        phase_id: Some(real_phase_id.clone()),
        runtime: crew::dispatch::Runtime::Internal,
        runtime_cmd: "openclaw".to_string(),
        machine: None,
        wait: true,
        compaction: crew::dispatch::CompactionDispatchArgs::default(),
        profile_name: None,
        config_path: None,
        force_container: false,
        max_completion_tokens: None,
        image: image.clone(),
        model_base_url_override: None,
    };
    registry
        .register(Arc::new(mission_run::MissionCoderStepKind {
            opts: Mutex::new(Some(opts)),
            wt_path: workdir.clone(),
            mission_id: mission_id.to_string(),
            phase_id: real_phase_id.clone(),
            session_id: session_id.clone(),
            role_id: role,
            result_slot: Arc::new(Mutex::new(None)),
        }))
        .map_err(|e| anyhow!("registering mission.coder: {e}"))?;

    registry
        .register(Arc::new(mission_run::MissionVerifyStepKind {
            wt_path: workdir,
            base,
            phase_id: real_phase_id,
            result_slot: Arc::new(Mutex::new(None)),
        }))
        .map_err(|e| anyhow!("registering mission.verify: {e}"))?;

    Ok(())
}

fn load_mission_for_brief(mission_id: &str) -> Result<Mission> {
    let text = std::fs::read_to_string(crew::lifecycle::mission_path(mission_id))
        .with_context(|| format!("reading mission.json for `{mission_id}`"))?;
    serde_json::from_str(&text).context("parsing mission.json")
}

fn load_phase_for_brief(mission_id: &str, phase_id: &str) -> Result<Phase> {
    let text = std::fs::read_to_string(crew::lifecycle::phase_path(mission_id, phase_id))
        .with_context(|| format!("reading phase JSON for `{phase_id}`"))?;
    serde_json::from_str(&text).context("parsing phase JSON")
}

/// Fold the interpreted graph's final step statuses into a
/// [`crew::envelope::MissionEnvelope`] — the generic (mission-type-
/// agnostic) status decision: every step Complete → Clean; some Complete
/// and some Error → Degraded (real output produced, but part of the run was
/// constrained); every relevant step Error (nothing completed) → Error. See
/// `envelope.rs`'s own module doc for the phase/mission-outcome mapping
/// this status feeds into.
fn build_envelope(
    mission_id: &str,
    config: &MissionConfig,
    real_phase_ids: &BTreeMap<String, String>,
    tasks: &[crew::types::Task],
    steps: &BTreeMap<String, crew::types::Step>,
) -> crew::envelope::MissionEnvelope {
    use crew::envelope::{MissionEnvelope, MissionOutcomeStatus};

    let errored: Vec<&str> = steps
        .values()
        .filter(|s| s.status == NodeStatus::Error)
        .map(|s| s.id.as_str())
        .collect();
    let completed: Vec<&str> = steps
        .values()
        .filter(|s| s.status == NodeStatus::Complete)
        .map(|s| s.id.as_str())
        .collect();

    let status = if errored.is_empty() {
        MissionOutcomeStatus::Clean
    } else if completed.is_empty() {
        MissionOutcomeStatus::Error
    } else {
        MissionOutcomeStatus::Degraded
    };

    let reason = if errored.is_empty() {
        None
    } else {
        Some(
            errored
                .iter()
                .map(|id| {
                    let out = steps[*id].output.clone().unwrap_or_default();
                    format!("{id}: {out}")
                })
                .collect::<Vec<_>>()
                .join("; "),
        )
    };

    let executed_phase_ids: Vec<&str> = config
        .phases
        .iter()
        .filter(|p| {
            let real_id = &real_phase_ids[&p.id];
            tasks.iter().any(|t| &t.phase_id == real_id)
        })
        .map(|p| real_phase_ids[&p.id].as_str())
        .collect();

    let mut envelope = MissionEnvelope::new(mission_id, status, &executed_phase_ids);
    envelope.reason = reason;
    if !errored.is_empty() && !completed.is_empty() {
        envelope.warnings = vec![format!("{} of {} step(s) errored during launch execution", errored.len(), steps.len())];
    }
    envelope.payload = serde_json::json!({
        "completed_steps": completed,
        "errored_steps": errored,
    });
    envelope
}

fn print_run_summary(mission_id: &str, steps: &BTreeMap<String, crew::types::Step>) {
    let complete = steps.values().filter(|s| s.status == NodeStatus::Complete).count();
    let errored = steps.values().filter(|s| s.status == NodeStatus::Error).count();
    println!(
        "\n{}",
        style::header(&format!("▶ mission `{mission_id}` finished — {complete} step(s) complete, {errored} errored"))
    );
    println!("  {}", style::dim(&format!("darkmux mission status   (or) darkmux mission debrief {mission_id}")));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::io::Write as _;
    use tempfile::{NamedTempFile, TempDir};

    /// Isolates both the crew root (mission/phase/config JSON) and the flow
    /// sink to TempDirs — mirrors `envelope.rs`'s own `CrewGuard`. Every
    /// test using this MUST be `#[serial_test::serial]` since env-var
    /// mutation is a global, cross-test concern.
    struct LaunchTestGuard {
        _tmp_crew: TempDir,
        _tmp_flows: TempDir,
        prev_crew: Option<String>,
        prev_flows: Option<String>,
    }

    impl LaunchTestGuard {
        fn new() -> Self {
            let tmp_crew = TempDir::new().unwrap();
            let tmp_flows = TempDir::new().unwrap();
            let prev_crew = env::var("DARKMUX_CREW_DIR").ok();
            let prev_flows = env::var("DARKMUX_FLOWS_DIR").ok();
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe {
                env::set_var("DARKMUX_CREW_DIR", tmp_crew.path());
                env::set_var("DARKMUX_FLOWS_DIR", tmp_flows.path());
            }
            Self { _tmp_crew: tmp_crew, _tmp_flows: tmp_flows, prev_crew, prev_flows }
        }

        fn write_config(&self, id: &str, json: &str) {
            let dir = crew::loader::mission_configs_dir();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{id}.json")), json).unwrap();
        }
    }

    impl Drop for LaunchTestGuard {
        fn drop(&mut self) {
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe {
                match &self.prev_crew {
                    Some(v) => env::set_var("DARKMUX_CREW_DIR", v),
                    None => env::remove_var("DARKMUX_CREW_DIR"),
                }
                match &self.prev_flows {
                    Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
                    None => env::remove_var("DARKMUX_FLOWS_DIR"),
                }
            }
        }
    }

    const FREEFORM_CONFIG: &str = r#"{
        "id": "freeform-test-mission",
        "name": "Freeform Test Mission",
        "description": "a hand-authored-style freeform test mission",
        "schema_version": "1.1",
        "phases": [
            {"id": "p1", "description": "first phase"},
            {"id": "p2", "description": "second phase"}
        ]
    }"#;

    // ── Generic launch path — freeform (no dispatch, no Docker) ────────

    #[test]
    #[serial_test::serial]
    fn freeform_launch_mints_instance_and_leaves_phases_manual() {
        let guard = LaunchTestGuard::new();
        guard.write_config("freeform-test-mission", FREEFORM_CONFIG);

        let exit = launch("freeform-test-mission", None, &[], 600).expect("launch should succeed");
        assert_eq!(exit, 0);

        // No operator inputs at all -> the id derives from an empty map,
        // deterministically, every time.
        let mission_id = derive_mission_id("freeform-test-mission", &BTreeMap::new()).unwrap();

        let mission_path = crew::lifecycle::mission_path(&mission_id);
        assert!(mission_path.is_file(), "mission.json must exist at {}", mission_path.display());
        let mission: Mission = serde_json::from_str(&std::fs::read_to_string(&mission_path).unwrap()).unwrap();
        assert_eq!(mission.status, MissionStatus::Active);
        assert!(mission.started_ts.is_some(), "launch drives mission_start_with_reasoning, not a bare Active write");
        assert_eq!(mission.phase_ids.len(), 2);
        assert_eq!(mission.phase_ids[0], format!("{mission_id}-p1"));
        assert_eq!(mission.phase_ids[1], format!("{mission_id}-p2"));

        for real_phase_id in &mission.phase_ids {
            let phase_path = crew::lifecycle::phase_path(&mission_id, real_phase_id);
            assert!(phase_path.is_file(), "phase JSON must exist at {}", phase_path.display());
            let phase: Phase = serde_json::from_str(&std::fs::read_to_string(&phase_path).unwrap()).unwrap();
            assert_eq!(phase.status, PhaseStatus::Planned, "freeform phases are never auto-started");
            assert!(phase.started_ts.is_none());
        }

        let snapshot_path = crew::lifecycle::config_snapshot_path(&mission_id);
        assert!(snapshot_path.is_file(), "config-snapshot.json must exist at {}", snapshot_path.display());
        let snapshot = crew::lifecycle::load_config_snapshot(&mission_id).unwrap().unwrap();
        assert_eq!(snapshot.id, "freeform-test-mission");
        assert_eq!(snapshot.phases.len(), 2);

        // No MissionEnvelope for a freeform launch — nothing executed, so
        // finalize_mission never ran.
        assert!(crew::lifecycle::load_envelope(&mission_id).unwrap().is_none());

        // Idempotent relaunch: same (empty) inputs must derive the SAME id.
        let exit2 = launch("freeform-test-mission", None, &[], 600).expect("relaunch should succeed");
        assert_eq!(exit2, 0);
        let mission_id2 = derive_mission_id("freeform-test-mission", &BTreeMap::new()).unwrap();
        assert_eq!(mission_id, mission_id2, "same inputs must derive the same instance id");
    }

    #[test]
    #[serial_test::serial]
    fn freeform_relaunch_after_close_reopens_the_terminal_instance() {
        let guard = LaunchTestGuard::new();
        guard.write_config("freeform-test-mission", FREEFORM_CONFIG);

        launch("freeform-test-mission", None, &[], 600).unwrap();
        let mission_id = derive_mission_id("freeform-test-mission", &BTreeMap::new()).unwrap();

        crew::lifecycle::mission_close_with_reasoning(&mission_id, Some("test close")).unwrap();
        let closed: Mission =
            serde_json::from_str(&std::fs::read_to_string(crew::lifecycle::mission_path(&mission_id)).unwrap())
                .unwrap();
        assert_eq!(closed.status, MissionStatus::Closed);

        // Relaunch: same config, same (empty) inputs -> reopens the SAME
        // instance (Packet 2 reopen semantics) rather than minting a
        // duplicate or erroring on "already exists".
        let exit = launch("freeform-test-mission", None, &[], 600).unwrap();
        assert_eq!(exit, 0);
        let reopened: Mission =
            serde_json::from_str(&std::fs::read_to_string(crew::lifecycle::mission_path(&mission_id)).unwrap())
                .unwrap();
        assert_eq!(reopened.status, MissionStatus::Active, "relaunch must reopen a terminal instance");
        assert!(reopened.closed_ts.is_none(), "reopen clears the prior closure");
        assert_eq!(reopened.id, mission_id, "relaunch reuses the SAME instance id, never a duplicate");
    }

    #[test]
    #[serial_test::serial]
    fn different_inputs_derive_a_distinct_instance_for_the_same_config() {
        let guard = LaunchTestGuard::new();
        guard.write_config("freeform-test-mission", FREEFORM_CONFIG);

        launch("freeform-test-mission", None, &["note=first".to_string()], 600).unwrap();
        launch("freeform-test-mission", None, &["note=second".to_string()], 600).unwrap();

        let mut m1 = BTreeMap::new();
        m1.insert("note".to_string(), serde_json::Value::String("first".to_string()));
        let mut m2 = BTreeMap::new();
        m2.insert("note".to_string(), serde_json::Value::String("second".to_string()));
        let id1 = derive_mission_id("freeform-test-mission", &m1).unwrap();
        let id2 = derive_mission_id("freeform-test-mission", &m2).unwrap();
        assert_ne!(id1, id2);
        assert!(crew::lifecycle::mission_path(&id1).is_file());
        assert!(crew::lifecycle::mission_path(&id2).is_file());
        let _ = guard;
    }

    #[test]
    #[serial_test::serial]
    fn missing_required_inputs_bails_with_a_copy_pasteable_example_and_mints_nothing() {
        let guard = LaunchTestGuard::new();
        // `coder-phase` is embedded — resolves with no user-tier file at all
        // (this test never writes one), and declares workdir/branch/base as
        // required inputs the operator hasn't supplied.
        let err = launch("coder-phase", None, &[], 600).expect_err("missing required inputs must bail");
        let msg = err.to_string();
        assert!(msg.contains("workdir"), "{msg}");
        assert!(msg.contains("branch"), "{msg}");
        assert!(msg.contains("base"), "{msg}");
        assert!(msg.contains("--param"), "expected a copy-pasteable --param example: {msg}");
        assert!(msg.contains("Example --input file"), "expected a copy-pasteable --input example: {msg}");
        assert!(!msg.contains("`mission_id`:"), "mission_id is launcher-supplied, never asked of the operator: {msg}");

        // Nothing minted for any coder-phase-derived id.
        let missions_dir = crew::loader::missions_dir();
        assert!(
            !missions_dir.is_dir() || std::fs::read_dir(&missions_dir).unwrap().next().is_none(),
            "a missing-inputs bail must not mint anything"
        );
        let _ = guard;
    }

    // ── coder-phase wiring — registration only, never a live dispatch ──
    // (mocked dispatches only, never real LMStudio — the actual
    // `MissionCoderStepKind::run` dispatch is exercised, mocked, by
    // `crates/darkmux-crew/tests/mock_dispatch_proof.rs` against the SAME
    // underlying `crew::dispatch::dispatch` primitive this module reuses
    // unchanged; a live coder-phase dogfood dispatch is the release-gate
    // discipline's job per CLAUDE.md, not a `cargo test`-embedded one.)

    #[test]
    #[serial_test::serial]
    fn coder_phase_registration_succeeds_with_a_real_git_repo_and_valid_inputs() {
        let guard = LaunchTestGuard::new();
        let loaded = mission_config::load("coder-phase").unwrap();
        let config = &loaded.config;

        let mut collected = BTreeMap::new();
        collected.insert("workdir".to_string(), serde_json::json!("/tmp/darkmux-mission-launch-test-worktree"));
        collected.insert("branch".to_string(), serde_json::json!("darkmux-test-branch"));
        collected.insert("base".to_string(), serde_json::json!("main"));
        collected.insert("role".to_string(), serde_json::json!("coder"));

        let mission_id = derive_mission_id("coder-phase", &collected).unwrap();
        let real_phase_ids = ensure_mission_and_phases(&mission_id, config).unwrap();

        let registry = crew::step_kinds::StepKindRegistry::with_builtins();
        register_coder_phase_kinds(&registry, &mission_id, config, &real_phase_ids, &collected, 600)
            .expect("registration must succeed against a real repo + valid inputs");

        for kind in CODER_PHASE_TIER3_KINDS {
            assert!(registry.get(kind).is_ok(), "kind `{kind}` must be registered");
        }
        let _ = guard;
    }

    #[test]
    #[serial_test::serial]
    fn register_coder_phase_kinds_bails_loud_naming_the_missing_input() {
        let guard = LaunchTestGuard::new();
        let loaded = mission_config::load("coder-phase").unwrap();
        let config = &loaded.config;
        let collected: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        let mission_id = derive_mission_id("coder-phase", &collected).unwrap();
        let real_phase_ids = ensure_mission_and_phases(&mission_id, config).unwrap();
        let registry = crew::step_kinds::StepKindRegistry::with_builtins();
        let err = register_coder_phase_kinds(&registry, &mission_id, config, &real_phase_ids, &collected, 600)
            .expect_err("must bail without workdir/branch/base supplied");
        assert!(err.to_string().contains("workdir"), "{err}");
        let _ = guard;
    }

    // ── Pure-function unit coverage (no filesystem) ─────────────────────

    #[test]
    fn collect_inputs_params_win_over_input_file() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, r#"{{"role":"file-role","base":"main"}}"#).unwrap();
        let collected = collect_inputs(Some(file.path()), &["role=param-role".to_string()]).unwrap();
        assert_eq!(collected.get("role"), Some(&serde_json::json!("param-role")));
        assert_eq!(collected.get("base"), Some(&serde_json::json!("main")));
    }

    #[test]
    fn collect_inputs_rejects_a_param_with_no_equals_sign() {
        let err = collect_inputs(None, &["not-a-kv-pair".to_string()]).unwrap_err();
        assert!(err.to_string().contains("key=value"));
    }

    #[test]
    fn derive_mission_id_is_deterministic_and_charset_safe() {
        let mut m = BTreeMap::new();
        m.insert("workdir".to_string(), serde_json::json!("/tmp/x"));
        let a = derive_mission_id("coder-phase", &m).unwrap();
        let b = derive_mission_id("coder-phase", &m).unwrap();
        assert_eq!(a, b, "same config id + same inputs must derive the same instance id");
        assert!(a.starts_with("coder-phase-"));

        let mut m2 = m.clone();
        m2.insert("workdir".to_string(), serde_json::json!("/tmp/y"));
        let c = derive_mission_id("coder-phase", &m2).unwrap();
        assert_ne!(a, c, "different inputs must derive a different instance id");
    }

    #[test]
    fn missing_required_inputs_excludes_mission_id_and_optional_fields() {
        let input = |name: &str, required: Option<bool>| mission_config::MissionInput {
            name: name.to_string(),
            description: None,
            required,
            extras: BTreeMap::new(),
        };
        let cfg = MissionConfig {
            id: "x".to_string(),
            name: "X".to_string(),
            description: None,
            schema_version: None,
            inputs: vec![
                input("mission_id", Some(true)),
                input("workdir", Some(true)),
                input("image", Some(false)),
            ],
            phases: Vec::new(),
            extras: BTreeMap::new(),
        };
        let missing = missing_required_inputs(&cfg, &BTreeMap::new());
        let names: Vec<&str> = missing.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["workdir"], "mission_id (launcher-supplied) and image (optional) must not appear");
    }
}
