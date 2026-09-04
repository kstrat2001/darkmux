//! `darkmux mission config list`/`show` (#1860) — a READ-ONLY projection of
//! the mission-CONFIG registry: "which configs are registered, and what
//! would this one do if I launched it." `role list`/`role show` already do
//! this for the role registry; nothing did it for `templates/builtin/
//! mission-configs/` until this module.
//!
//! **No new data, no new resolution logic, no mutation.** Every fact this
//! module renders is read straight from machinery that already exists and
//! is already trusted:
//!
//! - `mission_config::list_ids()` / `mission_config::load()` — the same
//!   user → on-disk → embedded search `mission launch` resolves through.
//! - `mission_launch::all_step_kinds()` — the SAME `StepKindRegistry`
//!   `mission launch` builds its execution registry from, so a step's
//!   "constructible" verdict here is the identical check `launch` exits `4`
//!   against, not a re-derived approximation.
//! - `darkmux_profiles::profiles::resolve_role_profile_with` — the SAME
//!   role → profile → model resolution `mission_launch_review` performs for
//!   every review role, given the SAME `RoleBinding` precedence (`--param`
//!   override > `role_profiles` map > `default_profile` fallback). And the
//!   same local-model gate every dispatch path applies before trusting a
//!   `ProfileModel`: `ProfileModel::require_n_ctx` (see
//!   `resourcing.rs::resolve_task_role`, `dispatch_internal.rs`,
//!   `darkmux-lab`'s `review.rs`) — a local model with no declared `n_ctx`
//!   is refused here exactly as `mission launch` would refuse it, never
//!   rendered as a healthy `not loaded`.
//! - `darkmux_gestalt::decide_residency` — the CANONICAL residency arbiter
//!   (ownership partition + ctx-sufficiency, #1274) every real acquire path
//!   plans against. **Not** the same rule `machine status` uses — that verb
//!   only partitions loaded models by namespace ownership (`is_darkmux_owned`)
//!   for a display grouping; it never asks whether a SPECIFIC placement's
//!   ctx is satisfied. Three different rules exist in this codebase
//!   (`machine status`'s ownership-only partition, this module's
//!   `decide_residency` call, and — historically, before this fix —
//!   a re-derived approximation here that diverged from both); this module
//!   now defers to `decide_residency` rather than adding a fourth.
//!
//! **`--param ROLE=PROFILE` applies only where `mission launch` would apply
//! it.** Role→profile overrides are converted into launch bindings ONLY on
//! the review route (`mission_launch_review.rs`, gated structurally by
//! `mission_launch::config_uses_review_kinds`) — for any other config (e.g.
//! `coder-phase`), `mission launch` ignores `--param <role>=<profile>`
//! entirely (a coder-phase `--param role=<id>` instead REBINDS the task's
//! `role_id`, a different knob `show` can't express). `show` mirrors this
//! exactly: on a non-review-route config, a supplied `--param` is neutered
//! (never stamped as `launch override (--param)` provenance) and surfaces a
//! warning instead of silently claiming a parity that doesn't exist.
//!
//! **Structure: pure builders + thin printers.** [`build_list`] and
//! [`build_show`] take already-loaded data and return plain, `Serialize`
//! data structs with no I/O of their own — every fact a test needs to
//! assert on is constructible in memory, no live LMStudio or profiles
//! registry required. [`render_list_text`]/[`render_show_text`] and the
//! `serde_json::to_*` calls in [`run`] both render off those SAME structs,
//! so `--json` and the human view can never drift apart on WHAT they show,
//! only on HOW.
//!
//! **Degrade, never fail (operator sovereignty, #44).** A config that fails
//! to load is one row naming the error, not a missing row — the same rule
//! `acp_panel::list_panel_commands` follows for the panel-command listing.
//! A role whose mapping is dangling, or whose profile registry couldn't be
//! read at all, or whose residency can't be checked because `lms` isn't
//! reachable, is one field naming the error, never an abort of the whole
//! `show`. The only thing that DOES exit non-zero is the config id itself
//! failing to resolve — nothing to show at all.

use crate::cli;
use crate::crew::mission_config::{self, FindingSeverity, LoadedMissionConfig, MissionConfig};
use crate::crew::step_kinds::StepKindRegistry;
use anyhow::{anyhow, Context, Result};
use darkmux_gestalt::{decide_residency, namespaced_identifier, Placement, ResidencyDecision, ResidentFact};
use darkmux_profiles::profiles::{resolve_role_profile_with, RoleBinding, RoleProfileSource};
use darkmux_types::{LoadedModel, ProfileRegistry};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

pub fn run(sub: cli::MissionConfigCmd) -> Result<i32> {
    match sub {
        cli::MissionConfigCmd::List { json } => list(json.json),
        cli::MissionConfigCmd::Show {
            id,
            params,
            profiles,
            json,
        } => show(&id, &params, profiles.profiles.as_deref(), json.json),
    }
}

// ── list ────────────────────────────────────────────────────────────────

/// One row of `mission config list` — either a fully loaded config's
/// summary, or (`error: Some(..)`) a config id that failed to load. Every
/// field is present in the JSON shape regardless (null on the failure
/// branch) so a machine reader never has to guess which fields a given row
/// carries.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ConfigListRow {
    pub id: String,
    pub name: Option<String>,
    pub source: Option<String>,
    pub manifest_path: Option<String>,
    pub phases: Option<usize>,
    pub tasks: Option<usize>,
    pub panel: Option<bool>,
    pub cmd: Option<String>,
    pub error: Option<String>,
}

/// Every discoverable mission-config id (`mission_config::list_ids()`,
/// already sorted), each loaded and summarized. A load failure on one id
/// never drops it from the list — it becomes an error row (mirrors
/// `acp_panel::list_panel_commands`'s "one broken override must not hide
/// the rest" rule) so a broken user-tier copy is visible, not silent.
pub(crate) fn build_list() -> Vec<ConfigListRow> {
    mission_config::list_ids()
        .into_iter()
        .map(|id| match mission_config::load(&id) {
            Ok(loaded) => {
                let total_tasks: usize =
                    loaded.config.phases.iter().map(|p| p.tasks.len()).sum();
                ConfigListRow {
                    id,
                    name: Some(loaded.config.name.clone()),
                    source: Some(loaded.source.label().to_string()),
                    manifest_path: Some(loaded.manifest_path.display().to_string()),
                    phases: Some(loaded.config.phases.len()),
                    tasks: Some(total_tasks),
                    panel: Some(loaded.config.panel.is_some()),
                    cmd: loaded.config.cmd.clone(),
                    error: None,
                }
            }
            Err(e) => ConfigListRow {
                id,
                name: None,
                source: None,
                manifest_path: None,
                phases: None,
                tasks: None,
                panel: None,
                cmd: None,
                error: Some(format!("{e:#}")),
            },
        })
        .collect()
}

fn render_list_text(rows: &[ConfigListRow]) -> String {
    let mut out = String::new();
    if rows.is_empty() {
        out.push_str("no mission configs registered\n");
        return out;
    }
    // Columns size to their content — ids and names are operator-authored
    // and routinely longer than any fixed width a table could guess at.
    let name_of = |r: &ConfigListRow| r.name.clone().unwrap_or_default();
    let id_w = rows.iter().map(|r| r.id.chars().count()).max().unwrap_or(2).max(2);
    let name_w = rows.iter().map(|r| name_of(r).chars().count()).max().unwrap_or(4).max(4);
    let src_w = rows
        .iter()
        .map(|r| r.source.as_deref().map(|s| s.chars().count()).unwrap_or(0))
        .max()
        .unwrap_or(6)
        .max(6);
    out.push_str(&format!(
        "{:<id_w$}  {:<name_w$}  {:<src_w$}  {:>6}  {:>5}  {:<5}  {}\n",
        "id", "name", "source", "phases", "tasks", "panel", "cmd"
    ));
    for row in rows {
        if let Some(err) = &row.error {
            let first_line = err.lines().next().unwrap_or(err);
            out.push_str(&format!("{:<id_w$}  (failed to load: {first_line})\n", row.id));
            continue;
        }
        out.push_str(&format!(
            "{:<id_w$}  {:<name_w$}  {:<src_w$}  {:>6}  {:>5}  {:<5}  {}\n",
            row.id,
            name_of(row),
            row.source.as_deref().unwrap_or(""),
            row.phases.unwrap_or(0),
            row.tasks.unwrap_or(0),
            if row.panel.unwrap_or(false) { "yes" } else { "no" },
            row.cmd.as_deref().unwrap_or("-"),
        ));
    }
    out
}

fn list(json: bool) -> Result<i32> {
    let rows = build_list();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "configs": rows }))?
        );
    } else {
        print!("{}", render_list_text(&rows));
    }
    Ok(0)
}

// ── show ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub(crate) struct InputJson {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
    /// (#2310 P4c-2 review item 3) `true` when the document declared
    /// `"ignored": true` on this input — mirrors `mission_config::
    /// MissionInput::ignored`, always present (never `Option`) so a
    /// `--json` consumer never has to distinguish "false" from "absent".
    pub ignored: bool,
    pub ignored_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PanelJson {
    pub description: Option<String>,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct GhVerbJson {
    pub verb: String,
    pub allowed: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct ModelJson {
    pub id: String,
    pub remote: bool,
    pub n_ctx: Option<u32>,
}

/// A task's `role_id` resolved to a profile + model, exactly as `mission
/// launch`/`dispatch` would resolve it right now. `residency` is always one
/// of `"loaded"` / `"loaded_stale_ctx"` / `"loaded_by_user"` /
/// `"not_loaded"` / `"remote"` / `"unavailable"` / `"unknown"`:
/// `"loaded_stale_ctx"` is `darkmux_gestalt::ResidencyDecision::Reconcile` —
/// a darkmux-owned resident shares the model but at an insufficient ctx, so
/// `mission launch` would unload + reload it (the #1135 shape) rather than
/// reuse it as-is. `"unknown"` covers every case where a model was never
/// reached (profile registry unavailable, dangling mapping, profile
/// declares no models, a local model missing `n_ctx`), which is when
/// `error` is set. `residency_detail` carries the human-readable
/// elaboration the text renderer prints; the JSON reader can ignore it and
/// key off `residency` alone.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RoleResolution {
    pub role_id: String,
    pub profile: Option<String>,
    pub provenance: Option<String>,
    pub model: Option<ModelJson>,
    pub residency: String,
    pub residency_detail: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StepJson {
    pub id: String,
    pub kind: String,
    pub constructible: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TaskJson {
    pub id: String,
    /// (#2302) The `enabled` FIELD verbatim — `Some(false)` is a task the
    /// document ships OFF and the mint prunes, `None` is a task that never
    /// declared the gate (the overwhelming majority) and runs. Surfaced
    /// because `mission config show` is where an operator answers "what
    /// would this launch actually do", and a pruned task that renders
    /// identically to a live one answers it wrong.
    pub enabled: Option<bool>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub depends_on: Vec<String>,
    pub reads: Vec<String>,
    pub role: Option<RoleResolution>,
    pub steps: Vec<StepJson>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PhaseJson {
    pub id: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub tasks: Vec<TaskJson>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RegistryJson {
    pub profiles_source: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ResidencyBlockJson {
    pub available: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ConfigShow {
    /// The id the caller ASKED for (`mission config show <id>`) — a
    /// user-tier file's own basename can differ from the `id` field its
    /// body declares (an alias). Compare against `id` below; when they
    /// differ, the text renderer names both so a shadowing alias is never
    /// silently invisible.
    pub requested_id: String,
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub source: String,
    pub manifest_path: String,
    pub schema_version: Option<String>,
    pub inputs: Vec<InputJson>,
    pub panel: Option<PanelJson>,
    pub cmd: Option<GhVerbJson>,
    pub phases: Vec<PhaseJson>,
    pub registry: RegistryJson,
    pub residency: ResidencyBlockJson,
    /// Planning-parity caveats — never empty-vs-absent ambiguity (always a
    /// real, possibly-empty array in JSON). Today's only producer: a
    /// `--param` override supplied for a config whose launcher doesn't
    /// apply role→profile overrides at all (see the module doc's
    /// "`--param` applies only where `mission launch` would apply it").
    pub warnings: Vec<String>,
}

/// A loaded profiles registry plus the path it resolved from — the two
/// facts [`build_show`] needs together (the registry to resolve against,
/// the path to report under `registry.profiles_source`). Owns the registry
/// rather than borrowing so callers can build it once from
/// `load_registry`'s already-owned `LoadedRegistry` without lifetime
/// gymnastics.
pub(crate) struct ProfilesCtx {
    pub registry: ProfileRegistry,
    pub path: String,
}

/// Resolve one role's binding into a [`RoleResolution`], never failing —
/// every error path (no profiles registry, dangling mapping, a profile
/// with no models, a default model absent from its own `models[]`) is
/// captured IN the returned value rather than propagated, so one bad role
/// binding never stops the rest of the graph from rendering (#44).
fn resolve_role(
    role_id: &str,
    profiles: Result<&ProfilesCtx, &str>,
    bindings: &dyn Fn(&str) -> RoleBinding,
    loaded_models: Result<&[LoadedModel], &str>,
) -> RoleResolution {
    let unresolved = |error: String| RoleResolution {
        role_id: role_id.to_string(),
        profile: None,
        provenance: None,
        model: None,
        residency: "unknown".to_string(),
        residency_detail: None,
        error: Some(error),
    };

    let ctx = match profiles {
        Ok(ctx) => ctx,
        Err(e) => return unresolved(format!("profile registry unavailable: {e}")),
    };

    let binding = bindings(role_id);
    let resolved = match resolve_role_profile_with(role_id, &binding, &ctx.registry) {
        Ok(r) => r,
        Err(e) => return unresolved(format!("{e:#}")),
    };
    let provenance = match resolved.source {
        RoleProfileSource::Overridden => "launch override (--param)",
        RoleProfileSource::Mapped => "role_profiles map",
        RoleProfileSource::DefaultFallback => "default_profile fallback",
    }
    .to_string();

    // Every early-return past this point has resolved a PROFILE (so
    // `profile`/`provenance` are known) but not yet a usable model — same
    // shape every time, so one closure builds it instead of three
    // hand-copied struct literals drifting apart.
    let bound_but_unusable = |error: String| RoleResolution {
        role_id: role_id.to_string(),
        profile: Some(resolved.profile_name.clone()),
        provenance: Some(provenance.clone()),
        model: None,
        residency: "unknown".to_string(),
        residency_detail: None,
        error: Some(error),
    };

    let Some(model_id) = resolved.profile.default_model_id() else {
        return bound_but_unusable(format!("profile \"{}\" declares no models", resolved.profile_name));
    };
    let Some(pm) = resolved.profile.models.iter().find(|m| m.id == model_id) else {
        return bound_but_unusable(format!(
            "profile \"{}\" names default model \"{model_id}\", absent from its own models[]",
            resolved.profile_name
        ));
    };

    // (merge-gate MUST-FIX 1) The SAME gate every real dispatch path
    // applies before trusting a local `ProfileModel` — see
    // `resourcing.rs::resolve_task_role`, `dispatch_internal.rs`, and
    // `darkmux-lab`'s `review.rs`, all of which call `require_n_ctx()` on a
    // non-remote model before staffing it. Without this, a local model
    // missing `n_ctx` rendered here as a healthy `not loaded` while
    // `mission launch` would refuse the whole run.
    if !pm.is_remote() {
        if let Err(e) = pm.require_n_ctx() {
            return bound_but_unusable(format!("{e:#}"));
        }
    }

    let (residency, residency_detail) = model_residency(pm, loaded_models);
    RoleResolution {
        role_id: role_id.to_string(),
        profile: Some(resolved.profile_name.clone()),
        provenance: Some(provenance),
        model: Some(ModelJson {
            id: pm.id.clone(),
            remote: pm.is_remote(),
            n_ctx: pm.n_ctx,
        }),
        residency,
        residency_detail,
        error: None,
    }
}

/// Whether `pm` (a resolved role's model) is currently loaded, and — for
/// the text renderer — the human-readable elaboration. Delegates the
/// verdict itself to `darkmux_gestalt::decide_residency` (#1274) — the
/// SAME ownership-partition + ctx-sufficiency arbiter every real
/// acquire path plans against — rather than re-deriving a comparison
/// that can (and did) diverge from it on two arms: an explicit-alias
/// resident counting as owned (the documented namespace opt-out,
/// `ownership.rs`'s `identifier == p.identifier` arm), and an
/// insufficient loaded ctx surfacing as `Reconcile` (`mission launch`
/// would unload + reload — the #1135 shape) rather than a bare `loaded`.
fn model_residency(
    pm: &darkmux_types::ProfileModel,
    loaded_models: Result<&[LoadedModel], &str>,
) -> (String, Option<String>) {
    if pm.is_remote() {
        return ("remote".to_string(), None);
    }
    match loaded_models {
        // (merge-gate CONSIDER 6) The cause already carries in the show-level
        // `residency.error` field / header line, printed ONCE — repeating
        // the whole message on every role line is noise, not information.
        Err(_) => ("unavailable".to_string(), None),
        Ok(models) => {
            let residents: Vec<ResidentFact> = models
                .iter()
                .map(|m| ResidentFact {
                    identifier: m.identifier.clone(),
                    model_key: m.model.clone(),
                    ctx: m.context,
                    est_bytes: None,
                })
                .collect();
            let placement = Placement {
                model_key: pm.id.clone(),
                identifier: namespaced_identifier(&pm.id, pm.identifier.as_deref()),
                min_ctx: pm.n_ctx.unwrap_or(0),
                seat: "mission-config-show".to_string(),
            };
            match decide_residency(&residents, &placement) {
                ResidencyDecision::Reuse { identifier, resident_ctx } => (
                    "loaded".to_string(),
                    Some(format!("loaded ({identifier}, ctx {resident_ctx})")),
                ),
                ResidencyDecision::Reconcile { stale_identifier, stale_ctx } => (
                    "loaded_stale_ctx".to_string(),
                    Some(format!(
                        "loaded at ctx {stale_ctx} ({stale_identifier}) — below the profile's \
                         {}; launch would reload",
                        pm.n_ctx.unwrap_or(0)
                    )),
                ),
                ResidencyDecision::ForeignDuplicate { foreign_identifier } => (
                    "loaded_by_user".to_string(),
                    Some(format!(
                        "loaded by user ({foreign_identifier}, not darkmux-managed) — darkmux \
                         will not dispatch to it, see CLAUDE.md namespace contract"
                    )),
                ),
                ResidencyDecision::LoadFresh => ("not_loaded".to_string(), None),
            }
        }
    }
}

/// Build the full [`ConfigShow`] for `loaded` — every phase/task/step, plus
/// (per task with a `role_id`) the role→profile→model resolution. Pure: no
/// I/O of its own, degrades every optional input independently
/// (`profiles`/`loaded_models` as `Err` never stops the graph from
/// rendering; only `error` fields fill in).
pub(crate) fn build_show(
    requested_id: &str,
    loaded: &LoadedMissionConfig,
    registry: &StepKindRegistry,
    profiles: Result<&ProfilesCtx, &str>,
    bindings: &dyn Fn(&str) -> RoleBinding,
    loaded_models: Result<&[LoadedModel], &str>,
    warnings: &[String],
) -> ConfigShow {
    let config: &MissionConfig = &loaded.config;

    let phases = config
        .phases
        .iter()
        .map(|phase| PhaseJson {
            id: phase.id.clone(),
            display_name: phase.display_name.clone(),
            description: phase.description.clone(),
            tasks: phase
                .tasks
                .iter()
                .map(|task| TaskJson {
                    id: task.id.clone(),
                    enabled: task.enabled,
                    display_name: task.display_name.clone(),
                    description: task.description.clone(),
                    depends_on: task.depends_on.clone(),
                    reads: task.reads.clone(),
                    role: task
                        .role_id
                        .as_deref()
                        .map(|role_id| resolve_role(role_id, profiles, bindings, loaded_models)),
                    steps: task
                        .steps
                        .iter()
                        .map(|step| StepJson {
                            id: step.id.clone(),
                            kind: step.kind.clone(),
                            constructible: registry.get(&step.kind).is_ok(),
                        })
                        .collect(),
                })
                .collect(),
        })
        .collect();

    let panel = config.panel.as_ref().map(|p| PanelJson {
        description: p.description.clone(),
        hint: p.hint.clone(),
    });
    let cmd = config.cmd.as_ref().map(|v| GhVerbJson {
        verb: v.clone(),
        allowed: darkmux_types::config_access::cmd_allowed(v),
    });

    let registry_json = match profiles {
        Ok(ctx) => RegistryJson {
            profiles_source: Some(ctx.path.clone()),
            error: None,
        },
        Err(e) => RegistryJson {
            profiles_source: None,
            error: Some(e.to_string()),
        },
    };
    let residency_json = match loaded_models {
        Ok(_) => ResidencyBlockJson { available: true, error: None },
        Err(e) => ResidencyBlockJson { available: false, error: Some(e.to_string()) },
    };

    ConfigShow {
        requested_id: requested_id.to_string(),
        id: config.id.clone(),
        name: config.name.clone(),
        description: config.description.clone(),
        source: loaded.source.label().to_string(),
        manifest_path: loaded.manifest_path.display().to_string(),
        schema_version: config.schema_version.clone(),
        inputs: config
            .inputs
            .iter()
            .map(|i| InputJson {
                name: i.name.clone(),
                description: i.description.clone(),
                required: i.required.unwrap_or(true),
                ignored: i.ignored.unwrap_or(false),
                ignored_reason: i.ignored_reason.clone(),
            })
            .collect(),
        panel,
        cmd,
        phases,
        registry: registry_json,
        residency: residency_json,
        warnings: warnings.to_vec(),
    }
}

/// The first sentence of a description, capped at [`DESCRIPTION_TEXT_CAP`]
/// chars on a word boundary — whichever comes first — with a
/// `… (--json for the full text)` marker whenever something was left out.
/// The built-in `review` opens with a 500-char sentence, so a sentence
/// boundary alone is not a cap.
fn first_sentence(d: &str) -> String {
    let d = d.trim();
    let sentence_end = d
        .char_indices()
        .find(|&(i, c)| c == '.' && d[i + 1..].chars().next().is_none_or(|n| n.is_whitespace()))
        .map(|(i, _)| i + 1)
        .unwrap_or(d.len());
    let mut cut = sentence_end;
    if d[..cut].chars().count() > DESCRIPTION_TEXT_CAP {
        // Back off to the last whitespace before the cap (char-safe).
        let cap_byte = d.char_indices().nth(DESCRIPTION_TEXT_CAP).map(|(i, _)| i).unwrap_or(d.len());
        cut = d[..cap_byte].rfind(char::is_whitespace).unwrap_or(cap_byte);
    }
    if cut < d.len() {
        format!("{} … (--json for the full text)", d[..cut].trim_end())
    } else {
        d.to_string()
    }
}

/// Text-view cap on a config description (chars). Roughly two terminal
/// lines: enough to say what the config is for, not its history.
const DESCRIPTION_TEXT_CAP: usize = 200;

fn render_role_line(role: &RoleResolution) -> String {
    if let Some(err) = &role.error {
        return format!("    ↳ role {} → ERROR: {err}\n", role.role_id);
    }
    let profile = role.profile.as_deref().unwrap_or("?");
    let provenance = role.provenance.as_deref().unwrap_or("?");
    let model = match &role.model {
        Some(m) => {
            let ctx = match m.n_ctx {
                Some(n) => format!("n_ctx {n}"),
                None if m.remote => "remote".to_string(),
                None => "n_ctx ?".to_string(),
            };
            format!("{} ({ctx})", m.id)
        }
        None => "?".to_string(),
    };
    // The JSON keeps the snake_case enum; the text reads as words.
    let residency = role
        .residency_detail
        .clone()
        .unwrap_or_else(|| role.residency.replace('_', " "));
    format!(
        "    ↳ role {} → profile {profile} ({provenance}) → model {model} · {residency}\n",
        role.role_id
    )
}

/// Every role id the graph actually declares a task under — used to group
/// the per-role override "pseudo-inputs" (`review`'s `review-probe-high`
/// etc.) into one line instead of one per role (CONSIDER 10).
fn declared_role_ids_in_graph(show: &ConfigShow) -> BTreeSet<&str> {
    show.phases
        .iter()
        .flat_map(|p| p.tasks.iter())
        .filter_map(|t| t.role.as_ref().map(|r| r.role_id.as_str()))
        .collect()
}

fn render_show_text(show: &ConfigShow) -> String {
    let mut out = String::new();
    if show.requested_id == show.id {
        out.push_str(&format!("mission config \"{}\" — {}\n", show.id, show.name));
    } else {
        // (CONSIDER 11) A user-tier file's own basename can differ from the
        // `id` its body declares (an alias) — name both, never just the one
        // that happens to match what was typed.
        out.push_str(&format!(
            "mission config \"{}\" (document id \"{}\") — {}\n",
            show.requested_id, show.id, show.name
        ));
    }
    out.push_str(&format!("  source: {} ({})\n", show.source, show.manifest_path));
    if let Some(v) = &show.schema_version {
        out.push_str(&format!("  schema_version: {v}\n"));
    }
    if let Some(d) = &show.description {
        // Record exhaustively, display selectively: a config's description
        // can run to paragraphs (the built-in `review` carries its whole
        // design history). The text view shows the first sentence and says
        // where the rest is; `--json` carries it verbatim.
        out.push_str(&format!("  description: {}\n", first_sentence(d)));
    }
    let role_ids = declared_role_ids_in_graph(show);
    let (role_inputs, other_inputs): (Vec<&InputJson>, Vec<&InputJson>) = show
        .inputs
        .iter()
        .partition(|i| !i.required && role_ids.contains(i.name.as_str()));
    if !show.inputs.is_empty() {
        out.push_str("  inputs:\n");
        for i in &other_inputs {
            let req = if i.required { "required" } else { "optional" };
            // (#2310 P4c-2 review item 3) An ignored input's line names
            // WHY, so an operator reading `mission config show` sees the
            // same signal the launch-time warning gives, without having to
            // launch first.
            if i.ignored {
                let reason = i.ignored_reason.as_deref().unwrap_or("no reason given");
                out.push_str(&format!("    {} ({req}, ignored: {reason})\n", i.name));
            } else {
                out.push_str(&format!("    {} ({req})\n", i.name));
            }
        }
        if !role_inputs.is_empty() {
            let names: Vec<&str> = role_inputs.iter().map(|i| i.name.as_str()).collect();
            out.push_str(&format!("    role overrides: {} (optional)\n", names.join(", ")));
        }
    }
    match &show.panel {
        Some(p) => out.push_str(&format!(
            "  panel: {}{}\n",
            p.description.as_deref().unwrap_or(&show.name),
            p.hint.as_ref().map(|h| format!(" (hint: {h})")).unwrap_or_default()
        )),
        None => out.push_str("  panel: (not panel-advertised)\n"),
    }
    match &show.cmd {
        Some(g) => out.push_str(&format!(
            "  cmd: {} ({})\n",
            g.verb,
            if g.allowed { "allowed" } else { "NOT allowed — see darkmux config set gh.*" }
        )),
        None => out.push_str("  cmd: (none)\n"),
    }
    out.push_str(&format!(
        "  profiles registry: {}\n",
        show
            .registry
            .profiles_source
            .clone()
            .unwrap_or_else(|| format!("unavailable ({})", show.registry.error.as_deref().unwrap_or("?")))
    ));
    if !show.residency.available {
        out.push_str(&format!(
            "  residency: unavailable ({})\n",
            show.residency.error.as_deref().unwrap_or("?")
        ));
    }
    for w in &show.warnings {
        out.push_str(&format!("  warning: {w}\n"));
    }
    out.push('\n');

    for phase in &show.phases {
        out.push_str(&format!(
            "{} \"{}\"\n",
            phase.id,
            phase.display_name.as_deref().unwrap_or(&phase.id)
        ));
        if let Some(d) = &phase.description {
            out.push_str(&format!("  {}\n", darkmux_types::style::dim(&first_sentence(d))));
        }
        for task in &phase.tasks {
            let deps = if task.depends_on.is_empty() {
                String::new()
            } else {
                format!("  deps: {}", task.depends_on.join(", "))
            };
            let label = match &task.display_name {
                Some(dn) if dn != &task.id => format!(" \"{dn}\""),
                _ => String::new(),
            };
            // (#2302) A task shipped OFF is pruned at mint, so say so on
            // its own line rather than leaving it to read as live work.
            let gate = if task.enabled == Some(false) { "  [disabled]" } else { "" };
            out.push_str(&format!("  task {}{label}{deps}{gate}\n", task.id));
            // Who does it, then what it does: the role line sits directly
            // under its task, ahead of the steps it staffs.
            if let Some(role) = &task.role {
                out.push_str(&render_role_line(role));
            }
            for step in &task.steps {
                let ctor = if step.constructible {
                    "constructible"
                } else {
                    "NOT constructible by this binary → launch would exit 4"
                };
                out.push_str(&format!("    step {}  kind {}  [{ctor}]\n", step.id, step.kind));
            }
        }
    }
    out
}

/// The `RoleBinding` precedence every launcher applies: an explicit
/// per-run override wins, else the `role_profiles` map binding, else
/// unmapped (falls through to `default_profile` at resolution). Extracted
/// to a pure, three-arm-testable function (merge-gate CONSIDER 4) — the
/// closure this used to be inlined as could have its arms silently swapped
/// without any test noticing.
fn binding_for(role: &str, overrides: &BTreeMap<String, String>, mapped: Option<String>) -> RoleBinding {
    if let Some(p) = overrides.get(role) {
        RoleBinding::Overridden(p.clone())
    } else if let Some(p) = mapped {
        RoleBinding::Mapped(p)
    } else {
        RoleBinding::Unmapped
    }
}

/// (merge-gate MUST-FIX 3) `--param ROLE=PROFILE` is applied as a launch
/// binding ONLY where `mission launch` would apply it: the review route
/// (`mission_launch_review.rs`), gated by the SAME structural test
/// `mission launch` itself uses to route there
/// (`mission_launch::config_uses_review_kinds`). Any other config (e.g.
/// `coder-phase`) has `--param` overrides silently ignored by `launch`
/// today — `show` must not claim a parity it can't deliver. Returns the
/// overrides actually eligible to apply (empty, with a warning, when the
/// config doesn't use the review kinds) plus zero or more warnings —
/// including one per override naming a role NO task in the graph declares
/// (an operator typo `show` can catch before it's a wasted `mission
/// launch` invocation).
fn effective_overrides_and_warnings(
    config: &MissionConfig,
    overrides: BTreeMap<String, String>,
) -> (BTreeMap<String, String>, Vec<String>) {
    if overrides.is_empty() {
        return (overrides, Vec::new());
    }
    if !crate::mission_launch::config_uses_review_kinds(config) {
        let roles: Vec<&str> = overrides.keys().map(String::as_str).collect();
        return (
            BTreeMap::new(),
            vec![format!(
                "--param {} ignored for planning parity: this config's launcher does not apply \
                 role→profile overrides on this route (only the review route does) — every \
                 role below resolves as a real `mission launch {}` would (role_profiles map / \
                 default_profile only)",
                roles.iter().map(|r| format!("{r}=…")).collect::<Vec<_>>().join(", "),
                config.id
            )],
        );
    }
    let declared: BTreeSet<&str> = config
        .phases
        .iter()
        .flat_map(|p| p.tasks.iter())
        .filter_map(|t| t.role_id.as_deref())
        .collect();
    let mut warnings = Vec::new();
    for role in overrides.keys() {
        if !declared.contains(role.as_str()) {
            warnings.push(format!(
                "--param {role}=… ignored: no task in this config declares role_id \"{role}\""
            ));
        }
    }
    (overrides, warnings)
}

/// `--param ROLE=PROFILE` overrides, exactly as `mission launch --param
/// <role>=<profile>` parses them (mirrors `mission_launch_review`'s own
/// `collect_role_overrides`), except a malformed entry bails immediately
/// with a copy-pasteable example rather than being silently dropped — a
/// `show` invocation with a typo'd override should never render as if the
/// override were absent.
fn parse_role_overrides(params: &[String]) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for raw in params {
        let Some((role, profile)) = raw.split_once('=') else {
            return Err(anyhow!(
                "darkmux mission config show: --param `{raw}` must be in `ROLE=PROFILE` form, \
                 e.g. --param review-judge=review-mid"
            ));
        };
        let (role, profile) = (role.trim(), profile.trim());
        if role.is_empty() || profile.is_empty() {
            return Err(anyhow!(
                "darkmux mission config show: --param `{raw}` must be in `ROLE=PROFILE` form, \
                 e.g. --param review-judge=review-mid"
            ));
        }
        map.insert(role.to_string(), profile.to_string());
    }
    Ok(map)
}

fn show(id: &str, params: &[String], profiles_file: Option<&str>, json: bool) -> Result<i32> {
    let loaded = mission_config::load(id).with_context(|| {
        format!(
            "loading mission config \"{id}\" — note: a user-tier copy \
             (~/.darkmux/mission-configs/{id}.json) or an on-disk template overrides an \
             embedded built-in; the failing file is named above if one was found"
        )
    })?;

    let raw_overrides = parse_role_overrides(params)?;
    let (overrides, mut warnings) = effective_overrides_and_warnings(&loaded.config, raw_overrides);
    let bindings = move |role: &str| -> RoleBinding {
        binding_for(role, &overrides, darkmux_types::config_access::role_profile(role))
    };

    let registry = crate::mission_launch::all_step_kinds()
        .context("building the step-kind registry")?;

    // (#2345 C2) Semantic validation, surfaced here too — same call
    // `mission launch` refuses a config on (`config.validate`), so a
    // config-authoring mistake (e.g. an `outcome_from` naming an unknown
    // task, or a `grow` template) is visible via `mission config show`
    // BEFORE anyone runs `mission launch` and hits the refusal — never a
    // second, drifting check. Only `Error`-severity findings: this show
    // command's existing `warnings` bucket is for launch-time actionable
    // items, and validate's own `Warning`-severity findings (schema-version
    // drift, an unrecognized step kind) are routine noise this command
    // doesn't otherwise surface — an `Error` finding is not.
    let known_kind_ids = registry.ids();
    let known_kinds: Vec<&str> = known_kind_ids.iter().map(String::as_str).collect();
    for f in loaded.config.validate(&known_kinds).into_iter().filter(|f| f.severity == FindingSeverity::Error) {
        warnings.push(format!("config validation: {f}"));
    }

    // (merge-gate CONSIDER 7) Mirrors `machine status`'s own rule
    // (src/main.rs, `cmd_machine_status`): an EXPLICIT `--profiles-file`
    // that fails to load is never silently swallowed — only the no-arg
    // default (fresh machine, no registry yet) degrades to "unavailable"
    // per-role rather than aborting the whole `show`.
    let profiles_owned: Result<ProfilesCtx, String> =
        match darkmux_profiles::profiles::load_registry(profiles_file) {
            Ok(lr) => Ok(ProfilesCtx { registry: lr.registry, path: lr.path.display().to_string() }),
            Err(e) if profiles_file.is_some() => {
                return Err(e.context("reading --profiles-file for `mission config show`"));
            }
            Err(e) => Err(format!("{e:#}")),
        };
    let profiles_ctx = profiles_owned.as_ref().map_err(|e| e.as_str());

    let loaded_models_owned = darkmux_profiles::lms::list_loaded().map_err(|e| format!("{e:#}"));
    let loaded_models = loaded_models_owned.as_deref().map_err(|e| e.as_str());

    let show = build_show(id, &loaded, &registry, profiles_ctx, &bindings, loaded_models, &warnings);

    if json {
        println!("{}", serde_json::to_string_pretty(&show)?);
    } else {
        print!("{}", render_show_text(&show));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crew::mission_config::{
        MissionConfigSource, MissionInput, PanelConfig, PhaseConfig, StepConfig, TaskConfig,
    };
    use crate::crew::step_kinds::ProceduralNoopStepKind;
    use darkmux_types::{ModelEndpoint, Profile, ProfileModel};
    use std::sync::Arc;

    // ── fixtures ───────────────────────────────────────────────────────

    fn step(id: &str, kind: &str) -> StepConfig {
        StepConfig {
            enabled: None,
            id: id.to_string(),
            kind: kind.to_string(),
            config: serde_json::Value::Null,
            gate: None,
            extras: Default::default(),
        }
    }

    fn task(id: &str, role_id: Option<&str>, steps: Vec<StepConfig>) -> TaskConfig {
        TaskConfig {
            enabled: None,
            id: id.to_string(),
            description: None,
            display_name: None,
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: role_id.map(str::to_string),
            run_on: None,
            steps,
            grow: None,
            extras: Default::default(),
        }
    }

    fn phase(id: &str, tasks: Vec<crate::crew::mission_config::TaskConfig>) -> PhaseConfig {
        PhaseConfig {
            enabled: None,
            id: id.to_string(),
            description: None,
            display_name: None,
            tasks,
            extras: Default::default(),
        }
    }

    fn loaded_doc(config: MissionConfig) -> LoadedMissionConfig {
        LoadedMissionConfig {
            config,
            manifest_path: "<test>/doc.json".into(),
            source: MissionConfigSource::User,
        }
    }

    fn doc(phases: Vec<PhaseConfig>) -> MissionConfig {
        MissionConfig {
            id: "m".to_string(),
            name: "M".to_string(),
            description: None,
            schema_version: None,
            inputs: Vec::new(),
            phases,
            panel: None,
            cmd: None,
            outcome_from: None,
            extras: Default::default(),
        }
    }

    fn local_model(id: &str, n_ctx: u32) -> ProfileModel {
        ProfileModel { id: id.to_string(), n_ctx: Some(n_ctx), ..Default::default() }
    }

    fn remote_model(id: &str) -> ProfileModel {
        ProfileModel {
            id: id.to_string(),
            endpoint: Some(ModelEndpoint {
                url: Some("https://example.azure.com/openai".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn reg(profiles: Vec<(&str, Vec<ProfileModel>)>, default: Option<&str>) -> ProfileRegistry {
        let map = profiles
            .into_iter()
            .map(|(name, models)| (name.to_string(), Profile { models, ..Default::default() }))
            .collect();
        ProfileRegistry { profiles: map, default_profile: default.map(String::from), ..Default::default() }
    }

    fn ctx(registry: ProfileRegistry) -> ProfilesCtx {
        ProfilesCtx { registry, path: "<test-profiles>.json".to_string() }
    }

    fn loaded_model(model: &str, namespaced: bool, ctx: u64) -> LoadedModel {
        let identifier = if namespaced {
            format!("darkmux:{model}")
        } else {
            model.to_string()
        };
        LoadedModel {
            identifier,
            model: model.to_string(),
            status: "loaded".to_string(),
            size: "1 GB".to_string(),
            context: ctx,
        }
    }

    // ── constructibility ─────────────────────────────────────────────

    #[test]
    fn step_constructibility_reflects_the_registry() {
        let registry = StepKindRegistry::new();
        registry.register(Arc::new(ProceduralNoopStepKind)).unwrap();
        let cfg = doc(vec![phase(
            "p1",
            vec![task(
                "t1",
                None,
                vec![step("s1", "procedural.noop"), step("s2", "totally.unknown")],
            )],
        )]);
        let loaded = loaded_doc(cfg);
        let show = build_show("m", &loaded, &registry, Err("no registry needed"), &|_| RoleBinding::Unmapped, Err("no lms needed"), &[]);
        let steps = &show.phases[0].tasks[0].steps;
        assert!(steps[0].constructible, "procedural.noop must be constructible");
        assert!(!steps[1].constructible, "totally.unknown must NOT be constructible");
    }

    // ── provenance ────────────────────────────────────────────────────

    #[test]
    fn provenance_names_the_winning_binding_tier() {
        let registry = StepKindRegistry::new();
        let profiles = reg(vec![("fast", vec![local_model("m-fast", 8000)])], Some("fast"));
        let cfg = doc(vec![phase(
            "p1",
            vec![
                task("overridden", Some("role-a"), vec![step("s1", "k")]),
                task("mapped", Some("role-b"), vec![step("s2", "k")]),
                task("unmapped", Some("role-c"), vec![step("s3", "k")]),
            ],
        )]);
        let loaded = loaded_doc(cfg);
        let pctx = ctx(profiles);
        let bindings = |role: &str| match role {
            "role-a" => RoleBinding::Overridden("fast".to_string()),
            "role-b" => RoleBinding::Mapped("fast".to_string()),
            _ => RoleBinding::Unmapped,
        };
        let show = build_show("m", &loaded, &registry, Ok(&pctx), &bindings, Ok(&[]), &[]);
        let roles: Vec<&RoleResolution> =
            show.phases[0].tasks.iter().map(|t| t.role.as_ref().unwrap()).collect();
        assert_eq!(roles[0].provenance.as_deref(), Some("launch override (--param)"));
        assert_eq!(roles[1].provenance.as_deref(), Some("role_profiles map"));
        assert_eq!(roles[2].provenance.as_deref(), Some("default_profile fallback"));
        for r in &roles {
            assert!(r.error.is_none(), "unexpected error: {:?}", r.error);
            assert_eq!(r.profile.as_deref(), Some("fast"));
        }
    }

    // ── dangling mapping ──────────────────────────────────────────────

    #[test]
    fn dangling_mapping_is_captured_per_role_and_the_rest_still_builds() {
        let registry = StepKindRegistry::new();
        let profiles = reg(vec![("fast", vec![local_model("m-fast", 8000)])], Some("fast"));
        let cfg = doc(vec![phase(
            "p1",
            vec![
                task("broken", Some("role-broken"), vec![step("s1", "k")]),
                task("fine", Some("role-fine"), vec![step("s2", "k")]),
            ],
        )]);
        let loaded = loaded_doc(cfg);
        let pctx = ctx(profiles);
        let bindings = |role: &str| match role {
            "role-broken" => RoleBinding::Mapped("ghost-profile".to_string()),
            _ => RoleBinding::Unmapped,
        };
        let show = build_show("m", &loaded, &registry, Ok(&pctx), &bindings, Ok(&[]), &[]);
        let broken = show.phases[0].tasks[0].role.as_ref().unwrap();
        assert!(broken.error.is_some(), "dangling mapping must be captured as an error");
        assert_eq!(broken.residency, "unknown");
        let fine = show.phases[0].tasks[1].role.as_ref().unwrap();
        assert!(fine.error.is_none(), "the rest of the graph must still resolve: {:?}", fine.error);
        assert_eq!(fine.profile.as_deref(), Some("fast"));
    }

    // ── n_ctx gate (merge-gate MUST-FIX 1) ─────────────────────────────
    // Every real dispatch path (resourcing.rs::resolve_task_role,
    // dispatch_internal.rs, darkmux-lab's review.rs) refuses a local model
    // with no declared n_ctx before staffing it. `show` must refuse the
    // same way, not render it as a healthy `not loaded`.

    #[test]
    fn local_model_without_n_ctx_is_refused_like_launch_would_refuse_it() {
        let registry = StepKindRegistry::new();
        let no_ctx_model = ProfileModel { id: "m-noctx".to_string(), n_ctx: None, ..Default::default() };
        let profiles = reg(vec![("fast", vec![no_ctx_model])], Some("fast"));
        let cfg = doc(vec![phase("p1", vec![task("t1", Some("role-a"), vec![step("s1", "k")])])]);
        let loaded = loaded_doc(cfg);
        let pctx = ctx(profiles);
        let show = build_show("m", &loaded, &registry, Ok(&pctx), &|_| RoleBinding::Unmapped, Ok(&[]), &[]);
        let role = show.phases[0].tasks[0].role.as_ref().unwrap();
        assert!(
            role.error.is_some(),
            "a local model with no n_ctx must be refused like `mission launch` refuses it"
        );
        assert!(role.error.as_ref().unwrap().contains("n_ctx"), "{:?}", role.error);
        assert!(role.model.is_none(), "no usable model was ever confirmed staffable");
        // Provenance/profile were still resolved (the failure is at the
        // n_ctx gate, not at role→profile resolution) — the operator sees
        // WHICH profile/binding was in play when it failed.
        assert_eq!(role.profile.as_deref(), Some("fast"));
    }

    #[test]
    fn remote_model_without_n_ctx_is_unaffected_by_the_n_ctx_gate() {
        let registry = StepKindRegistry::new();
        let remote = remote_model("gpt-4-remote"); // no n_ctx declared
        let profiles = reg(vec![("fast", vec![remote])], Some("fast"));
        let cfg = doc(vec![phase("p1", vec![task("t1", Some("role-a"), vec![step("s1", "k")])])]);
        let loaded = loaded_doc(cfg);
        let pctx = ctx(profiles);
        let show = build_show("m", &loaded, &registry, Ok(&pctx), &|_| RoleBinding::Unmapped, Ok(&[]), &[]);
        let role = show.phases[0].tasks[0].role.as_ref().unwrap();
        assert!(role.error.is_none(), "a remote model must never be gated on n_ctx: {:?}", role.error);
        assert_eq!(role.residency, "remote");
    }

    // ── residency ─────────────────────────────────────────────────────

    #[test]
    fn residency_loaded_darkmux_owned() {
        let m = local_model("m-a", 8000);
        let loaded = vec![loaded_model("m-a", true, 32000)];
        let (residency, detail) = model_residency(&m, Ok(&loaded));
        assert_eq!(residency, "loaded");
        assert!(detail.unwrap().contains("darkmux:m-a"));
    }

    #[test]
    fn residency_loaded_by_user_when_not_namespaced() {
        let m = local_model("m-a", 8000);
        let loaded = vec![loaded_model("m-a", false, 32000)];
        let (residency, detail) = model_residency(&m, Ok(&loaded));
        assert_eq!(residency, "loaded_by_user");
        assert!(detail.unwrap().contains("not darkmux-managed"));
    }

    #[test]
    fn residency_not_loaded_when_absent() {
        let m = local_model("m-a", 8000);
        let loaded: Vec<LoadedModel> = vec![loaded_model("some-other-model", true, 32000)];
        let (residency, detail) = model_residency(&m, Ok(&loaded));
        assert_eq!(residency, "not_loaded");
        assert!(detail.is_none());
    }

    #[test]
    fn residency_remote_never_consults_lms() {
        let m = remote_model("gpt-4-remote");
        // `Err` loaded_models — a remote model's residency must not depend
        // on `lms` being reachable at all.
        let (residency, detail) = model_residency(&m, Err("lms not found"));
        assert_eq!(residency, "remote");
        assert!(detail.is_none());
    }

    #[test]
    fn residency_unavailable_when_lms_fails() {
        // (merge-gate CONSIDER 6) The cause is printed ONCE, in the show-level
        // header (`residency: unavailable (<err>)`) — repeating the whole
        // message on every role line was noise. Per-role detail is bare
        // `None` here; the text renderer falls back to the residency word
        // itself ("unavailable").
        let m = local_model("m-a", 8000);
        let (residency, detail) = model_residency(&m, Err("lms: command not found"));
        assert_eq!(residency, "unavailable");
        assert!(detail.is_none(), "per-role detail must not repeat the error: {detail:?}");
    }

    // ── residency: canonical arbiter arms (merge-gate MUST-FIX 2) ──────
    // These two arms are exactly where a hand-rolled comparison diverged
    // from `darkmux_gestalt::decide_residency` — proven here against the
    // gestalt fixtures' own shape (`ownership.rs` explicit-alias arm,
    // `residency.rs` Reconcile arm).

    #[test]
    fn residency_reconcile_when_loaded_ctx_is_insufficient() {
        // A darkmux-owned resident shares the model but is loaded BELOW the
        // profile's declared n_ctx — `mission launch` would unload + reload
        // it (the #1135 shape), not silently reuse it as "loaded".
        let m = local_model("m-a", 262144);
        let loaded = vec![loaded_model("m-a", true, 4096)];
        let (residency, detail) = model_residency(&m, Ok(&loaded));
        assert_eq!(residency, "loaded_stale_ctx", "an insufficient loaded ctx must NOT read as plain `loaded`");
        let d = detail.expect("Reconcile must carry a human explanation");
        assert!(d.contains("4096"), "{d}");
        assert!(d.contains("262144"), "{d}");
    }

    #[test]
    fn residency_explicit_alias_counts_as_owned_not_loaded_by_user() {
        // The documented namespace opt-out (`ownership.rs`'s
        // `identifier == p.identifier` arm): a profile model with an
        // explicit `identifier` override, loaded under that EXACT bare
        // identifier, is darkmux's own — never "loaded by user", and never
        // told "darkmux will not dispatch to it" when it plainly will.
        let mut m = local_model("m-a", 8000);
        m.identifier = Some("my-custom-alias".to_string());
        let loaded = vec![LoadedModel {
            identifier: "my-custom-alias".to_string(),
            model: "m-a".to_string(),
            status: "loaded".to_string(),
            size: "1 GB".to_string(),
            context: 32000,
        }];
        let (residency, detail) = model_residency(&m, Ok(&loaded));
        assert_eq!(
            residency, "loaded",
            "an explicit-alias resident must count as darkmux's own, not user state: {detail:?}"
        );
    }

    // (`residency_loaded_by_user_when_not_namespaced`, above, already covers
    // the plain `ForeignDuplicate` arm — a bare non-namespaced, non-alias
    // resident.)

    // ── list: load failure surfaces as a row, never hides ─────────────

    #[test]
    #[serial_test::serial]
    fn list_row_for_a_load_failure_names_the_error_and_keeps_the_id() {
        // A user-tier override that fails to PARSE — `mission_config::load`
        // returns Err for this id, and `build_list` must still emit a row
        // for it (never silently drop it from the list).
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("mission-configs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("broken-config.json"), "{ not json").unwrap();
        let prev = std::env::var("DARKMUX_CREW_DIR").ok();
        unsafe { std::env::set_var("DARKMUX_CREW_DIR", tmp.path()) };

        let rows = build_list();

        unsafe {
            match &prev {
                Some(v) => std::env::set_var("DARKMUX_CREW_DIR", v),
                None => std::env::remove_var("DARKMUX_CREW_DIR"),
            }
        }

        let row = rows.iter().find(|r| r.id == "broken-config").expect("broken-config must still be listed");
        assert!(row.error.is_some(), "a parse failure must be captured as an error, not hidden");
        assert!(row.name.is_none());
        // The graph-bearing embedded built-ins, plus crawl's
        // documentation-only zero-graph one, must still be present
        // alongside it.
        assert!(rows.iter().any(|r| r.id == "review" && r.error.is_none()));
        assert!(rows.iter().any(|r| r.id == "coder-phase" && r.error.is_none()));
        assert!(rows.iter().any(|r| r.id == "crawl" && r.error.is_none()));
    }

    // ── JSON shape ────────────────────────────────────────────────────

    #[test]
    fn list_row_json_has_stable_null_keys_on_failure() {
        let row = ConfigListRow {
            id: "x".to_string(),
            name: None,
            source: None,
            manifest_path: None,
            phases: None,
            tasks: None,
            panel: None,
            cmd: None,
            error: Some("boom".to_string()),
        };
        let v = serde_json::to_value(&row).unwrap();
        for key in ["id", "name", "source", "manifest_path", "phases", "tasks", "panel", "cmd", "error"] {
            assert!(v.get(key).is_some(), "missing key {key} in {v}");
        }
        assert!(v["name"].is_null());
        assert_eq!(v["error"], "boom");
    }

    #[test]
    fn show_json_role_shape_has_stable_keys() {
        let registry = StepKindRegistry::new();
        let profiles = reg(vec![("fast", vec![local_model("m-fast", 8000)])], Some("fast"));
        let cfg = doc(vec![phase("p1", vec![task("t1", Some("role-a"), vec![step("s1", "k")])])]);
        let loaded = loaded_doc(cfg);
        let pctx = ctx(profiles);
        let show = build_show("m", &loaded, &registry, Ok(&pctx), &|_| RoleBinding::Unmapped, Ok(&[]), &[]);
        let v = serde_json::to_value(&show).unwrap();
        let role = &v["phases"][0]["tasks"][0]["role"];
        for key in ["role_id", "profile", "provenance", "model", "residency", "error"] {
            assert!(role.get(key).is_some(), "missing role key {key} in {role}");
        }
        assert!(v["registry"].get("profiles_source").is_some());
        assert!(v["residency"].get("available").is_some());
    }

    // ── text rendering ────────────────────────────────────────────────

    #[test]
    fn text_render_contains_the_role_resolution_line() {
        let registry = StepKindRegistry::new();
        let profiles = reg(vec![("fast", vec![local_model("m-fast", 8000)])], Some("fast"));
        let cfg = doc(vec![phase("p1", vec![task("t1", Some("role-a"), vec![step("s1", "k")])])]);
        let loaded = loaded_doc(cfg);
        let pctx = ctx(profiles);
        let bindings = |_: &str| RoleBinding::Mapped("fast".to_string());
        let show = build_show("m", &loaded, &registry, Ok(&pctx), &bindings, Ok(&[]), &[]);
        let text = render_show_text(&show);
        assert!(
            text.contains("↳ role role-a → profile fast (role_profiles map) → model m-fast"),
            "got:\n{text}"
        );
    }

    #[test]
    fn text_render_shows_not_constructible_hint() {
        let registry = StepKindRegistry::new(); // empty — nothing constructs
        let cfg = doc(vec![phase("p1", vec![task("t1", None, vec![step("s1", "unknown.kind")])])]);
        let loaded = loaded_doc(cfg);
        let show = build_show("m", &loaded, &registry, Err("n/a"), &|_| RoleBinding::Unmapped, Err("n/a"), &[]);
        let text = render_show_text(&show);
        assert!(text.contains("NOT constructible by this binary → launch would exit 4"), "got:\n{text}");
    }

    #[test]
    fn text_render_puts_the_role_line_under_its_task_before_the_steps() {
        // Who does it, then what it does — the operator reads the staffing
        // decision first, then the mechanics it staffs.
        let registry = StepKindRegistry::new();
        let profiles = reg(vec![("fast", vec![local_model("m-fast", 8000)])], Some("fast"));
        let cfg = doc(vec![phase("p1", vec![task("t1", Some("role-a"), vec![step("s1", "k")])])]);
        let loaded = loaded_doc(cfg);
        let pctx = ctx(profiles);
        let show = build_show("m", &loaded, &registry, Ok(&pctx), &|_| RoleBinding::Unmapped, Ok(&[]), &[]);
        let text = render_show_text(&show);
        let task_at = text.find("  task t1").expect("task line");
        let role_at = text.find("↳ role role-a").expect("role line");
        let step_at = text.find("step s1").expect("step line");
        assert!(task_at < role_at && role_at < step_at, "order task→role→step, got:\n{text}");
    }

    #[test]
    fn text_residency_reads_as_words_while_json_keeps_the_enum() {
        let registry = StepKindRegistry::new();
        let profiles = reg(vec![("fast", vec![local_model("m-fast", 8000)])], Some("fast"));
        let cfg = doc(vec![phase("p1", vec![task("t1", Some("role-a"), vec![step("s1", "k")])])]);
        let loaded = loaded_doc(cfg);
        let pctx = ctx(profiles);
        let show = build_show("m", &loaded, &registry, Ok(&pctx), &|_| RoleBinding::Unmapped, Ok(&[]), &[]);
        assert_eq!(show.phases[0].tasks[0].role.as_ref().unwrap().residency, "not_loaded");
        let text = render_show_text(&show);
        assert!(text.contains("· not loaded"), "got:\n{text}");
        assert!(!text.contains("not_loaded"), "got:\n{text}");
    }

    #[test]
    fn text_render_prints_warnings() {
        let registry = StepKindRegistry::new();
        let cfg = doc(vec![]);
        let loaded = loaded_doc(cfg);
        let warnings = vec!["--param coder=… ignored for planning parity".to_string()];
        let show = build_show(
            "m",
            &loaded,
            &registry,
            Err("n/a"),
            &|_| RoleBinding::Unmapped,
            Err("n/a"),
            &warnings,
        );
        let text = render_show_text(&show);
        assert!(text.contains("warning: --param coder=… ignored for planning parity"), "got:\n{text}");
    }

    #[test]
    fn text_render_shows_phase_description_and_task_display_name() {
        let registry = StepKindRegistry::new();
        let mut ph = phase("p1", vec![]);
        ph.description = Some("bundle, probe, dedup.".to_string());
        let mut t = task("t1", None, vec![step("s1", "k")]);
        t.display_name = Some("Bundle the diff".to_string());
        ph.tasks.push(t);
        let cfg = doc(vec![ph]);
        let loaded = loaded_doc(cfg);
        let show = build_show("m", &loaded, &registry, Err("n/a"), &|_| RoleBinding::Unmapped, Err("n/a"), &[]);
        let text = render_show_text(&show);
        assert!(text.contains("bundle, probe, dedup."), "phase description missing:\n{text}");
        assert!(text.contains("\"Bundle the diff\""), "task display_name missing:\n{text}");
    }

    #[test]
    fn text_render_groups_role_override_pseudo_inputs_into_one_line() {
        let registry = StepKindRegistry::new();
        let mut cfg = doc(vec![phase(
            "p1",
            vec![task("t1", Some("review-judge"), vec![step("s1", "k")])],
        )]);
        cfg.inputs = vec![
            MissionInput {
                name: "review-judge".to_string(),
                description: None,
                required: Some(false),
                ignored: None,
                ignored_reason: None,
                extras: Default::default(),
            },
            MissionInput {
                name: "diff_file".to_string(),
                description: None,
                required: Some(true),
                ignored: None,
                ignored_reason: None,
                extras: Default::default(),
            },
        ];
        let loaded = loaded_doc(cfg);
        let show = build_show("m", &loaded, &registry, Err("n/a"), &|_| RoleBinding::Unmapped, Err("n/a"), &[]);
        let text = render_show_text(&show);
        assert!(text.contains("role overrides: review-judge (optional)"), "got:\n{text}");
        assert!(!text.contains("    review-judge (optional)\n"), "must not ALSO list it individually:\n{text}");
        assert!(text.contains("diff_file (required)"), "an unrelated required input must render normally:\n{text}");
    }

    #[test]
    fn description_text_view_is_first_sentence_capped_and_marked() {
        // Short: verbatim, no marker.
        assert_eq!(first_sentence("Runs a thing."), "Runs a thing.");
        assert_eq!(first_sentence("no period at all"), "no period at all");
        // Two sentences: the first, marked.
        assert_eq!(
            first_sentence("Runs a thing. Then explains its whole history."),
            "Runs a thing. … (--json for the full text)"
        );
        // A `.` inside a path/version is not a sentence end.
        assert_eq!(first_sentence("Reads config.json v2.3 first. Then more."), "Reads config.json v2.3 first. … (--json for the full text)");
        // One 500-char sentence: capped on a word boundary, marked.
        let long = format!("{} end.", "word ".repeat(120).trim_end());
        let cut = first_sentence(&long);
        assert!(cut.ends_with("… (--json for the full text)"), "{cut}");
        assert!(cut.chars().count() <= DESCRIPTION_TEXT_CAP + 32, "{}", cut.chars().count());
        assert!(!cut.contains("wor …"), "cut on a word boundary: {cut}");
        // Char-safe on multibyte text.
        let uni = "é".repeat(400);
        assert!(first_sentence(&uni).ends_with("… (--json for the full text)"));
    }

    // ── --param parsing ───────────────────────────────────────────────

    #[test]
    fn param_parsing_rejects_missing_equals() {
        let err = parse_role_overrides(&["not-a-kv-pair".to_string()]).unwrap_err();
        assert!(err.to_string().contains("ROLE=PROFILE"));
    }

    #[test]
    fn param_parsing_accepts_role_equals_profile() {
        let map = parse_role_overrides(&["review-judge=review-mid".to_string()]).unwrap();
        assert_eq!(map.get("review-judge"), Some(&"review-mid".to_string()));
    }

    // ── binding precedence (merge-gate CONSIDER 4) ─────────────────────
    // Extracted so swapping the override/map/unmapped arms — which every
    // test above this point would have passed unnoticed while it lived as
    // an inline closure — now fails here, directly.

    #[test]
    fn binding_for_precedence_is_override_then_map_then_unmapped() {
        let mut overrides = BTreeMap::new();
        overrides.insert("r".to_string(), "ov".to_string());
        assert_eq!(
            binding_for("r", &overrides, Some("mp".to_string())),
            RoleBinding::Overridden("ov".to_string()),
            "an override present for this role must win even when a map binding also exists"
        );
        assert_eq!(
            binding_for("r2", &overrides, Some("mp".to_string())),
            RoleBinding::Mapped("mp".to_string()),
            "no override for this role → the map binding"
        );
        assert_eq!(
            binding_for("r2", &overrides, None),
            RoleBinding::Unmapped,
            "neither an override nor a map binding → Unmapped (falls to default_profile)"
        );
    }

    // ── launch-route parity for --param (merge-gate MUST-FIX 3) ────────
    // `mission launch` converts `--param <role>=<profile>` into a launch
    // binding ONLY on the review route. `show` must mirror that structural
    // gate, not apply the override universally and claim a parity that
    // doesn't hold for e.g. coder-phase.

    fn review_route_config() -> MissionConfig {
        doc(vec![phase(
            "p1",
            vec![task("review-judge-task", Some("review-judge"), vec![step("s1", "review.judge")])],
        )])
    }

    fn non_review_route_config() -> MissionConfig {
        doc(vec![phase(
            "p1",
            vec![task("build-coder", Some("coder"), vec![step("s1", "mission.coder")])],
        )])
    }

    #[test]
    fn param_overrides_apply_on_the_review_route() {
        let mut overrides = BTreeMap::new();
        overrides.insert("review-judge".to_string(), "deep".to_string());
        let (effective, warnings) = effective_overrides_and_warnings(&review_route_config(), overrides.clone());
        assert_eq!(effective, overrides, "review-route configs must apply the override unchanged");
        assert!(warnings.is_empty(), "a role the graph actually declares must not warn: {warnings:?}");
    }

    #[test]
    fn param_overrides_are_neutered_and_warned_off_the_non_review_route() {
        let mut overrides = BTreeMap::new();
        overrides.insert("coder".to_string(), "deep".to_string());
        let (effective, warnings) =
            effective_overrides_and_warnings(&non_review_route_config(), overrides);
        assert!(
            effective.is_empty(),
            "a non-review-route config's launcher ignores --param; show must not apply it either"
        );
        assert!(
            warnings.iter().any(|w| w.contains("--param") && w.contains("ignored")),
            "got: {warnings:?}"
        );
    }

    #[test]
    fn param_overrides_naming_an_undeclared_role_warn_on_the_review_route() {
        let mut overrides = BTreeMap::new();
        overrides.insert("review-judge".to_string(), "deep".to_string());
        overrides.insert("no-such-role".to_string(), "deep".to_string());
        let (effective, warnings) = effective_overrides_and_warnings(&review_route_config(), overrides.clone());
        assert_eq!(effective, overrides, "eligible overrides still apply even when a sibling is unconsumed");
        assert!(
            warnings.iter().any(|w| w.contains("no-such-role") && w.contains("no task")),
            "got: {warnings:?}"
        );
        assert!(
            !warnings.iter().any(|w| w.contains("review-judge") && w.contains("no task")),
            "a role the graph DOES declare must not be flagged unconsumed: {warnings:?}"
        );
    }

    #[test]
    fn no_overrides_supplied_produces_no_warnings_on_either_route() {
        let (effective, warnings) =
            effective_overrides_and_warnings(&non_review_route_config(), BTreeMap::new());
        assert!(effective.is_empty());
        assert!(warnings.is_empty(), "an empty --param set is never itself a warning: {warnings:?}");
    }

    // ── document id vs requested id (merge-gate CONSIDER 11) ───────────

    #[test]
    fn header_names_both_ids_when_a_user_tier_alias_shadows_the_document_id() {
        let registry = StepKindRegistry::new();
        let loaded = loaded_doc(doc(vec![])); // doc()'s own id is "m"
        let show = build_show("foo", &loaded, &registry, Err("n/a"), &|_| RoleBinding::Unmapped, Err("n/a"), &[]);
        assert_eq!(show.requested_id, "foo");
        assert_eq!(show.id, "m");
        let text = render_show_text(&show);
        assert!(text.contains("\"foo\" (document id \"m\")"), "got:\n{text}");
    }

    #[test]
    fn header_names_one_id_when_requested_matches_the_document() {
        let registry = StepKindRegistry::new();
        let loaded = loaded_doc(doc(vec![]));
        let show = build_show("m", &loaded, &registry, Err("n/a"), &|_| RoleBinding::Unmapped, Err("n/a"), &[]);
        let text = render_show_text(&show);
        assert!(text.starts_with("mission config \"m\" — M\n"), "got:\n{text}");
        assert!(!text.contains("document id"), "got:\n{text}");
    }

    // ── the graph-bearing embedded configs resolve cleanly end to end ──

    #[test]
    fn review_and_coder_phase_load_and_build_without_panicking() {
        let registry = crate::mission_launch::all_step_kinds().unwrap();
        for id in ["review", "coder-phase"] {
            let loaded = mission_config::load(id).unwrap();
            let show = build_show(id, &loaded, &registry, Err("no live profiles in this test"), &|_| RoleBinding::Unmapped, Err("no live lms in this test"), &[]);
            assert_eq!(show.id, id);
            assert!(!show.phases.is_empty());
        }
    }

    // (#2301) `crawl` is an ordinary graph-bearing config now — plan,
    // crawl, summarize, and (#2302) a create-mods phase that ships OFF — so
    // this asserts its whole shape rather than the "no graph by design"
    // inversion #1959 needed. Kept separate from the pair above because it
    // checks the per-rule track structure, not just that the document
    // builds.
    #[test]
    fn crawl_shows_four_phases_every_step_constructible_and_the_create_mod_task_disabled() {
        let registry = crate::mission_launch::all_step_kinds().unwrap();
        let loaded = mission_config::load("crawl").unwrap();
        let show = build_show(
            "crawl",
            &loaded,
            &registry,
            Err("no live profiles in this test"),
            &|_| RoleBinding::Unmapped,
            Err("no live lms in this test"),
            &[],
        );
        assert_eq!(show.id, "crawl");
        // (#2298 + #2301 + #2302) Four phases: one `crawl.plan` task per
        // built-in rule, one `crawl.unit` GROW template per rule, one
        // `crawl.summary`, and one `dispatch.internal` create-mod template.
        // Every step constructible by the launcher's own registry — a crawl
        // kind that failed to register would leave a config that cannot
        // execute, and that holds for create-mods too even though it
        // ships off: `enabled` gates the MINT, never whether the kind is
        // registered.
        let ids: Vec<&str> = show.phases.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["plan", "crawl", "summarize", "create-mods"], "{ids:?}");
        let kinds_of = |i: usize| -> Vec<&str> {
            show.phases[i].tasks.iter().flat_map(|t| t.steps.iter()).map(|s| s.kind.as_str()).collect()
        };
        assert_eq!(kinds_of(0), vec!["crawl.plan"; 4]);
        assert_eq!(kinds_of(1), vec!["crawl.unit"; 4]);
        assert_eq!(kinds_of(2), vec!["crawl.summary"]);
        assert_eq!(kinds_of(3), vec!["dispatch.internal"]);
        for phase in &show.phases {
            for step in phase.tasks.iter().flat_map(|t| t.steps.iter()) {
                assert!(step.constructible, "`{}` must be registered: step {}", step.kind, step.id);
            }
        }

        // (#2302) `show` REPORTS the gate. The create-mod task ships off, so it is
        // pruned at mint and must not read as live work here; every other
        // task in the document declares no gate at all and runs.
        assert_eq!(
            show.phases[3].tasks[0].enabled,
            Some(false),
            "the create-mod task ships disabled, and `show` says so"
        );
        for phase in &show.phases[..3] {
            for task in &phase.tasks {
                assert_eq!(task.enabled, None, "task `{}` declares no gate", task.id);
            }
        }

        // The same fact in the TEXT view, which is what an operator reads.
        let text = render_show_text(&show);
        assert!(
            text.contains("task create-mod") && text.contains("[disabled]"),
            "the text view marks the disabled task:\n{text}"
        );
    }

    #[test]
    fn panel_json_reflects_the_config_panel_block() {
        let mut cfg = doc(vec![]);
        cfg.panel = Some(PanelConfig {
            description: Some("desc".to_string()),
            hint: Some("hint".to_string()),
            extras: Default::default(),
        });
        let loaded = loaded_doc(cfg);
        let registry = StepKindRegistry::new();
        let show = build_show("m", &loaded, &registry, Err("n/a"), &|_| RoleBinding::Unmapped, Err("n/a"), &[]);
        let panel = show.panel.expect("panel must be Some");
        assert_eq!(panel.description.as_deref(), Some("desc"));
        assert_eq!(panel.hint.as_deref(), Some("hint"));
    }
}
