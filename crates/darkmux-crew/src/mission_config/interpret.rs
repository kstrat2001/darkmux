//! Config → executable graph interpreter (#1284 Packet 3, closes #1356's
//! promise structurally — see the module doc on `mission_config` and
//! `crates/darkmux-crew/src/mission_config/mod.rs`'s `ExpansionSpec`).
//!
//! [`interpret`] turns a parsed [`super::MissionConfig`] plus launch-time
//! [`LaunchParams`] into the `Vec<Task>` + `BTreeMap<String, Step>` shape
//! `darkmux-crew`'s `scheduler::run_step_graph` consumes — the SAME shape
//! `build_review_graph` (`darkmux-lab::lab::review`) and `default_phase_graph`
//! (`src/coder_phase.rs`) used to build by hand. This module owns exactly
//! three things, and only these three (per the packet's own scope — Tier 3
//! `StepKind` construction/registration stays mission-owned, #1352):
//!
//!   1. the placeholder-prefix phase-id substitution rule ([`TaskConfig`]'s
//!      doc in `mod.rs`) applied to every task/step id;
//!   2. `depends_on` edge resolution, INCLUDING rewriting a dependency that
//!      named a template task's id into the full set of that task's
//!      expanded real copies;
//!   3. the expansion primitive itself ([`super::ExpansionSpec`]) — turning
//!      one `TaskConfig` template into N real Task/Step copies, one per
//!      named item in a launcher-supplied collection.
//!
//! [`interpret`] does NOT construct `StepKind` instances, does NOT build a
//! `StepKindRegistry`, and does NOT know what any `Step.kind` id actually
//! DOES at runtime — a launcher calls [`interpret`], gets back real
//! `Task`/`Step` values, and separately registers its own Tier 3 kinds
//! against the SAME kind ids [`interpret`] produced (see
//! `darkmux_lab::lab::review::build_review_graph` and `coder_phase.rs`'s
//! `default_phase_graph` for the two production launchers).
//!
//! [`interpret`] assumes the config is well-formed (document-wide-unique
//! ids, no dangling `depends_on`) — the SAME assumption every other
//! `mission_config` consumer makes; semantic validation is
//! [`super::MissionConfig::validate`]'s separate job (contract 7), never
//! re-run here. A malformed config produces an `Err` from a dangling
//! `depends_on` reference (defensive, not a substitute for `validate`).

use super::{MissionConfig, StepConfig};
use crate::types::{NodeStatus, Step, Task};
use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Per-task overrides a launcher supplies at interpretation time, keyed by
/// the [`TaskConfig`]'s OWN (pre-substitution, pre-expansion) `id` — stable
/// across launches since it's the literal string in the document, unlike
/// the composed real id (which depends on the launcher's own real phase
/// id). Every field defaults to "keep the document's own value" — a
/// launcher only sets what it actually needs to override.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskOverride {
    pub role_id: Option<String>,
    pub profile_name: Option<String>,
    pub workdir: Option<PathBuf>,
    pub image: Option<String>,
    /// Overrides the whole rendered description (e.g. the coder-phase
    /// launcher's `dispatch \`{role}\` into the worktree`, where `{role}`
    /// is only known at launch).
    pub description: Option<String>,
    /// (#1398) Overrides `TaskConfig::display_name` — parity with
    /// `description`'s override above, for a launcher that wants to name a
    /// task differently than its static document default at launch time.
    pub display_name: Option<String>,
}

/// Everything a launcher supplies to [`interpret`] beyond the static
/// document — every genuinely per-launch value [`super::MissionConfig::inputs`]
/// documents the config as needing from its caller.
#[derive(Debug, Clone, Default)]
pub struct LaunchParams {
    /// `PhaseConfig.id` (the document's own id, possibly a placeholder
    /// prefix) → the launcher's REAL composed phase id. A phase absent
    /// from this map keeps its document id verbatim — the common case for
    /// a config whose phase id already IS what the launcher wants.
    pub phase_ids: BTreeMap<String, String>,
    /// `TaskConfig.id` → override — see [`TaskOverride`].
    pub task_overrides: BTreeMap<String, TaskOverride>,
    /// `StepConfig.id` → replacement `config` value. REPLACES, never
    /// merges, the document's own `config` — e.g. the review launcher
    /// overriding `review-judge-step`'s `concurrency` with the operator's
    /// resolved `config_access::review_judge_concurrency()`, never the
    /// document's own static default (see `CLAUDE.md`'s judge_concurrency
    /// decision for why the static JSON value is a documented default
    /// only, never load-bearing at launch).
    pub step_config_overrides: BTreeMap<String, serde_json::Value>,
    /// [`super::ExpansionSpec::over`] → the ordered item NAMES the launcher
    /// resolved for that collection (e.g. every staffed probe seat's name,
    /// in staffing order). A task whose `expand.over` names a collection
    /// missing from this map interprets as an EMPTY expansion (zero real
    /// copies) — the same "reduced coverage, not a hard error" posture a
    /// zero-seat crew already takes elsewhere in the review pipeline.
    ///
    /// (#1418) That leniency is for a genuinely EMPTY collection under a
    /// KEY THE LAUNCHER DOES SUPPLY (e.g. a zero-seat crew still inserts
    /// `"probe_seats" -> []`). A key ABSENT from this map entirely
    /// (nothing under that name at all) is a different, likelier-a-typo
    /// shape: [`interpret`] treats both as zero real copies (leniency is
    /// unchanged), but names the absent case in its returned warnings so
    /// the launcher can surface it instead of the run silently examining
    /// nothing.
    pub expansions: BTreeMap<String, Vec<String>>,
}

/// What [`interpret`] returns: the real `Vec<Task>` (document order,
/// expanded tasks appearing in place of their template) + `BTreeMap<String,
/// Step>` a `StepKindRegistry`-equipped caller hands to
/// `scheduler::run_step_graph`, + (#1418) any non-fatal warnings worth the
/// caller printing, currently just the absent-`expand.over`-key case (see
/// [`LaunchParams::expansions`]'s doc). Warnings are empty on a normal
/// interpretation; `interpret` has no printing/logging channel of its own,
/// so returning them is the smallest honest mechanism for a caller to
/// surface them.
pub type InterpretedGraph = (Vec<Task>, BTreeMap<String, Step>, Vec<String>);

/// See the module doc and [`InterpretedGraph`].
pub fn interpret(config: &MissionConfig, params: &LaunchParams) -> Result<InterpretedGraph> {
    check_params_reference_the_document(config, params)?;

    let mut tasks: Vec<Task> = Vec::new();
    let mut steps: BTreeMap<String, Step> = BTreeMap::new();
    let mut warnings: Vec<String> = Vec::new();
    // Document-level TaskConfig.id -> the real Task id(s) it produced (len
    // 1 for a non-expanding task, len N for an expanding one). Drives BOTH
    // the depends_on rewrite pass below AND is how a dependent task's
    // reference to a template resolves to every real expanded copy.
    let mut expansion_of: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for phase in &config.phases {
        let real_phase_id = substitute_phase_id(&phase.id, params);
        for task_cfg in &phase.tasks {
            let override_ = params.task_overrides.get(&task_cfg.id);
            match &task_cfg.expand {
                Some(spec) => {
                    // (#1284 review round 2) A multi-step template would
                    // silently drop every step after the first — refuse it
                    // loudly instead. `validate()` flags the same document
                    // shape ahead of time; this is the interpret-side guard
                    // for a caller that skipped validation.
                    if task_cfg.steps.len() > 1 {
                        bail!(
                            "task `{}` declares `expand` with {} steps — an expanding \
                             template task must have exactly ONE step (the expansion \
                             primitive clones one task/step pair per item; extra steps \
                             would be silently dropped)",
                            task_cfg.id,
                            task_cfg.steps.len()
                        );
                    }
                    let step_cfg = task_cfg.steps.first().ok_or_else(|| {
                        anyhow::anyhow!(
                            "task `{}` declares `expand` but has no steps to expand",
                            task_cfg.id
                        )
                    })?;
                    // (#1418) Absent vs empty: a key genuinely present with
                    // an empty Vec (a zero-seat crew, e.g.) stays silent;
                    // that leniency is deliberate and pinned by
                    // `zero_items_in_the_expansion_collection_produces_zero_
                    // real_copies_and_an_empty_dependency` below. A key with
                    // NO entry at all in `params.expansions` is a likelier
                    // config typo (the launcher meant to supply a collection
                    // under this exact name and didn't): same lenient
                    // zero-copy behavior, but named in `warnings` so the
                    // caller can print it instead of the run silently
                    // examining nothing.
                    let items = match params.expansions.get(&spec.over) {
                        Some(items) => items.clone(),
                        None => {
                            warnings.push(format!(
                                "task `{}` declares `expand.over: \"{}\"`, which has no \
                                 matching entry in the launcher's supplied expansions; \
                                 treated as zero real copies (leniency preserved), but an \
                                 absent key usually means a typo; a genuinely empty \
                                 collection is supplied as an explicit empty list",
                                task_cfg.id, spec.over
                            ));
                            Vec::new()
                        }
                    };
                    let mut real_ids = Vec::with_capacity(items.len());
                    for (index, name) in items.iter().enumerate() {
                        let real_task_id = render(&spec.task_id_pattern, index, name);
                        let real_step_id = render(&spec.step_id_pattern, index, name);
                        let real_kind = render_kind(&spec.kind_pattern, &step_cfg.kind, index, name);
                        let description = spec
                            .description_pattern
                            .as_deref()
                            .map(|p| render(p, index, name))
                            .or_else(|| task_cfg.description.clone())
                            .unwrap_or_default();
                        // (#1398) Same optional-pattern-with-unrendered-fallback
                        // shape as `description_pattern` above — an expanding
                        // template usually doesn't need a per-copy label
                        // (`display_name_pattern` absent falls back to the
                        // template's own `display_name` verbatim, unrendered).
                        let display_name = spec
                            .display_name_pattern
                            .as_deref()
                            .map(|p| render(p, index, name))
                            .or_else(|| task_cfg.display_name.clone());
                        // (#1475 packet 2) Per-copy role_id: `role_pattern`
                        // renders one DISTINCT role per expanded copy (the
                        // review probe stage binds copy 0 → review-probe-high,
                        // etc.); absent falls back to the template's single
                        // `role_id` (the pre-1.3 shared-role behavior).
                        let copy_role_id = spec
                            .role_pattern
                            .as_deref()
                            .map(|p| render(p, index, name))
                            .or_else(|| task_cfg.role_id.clone());
                        push_task(
                            &mut tasks,
                            &real_task_id,
                            &real_phase_id,
                            description,
                            display_name,
                            vec![real_step_id.clone()],
                            override_,
                            copy_role_id.as_deref(),
                        )?;
                        push_step(
                            &mut steps,
                            &real_step_id,
                            &real_task_id,
                            &real_kind,
                            step_config_for(step_cfg, params),
                        )?;
                        real_ids.push(real_task_id);
                    }
                    expansion_of.insert(task_cfg.id.clone(), real_ids);
                }
                None => {
                    let real_task_id = substitute_id(&task_cfg.id, &phase.id, &real_phase_id);
                    let mut step_ids = Vec::with_capacity(task_cfg.steps.len());
                    for step_cfg in &task_cfg.steps {
                        let real_step_id = substitute_id(&step_cfg.id, &phase.id, &real_phase_id);
                        step_ids.push(real_step_id.clone());
                        push_step(
                            &mut steps,
                            &real_step_id,
                            &real_task_id,
                            &step_cfg.kind,
                            step_config_for(step_cfg, params),
                        )?;
                    }
                    let description =
                        task_cfg.description.clone().unwrap_or_default();
                    push_task(
                        &mut tasks,
                        &real_task_id,
                        &real_phase_id,
                        description,
                        task_cfg.display_name.clone(),
                        step_ids,
                        override_,
                        task_cfg.role_id.as_deref(),
                    )?;
                    expansion_of.insert(task_cfg.id.clone(), vec![real_task_id]);
                }
            }
        }
    }

    // Second pass: resolve every TaskConfig's `depends_on` now that
    // `expansion_of` is complete for the WHOLE document — a dependency
    // that named a template task's id resolves to ALL of that template's
    // real expanded copies (dedup depending on every real probe task,
    // never just the template's single placeholder id).
    let real_index: BTreeMap<String, usize> =
        tasks.iter().enumerate().map(|(i, t)| (t.id.clone(), i)).collect();
    for phase in &config.phases {
        for task_cfg in &phase.tasks {
            let mut resolved_depends: Vec<String> = Vec::new();
            for dep in &task_cfg.depends_on {
                match expansion_of.get(dep) {
                    Some(real_ids) => resolved_depends.extend(real_ids.iter().cloned()),
                    None => bail!(
                        "task `{}` depends_on unknown task id `{dep}` — the config must \
                         validate cleanly (MissionConfig::validate) before interpretation",
                        task_cfg.id
                    ),
                }
            }
            if resolved_depends.is_empty() {
                continue;
            }
            if let Some(real_ids) = expansion_of.get(&task_cfg.id) {
                for real_id in real_ids {
                    if let Some(&idx) = real_index.get(real_id.as_str()) {
                        tasks[idx].depends_on = resolved_depends.clone();
                    }
                }
            }
        }
    }

    Ok((tasks, steps, warnings))
}

/// (#1284 review round 2, consider 4) Every launcher-supplied key must
/// reference something that actually EXISTS in the document — a typo'd key
/// silently matching nothing is worse than an error, because the launch
/// proceeds with the document's static value instead of the operator's
/// resolved one. The concrete hazard that motivated this: the
/// `"review-judge-step"` literal lives UNLINKED in both `review.rs` (the
/// launcher's `step_config_overrides` key) and `review.json` (the step id)
/// — if either side drifts, the judge's concurrency silently reverts to
/// the document's static `1`, discarding the operator's
/// `config.review.judge_concurrency`. `interpret` holds the whole document,
/// so the cross-check is cheap; drift now fails the launch loudly, naming
/// the dangling key.
fn check_params_reference_the_document(config: &MissionConfig, params: &LaunchParams) -> Result<()> {
    let phase_ids: std::collections::BTreeSet<&str> =
        config.phases.iter().map(|p| p.id.as_str()).collect();
    let mut task_ids = std::collections::BTreeSet::new();
    let mut step_ids = std::collections::BTreeSet::new();
    let mut expansion_names = std::collections::BTreeSet::new();
    for phase in &config.phases {
        for task_cfg in &phase.tasks {
            task_ids.insert(task_cfg.id.as_str());
            for step_cfg in &task_cfg.steps {
                step_ids.insert(step_cfg.id.as_str());
            }
            if let Some(spec) = &task_cfg.expand {
                expansion_names.insert(spec.over.as_str());
            }
        }
    }

    for key in params.phase_ids.keys() {
        if !phase_ids.contains(key.as_str()) {
            bail!(
                "LaunchParams.phase_ids names phase id `{key}`, which does not exist in \
                 mission config `{}` — a dangling key would silently leave the document's \
                 placeholder ids in persisted artifacts",
                config.id
            );
        }
    }
    for key in params.task_overrides.keys() {
        if !task_ids.contains(key.as_str()) {
            bail!(
                "LaunchParams.task_overrides names task id `{key}`, which does not exist in \
                 mission config `{}` — a dangling key would silently discard the launcher's \
                 override",
                config.id
            );
        }
    }
    for key in params.step_config_overrides.keys() {
        if !step_ids.contains(key.as_str()) {
            bail!(
                "LaunchParams.step_config_overrides names step id `{key}`, which does not \
                 exist in mission config `{}` — a dangling key would silently discard the \
                 launcher's override and run with the document's static step config",
                config.id
            );
        }
    }
    for key in params.expansions.keys() {
        if !expansion_names.contains(key.as_str()) {
            bail!(
                "LaunchParams.expansions names collection `{key}`, which no task's \
                 `expand.over` in mission config `{}` declares — a dangling key would \
                 silently expand nothing",
                config.id
            );
        }
    }
    Ok(())
}

fn substitute_phase_id(doc_phase_id: &str, params: &LaunchParams) -> String {
    params
        .phase_ids
        .get(doc_phase_id)
        .cloned()
        .unwrap_or_else(|| doc_phase_id.to_string())
}

/// The placeholder-prefix rule (`TaskConfig`'s doc in `mod.rs`): if `id` is
/// literally prefixed by `"<doc_phase_id>-"`, replace that PREFIX with
/// `"<real_phase_id>-"`, keeping everything after it unchanged. An id with
/// no such prefix (the FIXED-id convention, e.g. `build_review_graph`'s
/// `review-bundle-task`) passes through verbatim. Requires the literal `-`
/// separator (not just any shared prefix) so a phase id like `"build"`
/// doesn't accidentally match an unrelated id like `"buildup-task"`.
fn substitute_id(id: &str, doc_phase_id: &str, real_phase_id: &str) -> String {
    if real_phase_id == doc_phase_id {
        return id.to_string();
    }
    let prefix = format!("{doc_phase_id}-");
    match id.strip_prefix(prefix.as_str()) {
        Some(rest) => format!("{real_phase_id}-{rest}"),
        None => id.to_string(),
    }
}

fn render(pattern: &str, index: usize, name: &str) -> String {
    pattern.replace("{index}", &index.to_string()).replace("{name}", name)
}

fn render_kind(pattern: &str, template_kind: &str, index: usize, name: &str) -> String {
    pattern
        .replace("{kind}", template_kind)
        .replace("{index}", &index.to_string())
        .replace("{name}", name)
}

fn step_config_for(step_cfg: &StepConfig, params: &LaunchParams) -> serde_json::Value {
    params
        .step_config_overrides
        .get(&step_cfg.id)
        .cloned()
        .unwrap_or_else(|| step_cfg.config.clone())
}

#[allow(clippy::too_many_arguments)]
fn push_task(
    tasks: &mut Vec<Task>,
    real_task_id: &str,
    real_phase_id: &str,
    description: String,
    display_name: Option<String>,
    step_ids: Vec<String>,
    override_: Option<&TaskOverride>,
    doc_role_id: Option<&str>,
) -> Result<()> {
    // (#1284 review round 2, consider 5) Post-substitution/expansion FINAL
    // ids must be unique — an expansion pattern with neither `{index}` nor
    // `{name}` (or a substitution collision) would otherwise produce
    // same-id copies, and only this collision check catches the rendered
    // (post-pattern) form. `Vec::contains` over a graph-sized Vec is fine —
    // graphs here are tens of tasks, not thousands.
    if tasks.iter().any(|t| t.id == real_task_id) {
        bail!(
            "interpreted graph produced duplicate task id `{real_task_id}` — expansion \
             patterns must render a distinct id per item (include `{{index}}` or `{{name}}`)"
        );
    }
    let role_id = override_
        .and_then(|o| o.role_id.clone())
        .or_else(|| doc_role_id.map(String::from));
    let description = override_.and_then(|o| o.description.clone()).unwrap_or(description);
    let display_name = override_.and_then(|o| o.display_name.clone()).or(display_name);
    let profile_name = override_.and_then(|o| o.profile_name.clone());
    let workdir = override_.and_then(|o| o.workdir.clone());
    let image = override_.and_then(|o| o.image.clone());
    tasks.push(Task {
        id: real_task_id.to_string(),
        phase_id: real_phase_id.to_string(),
        description,
        display_name,
        step_ids,
        depends_on: Vec::new(),
        role_id,
        profile_name,
        workdir,
        image,
    });
    Ok(())
}

fn push_step(
    steps: &mut BTreeMap<String, Step>,
    real_step_id: &str,
    real_task_id: &str,
    kind: &str,
    config: serde_json::Value,
) -> Result<()> {
    let step = Step {
        id: real_step_id.to_string(),
        task_id: real_task_id.to_string(),
        kind: kind.to_string(),
        status: NodeStatus::Planned,
        config,
        started_ts: None,
        completed_ts: None,
        output: None,
    };
    // (#1284 review round 2, consider 5) The steps BTreeMap would silently
    // keep exactly one of two same-id steps — detect the collision on the
    // rendered final id instead (see `push_task`'s twin check).
    if steps.insert(real_step_id.to_string(), step).is_some() {
        bail!(
            "interpreted graph produced duplicate step id `{real_step_id}` — expansion \
             patterns must render a distinct id per item (include `{{index}}` or `{{name}}`)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission_config::{ExpansionSpec, PhaseConfig, TaskConfig};
    use std::collections::BTreeMap as Map;

    fn step(id: &str, kind: &str, config: serde_json::Value) -> StepConfig {
        StepConfig { id: id.to_string(), kind: kind.to_string(), config, extras: Map::new() }
    }

    fn task(id: &str, depends_on: &[&str], role_id: Option<&str>, steps: Vec<StepConfig>) -> TaskConfig {
        TaskConfig {
            id: id.to_string(),
            description: Some(format!("do {id}")),
            display_name: None,
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            role_id: role_id.map(String::from),
            steps,
            expand: None,
            extras: Map::new(),
        }
    }

    fn phase(id: &str, tasks: Vec<TaskConfig>) -> PhaseConfig {
        PhaseConfig { id: id.to_string(), description: None, display_name: None, tasks, extras: Map::new() }
    }

    fn doc(phases: Vec<PhaseConfig>) -> MissionConfig {
        MissionConfig {
            id: "m".to_string(),
            name: "M".to_string(),
            description: None,
            schema_version: None,
            inputs: Vec::new(),
            phases,
            extras: Map::new(),
        }
    }

    #[test]
    fn fixed_ids_pass_through_verbatim_when_phase_id_differs() {
        // Mirrors review.json's convention — task/step ids never carry the
        // phase-config id as a prefix, so substitution is a no-op even
        // though the launcher supplies a DIFFERENT real phase id.
        let cfg = doc(vec![phase(
            "investigate",
            vec![task("review-bundle-task", &[], None, vec![step("review-bundle-step", "review.bundle", serde_json::Value::Null)])],
        )]);
        let mut phase_ids = Map::new();
        phase_ids.insert("investigate".to_string(), "pr-review-123-investigate".to_string());
        let params = LaunchParams { phase_ids, ..Default::default() };

        let (tasks, steps, _warnings) = interpret(&cfg, &params).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "review-bundle-task", "fixed ids stay verbatim");
        assert_eq!(tasks[0].phase_id, "pr-review-123-investigate", "Task.phase_id is still the REAL phase id");
        assert_eq!(steps["review-bundle-step"].kind, "review.bundle");
    }

    #[test]
    fn placeholder_prefix_ids_substitute_the_real_phase_id() {
        // Mirrors coder-phase.json's convention — `build-coder` prefixed by
        // the phase-config id `build`.
        let cfg = doc(vec![phase(
            "build",
            vec![task("build-coder", &[], Some("coder"), vec![step("build-coder-step", "mission.coder", serde_json::Value::Null)])],
        )]);
        let mut phase_ids = Map::new();
        phase_ids.insert("build".to_string(), "s1".to_string());
        let params = LaunchParams { phase_ids, ..Default::default() };

        let (tasks, steps, _warnings) = interpret(&cfg, &params).unwrap();
        assert_eq!(tasks[0].id, "s1-coder");
        assert_eq!(tasks[0].phase_id, "s1");
        assert!(steps.contains_key("s1-coder-step"));
    }

    #[test]
    fn task_overrides_apply_role_workdir_image_and_description() {
        let cfg = doc(vec![phase(
            "build",
            vec![task("build-coder", &[], Some("coder"), vec![step("build-coder-step", "mission.coder", serde_json::Value::Null)])],
        )]);
        let mut phase_ids = Map::new();
        phase_ids.insert("build".to_string(), "s1".to_string());
        let mut task_overrides = Map::new();
        task_overrides.insert(
            "build-coder".to_string(),
            TaskOverride {
                role_id: Some("reviewer-bot".to_string()),
                workdir: Some(PathBuf::from("/tmp/wt")),
                image: Some("rust:slim".to_string()),
                description: Some("dispatch `reviewer-bot` into the worktree".to_string()),
                ..Default::default()
            },
        );
        let params = LaunchParams { phase_ids, task_overrides, ..Default::default() };

        let (tasks, _steps, _warnings) = interpret(&cfg, &params).unwrap();
        assert_eq!(tasks[0].role_id.as_deref(), Some("reviewer-bot"));
        assert_eq!(tasks[0].workdir, Some(PathBuf::from("/tmp/wt")));
        assert_eq!(tasks[0].image.as_deref(), Some("rust:slim"));
        assert_eq!(tasks[0].description, "dispatch `reviewer-bot` into the worktree");
    }

    #[test]
    fn doc_role_id_survives_when_no_override_is_supplied() {
        let cfg = doc(vec![phase(
            "build",
            vec![task("build-verify", &["build-coder"], Some("code-reviewer"), vec![step("build-verify-step", "mission.verify", serde_json::Value::Null)]),
                 task("build-coder", &[], Some("coder"), vec![step("build-coder-step", "mission.coder", serde_json::Value::Null)])],
        )]);
        let mut phase_ids = Map::new();
        phase_ids.insert("build".to_string(), "s1".to_string());
        let params = LaunchParams { phase_ids, ..Default::default() };

        let (tasks, _steps, _warnings) = interpret(&cfg, &params).unwrap();
        let verify = tasks.iter().find(|t| t.id == "s1-verify").unwrap();
        assert_eq!(verify.role_id.as_deref(), Some("code-reviewer"));
    }

    // ── (#1398) display_name threading ──────────────────────────────────

    #[test]
    fn task_display_name_survives_from_the_document_when_no_override_is_supplied() {
        let mut t = task("build-coder", &[], Some("coder"), vec![step("build-coder-step", "mission.coder", serde_json::Value::Null)]);
        t.display_name = Some("Build".to_string());
        let cfg = doc(vec![phase("build", vec![t])]);
        let mut phase_ids = Map::new();
        phase_ids.insert("build".to_string(), "s1".to_string());
        let params = LaunchParams { phase_ids, ..Default::default() };

        let (tasks, _steps, _warnings) = interpret(&cfg, &params).unwrap();
        assert_eq!(tasks[0].display_name.as_deref(), Some("Build"));
    }

    #[test]
    fn task_display_name_is_none_when_the_document_does_not_set_one() {
        let cfg = doc(vec![phase(
            "build",
            vec![task("build-coder", &[], Some("coder"), vec![step("build-coder-step", "mission.coder", serde_json::Value::Null)])],
        )]);
        let mut phase_ids = Map::new();
        phase_ids.insert("build".to_string(), "s1".to_string());
        let params = LaunchParams { phase_ids, ..Default::default() };

        let (tasks, _steps, _warnings) = interpret(&cfg, &params).unwrap();
        assert_eq!(tasks[0].display_name, None, "no display_name in the doc -> None, renderers fall back to id");
    }

    #[test]
    fn task_override_display_name_wins_over_the_document_default() {
        let mut t = task("build-coder", &[], Some("coder"), vec![step("build-coder-step", "mission.coder", serde_json::Value::Null)]);
        t.display_name = Some("Build".to_string());
        let cfg = doc(vec![phase("build", vec![t])]);
        let mut phase_ids = Map::new();
        phase_ids.insert("build".to_string(), "s1".to_string());
        let mut task_overrides = Map::new();
        task_overrides.insert(
            "build-coder".to_string(),
            TaskOverride { display_name: Some("Ship it".to_string()), ..Default::default() },
        );
        let params = LaunchParams { phase_ids, task_overrides, ..Default::default() };

        let (tasks, _steps, _warnings) = interpret(&cfg, &params).unwrap();
        assert_eq!(tasks[0].display_name.as_deref(), Some("Ship it"));
    }

    #[test]
    fn step_config_override_replaces_the_document_default() {
        let cfg = doc(vec![phase(
            "adjudicate",
            vec![task(
                "review-judge-task",
                &[],
                None,
                vec![step("review-judge-step", "review.judge", serde_json::json!({"concurrency": 1}))],
            )],
        )]);
        let mut step_config_overrides = Map::new();
        step_config_overrides.insert("review-judge-step".to_string(), serde_json::json!({"concurrency": 5}));
        let params = LaunchParams { step_config_overrides, ..Default::default() };

        let (_tasks, steps, _warnings) = interpret(&cfg, &params).unwrap();
        assert_eq!(steps["review-judge-step"].config, serde_json::json!({"concurrency": 5}));
    }

    #[test]
    fn step_config_falls_back_to_document_default_when_not_overridden() {
        let cfg = doc(vec![phase(
            "adjudicate",
            vec![task(
                "review-judge-task",
                &[],
                None,
                vec![step("review-judge-step", "review.judge", serde_json::json!({"concurrency": 1}))],
            )],
        )]);
        let params = LaunchParams::default();

        let (_tasks, steps, _warnings) = interpret(&cfg, &params).unwrap();
        assert_eq!(steps["review-judge-step"].config, serde_json::json!({"concurrency": 1}));
    }

    /// (#1475 packet 2) `role_pattern` renders a DISTINCT `role_id` per
    /// expanded copy from the item — the mechanism the review probe stage uses
    /// to bind copy 0 → review-probe-high, copy 1 → review-probe-mid, etc.
    /// Absent, every copy shares the template's single `role_id`.
    #[test]
    fn expansion_role_pattern_renders_a_distinct_role_per_copy() {
        let cfg = doc(vec![phase(
            "investigate",
            vec![TaskConfig {
                id: "probe-template".to_string(),
                description: None,
                display_name: None,
                depends_on: vec![],
                role_id: Some("fallback-role".to_string()),
                steps: vec![step("probe-template-step", "dispatch.map", serde_json::Value::Null)],
                expand: Some(ExpansionSpec {
                    over: "probe_roles".to_string(),
                    task_id_pattern: "probe-{index}-task".to_string(),
                    step_id_pattern: "probe-{index}-step".to_string(),
                    kind_pattern: "{kind}".to_string(),
                    description_pattern: None,
                    display_name_pattern: None,
                    role_pattern: Some("{name}".to_string()),
                    extras: Map::new(),
                }),
                extras: Map::new(),
            }],
        )]);
        let mut phase_ids = Map::new();
        phase_ids.insert("investigate".to_string(), "investigate".to_string());
        let mut expansions = Map::new();
        expansions.insert(
            "probe_roles".to_string(),
            vec!["review-probe-high".to_string(), "review-probe-mid".to_string()],
        );
        let params = LaunchParams { phase_ids, expansions, ..Default::default() };
        let (tasks, _steps, _warnings) = interpret(&cfg, &params).unwrap();
        let by_id: BTreeMap<&str, &Task> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();
        assert_eq!(by_id["probe-0-task"].role_id.as_deref(), Some("review-probe-high"));
        assert_eq!(by_id["probe-1-task"].role_id.as_deref(), Some("review-probe-mid"));
    }

    /// The expansion primitive's central case — a template task expanding
    /// into N real copies, one per staffed seat, AND a dependent task's
    /// `depends_on` rewriting from the template id to every real expanded
    /// id. Mirrors review.json's probe stage exactly.
    #[test]
    fn expansion_primitive_produces_one_copy_per_item_and_rewrites_dependents() {
        let cfg = doc(vec![phase(
            "investigate",
            vec![
                task("review-bundle-task", &[], None, vec![step("review-bundle-step", "review.bundle", serde_json::Value::Null)]),
                TaskConfig {
                    id: "review-probe-template-task".to_string(),
                    description: Some("PLACEHOLDER".to_string()),
                    display_name: None,
                    depends_on: vec!["review-bundle-task".to_string()],
                    role_id: None,
                    steps: vec![step("review-probe-template-step", "review.probe", serde_json::Value::Null)],
                    expand: Some(ExpansionSpec {
                        over: "probe_seats".to_string(),
                        task_id_pattern: "review-probe-{index}-task".to_string(),
                        step_id_pattern: "review-probe-{index}-step".to_string(),
                        kind_pattern: "{kind}:{name}".to_string(),
                        description_pattern: Some("probe seat `{name}`".to_string()),
                        display_name_pattern: Some("Probe `{name}`".to_string()),
                        role_pattern: None,
                        extras: Map::new(),
                    }),
                    extras: Map::new(),
                },
                task(
                    "review-dedup-task",
                    &["review-probe-template-task"],
                    None,
                    vec![step("review-dedup-step", "review.dedup", serde_json::Value::Null)],
                ),
            ],
        )]);
        let mut phase_ids = Map::new();
        phase_ids.insert("investigate".to_string(), "investigate".to_string());
        let mut expansions = Map::new();
        expansions.insert("probe_seats".to_string(), vec!["alpha".to_string(), "bravo".to_string()]);
        let params = LaunchParams { phase_ids, expansions, ..Default::default() };

        let (tasks, steps, _warnings) = interpret(&cfg, &params).unwrap();

        // bundle + 2 expanded probes + dedup = 4 real tasks.
        assert_eq!(tasks.len(), 4);
        let by_id: BTreeMap<&str, &Task> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();
        assert!(by_id.contains_key("review-probe-0-task"));
        assert!(by_id.contains_key("review-probe-1-task"));
        assert_eq!(by_id["review-probe-0-task"].description, "probe seat `alpha`");
        assert_eq!(by_id["review-probe-1-task"].description, "probe seat `bravo`");
        assert_eq!(by_id["review-probe-0-task"].depends_on, vec!["review-bundle-task".to_string()]);
        assert_eq!(by_id["review-probe-1-task"].depends_on, vec!["review-bundle-task".to_string()]);

        assert_eq!(steps["review-probe-0-step"].kind, "review.probe:alpha");
        assert_eq!(steps["review-probe-1-step"].kind, "review.probe:bravo");

        // (#1398) `display_name_pattern` renders per-copy, same as
        // `description_pattern` does.
        assert_eq!(by_id["review-probe-0-task"].display_name.as_deref(), Some("Probe `alpha`"));
        assert_eq!(by_id["review-probe-1-task"].display_name.as_deref(), Some("Probe `bravo`"));

        // the dedup task's depends_on rewrote from the SINGLE template id
        // to BOTH real expanded probe task ids.
        assert_eq!(
            by_id["review-dedup-task"].depends_on,
            vec!["review-probe-0-task".to_string(), "review-probe-1-task".to_string()]
        );
    }

    #[test]
    fn zero_items_in_the_expansion_collection_produces_zero_real_copies_and_an_empty_dependency() {
        let cfg = doc(vec![phase(
            "investigate",
            vec![
                TaskConfig {
                    id: "review-probe-template-task".to_string(),
                    description: None,
                    display_name: None,
                    depends_on: vec![],
                    role_id: None,
                    steps: vec![step("review-probe-template-step", "review.probe", serde_json::Value::Null)],
                    expand: Some(ExpansionSpec {
                        over: "probe_seats".to_string(),
                        task_id_pattern: "review-probe-{index}-task".to_string(),
                        step_id_pattern: "review-probe-{index}-step".to_string(),
                        kind_pattern: "{kind}:{name}".to_string(),
                        description_pattern: None,
                        display_name_pattern: None,
                        role_pattern: None,
                        extras: Map::new(),
                    }),
                    extras: Map::new(),
                },
                task(
                    "review-dedup-task",
                    &["review-probe-template-task"],
                    None,
                    vec![step("review-dedup-step", "review.dedup", serde_json::Value::Null)],
                ),
            ],
        )]);
        // NO "probe_seats" entry in expansions at all -- an ABSENT key,
        // which (#1418) is exactly the case `interpret` now also names in
        // its returned `warnings` (see the sibling test right below). The
        // ZERO-COPY behavior pinned here is unchanged, only the silence is
        // gone.
        let params = LaunchParams::default();

        let (tasks, _steps, _warnings) = interpret(&cfg, &params).unwrap();
        let by_id: BTreeMap<&str, &Task> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();
        assert!(!by_id.contains_key("review-probe-0-task"), "zero seats -> zero expanded copies");
        assert!(by_id["review-dedup-task"].depends_on.is_empty(), "an empty expansion resolves to an empty dependency, not a dangling reference");
    }

    /// (#1418) The sibling of the pinned test above: an ABSENT
    /// `expand.over` key (nothing under that name in `params.expansions`
    /// at all) surfaces a warning naming the task and the missing key,
    /// while keeping the SAME lenient zero-real-copies behavior. Leniency
    /// stays, silence goes.
    #[test]
    fn absent_expand_over_key_surfaces_a_warning_but_stays_lenient() {
        let cfg = doc(vec![phase(
            "investigate",
            vec![TaskConfig {
                id: "review-probe-template-task".to_string(),
                description: None,
                display_name: None,
                depends_on: vec![],
                role_id: None,
                steps: vec![step("review-probe-template-step", "review.probe", serde_json::Value::Null)],
                expand: Some(ExpansionSpec {
                    over: "probe_seats".to_string(),
                    task_id_pattern: "review-probe-{index}-task".to_string(),
                    step_id_pattern: "review-probe-{index}-step".to_string(),
                    kind_pattern: "{kind}:{name}".to_string(),
                    description_pattern: None,
                    display_name_pattern: None,
                    role_pattern: None,
                    extras: Map::new(),
                }),
                extras: Map::new(),
            }],
        )]);
        // NO "probe_seats" entry at all -- the ABSENT-key path.
        let params = LaunchParams::default();

        let (tasks, _steps, warnings) = interpret(&cfg, &params).unwrap();
        assert!(tasks.is_empty(), "an absent key still produces zero real copies (leniency preserved)");
        assert_eq!(warnings.len(), 1, "the absent key names exactly one warning: {warnings:?}");
        assert!(warnings[0].contains("review-probe-template-task"), "{}", warnings[0]);
        assert!(warnings[0].contains("probe_seats"), "{}", warnings[0]);
    }

    /// (#1418) The other half of the distinction: a key that IS present,
    /// mapped to a genuinely empty `Vec` (e.g. a real zero-seat crew),
    /// produces the SAME zero-real-copies result but NO warning. The
    /// leniency for a truly empty collection stays silent, only an absent
    /// key is named.
    #[test]
    fn present_but_empty_expansion_key_stays_silent() {
        let cfg = doc(vec![phase(
            "investigate",
            vec![TaskConfig {
                id: "review-probe-template-task".to_string(),
                description: None,
                display_name: None,
                depends_on: vec![],
                role_id: None,
                steps: vec![step("review-probe-template-step", "review.probe", serde_json::Value::Null)],
                expand: Some(ExpansionSpec {
                    over: "probe_seats".to_string(),
                    task_id_pattern: "review-probe-{index}-task".to_string(),
                    step_id_pattern: "review-probe-{index}-step".to_string(),
                    kind_pattern: "{kind}:{name}".to_string(),
                    description_pattern: None,
                    display_name_pattern: None,
                    role_pattern: None,
                    extras: Map::new(),
                }),
                extras: Map::new(),
            }],
        )]);
        let mut expansions = Map::new();
        expansions.insert("probe_seats".to_string(), Vec::new());
        let params = LaunchParams { expansions, ..Default::default() };

        let (tasks, _steps, warnings) = interpret(&cfg, &params).unwrap();
        assert!(tasks.is_empty(), "a present-but-empty key still produces zero real copies");
        assert!(warnings.is_empty(), "a genuinely empty collection under a present key stays silent: {warnings:?}");
    }

    #[test]
    fn cross_phase_depends_on_resolves_through_expansion_map() {
        // Mirrors `build_review_graph`'s synthesis -> dedup (investigate) +
        // verify (report) cross-phase edge.
        let cfg = doc(vec![
            phase(
                "investigate",
                vec![task("review-dedup-task", &[], None, vec![step("review-dedup-step", "review.dedup", serde_json::Value::Null)])],
            ),
            phase(
                "report",
                vec![
                    task("review-verify-task", &["review-dedup-task"], None, vec![step("review-verify-step", "review.verify", serde_json::Value::Null)]),
                    task(
                        "review-synthesis-task",
                        &["review-dedup-task", "review-verify-task"],
                        None,
                        vec![step("review-synthesis-step", "review.synthesis", serde_json::Value::Null)],
                    ),
                ],
            ),
        ]);
        let (tasks, _steps, _warnings) = interpret(&cfg, &LaunchParams::default()).unwrap();
        let by_id: BTreeMap<&str, &Task> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();
        assert_eq!(
            by_id["review-synthesis-task"].depends_on,
            vec!["review-dedup-task".to_string(), "review-verify-task".to_string()]
        );
    }

    #[test]
    fn dangling_depends_on_errors_rather_than_panicking() {
        let cfg = doc(vec![phase(
            "p1",
            vec![task("t1", &["ghost"], None, vec![step("s1", "dispatch.internal", serde_json::Value::Null)])],
        )]);
        let err = interpret(&cfg, &LaunchParams::default()).unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn every_produced_step_has_planned_status_and_no_timestamps() {
        let cfg = doc(vec![phase(
            "p1",
            vec![task("t1", &[], None, vec![step("s1", "dispatch.internal", serde_json::Value::Null)])],
        )]);
        let (_tasks, steps, _warnings) = interpret(&cfg, &LaunchParams::default()).unwrap();
        let s = &steps["s1"];
        assert_eq!(s.status, NodeStatus::Planned);
        assert!(s.started_ts.is_none());
        assert!(s.completed_ts.is_none());
        assert!(s.output.is_none());
    }

    // ── (#1284 review round 2, consider 4) dangling launcher keys ─────

    fn simple_doc() -> MissionConfig {
        doc(vec![phase(
            "p1",
            vec![task("t1", &[], None, vec![step("s1", "dispatch.internal", serde_json::Value::Null)])],
        )])
    }

    #[test]
    fn dangling_task_override_key_bails_naming_the_key() {
        let mut task_overrides = Map::new();
        task_overrides.insert("t1-typo".to_string(), TaskOverride::default());
        let params = LaunchParams { task_overrides, ..Default::default() };
        let err = interpret(&simple_doc(), &params).unwrap_err();
        assert!(err.to_string().contains("t1-typo"), "{err:#}");
    }

    #[test]
    fn dangling_step_config_override_key_bails_naming_the_key() {
        // The concrete hazard this guards (review round 2): the
        // "review-judge-step" literal lives unlinked in review.rs and
        // review.json — if the document renames the step, a silent no-match
        // would revert the operator's judge concurrency to the static 1.
        let mut step_config_overrides = Map::new();
        step_config_overrides.insert("review-judge-step-typo".to_string(), serde_json::json!({"concurrency": 4}));
        let params = LaunchParams { step_config_overrides, ..Default::default() };
        let err = interpret(&simple_doc(), &params).unwrap_err();
        assert!(err.to_string().contains("review-judge-step-typo"), "{err:#}");
    }

    #[test]
    fn dangling_expansion_key_bails_naming_the_key() {
        let mut expansions = Map::new();
        expansions.insert("probe_seatz".to_string(), vec!["alpha".to_string()]);
        let params = LaunchParams { expansions, ..Default::default() };
        let err = interpret(&simple_doc(), &params).unwrap_err();
        assert!(err.to_string().contains("probe_seatz"), "{err:#}");
    }

    #[test]
    fn dangling_phase_id_key_bails_naming_the_key() {
        let mut phase_ids = Map::new();
        phase_ids.insert("p1-typo".to_string(), "real-p1".to_string());
        let params = LaunchParams { phase_ids, ..Default::default() };
        let err = interpret(&simple_doc(), &params).unwrap_err();
        assert!(err.to_string().contains("p1-typo"), "{err:#}");
    }

    // ── (#1284 review round 2, consider 5) template + collision guards ─

    fn expanding_task(task_id_pattern: &str, step_id_pattern: &str, steps: Vec<StepConfig>) -> TaskConfig {
        TaskConfig {
            id: "template".to_string(),
            description: None,
            display_name: None,
            depends_on: vec![],
            role_id: None,
            steps,
            expand: Some(ExpansionSpec {
                over: "items".to_string(),
                task_id_pattern: task_id_pattern.to_string(),
                step_id_pattern: step_id_pattern.to_string(),
                kind_pattern: "{kind}:{name}".to_string(),
                description_pattern: None,
                display_name_pattern: None,
                role_pattern: None,
                extras: Map::new(),
            }),
            extras: Map::new(),
        }
    }

    fn two_item_params() -> LaunchParams {
        let mut expansions = Map::new();
        expansions.insert("items".to_string(), vec!["a".to_string(), "b".to_string()]);
        LaunchParams { expansions, ..Default::default() }
    }

    #[test]
    fn multi_step_template_task_is_a_loud_error_not_a_silent_drop() {
        let cfg = doc(vec![phase(
            "p1",
            vec![expanding_task(
                "t-{index}",
                "s-{index}",
                vec![
                    step("tpl-s1", "review.probe", serde_json::Value::Null),
                    step("tpl-s2", "review.probe", serde_json::Value::Null),
                ],
            )],
        )]);
        let err = interpret(&cfg, &two_item_params()).unwrap_err();
        assert!(err.to_string().contains("exactly ONE step"), "{err:#}");
    }

    #[test]
    fn expansion_patterns_without_placeholders_collide_and_bail() {
        // Neither {index} nor {name} in the patterns — both copies render
        // the SAME task/step id; the steps BTreeMap would silently keep one
        // without the collision check.
        let cfg = doc(vec![phase(
            "p1",
            vec![expanding_task(
                "t-fixed",
                "s-fixed",
                vec![step("tpl-s1", "review.probe", serde_json::Value::Null)],
            )],
        )]);
        let err = interpret(&cfg, &two_item_params()).unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err:#}");
    }
}
