//! One config struct per step kind. Each is the ONLY reader of its kind's
//! `Step.config`: the unknown-key gate derives its schema from the struct
//! (`schemars`), and the kind loads through [`super::load`], so a key the
//! gate accepts is a key the load accepts.
//!
//! Every struct is closed to the gate (a key it does not name is refused)
//! but tolerant at load. A task's `grow.config` merges every key into EVERY
//! step of its copies, and the scheduler stamps `grown_from`, so a step
//! legitimately carries keys a sibling step's kind reads; `deny_unknown_fields`
//! would fail those loads. The gate checks each kind against only the grow
//! keys that kind names (`super::gate`).
//!
//! Numbers and flags are [`Count`] and [`Flag`]: a `--param` value reaches a
//! step as text. A field that holds an open value says so in its own doc.

use crate::brief_refs::BriefRef;
use darkmux_types::param_scalar::{BlankableCount, Count, Flag};
use darkmux_types::{ModelEndpoint, session_id::SessionId};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

/// A model endpoint a step names: an id from the profile registry's
/// `endpoints`, or an inline endpoint definition.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum EndpointRef {
    Id(String),
    Inline(Box<ModelEndpoint>),
}

/// `dispatch.internal`: a full agentic dispatch of one role. The role, profile,
/// workdir and image come from the owning task first and from these keys only
/// when the task leaves them unset. The keys after `resume_from` are set by
/// darkmux's own launchers (the `darkmux dispatch` crew-of-one graph), not
/// authored.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct DispatchInternalConfig {
    pub role_id: Option<String>,
    /// The base prompt; prior-dependency output is prepended.
    pub message: Option<String>,
    pub timeout_seconds: Option<Count>,
    pub profile_name: Option<String>,
    pub image: Option<String>,
    /// `--profiles-file` passthrough.
    pub config_path: Option<String>,
    pub workdir: Option<String>,
    pub phase_id: Option<String>,
    pub session_id: Option<SessionId>,
    /// Records the brief carries, by store and key.
    pub brief_refs: Option<Vec<BriefRef>>,
    pub parse_verifiers: Option<Flag>,
    pub preserve_dispatch_result: Option<Flag>,
    pub skip_preflight: Option<Flag>,
    pub json: Option<Flag>,
    pub max_completion_tokens: Option<Count>,
    pub resume_from: Option<String>,
    pub timeout_override_seconds: Option<Count>,
}

/// What a one-model-call kind (`dispatch.single_shot`, `dispatch.map`) reads
/// besides its prompt.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ModelCallConfig {
    pub model: String,
    pub system: Option<String>,
    pub max_tokens: Option<Count>,
    pub timeout_seconds: Option<Count>,
    /// Local dialect only.
    pub temperature: Option<f64>,
    /// Present: the hosted dialect against this endpoint.
    pub endpoint: Option<EndpointRef>,
    /// Residency hints for a local model: the context length to load at, the
    /// loaded identifier, and the loadable key when it differs from `model`.
    pub n_ctx: Option<Count>,
    pub identifier: Option<String>,
    pub model_key: Option<String>,
    /// `--profiles-file` passthrough.
    pub config_path: Option<String>,
}

/// `dispatch.single_shot`: one chat-completions call, no agent loop.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SingleShotConfig {
    /// The base user message; prior-dependency output is prepended.
    pub user: Option<String>,
    #[serde(flatten)]
    pub call: ModelCallConfig,
}

/// Where a `dispatch.map` step's items come from.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct MapSource {
    /// The items, inline. An OPEN list: each item is data (a value, or a
    /// `{"system", "item"}` object), never config. Absent, the collection
    /// comes from a dependency's output.
    pub collection: Option<Vec<Value>>,
    /// Which dependency input carries the collection.
    pub collection_input: Option<String>,
}

/// The token allowance a `dispatch.map` step draws from.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct BucketSpec {
    /// Steps naming the same group share one token allowance.
    pub bucket_group: Option<String>,
    /// The allowance a launcher stamps for the step's group.
    pub bucket_budget: Option<Count>,
}

/// `dispatch.map`: one single-shot call per item of a collection.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MapConfig {
    /// The per-item prompt; `{item}` is replaced by the item.
    pub user_template: String,
    #[serde(flatten)]
    pub source: MapSource,
    pub retry_on_empty: Option<Count>,
    pub retry_on_error: Option<Count>,
    #[serde(flatten)]
    pub bucket: BucketSpec,
    #[serde(flatten)]
    pub call: ModelCallConfig,
}

/// `procedural.shell`: one shell command.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ShellConfig {
    pub command: String,
    /// Where the command runs; outranks the task's `workdir`.
    pub cwd: Option<String>,
    /// Where the command runs when the task names none.
    pub workdir: Option<String>,
}

/// `procedural.noop`: returns a fixed string.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct NoopConfig {
    /// The output; defaults to the step's id.
    pub output: Option<String>,
}

/// `mods.gate`: confirm the mods that name a finding.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ModsGateConfig {
    /// The finding key whose mods are gated.
    pub for_key: String,
    /// Run against a scratch copy with the mod's kit applied; absent, every
    /// mod records a skip.
    pub test_command: Option<String>,
    /// The checkout the kit is applied against.
    pub workdir: Option<String>,
}

/// `records.gather`: collect a run's findings and mods for delivery.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct RecordsGatherConfig {
    pub diff_file: Option<String>,
    pub not_attempted: Option<Vec<String>>,
}

/// `deliver.github_review`: render a review payload. `findings`, `mods` and
/// `diff` come embedded here as a group, or from a `records.gather` step's
/// output; `attribution`, `emit` and `head_sha` are always read from here.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct DeliverGithubReviewConfig {
    pub attribution: Option<String>,
    /// Where the payload is written; absent or `-`, stdout.
    pub emit: Option<String>,
    pub head_sha: Option<String>,
    /// OPEN: an array of finding records, checked by the record type when
    /// the step loads.
    pub findings: Option<Value>,
    /// OPEN: an array of gated mods, checked by the record type when the
    /// step loads.
    pub mods: Option<Value>,
    pub diff: Option<String>,
    /// OPEN: a delivery scope object, checked by its type when the step
    /// loads.
    pub scope: Option<Value>,
}

/// How a planning step sizes its work units.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct Sizing {
    pub max_sites_per_unit: Option<Count>,
    pub max_est_tokens_per_unit: Option<Count>,
}

/// What every planning kind reads.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PlanCommon {
    pub rule: String,
    pub sizing: Option<Sizing>,
    /// Skip fetching the workspace's remotes.
    pub no_fetch: Option<Flag>,
    /// Where the plan is written; absent, under the run.
    pub plan_out: Option<String>,
}

/// `crawl.plan`: plan one rule over a workspace tree.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CrawlPlanConfig {
    pub workspace: String,
    #[serde(flatten)]
    pub common: PlanCommon,
}

/// Where `plan.sites` reads its sites from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SitesSource {
    Tree,
    Diff,
}

/// `plan.sites`: plan one rule over a workspace tree or a diff.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PlanSitesConfig {
    /// A workspace spec path; absent, derived from `github` + `head_sha`.
    pub workspace: Option<String>,
    /// Defaults to the tree.
    pub source: Option<SitesSource>,
    /// Required when `source` is the diff.
    pub diff_file: Option<String>,
    pub head_sha: Option<String>,
    /// `owner/repo` or a GitHub URL.
    pub github: Option<String>,
    #[serde(flatten)]
    pub common: PlanCommon,
}

/// `crawl.unit`: dispatch one planned unit.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CrawlUnitConfig {
    /// The plan file the unit is in.
    pub plan: String,
    pub unit: String,
    pub rule: Option<String>,
    pub no_progress_turns: Option<Count>,
    /// Blank or `null` means the standing timeout resolution applies.
    pub timeout_seconds: Option<BlankableCount>,
    pub intent_file: Option<String>,
    pub draws: Option<Count>,
}

/// `crawl.summary`, `mission.worktree` and `mission.verify` read no config.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoConfig {}

/// `mission.coder`: every key is stamped by the launcher.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct MissionCoderConfig {
    pub timeout_seconds: Option<Count>,
    pub image: Option<String>,
    pub injected_budget_chars: Option<Count>,
}
