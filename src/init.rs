//! `darkmux init` — one-command integration setup.
//!
//! Installs skills + (optional) SessionStart hook + (optional) CLAUDE.md
//! merge so Claude Code knows about darkmux without further configuration.
//!
//! Safe to re-run; refreshes the bundled skills after a darkmux upgrade. The
//! skill-install step passes `refresh_darkmux: true` (#1426), so a re-run
//! overwrites an installed `darkmux-*` skill with this binary's embedded
//! copy — but ONLY when that installed copy is unmodified (#1927: content
//! hash-verified against what darkmux itself last wrote there). A
//! locally-edited `darkmux-*` skill, or one with no recorded provenance at
//! all (every skill installed before #1927), is protected instead — the edit
//! survives, and `--force` is the explicit "yes, overwrite it" escape hatch.
//! Never touches the operator's own non-darkmux skills either way. This is
//! what the doctor freshness check's fix_hint relies on; that hint (#1927)
//! now describes the mechanism rather than promising a refresh will happen.
//! The profile registry and config file keep their own never-overwrite
//! discipline; only the darkmux-owned skills ever refresh, and only when
//! provably safe to.

use crate::init_engine::{self, Outcome};
use crate::skills;
use anyhow::{Context, Result, anyhow};
use darkmux_profiles::engine;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct InitOptions {
    pub with_hook: bool,
    pub with_claude_md: Option<PathBuf>,
    pub with_agents_md: Option<PathBuf>,
    pub force: bool,
    pub dry_run: bool,
    /// (#316) `--engine-url`: the OpenAI-compatible server to record as this
    /// machine's engine, probed alone and chosen when it answers.
    pub engine_url: Option<String>,
}

#[derive(Debug, Default)]
pub struct InitReport {
    pub profile_registry_path: Option<PathBuf>,
    pub profile_registry_created: bool,
    pub profile_registry_already_present: bool,
    /// (#2038) The model id `init` wrote into the registry's worker slots in
    /// place of `<your-worker-model-id>`, chosen from what LM Studio has.
    pub worker_model_filled: Option<String>,
    /// (#2038) Why the placeholder was left in place, when it was.
    pub worker_model_unfilled_reason: Option<String>,
    /// (#2053) The utility (compactor) model id `init` wrote in place of one
    /// LM Studio does not have, chosen from what is downloaded.
    pub utility_model_filled: Option<String>,
    /// (#2053) Why the utility binding was left as written, when it was.
    pub utility_model_unfilled_reason: Option<String>,
    /// (#3020) The window `init` wrote into `internal.utility.n_ctx`, sized
    /// from the machine's AI headroom, when it changed the shipped one.
    pub utility_model_n_ctx: Option<u32>,
    /// (#3020) Why `init` removed the shipped `internal.utility` binding
    /// instead of registering a utility model, when it did.
    pub utility_model_unregistered_reason: Option<String>,
    /// (#316) The engine the models were planned from, when it is not LM
    /// Studio: the endpoint id (or URL) of an OpenAI-compatible server.
    pub models_engine: Option<String>,
    /// (#316, ENGINES.md 9.2) Which engine `init` found answering, and what
    /// it recorded as `default_engine`.
    pub engine: Option<Outcome>,
    /// (#316) Why the chosen engine could not be recorded, when it could not.
    pub engine_unrecorded: Option<String>,
    pub config_path: Option<PathBuf>,
    pub config_created: bool,
    pub config_already_present: bool,
    pub skills_targets: Vec<PathBuf>,
    pub skills_installed: Vec<String>,
    pub skills_overwritten: Vec<String>,
    pub skills_skipped: Vec<String>,
    /// (#1927) `darkmux-*` skills whose installed copy is locally modified
    /// (or has no recorded provenance) and so was NOT refreshed — the edit
    /// survives. Pass `--force` to overwrite one of these anyway.
    pub skills_protected: Vec<String>,
    /// (#1927) `darkmux-*` skills from `skills_protected` that `--force`
    /// overwrote anyway. Always a subset of `skills_overwritten`.
    pub skills_force_overwrote_modified: Vec<String>,
    pub hook_added: Option<PathBuf>,
    pub hook_already_present: bool,
    pub claude_md_path: Option<PathBuf>,
    pub claude_md_appended: bool,
    pub claude_md_already_present: bool,
    pub agents_md_path: Option<PathBuf>,
    pub agents_md_appended: bool,
    pub agents_md_already_present: bool,
}

/// The example profile registry, embedded at compile time. Copied to
/// `~/.darkmux/profiles.json` on `darkmux init` if no registry exists yet.
const EXAMPLE_PROFILES_JSON: &str = include_str!("../profiles.example.json");

/// The committed default config (`config.example.json`) — byte-equal to
/// `DarkmuxConfig::with_defaults()`'s pretty output, i.e. exactly what
/// `darkmux init` writes (modulo the personalized `machine_id`). `#[cfg(test)]`
/// only: not embedded in the release binary; it exists so
/// `example_config_matches_with_defaults` can drift-guard the committed
/// reference against the code (#661).
#[cfg(test)]
const EXAMPLE_CONFIG: &str = include_str!("../config.example.json");

pub fn init(opts: &InitOptions) -> Result<InitReport> {
    let mut report = InitReport::default();

    // 0) (#3143 item 5) Refuse a bad input before the first write, so a
    //    failing run leaves the machine as it found it.
    preflight(opts)?;

    // 1) Bootstrap the profile registry (the "fresh user has no config" case).
    let registry_path = bootstrap_registry(opts.dry_run, &mut report)?;

    // 1b) Bootstrap the config file (#661). Same never-overwrite discipline as
    //     the profile registry — the operator's config edits outrank a re-run.
    let (config_path, config_created) = bootstrap_config(opts.dry_run)?;
    report.config_already_present = !config_created;
    report.config_path = Some(config_path.clone());
    report.config_created = config_created;

    // 1c) (#316, ENGINES.md 9.2) Which engine answers on this machine, recorded
    //     as `default_engine` while none was chosen yet (`may_detect`), or
    //     when the operator names one with `--engine-url`.
    settle_engine(opts, &registry_path, &config_path, &mut report)?;

    // 1d) (#2038) Fill the placeholder worker model from what the engine has.
    //     Runs on a fresh registry AND on a re-run against one that still holds
    //     the placeholder (the operator downloaded a model after the first
    //     init). A registry without the placeholder is never touched, so an
    //     operator's own edits outrank this every time.
    if !opts.dry_run && registry_path.exists() {
        let engine = models_engine(&report, &registry_path, &config_path);
        fill_models(&registry_path, &engine, &mut report);
    }

    // 3)-6) Skills, and the optional hook and doc merges.
    install_integrations(opts, &mut report)?;

    Ok(report)
}

/// Every input `init` can refuse, checked without writing: a bad
/// `--engine-url`, a settings.json the hook cannot go into, and a malformed
/// CLAUDE.md or AGENTS.md block under `--force`. The merges run as dry runs,
/// so the check is the same code that refuses later.
fn preflight(opts: &InitOptions) -> Result<()> {
    if let Some(url) = opts.engine_url.as_deref() {
        init_engine::engine_url(url).map_err(|e| anyhow!("--engine-url: {e}"))?;
    }
    if opts.with_hook {
        ensure_session_start_hook(&claude_settings_path()?, true, opts.force)?;
    }
    if let Some(target) = opts.with_claude_md.as_ref() {
        ensure_claude_md_section(target, true, opts.force)?;
    }
    if let Some(target) = opts.with_agents_md.as_ref() {
        ensure_agents_md_section(target, true, opts.force)?;
    }
    Ok(())
}

/// Step 1: bootstrap the profile registry. Never overwrites — even with
/// --force — because the user's profile edits matter more than re-running
/// init. A dry run reports what it would create and writes nothing.
fn bootstrap_registry(dry_run: bool, report: &mut InitReport) -> Result<PathBuf> {
    let registry_path = user_profile_registry_path()?;
    report.profile_registry_path = Some(registry_path.clone());
    if registry_path.exists() {
        report.profile_registry_already_present = true;
    } else if !dry_run {
        if let Some(parent) = registry_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(&registry_path, EXAMPLE_PROFILES_JSON)
            .with_context(|| format!("writing {}", registry_path.display()))?;
        report.profile_registry_created = true;
    } else {
        // dry-run: report what would happen without writing
        report.profile_registry_created = true;
    }
    Ok(registry_path)
}

/// Step 3: install the skills, recording what happened on `report`.
fn install_skills_step(opts: &InitOptions, report: &mut InitReport) -> Result<()> {
    // 3) Skills install. `refresh_darkmux: true` (#1426) makes a re-run after a
    //    binary upgrade refresh an installed darkmux-* skill (idempotent
    //    refresh) — but only an unmodified one (#1927); a locally-edited copy
    //    is protected and needs `--force` to overwrite. The installer only
    //    ever writes darkmux-* names, so the operator's own skills are never
    //    touched regardless.
    let skills_report = skills::install_skills(&skills::InstallOptions {
        target: None,
        force: opts.force,
        dry_run: opts.dry_run,
        refresh_darkmux: true,
    })?;
    report.skills_targets = skills_report.targets;
    report.skills_installed = skills_report.installed;
    report.skills_overwritten = skills_report.overwritten;
    report.skills_skipped = skills_report.skipped;
    report.skills_protected = skills_report.protected;
    report.skills_force_overwrote_modified = skills_report.force_overwrote_modified;
    Ok(())
}

/// Steps 3-6: install the skills, then the optional SessionStart hook and
/// CLAUDE.md / AGENTS.md merges, recording each on `report`.
fn install_integrations(opts: &InitOptions, report: &mut InitReport) -> Result<()> {
    install_skills_step(opts, report)?;

    // 4) SessionStart hook (optional)
    if opts.with_hook {
        let settings = claude_settings_path()?;
        let result = ensure_session_start_hook(&settings, opts.dry_run, opts.force)?;
        report.hook_added = Some(settings);
        report.hook_already_present = !result;
    }

    // 5) CLAUDE.md merge (optional). `--force` refreshes a stale block in place
    //    (#1449 — the upgrade path); without it, an existing block is a no-op.
    if let Some(target) = opts.with_claude_md.as_ref() {
        let appended = ensure_claude_md_section(target, opts.dry_run, opts.force)?;
        report.claude_md_path = Some(target.clone());
        report.claude_md_appended = appended;
        report.claude_md_already_present = !appended;
    }

    // 6) AGENTS.md merge (optional). Same --force refresh semantics (#1449).
    if let Some(target) = opts.with_agents_md.as_ref() {
        let appended = ensure_agents_md_section(target, opts.dry_run, opts.force)?;
        report.agents_md_path = Some(target.clone());
        report.agents_md_appended = appended;
        report.agents_md_already_present = !appended;
    }
    Ok(())
}

/// (#316) Step 1c: detect the answering engines and record the choice.
/// An engine already chosen is kept unless the operator passed
/// `--engine-url` ([`init_engine::may_detect`]).
fn settle_engine(opts: &InitOptions, registry_path: &Path, config_path: &Path, report: &mut InitReport) -> Result<()> {
    let explicit = opts
        .engine_url
        .as_deref()
        .map(init_engine::engine_url)
        .transpose()
        .map_err(|e| anyhow!("--engine-url: {e}"))?;
    let text = fs::read_to_string(registry_path).unwrap_or_else(|_| EXAMPLE_PROFILES_JSON.to_string());
    let (engine, from) = init_engine::configured(config_path);
    if explicit.is_none() && !init_engine::may_detect(&text, &engine, from) {
        report.engine = Some(Outcome::Kept { engine, from });
        return Ok(());
    }
    let (answering, probed, error) = init_engine::detect(&init_engine::LiveProbe, &text, explicit.as_deref());
    let mut terminal = init_engine::TerminalAsk;
    let ask: Option<&mut dyn init_engine::Ask> = if opts.dry_run { None } else { Some(&mut terminal) };
    let mut outcome = init_engine::decide(answering, probed, explicit.as_deref(), error, ask);
    if let Err(reason) = record_engine(&mut outcome, &text, registry_path, config_path, opts.dry_run) {
        report.engine_unrecorded = Some(reason);
    }
    report.engine = Some(outcome);
    Ok(())
}

/// Write a chosen server: its endpoint into the registry (when new), then
/// `default_engine` into the config, through `config set`'s own checks.
/// Everything that could refuse is checked first ([`init_engine::record_blocker`]),
/// and a config write that still fails puts the registry back, so a refusal
/// never leaves half a record.
fn record_engine(
    outcome: &mut Outcome,
    text: &str,
    registry_path: &Path,
    config_path: &Path,
    dry_run: bool,
) -> std::result::Result<(), String> {
    let new_text = init_engine::plan_record(outcome, text)?;
    let Outcome::Chosen { endpoint: Some(id), .. } = &*outcome else {
        return Ok(());
    };
    if let Some(blocker) = init_engine::record_blocker(new_text.as_deref().unwrap_or(text), id, registry_path) {
        return Err(blocker);
    }
    if dry_run {
        return Ok(());
    }
    let previous = fs::read_to_string(registry_path).ok();
    if let Some(t) = new_text.as_deref() {
        fs::write(registry_path, t).map_err(|e| format!("writing {}: {e}", registry_path.display()))?;
    }
    let set = crate::config_cmd::set_at(config_path, darkmux_types::default_engine::KEY, id);
    if let (Err(e), Some(_)) = (&set, new_text.as_deref()) {
        return Err(format!("{e:#} ({})", put_back(registry_path, previous.as_deref())));
    }
    set.map(|_| ()).map_err(|e| format!("{e:#}"))
}

/// Put the registry back as it was before `record_engine` wrote it (removed,
/// when there was none), and say whether that worked.
fn put_back(registry_path: &Path, previous: Option<&str>) -> &'static str {
    let restored = match previous {
        Some(p) => fs::write(registry_path, p).is_ok(),
        None => fs::remove_file(registry_path).is_ok(),
    };
    if restored { "profiles.json was put back" } else { "profiles.json could NOT be put back" }
}

/// The engine whose models `init` plans from.
#[derive(Debug)]
enum ModelsEngine {
    Lmstudio,
    /// An OpenAI-compatible server, named as messages name it, and the ids it
    /// lists (or why it did not answer).
    Server { name: String, models: std::result::Result<Vec<String>, String> },
}

/// The engine chosen this run, else the one the config names.
fn models_engine(report: &InitReport, registry_path: &Path, config_path: &Path) -> ModelsEngine {
    // An engine found but not recorded is not the one dispatch will use.
    let recorded = report.engine.as_ref().filter(|_| report.engine_unrecorded.is_none());
    match recorded {
        Some(o) if o.server().is_some() => {
            let (name, models) = o.server().expect("checked");
            return ModelsEngine::Server { name, models: Ok(models.to_vec()) };
        }
        Some(Outcome::Chosen { .. }) => return ModelsEngine::Lmstudio,
        _ => {}
    }
    let (name, _) = init_engine::configured(config_path);
    if name.trim().eq_ignore_ascii_case(darkmux_types::default_engine::LMSTUDIO) {
        return ModelsEngine::Lmstudio;
    }
    let text = fs::read_to_string(registry_path).unwrap_or_default();
    let models = match init_engine::endpoint_url(&text, &name) {
        Some(url) => init_engine::Probe::served(&init_engine::LiveProbe, &url).map_err(|e| {
            format!("the default engine `{name}` did not answer `GET /models` at {url} ({e:#}). Start it, then re-run `darkmux init`.")
        }),
        None => Err(format!("the default engine `{name}` is not an endpoint in {}.", registry_path.display())),
    };
    ModelsEngine::Server { name, models }
}

/// Steps 1d and 1e: fill the worker placeholder and verify the utility
/// binding against what the engine has, recording each outcome on `report`.
fn fill_models(registry_path: &Path, engine: &ModelsEngine, report: &mut InitReport) {
    if let ModelsEngine::Server { name, models } = engine {
        report.models_engine = Some(name.clone());
        fill_models_on_server(registry_path, name, models, report);
        return;
    }
    match fill_worker_model(registry_path) {
        Ok(Some(id)) => report.worker_model_filled = Some(id),
        Ok(None) => {}
        Err(reason) => report.worker_model_unfilled_reason = Some(reason),
    }
    // (#2053) The utility binding ships as a literal id too; on a machine
    // whose key differs (publisher prefix, or no such model) the first
    // dispatch warned about its compactor. Verify it the same way.
    // (#3020) It also ships a window, sized here from the machine's AI
    // headroom; a machine that cannot hold one registers no utility.
    match fill_utility_model(registry_path) {
        Ok(Some(UtilityPlan::Bind { id, n_ctx })) => {
            report.utility_model_filled = id;
            report.utility_model_n_ctx = n_ctx;
        }
        Ok(Some(UtilityPlan::Unbind(reason))) => report.utility_model_unregistered_reason = Some(reason),
        Ok(Some(UtilityPlan::Missing(reason))) => report.utility_model_unfilled_reason = Some(reason),
        Ok(Some(UtilityPlan::Keep)) | Ok(None) => {}
        Err(reason) => report.utility_model_unfilled_reason = Some(reason),
    }
}

/// The fill-in-the-blank `profiles.example.json` ships in every worker slot.
/// `dispatch_internal::is_placeholder_model_id` catches any `<...>` id at
/// dispatch time; this is the one `init` knows how to fill.
pub const PLACEHOLDER_MODEL_ID: &str = "<your-worker-model-id>";

/// (#316 re-review) Whether a worker slot still names the placeholder: a
/// profile model whose `id` is it. The example's `_comment` mentions the
/// placeholder too, so a substring test reads a registry whose slots the
/// operator filled by hand as unfilled. A registry that does not parse has
/// no slot `init` can fill.
pub fn has_placeholder_slot(registry_json: &str) -> bool {
    darkmux_profiles::profiles::parse_registry_lenient(registry_json)
        .is_ok_and(|r| r.profiles.values().flat_map(|p| p.models.iter()).any(|m| m.id == PLACEHOLDER_MODEL_ID))
}

/// (#2038) Replace every placeholder worker id in the registry text with
/// `model_id`. Text-level on purpose: the registry is the operator's file,
/// and a parse-and-reserialize would reorder keys and drop their formatting.
/// `None` when the placeholder is absent, so callers can leave the file
/// byte-identical.
pub fn fill_placeholder_models(registry_json: &str, model_id: &str) -> Option<String> {
    if !registry_json.contains(PLACEHOLDER_MODEL_ID) {
        return None;
    }
    Some(registry_json.replace(PLACEHOLDER_MODEL_ID, model_id))
}

/// The share of RAM a first worker model may occupy on disk-size terms:
/// the OS, the KV cache at the profile's context, and a utility model all
/// need room, so the largest model that "fits" is the largest under this.
pub fn worker_model_budget_bytes(total_ram_gb: u32) -> u64 {
    u64::from(total_ram_gb) * 1_000_000_000 * 6 / 10
}

/// (#2038) Pick the worker model: a model LM Studio already has loaded wins
/// (the operator chose it), else the largest downloaded LLM under the RAM
/// budget. Embeddings are never workers. `loaded` entries may carry the
/// `darkmux:` namespace prefix; it is stripped for matching.
pub fn choose_worker_model(
    loaded: &[String],
    available: &[engine::ModelMeta],
    total_ram_gb: u32,
) -> Option<String> {
    let llms: Vec<&engine::ModelMeta> = available.iter().filter(|m| m.model_type == "llm").collect();
    for id in loaded {
        let key = darkmux_gestalt::bare_model_key(id);
        if let Some(m) = llms.iter().find(|m| m.model_key == key) {
            return Some(m.model_key.clone());
        }
    }
    let budget = worker_model_budget_bytes(total_ram_gb);
    llms.iter()
        .filter(|m| m.size_bytes <= budget)
        .max_by_key(|m| m.size_bytes)
        .map(|m| m.model_key.clone())
}

/// The IO half: read the registry, ask LM Studio, write the fill. `Ok(None)`
/// when there was no placeholder; `Err(reason)` when there was one and it
/// could not be filled (no `lms`, nothing downloaded), with the reason in
/// operator words.
fn fill_worker_model(registry_path: &std::path::Path) -> std::result::Result<Option<String>, String> {
    let text = fs::read_to_string(registry_path).map_err(|e| format!("reading {}: {e}", registry_path.display()))?;
    if !has_placeholder_slot(&text) {
        return Ok(None);
    }
    let available = match engine::lmstudio().catalog() {
        Ok(v) => v,
        Err(e) => {
            return Err(format!(
                "could not ask LM Studio what is downloaded ({e:#}). Install LM Studio and enable its \
                 command-line tool (`~/.lmstudio/bin/lms bootstrap`), download a model, then re-run \
                 `darkmux init`."
            ))
        }
    };
    let loaded: Vec<String> = engine::lmstudio().loaded()
        .map(|v| v.into_iter().map(|m| m.model).collect())
        .unwrap_or_default();
    let ram_gb = darkmux_hardware::detect().total_ram_gb;
    let Some(id) = choose_worker_model(&loaded, &available, ram_gb) else {
        return Err(no_worker_model_reason(ram_gb));
    };
    let filled = fill_placeholder_models(&text, &id).expect("placeholder was present");
    fs::write(registry_path, filled).map_err(|e| format!("writing {}: {e}", registry_path.display()))?;
    Ok(Some(id))
}

/// (#316, ENGINES.md 9.2 step 5) The worker model on an OpenAI-compatible
/// server: the one model it lists, when it lists one. It reports no sizes and
/// loads whichever listed model a request names, so with several `init`
/// cannot pick one, nor a context to load it at, and says so.
pub fn choose_server_worker(name: &str, models: &[String]) -> std::result::Result<String, String> {
    match models {
        [only] => Ok(only.clone()),
        [] => Err(format!("`{name}` lists no models. Start it with the model you want, then re-run `darkmux init`.")),
        _ => Err(format!(
            "`{name}` lists {} models and loads whichever one a request names; it reports neither their sizes nor \
             what it holds, so init cannot pick one or a context to load it at. Put the one you want in place of \
             `{PLACEHOLDER_MODEL_ID}` in profiles.json: {}.",
            models.len(),
            init_engine::name_models(models)
        )),
    }
}

/// The utility model on a server: the shipped id when it lists that, or the
/// same model under a publisher-prefixed key. Sizes are unknown there, so
/// there is no smallest-real-model fallback.
pub fn choose_server_utility(current: &str, models: &[String]) -> Option<String> {
    let metas: Vec<engine::ModelMeta> = models
        .iter()
        .map(|id| engine::ModelMeta {
            model_key: id.clone(),
            display_name: id.clone(),
            publisher: String::new(),
            size_bytes: 0,
            params_string: None,
            architecture: None,
            max_context_length: None,
            trained_for_tool_use: false,
            model_type: "llm".to_string(),
        })
        .collect();
    choose_utility_model(current, &metas)
}

/// The IO half of [`choose_server_worker`] and [`choose_server_utility`].
fn fill_models_on_server(
    registry_path: &Path,
    name: &str,
    models: &std::result::Result<Vec<String>, String>,
    report: &mut InitReport,
) {
    let Ok(text) = fs::read_to_string(registry_path) else {
        return;
    };
    let models = match models {
        Ok(m) => m,
        Err(reason) => {
            if has_placeholder_slot(&text) {
                report.worker_model_unfilled_reason = Some(reason.clone());
            }
            return;
        }
    };
    let read = text.clone();
    let text = fill_server_worker(text, name, models, report);
    let text = fill_server_utility(text, name, models, report);
    if text == read {
        return;
    }
    if let Err(e) = fs::write(registry_path, &text) {
        report.worker_model_unfilled_reason = Some(format!("writing {}: {e}", registry_path.display()));
        report.worker_model_filled = None;
        report.utility_model_filled = None;
        report.utility_model_unregistered_reason = None;
    }
}

/// `text` with the worker placeholder filled from the server's one model.
fn fill_server_worker(text: String, name: &str, models: &[String], report: &mut InitReport) -> String {
    if !has_placeholder_slot(&text) {
        return text;
    }
    match choose_server_worker(name, models) {
        Ok(id) => {
            let filled = fill_placeholder_models(&text, &id).expect("placeholder was present");
            report.worker_model_filled = Some(id);
            filled
        }
        Err(reason) => {
            report.worker_model_unfilled_reason = Some(reason);
            text
        }
    }
}

/// `text` with the shipped utility binding checked against what the server
/// lists. A hand-set binding, or one naming its own endpoint, is left alone.
fn fill_server_utility(text: String, name: &str, models: &[String], report: &mut InitReport) -> String {
    let Some((current, _)) = shipped_utility_binding(&text) else {
        return text;
    };
    if utility_on_unmanaged_endpoint(&text) {
        return text;
    }
    match choose_server_utility(&current, models) {
        Some(id) if id != current => rebind_server_utility(text, id, report),
        Some(_) => text,
        // (#316) Unbound, not kept: a server asked for a model it does not
        // list may fetch it (mlx_lm.server downloads it from Hugging Face) on
        // the first compaction or radio-routing call.
        None => unbind_server_utility(text, name, &current, report),
    }
}

/// `text` with the utility binding moved to `id`, the key the server lists it
/// under.
fn rebind_server_utility(text: String, id: String, report: &mut InitReport) -> String {
    match set_utility_binding(&text, &id, None) {
        Some(t) => {
            report.utility_model_filled = Some(id);
            t
        }
        None => text,
    }
}

/// `text` with the utility binding removed, and the reason on `report`.
fn unbind_server_utility(text: String, name: &str, current: &str, report: &mut InitReport) -> String {
    match remove_utility_binding(&text) {
        Some(t) => {
            report.utility_model_unregistered_reason = Some(format!(
                "`{name}` does not list the registry's utility model `{current}`, so no utility model is \
                 bound (compaction is off). Set `internal.utility` in profiles.json to a small instruct \
                 model it serves."
            ));
            t
        }
        None => text,
    }
}

/// Why no worker model was chosen from an answered catalog.
/// `ram_gb` is 0 when the RAM size could not be read, and then nothing
/// "fits"; the reason says that rather than naming a 0 GB budget.
fn no_worker_model_reason(ram_gb: u32) -> String {
    if ram_gb == 0 {
        return "could not detect this machine's RAM, so no downloaded LLM could be sized against it. \
                Load the model you want in LM Studio (a loaded model is chosen whatever its size), then \
                re-run `darkmux init`."
            .to_string();
    }
    format!(
        "LM Studio has no downloaded LLM that fits in {ram_gb} GB. Download one in LM Studio, then \
         re-run `darkmux init`."
    )
}

/// A utility model has to be a real model, not a toy: the smallest LLM at or
/// above this size. Keeps a 0.5B experiment from becoming the compactor.
const UTILITY_MODEL_MIN_BYTES: u64 = 1_000_000_000;

/// (#2053) The utility (compactor) model: the registry's `current` id if
/// LM Studio has it under that key; the bare key if `current` only differs
/// by a publisher prefix (`qwen/qwen3-4b-instruct-2507` vs
/// `qwen3-4b-instruct-2507`, the shape that shipped in the example
/// registry); else the smallest downloaded LLM that is a real model.
pub fn choose_utility_model(current: &str, available: &[engine::ModelMeta]) -> Option<String> {
    let llms: Vec<&engine::ModelMeta> = available.iter().filter(|m| m.model_type == "llm").collect();
    if llms.iter().any(|m| m.model_key == current) {
        return Some(current.to_string());
    }
    let bare = current.rsplit('/').next().unwrap_or(current);
    if let Some(m) = llms.iter().find(|m| m.model_key == bare || m.model_key.rsplit('/').next() == Some(bare)) {
        return Some(m.model_key.clone());
    }
    llms.iter()
        .filter(|m| m.size_bytes >= UTILITY_MODEL_MIN_BYTES)
        .min_by_key(|m| m.size_bytes)
        .map(|m| m.model_key.clone())
}

/// Locate the model id of the registry's `internal.utility` binding in the
/// raw text: the byte range of `<id>` in `"utility": { "id": "<id>", "n_ctx": N }`.
/// A hand-rolled scan rather than a regex dependency (the dep set is small on
/// purpose). `None` when the key is absent, or not followed by an object with
/// a string `id` (a bare string is not a valid binding).
fn utility_value_span(registry_json: &str) -> Option<(usize, usize)> {
    let ValueAt::Object(open) = string_value_after_key(registry_json, 0, "\"utility\"")? else {
        return None;
    };
    // The first `"id"` string inside the object.
    let close = open + registry_json[open..].find('}')?;
    match string_value_after_key(&registry_json[..close], open, "\"id\"")? {
        ValueAt::Str(start, end) => Some((start, end)),
        ValueAt::Object(_) => None,
    }
}

/// What follows `key:` in `text`, searching from `from`: a string's byte
/// range (exclusive of its quotes) or the byte offset of an object's `{`.
enum ValueAt {
    Str(usize, usize),
    Object(usize),
}

fn string_value_after_key(text: &str, from: usize, key: &str) -> Option<ValueAt> {
    let k = from + text[from..].find(key)?;
    let rest = &text[k + key.len()..];
    let colon = rest.find(':')?;
    if !rest[..colon].trim().is_empty() {
        return None;
    }
    let after = &rest[colon + 1..];
    let ws = after.len() - after.trim_start().len();
    let value = k + key.len() + colon + 1 + ws;
    if text[value..].starts_with('{') {
        return Some(ValueAt::Object(value));
    }
    if !text[value..].starts_with('"') {
        return None;
    }
    let start = value + 1;
    let end = start + text[start..].find('"')?;
    Some(ValueAt::Str(start, end))
}

/// (#3020) What `init` does to the shipped `internal.utility` binding.
#[derive(Debug, PartialEq, Eq)]
pub enum UtilityPlan {
    /// Nothing to change.
    Keep,
    /// Rewrite the binding: `id` and `n_ctx` are each `Some` only when that
    /// part changes (a publisher-prefixed id renamed to LM Studio's key, the
    /// shipped window resized to the machine).
    Bind { id: Option<String>, n_ctx: Option<u32> },
    /// Keep a binding the operator set, whose model LM Studio does not have;
    /// the reason says so in operator words.
    Missing(String),
    /// Remove the binding: this machine should not register a utility model.
    /// The reason is in operator words.
    Unbind(String),
}

/// (#3020) The utility model's window on this machine, in tokens: what is
/// left of the AI headroom after the worker's and the utility's weights,
/// split evenly between the two models' KV caches (both are resident while
/// a dispatch compacts, and neither has a measured claim on more), priced at
/// the ledger's size-derived KV rate, and never above `cap`. 0 when the
/// weights alone do not fit.
pub fn utility_window(headroom_bytes: u64, worker_bytes: u64, utility_bytes: u64, cap: u32) -> u32 {
    let kv_each = headroom_bytes.saturating_sub(worker_bytes).saturating_sub(utility_bytes) / 2;
    let rate = darkmux_profiles::model_ledger::fallback_kv_rate_for_size(utility_bytes).max(1);
    u32::try_from(kv_each / rate).unwrap_or(u32::MAX).min(cap)
}

/// (#3020) Decide the shipped utility binding from what LM Studio has and
/// what the machine can hold. `window` is the binding's `n_ctx` when it is
/// still the shipped one (init's to size), `None` when the operator set it
/// (never resized or removed, even when its model is missing). `headroom` is doctor's AI headroom in
/// bytes, `None` when it could not be read (the shipped window is kept).
/// `worker_bytes` is the largest worker model the registry names that LM
/// Studio has. `floor` is the smallest window worth registering: the
/// smallest window a worker profile on this machine declares, since the
/// compactor takes in what a worker's context holds.
pub fn plan_utility_binding(
    current: &str,
    window: Option<u32>,
    available: &[engine::ModelMeta],
    headroom: Option<u64>,
    worker_bytes: u64,
    floor: u32,
) -> UtilityPlan {
    let Some(id) = choose_utility_model(current, available) else {
        // A hand-set window makes the binding the operator's: keep it.
        if window.is_none() {
            return UtilityPlan::Missing(format!(
                "the registry's utility model `{current}` is not downloaded and no LLM of at least 1 GB is. \
                 Its binding has a hand-set window, so it is kept as written; download `{current}` in LM \
                 Studio, or edit `internal.utility` in profiles.json."
            ));
        }
        return UtilityPlan::Unbind(format!(
            "the registry's utility model `{current}` is not downloaded and no LLM of at least 1 GB is, so \
             no utility model is registered: compaction and radio routing are off. Download a small instruct model (a 4B \
             is ideal) in LM Studio, then add `\"utility\": {{ \"id\": \"<model>\", \"n_ctx\": <window> }}` \
             under `internal` in profiles.json."
        ));
    };
    let mut n_ctx = None;
    if let (Some(shipped), Some(headroom)) = (window, headroom) {
        let meta = available.iter().find(|m| m.model_type == "llm" && m.model_key == id).expect("chosen from available");
        let cap = meta.max_context_length.map_or(shipped, |max| max.min(shipped));
        let fits = utility_window(headroom, worker_bytes, meta.size_bytes, cap);
        if fits < floor.max(1) {
            let gb = headroom / (1024 * 1024 * 1024);
            return UtilityPlan::Unbind(format!(
                "this machine has {gb} GB available for AI (doctor's `RAM headroom`), room for a {fits}-token \
                 window for `{id}` (shipped as `{current}`) beside the worker, below the {floor} tokens worth \
                 registering. No utility model is registered, so compaction and radio routing are off here. This machine is best \
                 used as a fleet client: route work to a peer with `profile@machine`."
            ));
        }
        if fits != shipped {
            n_ctx = Some(fits);
        }
    }
    let id = (id != current).then_some(id);
    if id.is_none() && n_ctx.is_none() {
        return UtilityPlan::Keep;
    }
    UtilityPlan::Bind { id, n_ctx }
}

/// The byte range of the `n_ctx` number inside the `internal.utility` object.
fn utility_n_ctx_span(registry_json: &str) -> Option<(usize, usize)> {
    let ValueAt::Object(open) = string_value_after_key(registry_json, 0, "\"utility\"")? else {
        return None;
    };
    let close = open + registry_json[open..].find('}')?;
    let k = open + registry_json[open..close].find("\"n_ctx\"")?;
    let rest = &registry_json[k + "\"n_ctx\"".len()..close];
    let colon = rest.find(':')?;
    let after = &rest[colon + 1..];
    let start = k + "\"n_ctx\"".len() + colon + 1 + (after.len() - after.trim_start().len());
    let len = registry_json[start..close].find(|c: char| !c.is_ascii_digit())?;
    (len > 0).then_some((start, start + len))
}

/// (#3020) Rewrite the `internal.utility` binding's `id`, and its `n_ctx`
/// when `n_ctx` is `Some`. Text-level, like the other fills. `None` when the
/// registry has no object-form binding (or no `n_ctx` to rewrite).
pub fn set_utility_binding(registry_json: &str, id: &str, n_ctx: Option<u32>) -> Option<String> {
    let (vs, ve) = utility_value_span(registry_json)?;
    let mut out = registry_json.to_string();
    // The window sits after the id inside the object; rewrite it first so the
    // id's byte range is still valid.
    if let Some(n) = n_ctx {
        let (ns, ne) = utility_n_ctx_span(registry_json)?;
        if ns < ve {
            return None;
        }
        out.replace_range(ns..ne, &n.to_string());
    }
    out.replace_range(vs..ve, id);
    Some(out)
}

/// (#3020) Remove the `"utility": { .. }` member from the registry text,
/// with the comma that joined it to a sibling, so the file stays valid JSON
/// and keeps its shape. `None` when there is no object-form binding.
pub fn remove_utility_binding(registry_json: &str) -> Option<String> {
    let key = "\"utility\"";
    let ValueAt::Object(open) = string_value_after_key(registry_json, 0, key)? else {
        return None;
    };
    let key_start = registry_json.find(key)?;
    let end = open + registry_json[open..].find('}')? + 1;
    let before = registry_json[..key_start].trim_end();
    let after = &registry_json[end..];
    let after_trimmed = after.trim_start();
    Some(if let Some(b) = before.strip_suffix(',') {
        // A sibling before: drop `, "utility": {..}`.
        format!("{b}{after}")
    } else if let Some(a) = after_trimmed.strip_prefix(',') {
        // A sibling after: drop `"utility": {..}, `.
        format!("{}{}", &registry_json[..key_start], a.trim_start())
    } else {
        // The only member: keep the closing whitespace, drop the member's line.
        format!("{before}{after}")
    })
}

/// The AI headroom `init` sizes the utility window from: doctor's figure,
/// or a test's pinned value.
fn ai_headroom_bytes() -> Option<u64> {
    #[cfg(test)]
    if let Some(v) = *AI_HEADROOM_OVERRIDE.lock().unwrap() {
        return v;
    }
    darkmux_doctor::ai_headroom_bytes()
}

#[cfg(test)]
static AI_HEADROOM_OVERRIDE: std::sync::Mutex<Option<Option<u64>>> = std::sync::Mutex::new(None);

/// Pin (`Some`) or release (`None`) the headroom `init` reads, for tests.
#[cfg(test)]
fn set_ai_headroom_override(v: Option<Option<u64>>) {
    *AI_HEADROOM_OVERRIDE.lock().unwrap() = v;
}

/// The worker models the registry names and the smallest window a managed
/// worker declares: (largest worker size LM Studio lists, floor window).
fn worker_facts(registry_json: &str, available: &[engine::ModelMeta]) -> (u64, u32) {
    let Ok(reg) = serde_json::from_str::<darkmux_types::ProfileRegistry>(registry_json) else {
        return (0, 1);
    };
    let workers: Vec<&darkmux_types::ProfileModel> =
        reg.profiles.values().flat_map(|p| p.models.iter()).filter(|m| m.is_managed()).collect();
    let floor = workers.iter().filter_map(|m| m.n_ctx).min().unwrap_or(1);
    let bytes = workers
        .iter()
        .filter_map(|w| {
            let key = darkmux_gestalt::bare_model_key(&w.id);
            available.iter().find(|m| m.model_type == "llm" && darkmux_gestalt::bare_model_key(&m.model_key) == key)
        })
        .map(|m| m.size_bytes)
        .max()
        .unwrap_or(0);
    (bytes, floor)
}

/// The shipped binding's id, and its window when that is the shipped one
/// too: `None` when there is no binding or the operator set its id by hand.
fn shipped_utility_binding(text: &str) -> Option<(String, Option<u32>)> {
    let (vs, ve) = utility_value_span(text)?;
    let current = &text[vs..ve];
    // Only the value the example registry ships is init's to change. An
    // operator who set a utility id by hand, downloaded or not, keeps it.
    let shipped = utility_value_span(EXAMPLE_PROFILES_JSON).map(|(a, b)| &EXAMPLE_PROFILES_JSON[a..b]);
    if shipped != Some(current) {
        return None;
    }
    // (#3020) Likewise the window: only the shipped one is init's to size.
    let window_of = |t: &str| utility_n_ctx_span(t).and_then(|(a, b)| t[a..b].parse::<u32>().ok());
    let shipped_window = window_of(EXAMPLE_PROFILES_JSON);
    Some((current.to_string(), window_of(text).filter(|w| Some(*w) == shipped_window)))
}

/// The registry text after applying `plan`; `None` when nothing is written.
fn apply_utility_plan(text: &str, current: &str, plan: &UtilityPlan) -> Option<String> {
    match plan {
        UtilityPlan::Keep | UtilityPlan::Missing(_) => None,
        UtilityPlan::Bind { id, n_ctx } => set_utility_binding(text, id.as_deref().unwrap_or(current), *n_ctx),
        UtilityPlan::Unbind(_) => remove_utility_binding(text),
    }
}

/// (#3207) Whether the registry text's utility binding names an unmanaged
/// endpoint. Parsed the way the loader parses it (#1282: a mistyped entry is
/// quarantined, the rest stands), so a broken profile elsewhere cannot switch
/// this guard off. A registry that does not parse at all is not (the fill
/// goes on as before).
fn utility_on_unmanaged_endpoint(text: &str) -> bool {
    darkmux_profiles::profiles::parse_registry_lenient(text).is_ok_and(|mut r| {
        r.materialize_endpoints();
        r.utility_is_unmanaged()
    })
}

fn fill_utility_model(registry_path: &std::path::Path) -> std::result::Result<Option<UtilityPlan>, String> {
    let text = fs::read_to_string(registry_path).map_err(|e| format!("reading {}: {e}", registry_path.display()))?;
    let Some((current, window)) = shipped_utility_binding(&text) else {
        return Ok(None);
    };
    // (#3207) A binding on an endpoint darkmux does not manage is served
    // there, not by LM Studio: its downloads and RAM headroom say nothing
    // about it, so init leaves it as written.
    if utility_on_unmanaged_endpoint(&text) {
        return Ok(None);
    }
    // An unreachable lms was already reported by the worker fill.
    let Ok(available) = engine::lmstudio().catalog() else {
        return Ok(None);
    };
    let (worker_bytes, floor) = worker_facts(&text, &available);
    let headroom = if window.is_some() { ai_headroom_bytes() } else { None };
    let plan = plan_utility_binding(&current, window, &available, headroom, worker_bytes, floor);
    if let Some(filled) = apply_utility_plan(&text, &current, &plan) {
        fs::write(registry_path, filled).map_err(|e| format!("writing {}: {e}", registry_path.display()))?;
    } else if !matches!(plan, UtilityPlan::Missing(_)) {
        return Ok(None);
    }
    Ok(Some(plan))
}

/// (#2450) The profile registry `init` bootstraps, routed through the SAME
/// root resolution `init`'s own `config.json` write already uses
/// (`resolve(ForceUser)`, which honors `DARKMUX_HOME`) rather than straight at
/// `dirs::home_dir()`.
///
/// Probed and confirmed broken before this fix, and it was the worst-shaped
/// member of the class because it SPLIT THE INSTALL IN HALF:
/// `HOME=$A DARKMUX_HOME=$B darkmux init` wrote `config.json` to
/// `$B/config.json` (correct — that write already used `resolve`) while
/// writing `profiles.json` to `$A/.darkmux/profiles.json`, the operator's REAL
/// home. One command, two roots, no warning.
///
/// `ForceUser` (not `Auto`) keeps this byte-identical to the old behavior
/// whenever `DARKMUX_HOME` is unset — `resolve(ForceUser).root` IS
/// `~/.darkmux` then — so no existing install's registry moves. It only
/// changes where a `DARKMUX_HOME`-scoped install looks, which is the bug.
/// `darkmux_profiles::default_locations` was taught the same tier in the same
/// change, so the reader and this writer cannot disagree.
fn user_profile_registry_path() -> Result<PathBuf> {
    use darkmux_types::paths::{ResolveScope, resolve};
    // `.profiles` is the field `paths.rs` documents as "the canonical registry
    // path" — reading it here rather than re-joining the literal is what makes
    // that claim true instead of aspirational.
    Ok(resolve(ResolveScope::ForceUser).profiles)
}

/// Bootstrap `<root>/config.json` (#661). Writes the **full self-documenting
/// default config** (`DarkmuxConfig::with_defaults`) with `machine_id`
/// personalized to this machine — every common knob visible + editable, so the
/// operator tunes the file rather than hunting hidden code-defaults. The
/// integration features (`redis`, `audit`) are written as `enabled: false`
/// blocks with their sub-defaults populated (the full surface is discoverable
/// and one flip from on). Skip-if-exists (never overwrites, even under
/// `--force`: the operator's config edits outrank a re-run). Returns
/// `(path, created)`; `created` is `true` in dry-run to report the intent.
///
/// The write target routes through `paths::resolve(ForceUser).config`, so it
/// honors the `DARKMUX_HOME` bootstrap pointer and lands where the runtime
/// reads.
fn bootstrap_config(dry_run: bool) -> Result<(PathBuf, bool)> {
    use darkmux_types::config::DarkmuxConfig;
    use darkmux_types::paths::{ResolveScope, resolve};

    let config_path = resolve(ResolveScope::ForceUser).config;
    if config_path.exists() {
        return Ok((config_path, false));
    }
    if dry_run {
        return Ok((config_path, true));
    }
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut cfg = DarkmuxConfig::with_defaults();
    if let Some(name) = computer_name() {
        cfg.machine_id = Some(name);
    }
    let resolved_paths = resolve(ResolveScope::ForceUser);
    let is_default_home = dirs::home_dir().map(|h| h.join(".darkmux")) == Some(resolved_paths.root.clone());
    if !is_default_home {
        if let Some(hooks) = cfg.hooks.as_mut() {
            hooks.outbox_dir = Some(resolved_paths.root.join("hooks").display().to_string());
        }
        if let Some(audit) = cfg.audit.as_mut() {
            audit.dir = Some(resolved_paths.root.join("audit").display().to_string());
        }
    }
    let mut json = serde_json::to_string_pretty(&cfg).context("serializing config.json")?;
    json.push('\n');
    fs::write(&config_path, json).with_context(|| format!("writing {}", config_path.display()))?;
    Ok((config_path, true))
}

/// The friendly machine name to seed `machine_id` with, so the operator gets a
/// visible, editable identity in their config rather than a silent hostname
/// fallback at every record write. macOS `scutil --get LocalHostName` (the
/// Bonjour name — friendly *and* identifier-safe, no spaces) → `hostname`
/// (with a trailing `.local` trimmed) → `None`. Operator-sovereignty: the
/// seed is a starting point they'll likely rename to `studio`/`laptop`.
fn computer_name() -> Option<String> {
    fn run(cmd: &str, args: &[&str]) -> Option<String> {
        let out = std::process::Command::new(cmd).args(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if name.is_empty() { None } else { Some(name) }
    }

    #[cfg(target_os = "macos")]
    if let Some(name) = run("scutil", &["--get", "LocalHostName"]) {
        return Some(name);
    }
    // Trim a trailing `.local`, then re-check for empty — a host named literally
    // `.local` trims to `""`, which must stay `None` (cleanly omitted), never a
    // `machine_id: ""` written into the config.
    run("hostname", &[]).and_then(|h| {
        let trimmed = h.trim_end_matches(".local");
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

const HOOK_MARKER: &str = "darkmux:session-start";
const CLAUDE_MD_HEADER: &str = "<!-- darkmux:integration:start -->";
const CLAUDE_MD_FOOTER: &str = "<!-- darkmux:integration:end -->";
const AGENTS_MD_HEADER: &str = "<!-- darkmux:integration:agents:start -->";
const AGENTS_MD_FOOTER: &str = "<!-- darkmux:integration:agents:end -->";

/// Shared body of the integration section that `--with-claude-md` /
/// `--with-agents-md` emit into users' project docs. One constant feeds both
/// templates so they cannot drift from each other or from the 2.0 identity
/// (#1449).
const INTEGRATION_SECTION_BODY: &str = r#"# darkmux

This project uses [darkmux](https://github.com/kstrat2001/darkmux), a mission orchestrator and lab for local AI. You dispatch roles and launch missions to a crew of local-AI seats; each seat runs on the model its profile names: one of your own local models, or a hosted endpoint when a role needs frontier weights. darkmux keeps the right models resident at the right context under your RAM budget — you don't manage residency by hand.

## Available skills

- `/darkmux-status` — what's currently loaded
- `/darkmux-list-stacks` — see all available profiles
- `/darkmux-list-workloads` / `/darkmux-lab-run` — execute lab workloads
- `/darkmux-list-runs` / `/darkmux-analyze-run` / `/darkmux-compare-runs` — inspect run history

## Dispatch policy

Launch a config-defined mission with `darkmux mission launch <config>` and watch it run as a live task graph, gated on your sign-off; each run finalizes into a typed envelope. For a single turn, `darkmux dispatch <role> "<text>"` sends work to one seat. Before relying on a config, measure it with `darkmux lab run <workload>` (wall clock, compaction events, verify outcome) so your choices rest on numbers, not guesses."#;

fn claude_settings_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))?;
    Ok(home.join(".claude").join("settings.json"))
}

/// Add a SessionStart hook that runs `darkmux machine status` so Claude sees
/// the current stack at the start of every session. Returns true if the hook
/// was newly added (false if already present). (#1426 — `status` folded into
/// the `machine` family.)
fn ensure_session_start_hook(settings_path: &Path, dry_run: bool, force: bool) -> Result<bool> {
    if !settings_path.exists() {
        if dry_run {
            return Ok(true);
        }
        if let Some(parent) = settings_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(settings_path, "{}\n")?;
    }

    let raw = fs::read_to_string(settings_path)
        .with_context(|| format!("reading {}", settings_path.display()))?;
    let mut value: Value = serde_json::from_str(&raw).unwrap_or_else(|_| Value::Object(Default::default()));

    let hooks = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} is not a JSON object", settings_path.display()))?
        .entry("hooks".to_string())
        .or_insert(Value::Object(Default::default()));
    let hooks_obj = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("{}: `hooks` is not an object", settings_path.display()))?;
    let arr = hooks_obj
        .entry("SessionStart".to_string())
        .or_insert(Value::Array(Vec::new()));
    let arr = arr
        .as_array_mut()
        .ok_or_else(|| anyhow!("{}: `hooks.SessionStart` is not an array", settings_path.display()))?;

    let already_present = arr.iter().any(|h| {
        h.get("command")
            .and_then(|c| c.as_str())
            .map(|c| c.contains(HOOK_MARKER))
            .unwrap_or(false)
    });
    if already_present && !force {
        return Ok(false);
    }
    if already_present && force {
        arr.retain(|h| {
            !h.get("command")
                .and_then(|c| c.as_str())
                .map(|c| c.contains(HOOK_MARKER))
                .unwrap_or(false)
        });
    }

    arr.push(serde_json::json!({
        "type": "command",
        "command": format!("# {HOOK_MARKER}\n/usr/bin/env -S sh -c 'darkmux machine status 2>/dev/null || true'")
    }));

    if !dry_run {
        let pretty = serde_json::to_string_pretty(&value)?;
        fs::write(settings_path, pretty + "\n")?;
    }
    Ok(true)
}

/// Append (or replace, with --force) a darkmux integration section into a
/// CLAUDE.md file. Idempotent via the marker comments — running twice
/// without --force is a no-op. (#1449) With --force, a stale block is
/// refreshed in place: the content between the existing start/end markers is
/// replaced with the freshly generated block, and everything OUTSIDE the
/// markers (the user's own prose) is preserved. Without this, an existing
/// user's doc never receives the 2.0-clean generator — there was no upgrade
/// path at all.
fn ensure_claude_md_section(target: &Path, dry_run: bool, force: bool) -> Result<bool> {
    let existing = if target.exists() {
        fs::read_to_string(target)?
    } else {
        String::new()
    };

    let has_block = existing.contains(CLAUDE_MD_HEADER);
    if has_block && !force {
        return Ok(false);
    }

    let section = darkmux_claude_md_section();
    let new_contents = if has_block {
        // --force refresh: replace the marked block in place.
        replace_marked_section(target, &existing, CLAUDE_MD_HEADER, CLAUDE_MD_FOOTER, &section)?
    } else if existing.is_empty() {
        section
    } else {
        format!("{}\n\n{}", existing.trim_end(), section)
    };

    if !dry_run {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, new_contents)?;
    }
    Ok(true)
}

/// Replace the marked darkmux block `[header ..= footer]` in `existing` with
/// `section` (a freshly generated `header\n\nbody\n\nfooter\n`), preserving
/// everything before the start marker and after the end marker — the user's own
/// prose. (#1449)
///
/// Marker handling is robust: it bails LOUDLY rather than corrupt the file when
/// the end marker is missing (a half-written block) or the markers are out of
/// order (footer before header). The caller only reaches this when the header
/// is present, so a missing footer means a genuinely malformed block the
/// operator should fix by hand, never one we silently overwrite.
fn replace_marked_section(
    doc: &Path,
    existing: &str,
    header: &str,
    footer: &str,
    section: &str,
) -> Result<String> {
    let start = existing
        .find(header)
        .ok_or_else(|| anyhow!("darkmux start marker `{header}` unexpectedly absent"))?;
    let end = existing.find(footer).ok_or_else(|| {
        anyhow!(
            "darkmux start marker `{header}` is present but the end marker `{footer}` is missing \
             in {}, so darkmux will not rewrite the block. Fix the markers by hand, or remove \
             the darkmux block entirely and re-run.",
            doc.display()
        )
    })?;
    if end < start {
        return Err(anyhow!(
            "darkmux markers are out of order in {} (`{footer}` appears before `{header}`), \
             so darkmux will not rewrite the block. Fix the markers by hand and re-run.",
            doc.display()
        ));
    }
    let end_idx = end + footer.len();
    let before = &existing[..start];
    let after = &existing[end_idx..];
    // The generated section carries a trailing newline; drop it when splicing so
    // we don't inject a blank line ahead of the user's following prose.
    let block = section.trim_end_matches('\n');
    let mut out = String::with_capacity(before.len() + block.len() + after.len() + 1);
    out.push_str(before);
    out.push_str(block);
    out.push_str(after);
    // Preserve a trailing newline when the block sat at end-of-file.
    if after.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

fn darkmux_claude_md_section() -> String {
    format!(
        "{header}\n\n{body}\n\n{footer}\n",
        header = CLAUDE_MD_HEADER,
        body = INTEGRATION_SECTION_BODY,
        footer = CLAUDE_MD_FOOTER,
    )
}

/// Append (or replace, with --force) a darkmux integration section into an
/// AGENTS.md file. Idempotent via the marker comments — running twice
/// without --force is a no-op. (#1449) With --force, a stale block is refreshed
/// in place (see `ensure_claude_md_section` — same marker-replace semantics, so
/// an existing user's AGENTS.md finally receives the 2.0-clean generator).
fn ensure_agents_md_section(target: &Path, dry_run: bool, force: bool) -> Result<bool> {
    let existing = if target.exists() {
        fs::read_to_string(target)?
    } else {
        String::new()
    };

    let has_block = existing.contains(AGENTS_MD_HEADER);
    if has_block && !force {
        return Ok(false);
    }

    let section = darkmux_agents_md_section();
    let new_contents = if has_block {
        replace_marked_section(target, &existing, AGENTS_MD_HEADER, AGENTS_MD_FOOTER, &section)?
    } else if existing.is_empty() {
        section
    } else {
        format!("{}\n\n{}", existing.trim_end(), section)
    };

    if !dry_run {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, new_contents)?;
    }
    Ok(true)
}

fn darkmux_agents_md_section() -> String {
    format!(
        "{header}\n\n{body}\n\n{footer}\n",
        header = AGENTS_MD_HEADER,
        body = INTEGRATION_SECTION_BODY,
        footer = AGENTS_MD_FOOTER,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // (#316 finding B) A server that does not list the shipped utility model
    // must not keep it bound: mlx_lm.server fetches an unknown model id from
    // Hugging Face on the first compaction or radio-routing call.
    #[test]
    fn a_server_that_does_not_list_the_shipped_utility_unbinds_it() {
        let mut report = InitReport::default();
        let models = vec!["mlx-community/Muse-Glimmer-30B-8bit".to_string()];
        let text = fill_server_utility(EXAMPLE_PROFILES_JSON.to_string(), "local-8080", &models, &mut report);
        assert!(utility_value_span(&text).is_none(), "the binding was kept:\n{text}");
        serde_json::from_str::<serde_json::Value>(&text).expect("still valid JSON");
        let reason = report.utility_model_unregistered_reason.expect("a reason");
        assert!(reason.contains("local-8080") && reason.contains("internal.utility"), "{reason}");
        assert_eq!(report.utility_model_unfilled_reason, None);
    }

    /// (#3287) The other three answers a server gives about the shipped
    /// utility: listed under a publisher-prefixed key, the binding moves to
    /// it; listed under its own key, nothing changes; and a binding on an
    /// unmanaged endpoint, or one the operator set, is never touched.
    #[test]
    fn a_server_utility_is_rebound_kept_or_left_alone() {
        let shipped = {
            let (a, b) = utility_value_span(EXAMPLE_PROFILES_JSON).unwrap();
            EXAMPLE_PROFILES_JSON[a..b].to_string()
        };
        let bare = shipped.rsplit('/').next().unwrap();
        let prefixed = format!("mlx-community/{bare}");

        let mut report = InitReport::default();
        let listed = std::slice::from_ref(&prefixed);
        let text = fill_server_utility(EXAMPLE_PROFILES_JSON.to_string(), "mlx", listed, &mut report);
        let (a, b) = utility_value_span(&text).expect("still bound");
        assert_eq!(&text[a..b], prefixed, "moved to the key the server lists");
        assert_eq!(report.utility_model_filled.as_deref(), Some(prefixed.as_str()));
        assert_eq!(report.utility_model_unregistered_reason, None);

        let mut report = InitReport::default();
        let listed = std::slice::from_ref(&shipped);
        let text = fill_server_utility(EXAMPLE_PROFILES_JSON.to_string(), "mlx", listed, &mut report);
        assert_eq!(text, EXAMPLE_PROFILES_JSON, "listed as bound: unchanged");
        assert_eq!((report.utility_model_filled, report.utility_model_unregistered_reason), (None, None));

        let mut v: serde_json::Value = serde_json::from_str(EXAMPLE_PROFILES_JSON).unwrap();
        v["endpoints"]["mlx-local"] = serde_json::json!({ "url": "http://127.0.0.1:8080/v1" });
        v["internal"]["utility"]["endpoint"] = serde_json::json!("mlx-local");
        let unmanaged = serde_json::to_string_pretty(&v).unwrap();
        let hand_set = EXAMPLE_PROFILES_JSON.replace(&shipped, "operator/own-utility");
        for text in [unmanaged, hand_set] {
            let mut report = InitReport::default();
            let out = fill_server_utility(text.clone(), "mlx", &["other/model".to_string()], &mut report);
            assert_eq!(out, text, "left alone");
            assert_eq!((report.utility_model_filled, report.utility_model_unregistered_reason), (None, None));
        }
    }

    /// (#3287) `record_engine`'s undo: the registry is rewritten as it was,
    /// or removed when there was none, and a failed undo says so.
    #[test]
    fn put_back_restores_or_removes_the_registry_and_says_whether_it_could() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        fs::write(&path, "changed").unwrap();
        assert_eq!(put_back(&path, Some("before")), "profiles.json was put back");
        assert_eq!(fs::read_to_string(&path).unwrap(), "before");
        assert_eq!(put_back(&path, None), "profiles.json was put back");
        assert!(!path.exists(), "no registry before, none after");
        assert_eq!(put_back(&path, None), "profiles.json could NOT be put back");
        let unwritable = dir.path().join("no-such-dir").join("profiles.json");
        assert_eq!(put_back(&unwritable, Some("before")), "profiles.json could NOT be put back");
    }

    #[test]
    fn example_config_matches_with_defaults() {
        // The committed `config.example.json` is the single source of truth's
        // output: it must equal `to_string_pretty(with_defaults())` + a trailing
        // newline (exactly what `init` writes, modulo the personalized
        // machine_id). This is the drift guard — change a default in code and
        // this test fails until the example is regenerated.
        use darkmux_types::config::DarkmuxConfig;
        let expected = format!(
            "{}\n",
            serde_json::to_string_pretty(&DarkmuxConfig::with_defaults()).unwrap()
        );
        assert_eq!(
            EXAMPLE_CONFIG, expected,
            "config.example.json drifted from DarkmuxConfig::with_defaults() — regenerate it"
        );
        // And the integration features are present as `enabled: false` blocks
        // (visible surface, off), not absent.
        let cfg: DarkmuxConfig = serde_json::from_str(EXAMPLE_CONFIG).unwrap();
        assert_eq!(cfg.redis.as_ref().and_then(|r| r.enabled), Some(false));
        assert_eq!(cfg.audit.as_ref().and_then(|a| a.enabled), Some(false));
        // (#2093) The hooks block ships visible + off too, outbox_dir populated.
        assert_eq!(cfg.hooks.as_ref().and_then(|h| h.enabled), Some(false));
        assert_eq!(cfg.hooks.as_ref().and_then(|h| h.outbox_dir.as_deref()), Some("~/.darkmux/hooks"));
        assert_eq!(cfg.hooks.as_ref().and_then(|h| h.rules.as_ref().map(|r| r.is_empty())), Some(true));
        assert!(cfg.extras.is_empty(), "example must use only documented keys");
    }

    #[serial_test::serial]
    #[test]
    fn bootstrap_config_writes_full_personalized_and_skips_if_exists() {
        use darkmux_types::config::{CONFIG_SCHEMA_VERSION, DarkmuxConfig};
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()); }

        // First run writes the full self-documenting default config.
        let (path, created) = bootstrap_config(false).unwrap();
        assert!(created, "fresh config is created");
        assert_eq!(path, tmp.path().join("config.json"));
        let cfg: DarkmuxConfig = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.schema_version.as_deref(), Some(CONFIG_SCHEMA_VERSION));
        assert!(cfg.machine_id.is_some(), "machine_id seeded from the computer name");
        // Full, visible surface: the feature blocks are present + off, and the
        // scalar defaults are written (not left to invisible code-defaults).
        assert_eq!(cfg.redis.as_ref().and_then(|r| r.enabled), Some(false));
        assert_eq!(cfg.redis.as_ref().and_then(|r| r.maxlen), Some(10_000));
        assert_eq!(cfg.redis.as_ref().and_then(|r| r.telemetry_maxlen), Some(10_000));
        assert_eq!(cfg.audit.as_ref().and_then(|a| a.enabled), Some(false));
        assert_eq!(
            cfg.audit.as_ref().and_then(|a| a.dir.as_deref()),
            Some(tmp.path().join("audit").to_str().unwrap()),
            "audit dir must resolve under DARKMUX_HOME, not literal ~/.darkmux"
        );
        assert_eq!(
            cfg.hooks.as_ref().and_then(|h| h.outbox_dir.as_deref()),
            Some(tmp.path().join("hooks").to_str().unwrap()),
            "hooks outbox dir must resolve under DARKMUX_HOME, not literal ~/.darkmux"
        );
        assert_eq!(
            cfg.runtime.as_ref().and_then(|r| r.inactivity_timeout_seconds),
            Some(600)
        );
        // (#1276) The bounded model-load phase ships visible, same pattern.
        assert_eq!(
            cfg.runtime.as_ref().and_then(|r| r.model_load_timeout_seconds),
            Some(600)
        );
        // (#2678) The run-level wall-clock bound ships visible too, at its
        // unbounded (`0`) default — an existing mission's behavior must not
        // change until an operator opts in.
        assert_eq!(
            cfg.runtime.as_ref().and_then(|r| r.mission_wall_clock_timeout_seconds),
            Some(0)
        );
        // (#2107, #1833) The daemon host-sampler cadence ships visible too.
        assert_eq!(
            cfg.runtime.as_ref().and_then(|r| r.host_sampler_interval_ms),
            Some(5000)
        );
        // (#2093) The hooks feature block ships visible + off, same pattern.
        assert_eq!(cfg.hooks.as_ref().and_then(|h| h.enabled), Some(false));
        // The written config personalizes machine_id away from the placeholder.
        assert_ne!(cfg.machine_id.as_deref(), Some("my-machine"));
        // Derived/advanced fields stay absent (dirs derived; caps = uncapped).
        assert!(cfg.dirs.is_none(), "derived dirs are surfaced by doctor, not frozen");

        // Second run never overwrites — returns (same path, false).
        let (path2, created2) = bootstrap_config(false).unwrap();
        assert_eq!(path2, path);
        assert!(!created2, "existing config is left untouched");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    #[serial_test::serial]
    #[test]
    fn bootstrap_config_dry_run_does_not_write() {
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var("DARKMUX_HOME").ok();
        unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()); }

        let (path, created) = bootstrap_config(true).unwrap();
        assert!(created, "dry-run reports the intent to create");
        assert!(!path.exists(), "dry-run writes nothing");

        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    #[test]
    fn claude_md_section_includes_marker_comments() {
        let s = darkmux_claude_md_section();
        assert!(s.contains(CLAUDE_MD_HEADER));
        assert!(s.contains(CLAUDE_MD_FOOTER));
        assert!(s.contains(INTEGRATION_SECTION_BODY));
    }

    #[test]
    fn agents_md_section_includes_marker_comments() {
        let s = darkmux_agents_md_section();
        assert!(s.contains(AGENTS_MD_HEADER));
        assert!(s.contains(AGENTS_MD_FOOTER));
        assert!(s.contains(INTEGRATION_SECTION_BODY));
    }

    #[test]
    fn integration_section_body_teaches_current_identity() {
        // Both templates share this body, so one guard covers both.
        let s = INTEGRATION_SECTION_BODY;
        assert!(s.contains("/darkmux-status"));
        assert!(s.contains("mission orchestrator and lab"));
        assert!(s.contains("mission launch"));
        assert!(s.contains("lab run"));
        // The retired multiplexer/swap identity must not resurface (#1449).
        assert!(!s.contains("multiplex"));
        assert!(!s.contains("swap"));
    }

    #[test]
    fn ensure_claude_md_appends_to_existing() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("CLAUDE.md");
        fs::write(&p, "# Project\n\nExisting content here.\n").unwrap();
        let appended = ensure_claude_md_section(&p, false, false).unwrap();
        assert!(appended);
        let after = fs::read_to_string(&p).unwrap();
        assert!(after.contains("Existing content here"));
        assert!(after.contains(CLAUDE_MD_HEADER));
        assert!(after.contains("# darkmux"));
    }

    #[test]
    fn ensure_claude_md_creates_when_missing() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("CLAUDE.md");
        let appended = ensure_claude_md_section(&p, false, false).unwrap();
        assert!(appended);
        assert!(p.exists());
    }

    #[test]
    fn ensure_claude_md_idempotent() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("CLAUDE.md");
        ensure_claude_md_section(&p, false, false).unwrap();
        let second = ensure_claude_md_section(&p, false, false).unwrap();
        assert!(!second);
        // Verify the section appears exactly once.
        let after = fs::read_to_string(&p).unwrap();
        assert_eq!(after.matches(CLAUDE_MD_HEADER).count(), 1);
    }

    #[test]
    fn ensure_agents_md_idempotent() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("AGENTS.md");
        ensure_agents_md_section(&p, false, false).unwrap();
        let second = ensure_agents_md_section(&p, false, false).unwrap();
        assert!(!second);
        let after = fs::read_to_string(&p).unwrap();
        assert_eq!(after.matches(AGENTS_MD_HEADER).count(), 1);
    }

    #[test]
    fn ensure_claude_md_force_refreshes_block_in_place() {
        // (#1449) --force replaces a stale block between the markers while
        // preserving the user's prose above and below — the upgrade path.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("CLAUDE.md");
        let stale = format!(
            // drift-guard:allow darkmux swap — deliberate STALE fixture; the test proves --force strips this retired verb (#1469)
            "# My Project\n\nMy own notes above.\n\n{CLAUDE_MD_HEADER}\n\n# darkmux\n\nSTALE: multiplex local LLM stacks with `darkmux swap`.\n\n{CLAUDE_MD_FOOTER}\n\nMy own notes below.\n"
        );
        fs::write(&p, &stale).unwrap();

        // Without --force it's a no-op (the stale block survives).
        let noop = ensure_claude_md_section(&p, false, false).unwrap();
        assert!(!noop);
        assert!(fs::read_to_string(&p).unwrap().contains("STALE"));

        // With --force the block refreshes to the clean generator output.
        let changed = ensure_claude_md_section(&p, false, true).unwrap();
        assert!(changed);
        let after = fs::read_to_string(&p).unwrap();
        assert!(!after.contains("STALE"), "stale block must be gone: {after}");
        assert!(!after.contains("multiplex"), "retired identity gone: {after}");
        assert!(!after.contains("darkmux swap"), "retired verb gone: {after}"); // drift-guard:allow darkmux swap — test asserts the retired verb is stripped (#1469)
        assert!(after.contains("mission orchestrator and lab"));
        // User prose on both sides is preserved.
        assert!(after.contains("My own notes above."));
        assert!(after.contains("My own notes below."));
        // Exactly one block.
        assert_eq!(after.matches(CLAUDE_MD_HEADER).count(), 1);
        assert_eq!(after.matches(CLAUDE_MD_FOOTER).count(), 1);
    }

    #[test]
    fn ensure_claude_md_force_bails_on_missing_end_marker() {
        // (#1449) A half-written block (start present, end missing) must NOT be
        // silently rewritten — bail loudly and leave the file untouched.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("CLAUDE.md");
        let malformed = format!("# Project\n\n{CLAUDE_MD_HEADER}\n\n# darkmux\n\nno end marker here.\n");
        fs::write(&p, &malformed).unwrap();

        let err = ensure_claude_md_section(&p, false, true).unwrap_err();
        assert!(
            err.to_string().contains("end marker"),
            "error names the missing end marker: {err}"
        );
        // File is untouched.
        assert_eq!(fs::read_to_string(&p).unwrap(), malformed);
    }

    #[test]
    fn ensure_claude_md_force_bails_on_out_of_order_markers() {
        // (#1449) footer-before-header is malformed — bail, don't corrupt.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("CLAUDE.md");
        let malformed = format!("{CLAUDE_MD_FOOTER}\n\nbody\n\n{CLAUDE_MD_HEADER}\n");
        fs::write(&p, &malformed).unwrap();

        let err = ensure_claude_md_section(&p, false, true).unwrap_err();
        assert!(
            err.to_string().contains("out of order"),
            "error names the ordering problem: {err}"
        );
        assert_eq!(fs::read_to_string(&p).unwrap(), malformed);
    }

    #[test]
    fn ensure_claude_md_dry_run_does_not_write() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("CLAUDE.md");
        let appended = ensure_claude_md_section(&p, true, false).unwrap();
        assert!(appended);
        assert!(!p.exists());
    }

    #[test]
    fn ensure_session_start_hook_creates_settings() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(".claude/settings.json");
        let added = ensure_session_start_hook(&p, false, false).unwrap();
        assert!(added);
        let raw = fs::read_to_string(&p).unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        let arr = parsed["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert!(arr[0]["command"].as_str().unwrap().contains(HOOK_MARKER));
    }

    #[test]
    fn ensure_session_start_hook_idempotent() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(".claude/settings.json");
        ensure_session_start_hook(&p, false, false).unwrap();
        let second = ensure_session_start_hook(&p, false, false).unwrap();
        assert!(!second);
        let raw = fs::read_to_string(&p).unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        let arr = parsed["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
    }

    #[test]
    fn ensure_session_start_hook_force_replaces() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(".claude/settings.json");
        ensure_session_start_hook(&p, false, false).unwrap();
        let second = ensure_session_start_hook(&p, false, true).unwrap();
        assert!(second);
        let arr = serde_json::from_str::<Value>(&fs::read_to_string(&p).unwrap()).unwrap()
            ["hooks"]["SessionStart"]
            .as_array()
            .cloned()
            .unwrap();
        assert_eq!(arr.len(), 1); // not duplicated
    }

    /// The embedded example registry parses as valid JSON. Caught at build time
    /// via `include_str!`, but verifying serde-shaped is a cheap belt-and-braces.
    /// (#2055) The shipped example is the FIRST darkmux artifact a new user
    /// reads — `init` writes it verbatim as their `profiles.json`. It must not
    /// name a subsystem darkmux no longer has: the pre-2.0 openclaw shell-out
    /// (removed in #1405) survived here for four releases as a `Requires
    /// OpenClaw >= ...` note on a profile description, so a fresh install's
    /// own registry told the operator to go install a dependency that does not
    /// exist. Text-level and case-insensitive on purpose — the failure mode is
    /// prose in a `description`, not a structural field any parser would catch.
    #[test]
    fn the_shipped_example_names_no_retired_subsystem() {
        let lowered = EXAMPLE_PROFILES_JSON.to_ascii_lowercase();
        // drift-guard:allow crew sync — this test ASSERTS the retirement; the
        // drift-guard:allow darkmux swap — names here are the needles, not prose.
        for retired in ["openclaw", "crew sync", "darkmux swap"] {
            assert!(
                !lowered.contains(retired),
                "the shipped example registry names the retired `{retired}` — \
                 a fresh `darkmux init` would hand the operator a file \
                 referencing a subsystem this binary does not have"
            );
        }
    }

    #[test]
    fn embedded_example_profiles_parses_as_json() {
        let parsed: serde_json::Value =
            serde_json::from_str(EXAMPLE_PROFILES_JSON).expect("profiles.example.json must parse");
        assert!(parsed.get("profiles").is_some(), "missing 'profiles' field");
    }

    /// `darkmux init` writes `EXAMPLE_PROFILES_JSON` VERBATIM to
    /// `~/.darkmux/profiles.json` — the next `darkmux` invocation that reads
    /// the registry runs it through `load_registry`'s validation pass
    /// (profile-level only). A fresh `darkmux init` must never hand the
    /// operator a registry that fails that pass. Since #1426 ship-2 the example
    /// carries NO `crews` map (review staffing is derived by the resourcing
    /// resolver from the active profile) — this asserts that too, so the
    /// example doesn't reintroduce the retired declaration surface.
    ///
    /// Also drift-guards the example's stamped `schema_version` against
    /// `PROFILES_SCHEMA_VERSION` (the same committed-reference-vs-code
    /// discipline as `example_config_matches_with_defaults`) — a schema bump
    /// that forgets to restamp the example fails here, not on an operator's
    /// machine.
    #[test]
    fn embedded_example_profiles_passes_full_registry_validation() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("profiles.json");
        fs::write(&p, EXAMPLE_PROFILES_JSON).unwrap();
        let loaded = darkmux_profiles::profiles::load_registry(Some(p.to_str().unwrap()))
            .expect("embedded example registry must pass load_registry's validation");
        // (#1426 ship-2) The example ships no declared crews map — staffing is
        // derived from the profile roster, never a `crews` entry.
        assert!(
            !loaded.registry.extras.contains_key("crews"),
            "the shipped example must not reintroduce the retired `crews` map"
        );
        assert_eq!(
            loaded.registry.schema_version.as_deref(),
            Some(darkmux_types::PROFILES_SCHEMA_VERSION),
            "profiles.example.json schema_version must match PROFILES_SCHEMA_VERSION"
        );
        // (#2902 step 4) The shipped example is clean under the ONE registry
        // validation, advice included: its hosted endpoint is declared once
        // under `endpoints` and named by id, the spelling doctor asks for.
        let issues = loaded.registry.validate();
        assert!(issues.is_empty(), "the example registry must validate clean: {issues:?}");
        let hosted = &loaded.registry.profiles["hosted-frontier"].models[0];
        assert_eq!(hosted.endpoint.as_ref().and_then(|e| e.named_id()), Some("azure-openai"));
        assert_eq!(hosted.endpoint_kind().unwrap(), darkmux_types::EndpointKind::Unmanaged);
    }

    #[test]
    fn ensure_session_start_hook_preserves_existing_settings() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join(".claude/settings.json");
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(
            &p,
            r#"{"theme":"dark","hooks":{"SessionStart":[{"type":"command","command":"echo prior"}]}}"#,
        )
        .unwrap();
        ensure_session_start_hook(&p, false, false).unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(parsed["theme"], "dark");
        let arr = parsed["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(arr.len(), 2); // existing preserved + ours appended
    }

    // ── #2038: init fills the placeholder model from what LM Studio has ──

    fn meta(key: &str, size_gb: u64, kind: &str) -> engine::ModelMeta {
        engine::ModelMeta {
            model_key: key.to_string(),
            display_name: key.to_string(),
            publisher: String::new(),
            size_bytes: size_gb * 1_000_000_000,
            params_string: None,
            architecture: None,
            max_context_length: Some(32_768),
            trained_for_tool_use: true,
            model_type: kind.to_string(),
        }
    }

    #[test]
    fn fill_replaces_every_placeholder_and_nothing_else() {
        let reg = r#"{"default_profile":"balanced","profiles":{"fast":{"models":[{"id":"<your-worker-model-id>","n_ctx":32000}]},"gpt":{"models":[{"id":"mlx-community/gpt-oss-120b","n_ctx":100000}]}}}"#;
        let out = fill_placeholder_models(reg, "qwen/qwen3.6-35b-a3b").expect("placeholder present");
        assert!(!out.contains(PLACEHOLDER_MODEL_ID), "{out}");
        assert!(out.contains(r#""id":"qwen/qwen3.6-35b-a3b","n_ctx":32000"#), "{out}");
        assert!(out.contains("mlx-community/gpt-oss-120b"), "a real id is never touched: {out}");
        assert_eq!(fill_placeholder_models(&out, "other"), None, "a registry without the placeholder is left alone");
    }

    #[test]
    fn choose_prefers_a_loaded_model_then_the_largest_llm_that_fits() {
        let avail = vec![meta("small-4b", 3, "llm"), meta("big-70b", 45, "llm"), meta("huge-120b", 70, "llm"), meta("embed", 1, "embedding")];
        // A loaded model wins outright.
        assert_eq!(choose_worker_model(&["small-4b".to_string()], &avail, 128).as_deref(), Some("small-4b"));
        // Nothing loaded: the largest LLM that fits the RAM budget.
        assert_eq!(choose_worker_model(&[], &avail, 128).as_deref(), Some("huge-120b"));
        assert_eq!(choose_worker_model(&[], &avail, 80).as_deref(), Some("big-70b"));
        assert_eq!(choose_worker_model(&[], &avail, 16).as_deref(), Some("small-4b"));
        // Embeddings are never a worker; nothing downloaded is None.
        assert_eq!(choose_worker_model(&[], &[meta("embed", 1, "embedding")], 128), None);
        assert_eq!(choose_worker_model(&[], &[], 128), None);
        // A loaded id that is not in the catalog (a namespaced identifier) still counts.
        assert_eq!(choose_worker_model(&["darkmux:big-70b".to_string()], &avail, 128).as_deref(), Some("big-70b"));
    }

    #[test]
    fn worker_model_budget_leaves_headroom() {
        // The budget is a fraction of RAM, never all of it: the OS, the KV
        // cache, and a utility model need room too.
        assert!(worker_model_budget_bytes(32) < 32 * 1_000_000_000);
        assert!(worker_model_budget_bytes(32) >= 16 * 1_000_000_000);
    }

    // ── #2053: init verifies the utility binding against LM Studio ────────

    #[test]
    fn utility_model_keeps_a_present_key_and_otherwise_picks_the_smallest_real_llm() {
        let avail = vec![meta("qwen3-4b-instruct-2507", 2, "llm"), meta("big-70b", 45, "llm"), meta("tiny-0.5b", 0, "llm"), meta("embed", 1, "embedding")];
        // Present as written: keep it, even if a smaller one exists.
        assert_eq!(choose_utility_model("big-70b", &avail).as_deref(), Some("big-70b"));
        // A publisher-prefixed id that LM Studio knows by its bare key: the bare key wins.
        assert_eq!(choose_utility_model("qwen/qwen3-4b-instruct-2507", &avail).as_deref(), Some("qwen3-4b-instruct-2507"));
        // Absent entirely: the smallest LLM that is at least a real model (>= 1 GB), never an embedding.
        assert_eq!(choose_utility_model("nobody/4b", &avail).as_deref(), Some("qwen3-4b-instruct-2507"));
        // Nothing usable downloaded.
        assert_eq!(choose_utility_model("nobody/4b", &[meta("embed", 1, "embedding")]), None);
    }

    /// The removed bare-string binding is not one init recognizes.
    #[test]
    fn set_utility_binding_ignores_a_bare_string() {
        let reg = r#"{"internal":{"utility":"qwen/qwen3-4b-instruct-2507"},"profiles":{}}"#;
        assert_eq!(set_utility_binding(reg, "x", None), None);
    }

    /// (#2914) The object form `"utility": { "id": .., "n_ctx": .. }` — the
    /// shape the example now ships — rewrites only the `id`, keeping the
    /// window and the operator's file shape. A profile model with the same
    /// id (a leftover from before #2914) is still not the binding.
    #[test]
    fn set_utility_binding_rewrites_the_id_inside_the_object_form() {
        let reg = r#"{"internal":{"utility":{"id":"qwen/qwen3-4b-instruct-2507","n_ctx":120000}},"profiles":{"fast":{"models":[{"id":"qwen/qwen3-4b-instruct-2507","n_ctx":32000}]}}}"#;
        let out = set_utility_binding(reg, "qwen3-4b-instruct-2507", None).expect("binding present");
        assert!(out.contains(r#""utility":{"id":"qwen3-4b-instruct-2507","n_ctx":120000}"#), "{out}");
        assert!(out.contains(r#""models":[{"id":"qwen/qwen3-4b-instruct-2507""#), "the profile model keeps its id: {out}");
        // An object with no `id` string is not a binding the scanner can fill.
        assert_eq!(utility_value_span(r#"{"internal":{"utility":{"n_ctx":1}}}"#), None);
    }

    /// The scanner reads the shipped example's own binding, so the gate that
    /// protects an operator's hand-set value compares against the real
    /// shipped string, not a copy that could drift.
    #[test]
    fn the_shipped_utility_binding_is_readable_from_the_example() {
        let (a, b) = utility_value_span(EXAMPLE_PROFILES_JSON).expect("example registry declares internal.utility");
        assert_eq!(&EXAMPLE_PROFILES_JSON[a..b], "qwen/qwen3-4b-instruct-2507");
        // (#2914) The example ships the object form, window included.
        let reg: darkmux_types::ProfileRegistry = serde_json::from_str(EXAMPLE_PROFILES_JSON).unwrap();
        assert_eq!(reg.utility_model_n_ctx(), Some(120_000), "the shipped binding declares its window");
    }

    // ── #3020: the utility window is sized from the machine's AI RAM ──────

    const GIB: u64 = 1024 * 1024 * 1024;

    /// A 2 GB utility candidate whose own maximum window is above the shipped one.
    fn util_meta() -> engine::ModelMeta {
        engine::ModelMeta { max_context_length: Some(262_144), ..meta("qwen3-4b-instruct-2507", 2, "llm") }
    }

    #[test]
    fn utility_window_splits_what_is_left_after_both_models_between_their_kv_caches() {
        let rate = darkmux_profiles::model_ledger::fallback_kv_rate_for_size(2 * GIB);
        // 2 GiB worker + 2 GiB utility, then 2 * 50_000 tokens of KV: 50_000 each.
        let headroom = 4 * GIB + 2 * 50_000 * rate;
        assert_eq!(utility_window(headroom, 2 * GIB, 2 * GIB, 120_000), 50_000);
        // Never above the cap (the shipped window, or the model's own maximum).
        assert_eq!(utility_window(1024 * GIB, 2 * GIB, 2 * GIB, 120_000), 120_000);
        // Less headroom than the two models' weights: no window at all.
        assert_eq!(utility_window(3 * GIB, 2 * GIB, 2 * GIB, 120_000), 0);
        assert_eq!(utility_window(0, 0, 2 * GIB, 120_000), 0);
    }

    #[test]
    fn plan_unbinds_on_a_machine_with_no_ai_headroom_and_names_the_fleet_route() {
        let avail = vec![util_meta()];
        let plan = plan_utility_binding("qwen/qwen3-4b-instruct-2507", Some(120_000), &avail, Some(0), 0, 32_000);
        let UtilityPlan::Unbind(reason) = plan else { panic!("expected Unbind, got {plan:?}") };
        assert!(reason.contains("0 GB available for AI"), "{reason}");
        assert!(reason.contains("profile@machine"), "{reason}");
        // No usable LLM downloaded is an unbind too, not a stale id left behind.
        let plan = plan_utility_binding("qwen/qwen3-4b-instruct-2507", Some(120_000), &[meta("embed", 1, "embedding")], Some(500 * GIB), 0, 32_000);
        assert!(matches!(plan, UtilityPlan::Unbind(ref r) if r.contains("no LLM of at least 1 GB")), "{plan:?}");
        // ...unless the window is hand-set: then the binding is the operator's
        // and stays, and init says its model is not downloaded.
        let plan = plan_utility_binding("qwen/qwen3-4b-instruct-2507", None, &[meta("embed", 1, "embedding")], Some(500 * GIB), 0, 32_000);
        assert!(matches!(plan, UtilityPlan::Missing(ref r) if r.contains("is not downloaded") && r.contains("kept")), "{plan:?}");
    }

    #[test]
    fn plan_shrinks_the_shipped_window_to_what_fits_and_keeps_a_hand_set_one() {
        let avail = vec![util_meta()];
        let size = 2 * 1_000_000_000;
        let rate = darkmux_profiles::model_ledger::fallback_kv_rate_for_size(size);
        let headroom = size + 2 * 60_000 * rate;
        assert_eq!(
            plan_utility_binding("qwen3-4b-instruct-2507", Some(120_000), &avail, Some(headroom), 0, 32_000),
            UtilityPlan::Bind { id: None, n_ctx: Some(60_000) }
        );
        // The model's own maximum caps the window too.
        let capped = vec![meta("qwen3-4b-instruct-2507", 2, "llm")];
        assert_eq!(
            plan_utility_binding("qwen3-4b-instruct-2507", Some(120_000), &capped, Some(1024 * GIB), 0, 32_000),
            UtilityPlan::Bind { id: None, n_ctx: Some(32_768) }
        );
        // Below the floor: unbound, naming the window it could have had.
        let tight = size + 2 * 10_000 * rate;
        let plan = plan_utility_binding("qwen3-4b-instruct-2507", Some(120_000), &avail, Some(tight), 0, 32_000);
        assert!(matches!(plan, UtilityPlan::Unbind(ref r) if r.contains("10000")), "{plan:?}");
        // Ample headroom and the id present: nothing to change.
        assert_eq!(plan_utility_binding("qwen3-4b-instruct-2507", Some(120_000), &avail, Some(1024 * GIB), 0, 32_000), UtilityPlan::Keep);
        // A window that is not init's to size (None) is never resized or unbound.
        assert_eq!(plan_utility_binding("qwen3-4b-instruct-2507", None, &avail, Some(0), 0, 32_000), UtilityPlan::Keep);
        // Headroom that cannot be read leaves the window as shipped.
        assert_eq!(plan_utility_binding("qwen3-4b-instruct-2507", Some(120_000), &avail, None, 0, 32_000), UtilityPlan::Keep);
        // A publisher-prefixed id is still renamed when the window stays.
        assert_eq!(
            plan_utility_binding("qwen/qwen3-4b-instruct-2507", Some(120_000), &avail, Some(1024 * GIB), 0, 32_000),
            UtilityPlan::Bind { id: Some("qwen3-4b-instruct-2507".into()), n_ctx: None }
        );
    }

    #[test]
    fn set_and_remove_rewrite_only_the_utility_binding() {
        let reg = "{\n  \"profiles\": {\"fast\": {\"models\": [{\"id\": \"q\", \"n_ctx\": 120000}]}},\n  \"internal\": {\n    \"utility\": { \"id\": \"q\", \"n_ctx\": 120000 }\n  }\n}";
        let out = set_utility_binding(reg, "q2", Some(48_000)).expect("binding present");
        assert!(out.contains(r#""utility": { "id": "q2", "n_ctx": 48000 }"#), "{out}");
        assert!(out.contains(r#"[{"id": "q", "n_ctx": 120000}]"#), "the profile model is untouched: {out}");
        let out = remove_utility_binding(reg).expect("binding present");
        let v: Value = serde_json::from_str(&out).expect("still valid JSON");
        assert_eq!(v["internal"], serde_json::json!({}), "{out}");
        assert_eq!(v["profiles"]["fast"]["models"][0]["n_ctx"], 120_000);
        // A sibling member keeps the object valid whichever side it is on.
        for reg in [r#"{"internal":{"x":1,"utility":{"id":"q","n_ctx":1}}}"#, r#"{"internal":{"utility":{"id":"q","n_ctx":1}, "x":1}}"#] {
            let v: Value = serde_json::from_str(&remove_utility_binding(reg).unwrap()).unwrap();
            assert_eq!(v["internal"], serde_json::json!({"x": 1}), "{reg}");
        }
        assert_eq!(remove_utility_binding(r#"{"profiles":{}}"#), None);
    }
}


#[cfg(test)]
#[path = "init_tests.rs"]
mod init_tests;
