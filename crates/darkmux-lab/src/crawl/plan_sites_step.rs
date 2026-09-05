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
//! **`workspace` is optional when `github` + `head_sha` are present.**
//! `.github/workflows/darkmux-review.yml` launches `review` from a
//! self-hosted runner with `--param github=<owner/repo> --param
//! head_sha=<sha>` and NO `workspace` — there is no operator-authored
//! workspace spec on that runner. When `config.workspace` is absent and
//! both of those ARE present, this kind DERIVES the one-source
//! `WorkspaceSpec` itself ([`derive_workspace_spec`]): a single `git`
//! source whose origin is the repository's GitHub URL, materialized through
//! the SAME `workspace_spec::materialize` path an on-disk spec takes.
//!
//! **The derived origin is an ANONYMOUS `https://github.com/<owner>/<repo>`
//! URL, so the derivation reaches PUBLIC repositories only.** darkmux's own
//! repository is public, which is why its self-review workflow needs
//! nothing more; a PRIVATE repository has to supply an explicit `workspace`
//! spec whose source names an ssh origin, or an https origin a credential
//! helper configured on that machine can authenticate. No token is read,
//! injected, or inferred here — a private clone failing is a loud git
//! error, never a silent empty review.
//! An explicit `workspace` always wins — the derivation is the fallback,
//! never an override. With neither, the step refuses by name at parse
//! time rather than failing partway through `run`. When the launch also
//! passes `pr` (the pull-request number), the derived source's ref is
//! `refs/pull/<pr>/head` rather than the bare sha — a FORK PR's head is
//! reachable from nothing else.
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
            Source::Tree => plan_step::plan_one_rule(&cfg.as_tree_config()?)?,
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
    /// `None` when the launch supplied no workspace spec — legal only when
    /// `github` + `head_sha` are both present, in which case
    /// [`derive_workspace_spec`] builds the spec instead. See this module's
    /// own doc.
    workspace: Option<PathBuf>,
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
    /// The diff's own head sha. With `github` and no `workspace`, this is
    /// the `ref` of the derived source — see [`derive_workspace_spec`].
    head_sha: Option<String>,
    /// `owner/repo` (or a full GitHub URL). With `head_sha` and no
    /// `workspace`, this is the origin of the derived source.
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
        let opt_str = |key: &str| -> Option<String> {
            step.config
                .get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(String::from)
        };
        let workspace = opt_str("workspace").map(PathBuf::from);
        let head_sha = opt_str("head_sha");
        let github = opt_str("github");
        if workspace.is_none() && !(head_sha.is_some() && github.is_some()) {
            anyhow::bail!(
                "step `{}`: `{PLAN_SITES_KIND}` requires config.workspace, or both \
                 config.github and config.head_sha to derive one",
                step.id
            );
        }
        // (#2310 P4c-2 review MUST-do 1) Shared with `plan_step.rs` so the
        // two `plan.*` kinds cannot silently drift back apart on
        // CLI-string leniency — this kind still parsed `sizing`/`no_fetch`
        // STRICTLY (`as_u64`/`as_bool`) after item 0 shipped item 0's
        // generic substitution, which always carries a `--param`-sourced
        // value through as a JSON string; the strict parse silently
        // dropped every such override for `review.json`'s own
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
        Ok(Self { rule, workspace, params, fetch, plan_out, source, diff_file, head_sha, github })
    }

    /// The workspace spec this step plans against — loaded from
    /// `config.workspace` when the launch named one, else DERIVED from
    /// `github` + `head_sha` (see this module's doc). `from_step` refuses a
    /// config with neither, so the `else` branch's `expect`s document that
    /// invariant rather than re-threading two `Option`s.
    fn resolve_spec(&self) -> Result<(WorkspaceSpec, Vec<String>)> {
        match &self.workspace {
            Some(path) => WorkspaceSpec::load(path)
                .with_context(|| format!("loading workspace spec {}", path.display())),
            None => {
                let github = self.github.as_deref().expect("from_step refuses no workspace with no github");
                let head_sha =
                    self.head_sha.as_deref().expect("from_step refuses no workspace with no head_sha");
                Ok((derive_workspace_spec(github, head_sha)?, Vec::new()))
            }
        }
    }

    /// `Source::Tree`'s config, in `plan_step::plan_one_rule`'s own shape —
    /// the byte-identical-to-`crawl.plan` guarantee this module's doc
    /// promises lives entirely in reusing that struct/function, not in
    /// re-deriving them here.
    fn as_tree_config(&self) -> Result<plan_step::PlanStepConfig> {
        let workspace = self.workspace.clone().ok_or_else(|| {
            anyhow!(
                "`{PLAN_SITES_KIND}` config.source=\"tree\" requires config.workspace \
                 (the github/head_sha derivation is diff-scoped)"
            )
        })?;
        Ok(plan_step::PlanStepConfig {
            rule: self.rule.clone(),
            workspace,
            params: self.params,
            fetch: self.fetch,
            plan_out: None, // `run` above resolves + writes the path itself either way
        })
    }
}

/// (#2310 P4d; #2404 P4d round 3) Build the one-source [`WorkspaceSpec`] a
/// `github` + `head_sha` launch implies: a single `git` source at the
/// repository's GitHub URL, pinned to the bare `head_sha`. `github` is
/// accepted as `owner/repo` or as a full URL (with or without a trailing
/// `.git`); anything else is refused by name. Pure — it builds the spec
/// and nothing else; the caller materializes it through the same path an
/// on-disk spec takes.
///
/// **The ref is always the bare `head_sha`, never a `refs/pull/<pr>/head`
/// literal.** A pull request FROM A FORK has no branch in the reviewed
/// repository, so a naive `git rev-parse <sha>` against a mirror that only
/// fetched `+refs/heads/*` fails with git's opaque "Needed a single
/// revision". That is solved one layer down instead: the mirror
/// `workspace_spec::materialize` maintains no longer fetches
/// `+refs/pull/*/head` unconditionally (#2404 P4d round 3 — the measured 739MB
/// cost per mirror was not worth paying on every same-repo source that
/// never needs the pull namespace) — a rev-parse miss against the
/// ordinary heads+tags mirror triggers exactly ONE miss-recovery fetch of
/// the pull-heads namespace instead, and (round 4) that recovery fetch is
/// a one-shot `git fetch` ARGUMENT that never gets persisted into the
/// mirror's own `remote.origin.fetch` config — so a fork sha is still
/// reachable by the time anything tries to resolve it, without paying the
/// cost again on every later ordinary fetch of that mirror. Verified live
/// against a real fork PR (`git rev-parse <fork-sha>^{commit}` resolves
/// after that miss-recovery fetch). Naming a `refs/pull/
/// <pr>/head` literal here would pin this spec to a MOVING ref (the PR's
/// head can force-push after this spec is minted, changing what materializes
/// on a later re-run at the "same" spec); the bare sha stays a fixed point
/// regardless of what the PR does afterward, and the fetch above is what
/// makes that fixed point reachable.
pub(crate) fn derive_workspace_spec(github: &str, head_sha: &str) -> Result<WorkspaceSpec> {
    let trimmed = github.trim().trim_end_matches('/');
    let slug = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .or_else(|| trimmed.strip_prefix("git@github.com:"))
        .unwrap_or(trimmed)
        .trim_end_matches(".git");
    let mut parts = slug.split('/');
    let (owner, repo) = match (parts.next(), parts.next(), parts.next()) {
        (Some(o), Some(r), None) if !o.is_empty() && !r.is_empty() => (o, r),
        _ => {
            anyhow::bail!(
                "`{PLAN_SITES_KIND}`: config.github must be `owner/repo` or a GitHub URL, got {github:?}"
            )
        }
    };
    let head_sha = head_sha.trim();
    if head_sha.is_empty() {
        anyhow::bail!("`{PLAN_SITES_KIND}`: config.head_sha must not be empty");
    }
    Ok(WorkspaceSpec {
        schema_version: None,
        name: Some(repo.to_string()),
        root: None,
        sources: vec![darkmux_crew::workspace_spec::SourceSpec {
            id: repo.to_string(),
            git: Some(format!("https://github.com/{owner}/{repo}")),
            path: None,
            git_ref: Some(head_sha.to_string()),
            extras: BTreeMap::new(),
        }],
        include: None,
        exclude: None,
        edges: Vec::new(),
        rules: Vec::new(),
        extras: BTreeMap::new(),
    })
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

    let (spec, spec_warnings) = cfg.resolve_spec()?;
    for w in &spec_warnings {
        eprintln!("[darkmux] warning: plan.sites: {w}");
    }
    // (#2399) INVARIANT: `materialized` OWNS the workspace lock, and
    // `plan_diff_rule` below reads every file in the trees it names. The
    // binding must stay alive across that call — see the same note in
    // `plan_step.rs`; this is the step #2397 made 8-wide, so it is the one
    // that races.
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

    /// (#2310 P4d — RED before the derivation landed: `from_step` required
    /// `config.workspace` unconditionally, so the workflow's own params
    /// (`github` + `head_sha`, no workspace) were refused at parse time.)
    /// The derived spec is exactly one source, pointing at the repository's
    /// GitHub URL, at the launch's `head_sha`. Pure — no materialization,
    /// no network.
    #[test]
    fn a_github_slug_and_head_sha_derive_one_source_at_that_sha() {
        let spec = derive_workspace_spec("kstrat2001/darkmux", "abc123def").unwrap();
        assert_eq!(spec.sources.len(), 1, "exactly one source: {:?}", spec.sources);
        let src = &spec.sources[0];
        assert_eq!(src.git.as_deref(), Some("https://github.com/kstrat2001/darkmux"));
        assert_eq!(src.path, None, "a derived source is a git origin, never a local path");
        assert_eq!(src.resolved_ref(), "abc123def");
    }

    /// The workflow passes `github` as `$REPO` (`owner/repo`), but an
    /// operator pasting a browser URL must resolve to the SAME origin.
    #[test]
    fn a_full_github_url_derives_the_same_origin_as_the_slug() {
        let from_url = derive_workspace_spec("https://github.com/kstrat2001/darkmux.git", "sha1").unwrap();
        let from_slug = derive_workspace_spec("kstrat2001/darkmux", "sha1").unwrap();
        assert_eq!(from_url.sources[0].git, from_slug.sources[0].git);
    }

    #[test]
    fn a_github_value_that_is_not_owner_slash_repo_is_refused_by_name() {
        let err = derive_workspace_spec("darkmux", "sha1").unwrap_err();
        assert!(err.to_string().contains("owner/repo"), "{err}");
    }

    /// The workflow's own invocation shape: no `workspace`, but `github` +
    /// `head_sha` present. Must parse, and must resolve to the derived
    /// one-source spec.
    #[test]
    fn no_workspace_with_github_and_head_sha_parses_and_resolves_to_the_derived_spec() {
        let cfg = SitesStepConfig::from_step(&step(serde_json::json!({
            "rule": "swallowed-error", "source": "diff", "diff_file": "/tmp/d.diff",
            "github": "kstrat2001/darkmux", "head_sha": "abc123def"
        })))
        .unwrap();
        assert!(cfg.workspace.is_none());
        let (spec, warnings) = cfg.resolve_spec().unwrap();
        assert!(warnings.is_empty());
        assert_eq!(spec.sources.len(), 1);
        assert_eq!(spec.sources[0].resolved_ref(), "abc123def");
    }

    /// An explicit `workspace` is never overridden by the derivation.
    #[test]
    fn an_explicit_workspace_wins_over_the_github_derivation() {
        let cfg = SitesStepConfig::from_step(&step(serde_json::json!({
            "rule": "swallowed-error", "workspace": "/tmp/ws.json", "source": "diff",
            "diff_file": "/tmp/d.diff", "github": "kstrat2001/darkmux", "head_sha": "abc123def"
        })))
        .unwrap();
        assert_eq!(cfg.workspace.as_deref(), Some(std::path::Path::new("/tmp/ws.json")));
    }

    #[test]
    fn neither_workspace_nor_github_head_sha_is_refused_naming_both_ways_in() {
        let err = SitesStepConfig::from_step(&step(serde_json::json!({
            "rule": "swallowed-error", "source": "diff", "diff_file": "/tmp/d.diff"
        })))
        .unwrap_err();
        assert!(err.to_string().contains("config.workspace"), "{err}");
        assert!(err.to_string().contains("config.github"), "{err}");
    }

    #[test]
    fn a_tree_source_with_no_workspace_is_refused_naming_workspace() {
        let cfg = SitesStepConfig::from_step(&step(serde_json::json!({
            "rule": "swallowed-error", "github": "kstrat2001/darkmux", "head_sha": "abc"
        })))
        .unwrap();
        let err = cfg.as_tree_config().unwrap_err();
        assert!(err.to_string().contains("config.workspace"), "{err}");
    }


    #[test]
    fn the_kind_registers_beside_the_builtins() {
        let registry = StepKindRegistry::with_builtins();
        register(&registry).unwrap();
        assert!(registry.get(PLAN_SITES_KIND).is_ok());
    }
}
