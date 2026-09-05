//! Config → executable graph interpreter (#1284 Packet 3, closes #1356's
//! promise structurally — see the module doc on `mission_config`).
//!
//! [`interpret`] turns a parsed [`super::MissionConfig`] plus launch-time
//! [`LaunchParams`] into the `Vec<Task>` + `BTreeMap<String, Step>` shape
//! `darkmux-crew`'s `scheduler::run_step_graph` consumes — the SAME shape
//! `build_review_graph` (`darkmux-lab::lab::review`) and `default_phase_graph`
//! (`src/coder_phase.rs`) used to build by hand. This module owns exactly
//! two things, and only these two (per the packet's own scope — Tier 3
//! `StepKind` construction/registration stays mission-owned, #1352):
//!
//!   1. the placeholder-prefix phase-id substitution rule ([`TaskConfig`]'s
//!      doc in `mod.rs`) applied to every task/step id;
//!   2. `depends_on`/`reads` edge resolution.
//!
//! (#1550 cluster item 2) A THIRD thing — a generic "one `TaskConfig`
//! template expands into N real Task/Step copies, one per launcher-supplied
//! collection item" primitive (`ExpansionSpec`/`expand`/`LaunchParams::
//! expansions`) — lived here from Packet 3 (schema 1.1) through #1512's
//! review dissolution, then retired here: BOTH production launchers
//! (`build_review_graph` and `src/mission_launch.rs`) always constructed
//! `LaunchParams` with an empty `expansions` map, so a document declaring
//! `expand` interpreted to ZERO real copies on every real run — fully
//! specified, fully interpreted, never actually fed. #1512's dissolution is
//! WHY: the review probe stage that motivated this primitive moved to
//! static, explicitly-declared probe tasks (one `role_id` per task,
//! discovered structurally — see `darkmux_crew::resourcing`'s module doc),
//! so the one real consumer stopped needing runtime expansion. Retired
//! rather than wired to a still-nonexistent second use case (pre-1.0, small
//! audience — no deprecation shim); a document that still declares `expand`
//! now just carries an inert key (lenient-on-read `extras` overflow,
//! contract 7) instead of silently interpreting to zero tasks.
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
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Per-task overrides a launcher supplies at interpretation time, keyed by
/// the [`TaskConfig`]'s OWN (pre-substitution) `id` — stable
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
    /// (#2310 P4c-2 item 0) Every input the launch actually collected
    /// (`--input`/`--param`), keyed by name — the values [`super::
    /// substitute_step_config`] resolves `{{<input-id>}}` placeholders
    /// from. Applied AFTER `step_config_overrides` (a launcher's explicit
    /// override wins outright), to every step's config in the document AND
    /// to every grown copy's config (see `interpret_grown`). Replaces the
    /// old `crawl_plan_step_overrides`, which only ever substituted
    /// `{{workspace}}` and only for `crawl.plan` steps.
    pub input_values: BTreeMap<String, serde_json::Value>,
}

/// What [`interpret`] returns: the real `Vec<Task>` (document order) +
/// `BTreeMap<String, Step>` a `StepKindRegistry`-equipped caller hands to
/// `scheduler::run_step_graph`, + any non-fatal warnings worth the caller
/// printing. `interpret` has no printing/logging channel of its own, so
/// returning them is the smallest honest mechanism for a caller to surface
/// them. (#1550 cluster item 2) Always empty today — the one warning kind
/// this ever carried (an absent `expand.over` collection) went with the
/// retired expansion primitive; kept as a `Vec` (not removed) since it's
/// part of this function's public return shape and a genuinely-useful
/// mechanism for a FUTURE non-fatal interpretation finding, not a lying
/// surface — an always-empty Vec is an honest "nothing to report," never a
/// promise the caller can't verify.
pub type InterpretedGraph = (Vec<Task>, BTreeMap<String, Step>, Vec<String>);

/// See the module doc and [`InterpretedGraph`].
pub fn interpret(config: &MissionConfig, params: &LaunchParams) -> Result<InterpretedGraph> {
    check_params_reference_the_document(config, params)?;
    let declared: BTreeSet<String> = config.inputs.iter().map(|i| i.name.clone()).collect();

    let mut tasks: Vec<Task> = Vec::new();
    let mut steps: BTreeMap<String, Step> = BTreeMap::new();
    let warnings: Vec<String> = Vec::new();
    // Document-level TaskConfig.id -> the real Task id it produced. Always
    // len 1 per entry today (#1550 cluster item 2 retired the expansion
    // primitive that could produce more than one) — kept as a `Vec<String>`
    // rather than a bare `String` so the `depends_on`/`reads` rewrite pass
    // below (which `.extend()`s from it) doesn't need a second shape.
    let mut expansion_of: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for phase in &config.phases {
        let real_phase_id = substitute_phase_id(&phase.id, params);
        for task_cfg in &phase.tasks {
            // (#2300) A task declaring `grow` is a TEMPLATE, not a task: its
            // real copies are minted at the phase boundary, once the `from`
            // task's output exists (see `mission_config::grow`). It maps to
            // ZERO real ids here, which is why `validate` makes naming a
            // template in `depends_on`/`reads` an Error — an edge to a
            // template can only ever resolve to nothing.
            if task_cfg.grow.is_some() {
                expansion_of.insert(task_cfg.id.clone(), Vec::new());
                continue;
            }
            let override_ = params.task_overrides.get(&task_cfg.id);
            let real_task_id = substitute_id(&task_cfg.id, &phase.id, &real_phase_id);
            let mut step_ids = Vec::with_capacity(task_cfg.steps.len());
            for step_cfg in &task_cfg.steps {
                let real_step_id = substitute_id(&step_cfg.id, &phase.id, &real_phase_id);
                step_ids.push(real_step_id.clone());
                let config = step_config_for(step_cfg, params, &declared)?;
                push_step(
                    &mut steps,
                    &real_step_id,
                    &real_task_id,
                    &step_cfg.kind,
                    config,
                    step_cfg.gate.clone(),
                )?;
            }
            let description = task_cfg.description.clone().unwrap_or_default();
            push_task(
                &mut tasks,
                &real_task_id,
                &real_phase_id,
                description,
                task_cfg.display_name.clone(),
                step_ids,
                override_,
                task_cfg.role_id.as_deref(),
                task_cfg.run_on.as_deref(),
            )?;
            expansion_of.insert(task_cfg.id.clone(), vec![real_task_id]);
        }
    }

    // Second pass: resolve every TaskConfig's `depends_on` AND `reads`
    // (#1619 — the ledger relation resolves identically) now that
    // `expansion_of` is complete for the WHOLE document.
    let real_index: BTreeMap<String, usize> =
        tasks.iter().enumerate().map(|(i, t)| (t.id.clone(), i)).collect();
    for phase in &config.phases {
        for task_cfg in &phase.tasks {
            let resolve = |relation: &str, entries: &[String]| -> anyhow::Result<Vec<String>> {
                let mut resolved: Vec<String> = Vec::new();
                for dep in entries {
                    match expansion_of.get(dep) {
                        Some(real_ids) => resolved.extend(real_ids.iter().cloned()),
                        None => bail!(
                            "task `{}` {relation} unknown task id `{dep}` — the config must \
                             validate cleanly (MissionConfig::validate) before interpretation",
                            task_cfg.id
                        ),
                    }
                }
                Ok(resolved)
            };
            let resolved_depends = resolve("depends_on", &task_cfg.depends_on)?;
            let resolved_reads = resolve("reads", &task_cfg.reads)?;
            if resolved_depends.is_empty() && resolved_reads.is_empty() {
                continue;
            }
            if let Some(real_ids) = expansion_of.get(&task_cfg.id) {
                for real_id in real_ids {
                    if let Some(&idx) = real_index.get(real_id.as_str()) {
                        if !resolved_depends.is_empty() {
                            tasks[idx].depends_on = resolved_depends.clone();
                        }
                        if !resolved_reads.is_empty() {
                            tasks[idx].reads = resolved_reads.clone();
                        }
                    }
                }
            }
        }
    }

    // (#2341) A step id may never equal a task id: a later step's `input`
    // keys the task's reads/depends_on by TASK id and its predecessor by STEP
    // id in one map, and the predecessor would silently win the collision.
    // Shipped configs avoid it by convention (`-task`/`-step` suffixes); a
    // hand-built one is refused here, loudly, rather than at run time.
    for step_id in steps.keys() {
        if tasks.iter().any(|t| &t.id == step_id) {
            bail!(
                "interpreted graph: step id `{step_id}` equals a task id; a later step's inputs key reads by task id \
                 and the predecessor by step id, so the two would collide (#2341) - rename one of them"
            );
        }
    }
    Ok((tasks, steps, warnings))
}

/// (#2300) Document task id -> the real Task id(s) it mints, WITHOUT
/// building the graph — the map [`interpret_grown`] needs to resolve a
/// grown copy's inherited `depends_on`/`reads`. A `grow` template maps to
/// an empty vec here for the same reason it does inside [`interpret`].
pub fn real_task_ids(
    config: &MissionConfig,
    params: &LaunchParams,
) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for phase in &config.phases {
        let real_phase_id = substitute_phase_id(&phase.id, params);
        for task_cfg in &phase.tasks {
            let real = if task_cfg.grow.is_some() {
                Vec::new()
            } else {
                vec![substitute_id(&task_cfg.id, &phase.id, &real_phase_id)]
            };
            out.insert(task_cfg.id.clone(), real);
        }
    }
    out
}

/// (#2300) Interpret already-grown, concrete [`TaskConfig`]s into real
/// `Task`/`Step` values for ONE phase — the phase-boundary counterpart to
/// [`interpret`], which mints only the statically-declared graph.
///
/// `real_ids` is [`real_task_ids`]'s map, used to resolve each copy's
/// inherited `depends_on`/`reads` (which name DOCUMENT task ids, exactly
/// as they do in [`interpret`]). An entry resolving to zero real ids
/// contributes no edge; a name absent from the map is an error, same as
/// in [`interpret`]'s own second pass.
///
/// The same id rules apply as everywhere else: the placeholder-prefix
/// substitution runs over both task and step ids, and duplicate rendered
/// ids are refused rather than silently collapsing.
///
/// (#2310 P4c-2 item 0) `declared`/`collected` are the SAME pair
/// [`interpret`] substitutes the statically-declared graph's step configs
/// from — a grown copy's config already had its `{{item.<field>}}`/
/// `{{from.output}}` placeholders resolved by `grow_task` before it reaches
/// here (growth is a separate, earlier substitution pass over a different
/// namespace), so what's left to resolve is exactly the declared-input
/// placeholders a grow template's static keys may also carry (e.g.
/// `review-v2.json`'s `unit-<rule>` tasks wiring `{{intent_file}}` into
/// every grown unit's config).
pub fn interpret_grown(
    grown: &[super::TaskConfig],
    doc_phase_id: &str,
    real_phase_id: &str,
    real_ids: &BTreeMap<String, Vec<String>>,
    declared: &BTreeSet<String>,
    collected: &BTreeMap<String, serde_json::Value>,
) -> Result<(Vec<Task>, BTreeMap<String, Step>)> {
    let mut tasks: Vec<Task> = Vec::new();
    let mut steps: BTreeMap<String, Step> = BTreeMap::new();
    for task_cfg in grown {
        let real_task_id = substitute_id(&task_cfg.id, doc_phase_id, real_phase_id);
        let mut step_ids = Vec::with_capacity(task_cfg.steps.len());
        for step_cfg in &task_cfg.steps {
            let real_step_id = substitute_id(&step_cfg.id, doc_phase_id, real_phase_id);
            step_ids.push(real_step_id.clone());
            let config = super::substitute_step_config(
                &step_cfg.config,
                declared,
                collected,
                &format!("step `{real_step_id}`"),
            )?;
            push_step(
                &mut steps,
                &real_step_id,
                &real_task_id,
                &step_cfg.kind,
                config,
                step_cfg.gate.clone(),
            )?;
        }
        push_task(
            &mut tasks,
            &real_task_id,
            real_phase_id,
            task_cfg.description.clone().unwrap_or_default(),
            task_cfg.display_name.clone(),
            step_ids,
            None,
            task_cfg.role_id.as_deref(),
            task_cfg.run_on.as_deref(),
        )?;
        let resolve = |relation: &str, entries: &[String]| -> Result<Vec<String>> {
            let mut resolved: Vec<String> = Vec::new();
            for dep in entries {
                match real_ids.get(dep) {
                    Some(ids) => resolved.extend(ids.iter().cloned()),
                    None => bail!(
                        "grown task `{}` {relation} unknown task id `{dep}` — the config must \
                         validate cleanly (MissionConfig::validate) before interpretation",
                        task_cfg.id
                    ),
                }
            }
            Ok(resolved)
        };
        let last = tasks.last_mut().expect("just pushed");
        last.depends_on = resolve("depends_on", &task_cfg.depends_on)?;
        last.reads = resolve("reads", &task_cfg.reads)?;
    }
    Ok((tasks, steps))
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
    for phase in &config.phases {
        for task_cfg in &phase.tasks {
            task_ids.insert(task_cfg.id.as_str());
            for step_cfg in &task_cfg.steps {
                step_ids.insert(step_cfg.id.as_str());
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

/// (#2310 P4c-2 item 0) A launcher's explicit `step_config_overrides` entry
/// (e.g. the review launcher's judge concurrency) REPLACES the document's
/// config outright and is never itself re-substituted — it's already a
/// resolved value, not a template. Otherwise, the document's own config
/// runs through [`super::substitute_step_config`] against every input the
/// launch collected.
fn step_config_for(
    step_cfg: &StepConfig,
    params: &LaunchParams,
    declared: &BTreeSet<String>,
) -> Result<serde_json::Value> {
    if let Some(overridden) = params.step_config_overrides.get(&step_cfg.id) {
        return Ok(overridden.clone());
    }
    super::substitute_step_config(&step_cfg.config, declared, &params.input_values, &format!("step `{}`", step_cfg.id))
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
    doc_run_on: Option<&[String]>,
) -> Result<()> {
    // (#1284 review round 2, consider 5) Post-substitution FINAL ids must be
    // unique — a phase-id substitution collision (two documents composed
    // under the same real phase id) would otherwise produce same-id
    // task/step pairs. `Vec::contains` over a graph-sized Vec is fine —
    // graphs here are tens of tasks, not thousands.
    if tasks.iter().any(|t| t.id == real_task_id) {
        bail!("interpreted graph produced duplicate task id `{real_task_id}`");
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
        reads: Vec::new(),
        role_id,
        profile_name,
        workdir,
        image,
        run_on: doc_run_on.map(|v| v.to_vec()).unwrap_or_else(crate::types::default_run_on),
    });
    Ok(())
}

fn push_step(
    steps: &mut BTreeMap<String, Step>,
    real_step_id: &str,
    real_task_id: &str,
    kind: &str,
    config: serde_json::Value,
    // (#1684 Packet 2) Threaded straight from `StepConfig::gate` — this
    // function does no resolution/validation of the value (that's
    // `MissionConfig::validate`'s job); it just carries the document's
    // declaration onto the executable `Step` verbatim.
    gate: Option<String>,
) -> Result<()> {
    let step = Step {
        id: real_step_id.to_string(),
        task_id: real_task_id.to_string(),
        kind: kind.to_string(),
        gate,
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
        bail!("interpreted graph produced duplicate step id `{real_step_id}`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission_config::{PhaseConfig, TaskConfig};
    use std::collections::BTreeMap as Map;

    fn step(id: &str, kind: &str, config: serde_json::Value) -> StepConfig {
        StepConfig { id: id.to_string(), kind: kind.to_string(), config, gate: None, enabled: None, extras: Map::new() }
    }

    fn gated_step(id: &str, kind: &str, config: serde_json::Value, gate: &str) -> StepConfig {
        StepConfig { gate: Some(gate.to_string()), ..step(id, kind, config) }
    }

    fn task(id: &str, depends_on: &[&str], role_id: Option<&str>, steps: Vec<StepConfig>) -> TaskConfig {
        TaskConfig {
            excludes: Vec::new(),
            enabled: None,
            id: id.to_string(),
            description: Some(format!("do {id}")),
            display_name: None,
            depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
            reads: Vec::new(),
            role_id: role_id.map(String::from),
            run_on: None,
            steps,
            grow: None,
            extras: Map::new(),
        }
    }

    fn phase(id: &str, tasks: Vec<TaskConfig>) -> PhaseConfig {
        PhaseConfig { id: id.to_string(), description: None, display_name: None, tasks, enabled: None, extras: Map::new() }
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
            extras: Map::new(),
        }
    }

    #[test]
    fn fixed_ids_pass_throucmdatim_when_phase_id_differs() {
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

    // (#1550 cluster item 2) The expansion primitive's tests
    // (`expansion_role_pattern_renders_a_distinct_role_per_copy`,
    // `expansion_primitive_produces_one_copy_per_item_and_rewrites_dependents`,
    // `zero_items_in_the_expansion_collection_produces_zero_real_copies_and_an_empty_dependency`,
    // `absent_expand_over_key_surfaces_a_warning_but_stays_lenient`,
    // `present_but_empty_expansion_key_stays_silent`) were removed here along
    // with `ExpansionSpec`/`TaskConfig::expand`/`LaunchParams::expansions` —
    // see the module doc's #1550 note for why (both production launchers
    // always fed an empty `expansions` map, so a document declaring `expand`
    // interpreted to zero real copies on every real run).

    #[test]
    fn cross_phase_depends_on_resolves_across_phases() {
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

    // ── (#1684 Packet 2) `gate` threads onto the executable Step ───────

    #[test]
    fn interpret_threads_the_gate_field_onto_the_executable_step_verbatim() {
        let cfg = doc(vec![phase(
            "p1",
            vec![task(
                "t1",
                &[],
                None,
                vec![gated_step("s1", "procedural.noop", serde_json::Value::Null, "operator")],
            )],
        )]);
        let (_tasks, steps, _warnings) = interpret(&cfg, &LaunchParams::default()).unwrap();
        assert_eq!(steps["s1"].gate.as_deref(), Some("operator"));
    }

    #[test]
    fn interpret_leaves_gate_none_for_an_ungated_step() {
        let cfg = doc(vec![phase(
            "p1",
            vec![task("t1", &[], None, vec![step("s1", "procedural.noop", serde_json::Value::Null)])],
        )]);
        let (_tasks, steps, _warnings) = interpret(&cfg, &LaunchParams::default()).unwrap();
        assert_eq!(steps["s1"].gate, None);
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
    fn dangling_phase_id_key_bails_naming_the_key() {
        let mut phase_ids = Map::new();
        phase_ids.insert("p1-typo".to_string(), "real-p1".to_string());
        let params = LaunchParams { phase_ids, ..Default::default() };
        let err = interpret(&simple_doc(), &params).unwrap_err();
        assert!(err.to_string().contains("p1-typo"), "{err:#}");
    }

    // (#1550 cluster item 2) `dangling_expansion_key_bails_naming_the_key`,
    // `expanding_task`/`two_item_params`, `multi_step_template_task_is_a_
    // loud_error_not_a_silent_drop`, and `expansion_patterns_without_
    // placeholders_collide_and_bail` were removed with the expansion
    // primitive — see the module doc's #1550 note. The duplicate-id
    // collision guard itself (`push_task`/`push_step`) is still real code
    // (a phase-id substitution collision could still trigger it), just no
    // longer reachable via `expand`'s no-placeholder case; there is no
    // OTHER known way to trigger a same-document-id collision today, so no
    // replacement test was added for it in isolation.

    /// (#2341 review) A step id that equals a task id would collide in a
    /// later step's `input` map — reads are keyed by TASK id, the chained
    /// predecessor by STEP id — and the predecessor would silently win.
    /// Refuse the document instead.
    #[test]
    fn a_step_id_equal_to_a_task_id_is_refused() {
        let cfg = doc(vec![phase(
            "p",
            vec![
                task("a", &[], None, vec![step("a-step", "procedural.noop", serde_json::json!({}))]),
                task("multi", &["a"], None, vec![
                    step("a", "procedural.noop", serde_json::json!({})),
                    step("multi-2", "procedural.noop", serde_json::json!({})),
                ]),
            ],
        )]);
        let err = interpret(&cfg, &LaunchParams::default()).expect_err("a step id equal to a task id must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("step id `a`") && msg.contains("task id"), "{msg}");
    }

}
