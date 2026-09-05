//! (#2310 P4c) `plan.sites` — the generic diff/tree plan step.
//!
//! `crawl.plan` (`plan_step.rs`) plans exactly one thing: a `site`/`read`/
//! `edge`-kind rule over a whole-tree walk. Review needs the same
//! "resolve the rule, materialize the source, run the mechanical planner,
//! write the plan, hand its path downstream" control flow but with a
//! DIFFERENT source enumeration — a diff's hunks rather than every line of
//! every file — which is exactly what DESIGN.md's "Code review as a
//! second config on the crawl's building blocks" names: "The control flow
//! is the same for review; only the source enumeration differs (a tree
//! walk against a diff's hunks). The planner's source becomes a strategy
//! on one shared pattern."
//!
//! This kind IS that strategy, expressed as one `"source"` config field:
//!
//! ```json
//! {
//!   "rule": "swallowed-error",
//!   "workspace": "/path/to/workspace-spec.json",
//!   "source": "tree",
//!   "sizing": { "max_sites_per_unit": 40, "max_est_tokens_per_unit": 16000 },
//!   "no_fetch": false,
//!   "plan_out": "/optional/explicit/path.json"
//! }
//! ```
//!
//! ```json
//! {
//!   "rule": "intent-vs-diff",
//!   "workspace": "/path/to/workspace-spec.json",
//!   "source": "diff",
//!   "diff_file": "/path/to/the.diff",
//!   "head_sha": "abc123...",
//!   "github": "owner/repo"
//! }
//! ```
//!
//! `source` defaults to `"tree"` — the SAME behavior `crawl.plan` has
//! always had, byte-identical: `"source": "tree"` (or an absent `source`)
//! calls `plan_step::plan_one_rule` DIRECTLY, the exact function
//! `CrawlPlanStepKind::run` calls, so `crawl.json`'s tree-source plans and
//! this kind's tree-source plans are the same code path, not two
//! implementations that happen to agree today. `"source": "diff"` instead
//! materializes `workspace` (which must resolve to exactly one source —
//! see `plan::plan_diff_rule`'s own doc) and reads the diff at
//! `config.diff_file`, then calls `plan::plan_diff_rule`.
//!
//! **`head_sha`/`github` are accepted but not yet wired to a fetch.**
//! DESIGN.md says the diff source's sha/ref "come from the launch inputs
//! `head_sha`/`github` or the diff file's own header", and a review-v2
//! launch's `--param head_sha=`/`--param github=` DO reach this step's
//! config (see `review-v2.json`'s `plan-<rule>` phase, which templates
//! them in) — but this packet's proof runs "over a tempdir tree of the
//! post-diff fixture" (the P4 brief's own live-plumbing-proof recipe): the
//! operator checks out the diff's head into a `workspace` spec's `path`
//! source themselves, same as any other `workspace_spec` source. A
//! `github`-sourced fetch that materializes a tree from a PR reference
//! with no local checkout is real future work (the old bespoke launcher's
//! `resolve_source`/`GithubApi` path did this for the funnel's bundler,
//! which never needed a tree — only diff hunks); wiring `plan.sites` to
//! it is out of this packet's scope, and is a real gap named rather than
//! papered over: today's written `Plan.sources[0].sha`/`.git_ref` come
//! ONLY from the checked-out tree the operator's `workspace` spec names,
//! never from `config.head_sha`/`config.github`. Both fields still ride
//! the step config, parsed but unused, so a launch's params don't need a
//! second, `plan.sites`-specific spelling later when the fetch lands.
//!
//! Tier 3 by #1352's test, same reasoning as `plan_step.rs`: this is
//! genuinely new control flow (a strategy selector over two sources), not
//! config over an existing kind. Co-located with the crawl module for now
//! (a Tier 2 `step_kinds::patterns` promotion is the natural next step
//! once `plan_step.rs`/`plan_sites_step.rs` actually merge — P4d's call,
//! per DESIGN.md's "P4d ... let the planner be the shared pattern").

use crate::crawl::plan::{self, Plan, PlanParams};
use crate::crawl::plan_step::{self, CRAWL_PLAN_OUTPUT_KIND};
use anyhow::{anyhow, Context, Result};
use darkmux_crew::rules;
use darkmux_crew::step_kinds::{Port, SeatClaim, StepKind, StepKindRegistry, StepOutcome, StepRunCtx};
use darkmux_crew::types::{Step, Task};
use darkmux_crew::workspace_spec::{materialize, MaterializeOptions, WorkspaceSpec};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

pub const PLAN_SITES_KIND: &str = "plan.sites";

/// (#2310) The `plan.sites` step kind — see this module's own doc for the
/// tree/diff strategy split. `Plan::source_kind` (in `plan.rs`) records
/// which strategy produced a given written plan.
pub struct PlanSitesStepKind;

impl StepKind for PlanSitesStepKind {
    /// (#2394) Enumerates the sites to crawl. No model.
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
        PLAN_SITES_KIND
    }

    fn display_name(&self) -> &'static str {
        "Plan sites"
    }

    /// (#2301 precedent) The port label is the SAME wrapper kind
    /// `crawl.plan` provides — `CRAWL_PLAN_OUTPUT_KIND`, not a new
    /// `"plan.sites"` content id — so `crawl.unit`'s `requires()` (which
    /// names that one port) reads a plan from EITHER planner with no
    /// modification. Two producers, one content shape.
    fn provides(&self) -> &'static [Port] {
        const PORTS: [Port; 1] = [Port::data(CRAWL_PLAN_OUTPUT_KIND)];
        &PORTS
    }

    fn run(&self, step: &Step, task: &Task, _input: &BTreeMap<String, String>) -> Result<StepOutcome> {
        let cfg = SitesStepConfig::from_step(step)?;
        let out_path = match &cfg.plan_out {
            Some(p) => p.clone(),
            None => plan_step::default_plan_path(task, &cfg.rule)?,
        };
        let the_plan = match cfg.source {
            Source::Tree => plan_step::plan_one_rule(&cfg.as_tree_config())?,
            Source::Diff => plan_diff(&cfg)?,
        };
        let wrapped = darkmux_crew::step_output::Output::wrap(
            CRAWL_PLAN_OUTPUT_KIND,
            the_plan,
            darkmux_crew::step_output::Producer::of(&plan_step::mission_id_of(task), &task.id, &step.id),
        );
        plan_step::write_plan(&out_path, &wrapped)?;
        Ok(StepOutcome { output: darkmux_crew::step_output::ref_output_string(&out_path), flow_records: Vec::new() })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Tree,
    Diff,
}

#[derive(Debug, Clone)]
struct SitesStepConfig {
    rule: String,
    workspace: PathBuf,
    params: PlanParams,
    fetch: bool,
    plan_out: Option<PathBuf>,
    source: Source,
    /// Required when `source == Diff`; canonicalized when the path exists
    /// on disk at parse time (a relative path must not silently depend on
    /// the process's cwd surviving to `run`'s later `std::fs::
    /// read_to_string`; see `P4-brief-draft.md`'s `path_input` note on the
    /// old launcher's own version of this bug). Left AS GIVEN when
    /// canonicalize fails (a not-yet-materialized path, or a genuine typo)
    /// so the real error surfaces at the actual read in `plan_diff`,
    /// naming the path the operator wrote, not a canonicalize failure
    /// about a path that was never going to resolve anyway.
    diff_file: Option<PathBuf>,
    /// Accepted, parsed, and NOT YET used — see this module's doc for why.
    #[allow(dead_code)]
    head_sha: Option<String>,
    /// Accepted, parsed, and NOT YET used — see this module's doc for why.
    #[allow(dead_code)]
    github: Option<String>,
}

impl SitesStepConfig {
    fn from_step(step: &Step) -> Result<Self> {
        let str_field = |key: &str| -> Result<String> {
            step.config
                .get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(String::from)
                .ok_or_else(|| anyhow!("step `{}`: `{PLAN_SITES_KIND}` requires config.{key}", step.id))
        };
        let rule = str_field("rule")?;
        let workspace = PathBuf::from(str_field("workspace")?);
        // (#2310 P4c-2 review MUST-do 1) Shared with `plan_step.rs` so the
        // two `plan.*` kinds cannot silently drift back apart on
        // CLI-string leniency — this kind still parsed `sizing`/`no_fetch`
        // STRICTLY (`as_u64`/`as_bool`) after item 0 shipped item 0's
        // generic substitution, which always carries a `--param`-sourced
        // value through as a JSON string; the strict parse silently
        // dropped every such override for `review-v2.json`'s own
        // `plan.sites` steps.
        let (params, fetch) = plan::parse_sizing_and_no_fetch(&step.config, &step.id, PLAN_SITES_KIND)?;
        let plan_out = step.config.get("plan_out").and_then(|v| v.as_str()).map(PathBuf::from);
        let source = match step.config.get("source").and_then(|v| v.as_str()) {
            None | Some("tree") => Source::Tree,
            Some("diff") => Source::Diff,
            Some(other) => {
                anyhow::bail!(
                    "step `{}`: `{PLAN_SITES_KIND}` config.source must be \"tree\" or \"diff\", got {other:?}",
                    step.id
                )
            }
        };
        let diff_file = step.config.get("diff_file").and_then(|v| v.as_str()).map(|s| {
            let p = PathBuf::from(s);
            std::fs::canonicalize(&p).unwrap_or(p)
        });
        if source == Source::Diff && diff_file.is_none() {
            anyhow::bail!(
                "step `{}`: `{PLAN_SITES_KIND}` config.source=\"diff\" requires config.diff_file",
                step.id
            );
        }
        let head_sha = step.config.get("head_sha").and_then(|v| v.as_str()).map(String::from);
        let github = step.config.get("github").and_then(|v| v.as_str()).map(String::from);
        Ok(Self { rule, workspace, params, fetch, plan_out, source, diff_file, head_sha, github })
    }

    /// `Source::Tree`'s config, in `plan_step::plan_one_rule`'s own shape —
    /// the byte-identical-to-`crawl.plan` guarantee this module's doc
    /// promises lives entirely in reusing that struct/function, not in
    /// re-deriving them here.
    fn as_tree_config(&self) -> plan_step::PlanStepConfig {
        plan_step::PlanStepConfig {
            rule: self.rule.clone(),
            workspace: self.workspace.clone(),
            params: self.params,
            fetch: self.fetch,
            plan_out: None, // `run` above resolves + writes the path itself either way
        }
    }
}

fn plan_diff(cfg: &SitesStepConfig) -> Result<Plan> {
    let (rules_vec, warnings) = rules::resolve_default(std::slice::from_ref(&cfg.rule))?;
    for w in warnings.iter().filter(|w| w.contains(&cfg.rule)) {
        eprintln!("[darkmux] warning: plan.sites: {w}");
    }
    // `resolve_default` errors loudly (naming every known rule) on an
    // unresolvable id, so reaching here means exactly one rule resolved.
    let rule = rules_vec
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("plan.sites: rule '{}' resolved to nothing", cfg.rule))?;

    let (spec, spec_warnings) = WorkspaceSpec::load(&cfg.workspace)
        .with_context(|| format!("loading workspace spec {}", cfg.workspace.display()))?;
    for w in &spec_warnings {
        eprintln!("[darkmux] warning: plan.sites: {w}");
    }
    let materialized = materialize(&spec, MaterializeOptions { fetch: cfg.fetch, read_only: true })
        .with_context(|| format!("materializing workspace '{}'", spec.effective_name()))?;

    // `from_step` refuses a diff source with no `diff_file` before this
    // function is ever called — `expect` documents that invariant rather
    // than re-threading an `Option` through a function whose only caller
    // already proved it `Some`.
    let diff_file = cfg.diff_file.as_ref().expect("SitesStepConfig::from_step refuses source=diff with no diff_file");
    let diff_text = std::fs::read_to_string(diff_file)
        .with_context(|| format!("reading diff file {}", diff_file.display()))?;

    // (#2310 fix-loop B1) The diff FILE is named in this context line so
    // `plan_diff_rule`'s zero-files refusal — whose message talks about
    // diff dialects — points the operator at the exact file to inspect,
    // not just at the rule and the workspace.
    plan::plan_diff_rule(&materialized, &rule, &diff_text, cfg.params).with_context(|| {
        format!(
            "planning rule '{}' over the diff at {} against workspace '{}'",
            rule.id,
            diff_file.display(),
            spec.effective_name()
        )
    })
}

/// Register `plan.sites` beside `crawl.plan`/`crawl.unit`/`crawl.summary` —
/// called from `plan_step::register_crawl_kinds` so one call still gives a
/// launcher every kind either config declares.
pub fn register(registry: &StepKindRegistry) -> Result<()> {
    registry.register(Arc::new(PlanSitesStepKind)).context("registering plan.sites")
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_crew::types::NodeStatus;

    fn step(config: serde_json::Value) -> Step {
        Step {
            id: "plan-step".into(),
            task_id: "plan-task".into(),
            kind: PLAN_SITES_KIND.into(),
            gate: None,
            status: NodeStatus::Planned,
            config,
            started_ts: None,
            completed_ts: None,
            output: None,
        }
    }

    #[test]
    fn source_defaults_to_tree() {
        let cfg = SitesStepConfig::from_step(&step(serde_json::json!({
            "rule": "swallowed-error", "workspace": "/tmp/ws.json"
        })))
        .unwrap();
        assert_eq!(cfg.source, Source::Tree);
    }

    /// (#2310 P4c-2 review MUST-do 1 — proven) This kind used to parse
    /// `sizing`/`no_fetch` STRICTLY (`as_u64`/`as_bool`) even after item 0
    /// made `--param`-sourced values reach here as JSON strings — silently
    /// dropping every such override. Now shared with `plan_step.rs` via
    /// `plan::parse_sizing_and_no_fetch`.
    #[test]
    fn sizing_and_no_fetch_parse_leniently_from_cli_param_strings() {
        let cfg = SitesStepConfig::from_step(&step(serde_json::json!({
            "rule": "swallowed-error", "workspace": "/tmp/ws.json",
            "sizing": {"max_sites_per_unit": "7", "max_est_tokens_per_unit": "1200"},
            "no_fetch": "true"
        })))
        .unwrap();
        assert_eq!(cfg.params.max_sites_per_unit, 7);
        assert_eq!(cfg.params.max_est_tokens_per_unit, 1200);
        assert!(!cfg.fetch, "`no_fetch: \"true\"` (a string) must be honored, not silently ignored");
    }

    #[test]
    fn source_diff_without_diff_file_is_refused_by_name() {
        let err = SitesStepConfig::from_step(&step(serde_json::json!({
            "rule": "swallowed-error", "workspace": "/tmp/ws.json", "source": "diff"
        })))
        .unwrap_err();
        assert!(err.to_string().contains("diff_file"), "{err}");
    }

    #[test]
    fn an_unrecognized_source_value_is_refused_naming_both_valid_ones() {
        let err = SitesStepConfig::from_step(&step(serde_json::json!({
            "rule": "swallowed-error", "workspace": "/tmp/ws.json", "source": "branch"
        })))
        .unwrap_err();
        assert!(err.to_string().contains("\"tree\""), "{err}");
        assert!(err.to_string().contains("\"diff\""), "{err}");
    }

    #[test]
    fn source_diff_with_diff_file_parses() {
        let cfg = SitesStepConfig::from_step(&step(serde_json::json!({
            "rule": "swallowed-error", "workspace": "/tmp/ws.json", "source": "diff", "diff_file": "/tmp/d.diff"
        })))
        .unwrap();
        assert_eq!(cfg.source, Source::Diff);
        assert_eq!(cfg.diff_file.as_deref(), Some(std::path::Path::new("/tmp/d.diff")));
    }

    #[test]
    fn the_kind_registers_beside_the_builtins() {
        let registry = StepKindRegistry::with_builtins();
        register(&registry).unwrap();
        assert!(registry.get(PLAN_SITES_KIND).is_ok());
    }
}
