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
//! (#2310 P4c) A second mission plans now — `review-v2.json` uses
//! `plan_sites_step::PLAN_SITES_KIND` (`"plan.sites"`, this file's
//! sibling), whose `"source": "tree"` config literally calls this
//! module's own [`plan_one_rule`], so `crawl.plan`'s tree-source behavior
//! stays byte-identical (the crawl-plan golden is untouched by this
//! packet). This kind is deliberately NOT retired or renamed here — the
//! brief this packet implements says so explicitly, and P4d is where
//! `crawl.plan` becomes `plan.sites`'s tree-source alias for real (or
//! retires) once the crawl side of the consolidation is decided. Until
//! then, `crawl.plan` is the kind `crawl.json` names; `plan.sites` is the
//! kind `review-v2.json` names; both produce the same `Plan` content
//! (`CRAWL_PLAN_OUTPUT_KIND`), so `crawl.unit` reads either without
//! modification.
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
//!
//! (#2310 P4c-2 item 0/review item 6) A launcher no longer injects
//! `sizing`/`no_fetch` into this step's config on its own — a mission
//! config's OWN document must declare the input AND reference
//! `{{max_sites_per_unit}}`/`{{max_est_tokens_per_unit}}`/`{{no_fetch}}`
//! (as `crawl.json` does) for an operator's `--param` to reach here; an
//! undeclared or unreferenced `--param` is now inert, not silently
//! applied to every `crawl.plan` step in the document.

use crate::crawl::plan::{self, Plan, PlanParams};
use anyhow::{anyhow, bail, Context, Result};
use darkmux_crew::rules;
use darkmux_crew::step_kinds::{Port, SeatClaim, StepKind, StepKindRegistry, StepOutcome, StepRunCtx};
use darkmux_crew::types::{Step, Task};
use darkmux_crew::workspace_spec::{materialize, MaterializeOptions, WorkspaceSpec};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const CRAWL_PLAN_KIND: &str = "crawl.plan";

/// (#2301) The CONTENT id of what this kind produces — the value a
/// consumer checks before deserializing the body as a [`Plan`]. Same
/// string as the step kind here because the kind produces exactly one
/// thing; they are separate concepts and are allowed to diverge.
pub const CRAWL_PLAN_OUTPUT_KIND: &str = "crawl.plan";

pub struct CrawlPlanStepKind;

impl StepKind for CrawlPlanStepKind {
    /// (#2394) Enumerates the crawl's units from the workspace. No model — the
    /// units it plans dispatch, this does not.
    fn seat(
        &self,
        _step: &Step,
        _task: &Task,
        _input: &BTreeMap<String, String>,
        _ctx: &StepRunCtx,
    ) -> SeatClaim {
        SeatClaim::NoModel
    }

    fn id(&self) -> &'static str {
        CRAWL_PLAN_KIND
    }

    fn display_name(&self) -> &'static str {
        "Plan"
    }

    /// (#2301) A data port's LABEL is the same string as the wrapper
    /// `kind` the output carries, so a graph validator can compare a
    /// producer's `provides` against a consumer's `requires` without a
    /// rename table in between.
    fn provides(&self) -> &'static [Port] {
        const PORTS: [Port; 1] = [Port::data(CRAWL_PLAN_OUTPUT_KIND)];
        &PORTS
    }

    fn run(&self, step: &Step, task: &Task, _input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        let cfg = PlanStepConfig::from_step(step)?;
        let out_path = match &cfg.plan_out {
            Some(p) => p.clone(),
            None => default_plan_path(task, &cfg.rule)?,
        };
        let the_plan = plan_one_rule(&cfg)?;
        // (#2301) The plan is WRAPPED on disk: `Output<Plan>` carries the
        // content id + producer beside the body, so a consumer checks what
        // it is holding before reading it. The step's own `output` is a
        // `ref` to that file rather than the file's bytes — a plan is
        // large, and every consumer wants the path anyway.
        let wrapped = darkmux_crew::step_output::Output::wrap(
            CRAWL_PLAN_OUTPUT_KIND,
            the_plan,
            darkmux_crew::step_output::Producer::of(&mission_id_of(task), &task.id, &step.id),
        );
        write_plan(&out_path, &wrapped)?;
        Ok(StepOutcome {
            output: darkmux_crew::step_output::ref_output_string(&out_path),
            flow_records: Vec::new(),
        })
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
        // (#2310 P4c-2 review MUST-do 1) Shared with `plan_sites_step.rs`
        // so the two `plan.*` kinds cannot silently drift back apart on
        // CLI-string leniency the way they did before this review.
        let (params, fetch) = plan::parse_sizing_and_no_fetch(&step.config, &step.id, CRAWL_PLAN_KIND)?;
        let plan_out = step.config.get("plan_out").and_then(|v| v.as_str()).map(PathBuf::from);
        Ok(Self { rule, workspace, params, fetch, plan_out })
    }
}

/// Plan ONE rule against the spec's workspace. Refuses an unknown rule
/// naming the known ones (the rules module's own message).
///
/// (#2310 P4c review round 2, MUST FIX 2) Also refuses a rule that does
/// not declare TREE scope — `Rule::scope` was, before this, enforced by
/// nothing: a diff-only rule (`test-gap`, part of the review catalog)
/// could be silently planned by a tree walk, producing a plan the rule's
/// own `match`/`no_match` prose was never written against (every
/// diff-only rule's prose talks about "this hunk", not "this file"). This
/// is the tree-side twin of `plan::plan_diff_rule`'s own `RuleKind::Site`
/// refusal just below — same shape, same reasoning, the OTHER half of
/// what makes `Rule::scope_or_default`'s doc ("read it through this,
/// never the field directly") actually true rather than aspirational.
pub fn plan_one_rule(cfg: &PlanStepConfig) -> Result<Plan> {
    let (rules_vec, warnings) = rules::resolve_default(std::slice::from_ref(&cfg.rule))?;
    // Load warnings cover every rule file; this step speaks only for its own.
    for w in warnings.iter().filter(|w| w.contains(&cfg.rule)) {
        eprintln!("[darkmux] warning: crawl.plan: {w}");
    }
    for rule in &rules_vec {
        if !rule.applies_to_scope(rules::RuleScope::Tree) {
            let scope = serde_json::to_string(rule.scope_or_default()).unwrap_or_default();
            bail!(
                "rule '{}' declares scope {scope} — a rule with no `tree` scope cannot be planned \
                 by a tree walk (`crawl.plan` / `plan.sites`'s `\"source\": \"tree\"`)",
                rule.id
            );
        }
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

/// The mission a task belongs to, or an empty string — provenance never
/// fails a step that is otherwise fine.
///
/// `pub(crate)` (#2310 P4c, was module-private): `plan_sites_step`'s
/// `"plan.sites"` diff-source path writes the SAME `Output<Plan>` wrapper
/// shape this module's `crawl.plan` writes, and reuses this helper rather
/// than re-deriving it — one place "which mission does this task belong
/// to" is answered, not two.
pub(crate) fn mission_id_of(task: &Task) -> String {
    darkmux_crew::loader::load_phases()
        .ok()
        .and_then(|ps| ps.iter().find(|p| p.id == task.phase_id).map(|p| p.mission_id.clone()))
        .unwrap_or_default()
}

/// `pub(crate)` (#2310 P4c, was module-private) — same reasoning as
/// `mission_id_of` above.
pub(crate) fn write_plan(path: &Path, the_plan: &darkmux_crew::step_output::Output<Plan>) -> Result<()> {
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
/// builder beside the review and coder-phase kinds. (#2301) The dispatch
/// half — `crawl.unit` + `crawl.summary` — registers here too, so one call
/// still gives a launcher every kind `crawl.json` declares.
pub fn register_crawl_kinds(registry: &StepKindRegistry) -> Result<()> {
    registry.register(Arc::new(CrawlPlanStepKind)).context("registering crawl.plan")?;
    crate::crawl::unit_step::register(registry)?;
    // (#2310 P4c) `plan.sites` — review-v2.json's planner. Registered
    // alongside the crawl kinds (not a separate top-level call in
    // `all_step_kinds`) so one call still gives a launcher every kind
    // either config declares, matching this function's own existing
    // reasoning ("Registered here so `mission config show crawl`
    // validates its graph...").
    crate::crawl::plan_sites_step::register(registry)?;
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
            run_on: darkmux_crew::types::default_run_on(),
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
        assert_eq!(
            outcome.output,
            darkmux_crew::step_output::ref_output_string(&out),
            "(#2301) the output is a `ref` NAMING the plan file, not the file's bytes"
        );
        let envelope = darkmux_crew::step_output::Output::<Plan>::read(&outcome.output, CRAWL_PLAN_OUTPUT_KIND)
            .expect("the ref resolves and the content id checks out");
        assert_eq!(envelope.kind, CRAWL_PLAN_OUTPUT_KIND);
        let written: Plan = envelope.body;
        assert_eq!(written.rules, vec!["unnamed-predicate".to_string()]);
        assert_eq!(written.params.unwrap().max_sites_per_unit, 7, "the sizing knob the plan was cut with");
        assert_eq!(written.sources.len(), 1);
        assert_eq!(written.sources[0].sha.len(), 40, "the sha the plan was cut at: {:?}", written.sources[0].sha);
        assert!(!written.units.is_empty(), "the prefilter hit the unnamed predicate: {written:?}");
        assert_eq!(written.schema_version, plan::PLAN_SCHEMA_VERSION);
    }

    #[test]
    fn a_written_plan_is_refused_when_read_as_the_wrong_content_id() {
        // (#2301) Mutation-kill for the kind check: the same file read
        // under a different expectation stops, naming both.
        let source = git_repo_with(&[("src/a.ts", UNNAMED)]);
        let root = TempDir::new().unwrap();
        let spec = spec_file(root.path(), source.path());
        let out = root.path().join("plan.json");
        let step = step_with(serde_json::json!({
            "rule": "unnamed-predicate", "workspace": spec.to_string_lossy(), "plan_out": out.to_string_lossy()
        }));
        let outcome = CrawlPlanStepKind.run(&step, &task(), &BTreeMap::new()).unwrap();
        let err = darkmux_crew::step_output::Output::<Plan>::read(&outcome.output, "crawl.unit-outcome")
            .unwrap_err()
            .to_string();
        assert!(err.contains("crawl.plan") && err.contains("crawl.unit-outcome"), "{err}");
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

    /// (#2310 P4c review round 2, MUST FIX 2 — proven) `Rule::scope` was
    /// enforced by NOTHING: a rule declaring `scope: ["diff"]` only
    /// (`test-gap`, part of the review catalog) could still be planned by
    /// the TREE walk with no error at all, silently producing a plan a
    /// diff-scoped rule's own prose was never written for. `plan_one_rule`
    /// must refuse a rule that does not declare tree scope, naming the
    /// rule and its declared scope.
    #[test]
    fn plan_one_rule_refuses_a_rule_with_no_tree_scope() {
        let source = git_repo_with(&[("src/a.ts", UNNAMED)]);
        let root = TempDir::new().unwrap();
        let spec = spec_file(root.path(), source.path());
        let out = root.path().join("plan.json");
        let step = step_with(serde_json::json!({
            "rule": "test-gap", "workspace": spec.to_string_lossy(), "plan_out": out.to_string_lossy()
        }));
        let err = CrawlPlanStepKind.run(&step, &task(), &BTreeMap::new()).unwrap_err();
        assert!(!out.exists(), "a refused plan writes nothing");
        let msg = format!("{err:#}");
        assert!(msg.contains("test-gap"), "{msg}");
        assert!(msg.contains("diff"), "the message must name the rule's actual declared scope: {msg}");
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

    /// (#2310 P4c-2 item 0) `sizing.*`/`no_fetch` now reach this step
    /// through `mission_config::substitute_step_config`'s generic
    /// `{{<input-id>}}` substitution, which carries a `--param`-sourced
    /// value through as the JSON STRING the CLI always collects it as —
    /// `crawl.json`'s own `sizing`/`no_fetch` placeholders substitute to
    /// exactly this shape on a real `--param max_sites_per_unit=7 --param
    /// no_fetch=true` launch. Lenient-on-read: string-or-typed, same as
    /// `bool_param` at the CLI layer.
    #[test]
    fn sizing_and_no_fetch_parse_leniently_from_cli_param_strings() {
        let step = step_with(serde_json::json!({
            "rule": "unnamed-predicate", "workspace": "/x.json",
            "sizing": {"max_sites_per_unit": "7", "max_est_tokens_per_unit": "1200"},
            "no_fetch": "true"
        }));
        let cfg = PlanStepConfig::from_step(&step).unwrap();
        assert_eq!(cfg.params.max_sites_per_unit, 7);
        assert_eq!(cfg.params.max_est_tokens_per_unit, 1200);
        assert!(!cfg.fetch, "`no_fetch: \"true\"` (a string) must be honored, not silently ignored");

        let step = step_with(serde_json::json!({
            "rule": "unnamed-predicate", "workspace": "/x.json", "no_fetch": "false"
        }));
        assert!(PlanStepConfig::from_step(&step).unwrap().fetch, "a string \"false\" must not fetch=false");
    }

    /// (#2310 P4c-2 item 0) A `sizing`/`no_fetch` key the generic
    /// substitution OMITTED (the operator left the input unset) must
    /// behave exactly as if the document never declared the placeholder —
    /// the built-in defaults, not an error.
    #[test]
    fn an_absent_sizing_or_no_fetch_key_keeps_the_defaults() {
        let step = step_with(serde_json::json!({
            "rule": "unnamed-predicate", "workspace": "/x.json", "sizing": {}
        }));
        let cfg = PlanStepConfig::from_step(&step).unwrap();
        assert_eq!(cfg.params.max_sites_per_unit, PlanParams::default().max_sites_per_unit);
        assert!(cfg.fetch, "no `no_fetch` key at all must default to fetching");
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn without_plan_out_a_phase_with_no_record_is_a_refusal_not_a_guess() {
        let home = TempDir::new().unwrap();
        let _guard = HomeGuard::set(home.path());
        let err = default_plan_path(&task(), "unnamed-predicate").unwrap_err();
        assert!(format!("{err}").contains("no-such-phase"), "{err}");
    }

    #[test]
    #[serial_test::serial] // scopes DARKMUX_HOME, a process-global
    fn without_plan_out_the_plan_lands_under_the_mission_that_owns_the_phase() {
        let home = TempDir::new().unwrap();
        let _guard = HomeGuard::set(home.path());
        let phase = darkmux_crew::types::Phase {
            id: "crawl-77-plan".into(),
            mission_id: "crawl-77".into(),
            description: String::new(),
            display_name: None,
            status: darkmux_crew::types::PhaseStatus::Planned,
            created_ts: 1,
            started_ts: None,
            completed_ts: None,
            abandoned_ts: None,
            task_ids: vec!["plan-unnamed-predicate-task".into()],
        };
        darkmux_crew::lifecycle::save_phase(&phase).unwrap();
        let mut t = task();
        t.phase_id = "crawl-77-plan".into();
        let path = default_plan_path(&t, "unnamed-predicate").unwrap();
        assert_eq!(
            path,
            darkmux_crew::loader::missions_dir().join("crawl-77").join("plan").join("unnamed-predicate.json"),
            "the MISSION's directory, resolved through the phase record — not the phase id"
        );
        assert!(path.starts_with(home.path()), "{}", path.display());
    }

    /// Scopes `DARKMUX_HOME` for one test and restores the prior value.
    struct HomeGuard(Option<String>);
    impl HomeGuard {
        fn set(p: &Path) -> Self {
            let prior = std::env::var("DARKMUX_HOME").ok();
            std::env::set_var("DARKMUX_HOME", p);
            Self(prior)
        }
    }
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
        }
    }

    #[test]
    fn the_kind_registers_beside_the_builtins() {
        let registry = StepKindRegistry::with_builtins();
        register_crawl_kinds(&registry).unwrap();
        assert!(registry.ids().iter().any(|id| id == CRAWL_PLAN_KIND));
    }
}
