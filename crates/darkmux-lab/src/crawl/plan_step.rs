//! (#2298) `crawl.plan` — the crawl's planning as a step kind.
//!
//! One kind, one rule per step. Control flow: load the workspace spec,
//! materialize it, run the mechanical planner for THIS rule, write the plan
//! beside the run, hand the plan's path downstream as the step's `output`.
//! The site producer is a mux keyed by the rule's declared prefilter shape;
//! today that shape is the regex list every built-in rule declares, and the
//! planner (`super::plan`) owns it. A rule declaring the reserved
//! `{"command": ...}` shape is refused at rule load (`darkmux_crew::rules`)
//! until #2297 builds it — never a step kind per rule.
//!
//! Tier 3 by #1352's test: the control flow is new (survey → prefilter →
//! size → write), so it is a kind, and it is co-located with the crawl
//! module rather than in `darkmux-crew`'s shared `step_kinds/` because no
//! second mission plans. Promote when one does.
//!
//! Step config:
//!
//! ```json
//! {
//!   "rule": "unnamed-predicate",
//!   "workspace": "/path/to/workspace-spec.json",
//!   "sizing": { "max_sites_per_unit": 40, "max_est_tokens_per_unit": 16000 },
//!   "no_fetch": false,
//!   "plan_out": "/optional/explicit/path.json"
//! }
//! ```
//!
//! `rule` and `workspace` are required. Without `plan_out` the plan is
//! written under the run: `<missions>/<mission-id>/plan/<rule>.json`, the
//! mission being the one that owns the step's task's phase.

use crate::crawl::plan::{self, Plan, PlanParams};
use anyhow::{anyhow, bail, Context, Result};
use darkmux_crew::rules;
use darkmux_crew::step_kinds::{Port, StepKind, StepKindRegistry, StepOutcome};
use darkmux_crew::types::{Step, Task};
use darkmux_crew::workspace_spec::{materialize, MaterializeOptions, WorkspaceSpec};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const CRAWL_PLAN_KIND: &str = "crawl.plan";

pub struct CrawlPlanStepKind;

impl StepKind for CrawlPlanStepKind {
    fn id(&self) -> &'static str {
        CRAWL_PLAN_KIND
    }

    fn display_name(&self) -> &'static str {
        "Plan"
    }

    fn provides(&self) -> &'static [Port] {
        const PORTS: [Port; 1] = [Port::data("plan")];
        &PORTS
    }

    fn run(&self, step: &Step, task: &Task, _input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        let cfg = PlanStepConfig::from_step(step)?;
        let out_path = match &cfg.plan_out {
            Some(p) => p.clone(),
            None => default_plan_path(task, &cfg.rule)?,
        };
        let the_plan = plan_one_rule(&cfg)?;
        write_plan(&out_path, &the_plan)?;
        Ok(StepOutcome { output: out_path.display().to_string(), flow_records: Vec::new() })
    }
}

/// The parsed step config. Kept separate from the kind so the planning
/// itself is testable without a `Step`.
#[derive(Debug, Clone)]
pub struct PlanStepConfig {
    pub rule: String,
    pub workspace: PathBuf,
    pub params: PlanParams,
    pub fetch: bool,
    pub plan_out: Option<PathBuf>,
}

impl PlanStepConfig {
    pub fn from_step(step: &Step) -> Result<Self> {
        let str_field = |key: &str| -> Result<String> {
            step.config
                .get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(String::from)
                .ok_or_else(|| anyhow!("step `{}`: `{CRAWL_PLAN_KIND}` requires config.{key}", step.id))
        };
        let rule = str_field("rule")?;
        let workspace = PathBuf::from(str_field("workspace")?);
        let mut params = PlanParams::default();
        if let Some(sizing) = step.config.get("sizing") {
            for (key, slot) in [
                ("max_sites_per_unit", &mut params.max_sites_per_unit),
                ("max_est_tokens_per_unit", &mut params.max_est_tokens_per_unit),
            ] {
                if let Some(v) = sizing.get(key) {
                    let n = v.as_u64().filter(|n| *n > 0).ok_or_else(|| {
                        anyhow!(
                            "step `{}`: `{CRAWL_PLAN_KIND}` config.sizing.{key} must be a positive integer, got {v}",
                            step.id
                        )
                    })?;
                    *slot = usize::try_from(n).context("sizing value does not fit usize")?;
                }
            }
        }
        let fetch = !step.config.get("no_fetch").and_then(|v| v.as_bool()).unwrap_or(false);
        let plan_out = step.config.get("plan_out").and_then(|v| v.as_str()).map(PathBuf::from);
        Ok(Self { rule, workspace, params, fetch, plan_out })
    }
}

/// Plan ONE rule against the spec's workspace. Refuses an unknown rule
/// naming the known ones (the rules module's own message).
pub fn plan_one_rule(cfg: &PlanStepConfig) -> Result<Plan> {
    let (rules_vec, warnings) = rules::resolve_default(std::slice::from_ref(&cfg.rule))?;
    for w in &warnings {
        eprintln!("[darkmux] warning: crawl.plan: {w}");
    }
    let (spec, spec_warnings) = WorkspaceSpec::load(&cfg.workspace)
        .with_context(|| format!("loading workspace spec {}", cfg.workspace.display()))?;
    for w in &spec_warnings {
        eprintln!("[darkmux] warning: crawl.plan: {w}");
    }
    let materialized = materialize(&spec, MaterializeOptions { fetch: cfg.fetch, read_only: true })
        .with_context(|| format!("materializing workspace '{}'", spec.effective_name()))?;
    plan::plan_with_params(&materialized, &rules_vec, cfg.params)
        .with_context(|| format!("planning rule '{}' over workspace '{}'", cfg.rule, spec.effective_name()))
}

/// `<missions>/<mission-id>/plan/<rule>.json`, the mission being the one
/// that owns the step's task's phase. A phase record that cannot be found
/// is a refusal, not a guess at a directory.
pub fn default_plan_path(task: &Task, rule: &str) -> Result<PathBuf> {
    let phases = darkmux_crew::loader::load_phases().context("loading phase records to locate the run")?;
    let phase = phases.iter().find(|p| p.id == task.phase_id).ok_or_else(|| {
        anyhow!(
            "`{CRAWL_PLAN_KIND}`: task `{}` names phase `{}`, which has no record — pass config.plan_out \
             to write the plan elsewhere",
            task.id,
            task.phase_id
        )
    })?;
    if !rules::is_safe_rule_id(rule) {
        bail!("`{CRAWL_PLAN_KIND}`: rule id {rule:?} is not usable as a file name");
    }
    Ok(darkmux_crew::loader::missions_dir().join(&phase.mission_id).join("plan").join(format!("{rule}.json")))
}

fn write_plan(path: &Path, the_plan: &Plan) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(the_plan)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("moving the plan into place at {}", path.display()))?;
    Ok(())
}

/// Register the crawl's step kinds. Called from the launcher's registry
/// builder beside the review and coder-phase kinds.
pub fn register_crawl_kinds(registry: &StepKindRegistry) -> Result<()> {
    registry.register(Arc::new(CrawlPlanStepKind)).context("registering crawl.plan")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_crew::types::NodeStatus;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn git_repo_with(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git").current_dir(dir.path()).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        for (name, body) in files {
            let p = dir.path().join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, body).unwrap();
        }
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "fixture"]);
        dir
    }

    /// A spec whose `root` keeps every mirror/tree inside the test's own tempdir.
    fn spec_file(root: &Path, source: &Path) -> PathBuf {
        let spec = serde_json::json!({
            "name": "plan-step-fixture",
            "root": root.join("ws").to_string_lossy(),
            "sources": [{"id": "app", "path": source.to_string_lossy(), "ref": "main"}]
        });
        let p = root.join("workspace.json");
        fs::write(&p, spec.to_string()).unwrap();
        p
    }

    fn step_with(config: serde_json::Value) -> Step {
        Step {
            id: "plan-unnamed-predicate".into(),
            task_id: "plan-unnamed-predicate-task".into(),
            kind: CRAWL_PLAN_KIND.into(),
            gate: None,
            status: NodeStatus::Planned,
            config,
            started_ts: None,
            completed_ts: None,
            output: None,
        }
    }

    fn task() -> Task {
        Task {
            id: "plan-unnamed-predicate-task".into(),
            phase_id: "no-such-phase".into(),
            description: String::new(),
            display_name: None,
            step_ids: vec!["plan-unnamed-predicate".into()],
            depends_on: Vec::new(),
            reads: Vec::new(),
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        }
    }

    const UNNAMED: &str = "export function f(a: boolean, b: string, c: number) {\n  if (a && (b === \"x\" || c > 3)) {\n    return 1;\n  }\n  return 0;\n}\n";

    #[test]
    fn the_step_plans_one_rule_writes_the_plan_and_outputs_its_path() {
        let source = git_repo_with(&[("src/a.ts", UNNAMED)]);
        let root = TempDir::new().unwrap();
        let spec = spec_file(root.path(), source.path());
        let out = root.path().join("out").join("plan.json");
        let step = step_with(serde_json::json!({
            "rule": "unnamed-predicate",
            "workspace": spec.to_string_lossy(),
            "plan_out": out.to_string_lossy(),
            "sizing": {"max_sites_per_unit": 7}
        }));
        let outcome = CrawlPlanStepKind.run(&step, &task(), &BTreeMap::new()).unwrap();
        assert_eq!(outcome.output, out.display().to_string(), "the output IS the plan's path");
        let written: Plan = serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(written.rules, vec!["unnamed-predicate".to_string()]);
        assert_eq!(written.params.unwrap().max_sites_per_unit, 7, "the sizing knob the plan was cut with");
        assert_eq!(written.sources.len(), 1);
        assert_eq!(written.sources[0].sha.len(), 40, "the sha the plan was cut at: {:?}", written.sources[0].sha);
        assert!(!written.units.is_empty(), "the prefilter hit the unnamed predicate: {written:?}");
        assert_eq!(written.schema_version, plan::PLAN_SCHEMA_VERSION);
    }

    #[test]
    fn an_unknown_rule_is_refused_naming_the_known_ones() {
        let source = git_repo_with(&[("src/a.ts", UNNAMED)]);
        let root = TempDir::new().unwrap();
        let spec = spec_file(root.path(), source.path());
        let out = root.path().join("plan.json");
        let step = step_with(serde_json::json!({
            "rule": "no-such-rule", "workspace": spec.to_string_lossy(), "plan_out": out.to_string_lossy()
        }));
        let err = CrawlPlanStepKind.run(&step, &task(), &BTreeMap::new()).unwrap_err();
        assert!(!out.exists(), "a refused plan writes nothing");
        let msg = format!("{err:#}");
        assert!(msg.contains("no-such-rule") && msg.contains("unnamed-predicate"), "{msg}");
    }

    #[test]
    fn a_missing_required_field_is_refused_by_name() {
        let step = step_with(serde_json::json!({"workspace": "/nowhere.json"}));
        let err = PlanStepConfig::from_step(&step).unwrap_err();
        assert!(format!("{err}").contains("config.rule"), "{err}");
        let step = step_with(serde_json::json!({"rule": "unnamed-predicate"}));
        let err = PlanStepConfig::from_step(&step).unwrap_err();
        assert!(format!("{err}").contains("config.workspace"), "{err}");
        let step = step_with(serde_json::json!({
            "rule": "unnamed-predicate", "workspace": "/x.json", "sizing": {"max_sites_per_unit": 0}
        }));
        let err = PlanStepConfig::from_step(&step).unwrap_err();
        assert!(format!("{err}").contains("positive integer"), "{err}");
    }

    #[test]
    fn without_plan_out_a_phase_with_no_record_is_a_refusal_not_a_guess() {
        let err = default_plan_path(&task(), "unnamed-predicate").unwrap_err();
        assert!(format!("{err}").contains("no-such-phase"), "{err}");
    }

    #[test]
    fn the_kind_registers_beside_the_builtins() {
        let registry = StepKindRegistry::with_builtins();
        register_crawl_kinds(&registry).unwrap();
        assert!(registry.ids().iter().any(|id| id == CRAWL_PLAN_KIND));
    }
}
