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
//! Numbers and flags are [`Count`], [`Decimal`] and [`Flag`]: a `--param`
//! value reaches a step as text. A field that holds an open value says so in its own doc.

use super::{require_text, ConfigRules, RuleViolation};
use crate::brief_refs::{BriefRef, BriefRefKind};
use darkmux_types::param_scalar::{BlankableCount, Count, Decimal, Flag};
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

/// Kinds whose config has no rule beyond its types.
macro_rules! no_rules {
    ($($config:ty),+) => { $(impl ConfigRules for $config {})+ };
}

no_rules!(
    SingleShotConfig,
    ShellConfig,
    NoopConfig,
    RecordsGatherConfig,
    NoConfig,
    MissionCoderConfig
);

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

impl ConfigRules for DispatchInternalConfig {
    /// A ref of a kind this build does not name is a typo in the config, not
    /// an archive to read tolerantly: refuse it before anything runs.
    fn check(&self) -> Result<(), RuleViolation> {
        let unknown = self.brief_refs.iter().flatten().any(|r| r.kind == BriefRefKind::Unknown);
        if unknown {
            return Err(RuleViolation::new("brief_refs", "a ref's kind is `finding` or `mod`"));
        }
        Ok(())
    }
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
    pub temperature: Option<Decimal>,
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

impl ModelCallConfig {
    /// The sampling temperature: the configured one, else 0.7.
    pub fn temperature(&self) -> f32 {
        self.temperature.map_or(0.7, |t| t.0) as f32
    }
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
    pub call: ModelCallConfig,
}

impl ConfigRules for MapConfig {
    fn check(&self) -> Result<(), RuleViolation> {
        for (key, budget) in [("retry_on_empty", self.retry_on_empty), ("retry_on_error", self.retry_on_error)] {
            if let Some(n) = budget.filter(|n| n.as_u32().is_none()) {
                return Err(RuleViolation::new(key, format!("({}) exceeds the maximum of {}", n.0, u32::MAX)));
            }
        }
        Ok(())
    }
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

impl ConfigRules for ModsGateConfig {
    fn check(&self) -> Result<(), RuleViolation> {
        require_text("for_key", &self.for_key)
    }
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

impl Sizing {
    /// The two limits, each `None` when unset. A limit is a positive integer
    /// that fits a `usize`.
    pub fn limits(&self) -> Result<(Option<usize>, Option<usize>), RuleViolation> {
        let limit = |key: &str, count: Option<Count>| {
            let Some(count) = count else { return Ok(None) };
            count
                .as_usize()
                .filter(|n| *n > 0)
                .map(Some)
                .ok_or_else(|| RuleViolation::new(key, format!("must be a positive integer, got {}", count.0)))
        };
        Ok((
            limit("sizing.max_sites_per_unit", self.max_sites_per_unit)?,
            limit("sizing.max_est_tokens_per_unit", self.max_est_tokens_per_unit)?,
        ))
    }
}

/// A rule id that names a file: every `+`-joined part is one safe path
/// component ([`crate::rules::first_unsafe_rule_part`]).
fn require_safe_rule(key: &str, rule: &str) -> Result<(), RuleViolation> {
    match crate::rules::first_unsafe_rule_part(rule) {
        Some(part) => Err(RuleViolation::new(
            key,
            format!("`{part}` is not a safe path component: {}", crate::rules::SAFE_RULE_ID_SHAPE),
        )),
        None => Ok(()),
    }
}

/// The `(owner, repo)` a `github` value names: `owner/repo`, or a GitHub URL
/// with or without a trailing `.git`.
pub fn github_repo(github: &str) -> Option<(&str, &str)> {
    let trimmed = github.trim().trim_end_matches('/');
    let slug = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .or_else(|| trimmed.strip_prefix("git@github.com:"))
        .unwrap_or(trimmed)
        .trim_end_matches(".git");
    let mut parts = slug.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(repo), None) if !owner.is_empty() && !repo.is_empty() => Some((owner, repo)),
        _ => None,
    }
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

impl ConfigRules for PlanCommon {
    fn check(&self) -> Result<(), RuleViolation> {
        require_text("rule", &self.rule)?;
        require_safe_rule("rule", &self.rule)?;
        self.sizing.as_ref().map_or(Ok(()), |s| s.limits().map(|_| ()))
    }
}

/// `crawl.plan`: plan one rule over a workspace tree.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CrawlPlanConfig {
    pub workspace: String,
    #[serde(flatten)]
    pub common: PlanCommon,
}

impl ConfigRules for CrawlPlanConfig {
    fn check(&self) -> Result<(), RuleViolation> {
        self.common.check()?;
        require_text("workspace", &self.workspace)
    }
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

impl PlanSitesConfig {
    fn named(text: &Option<String>) -> Option<&str> {
        text.as_deref().filter(|t| !t.trim().is_empty())
    }
}

impl ConfigRules for PlanSitesConfig {
    fn check(&self) -> Result<(), RuleViolation> {
        self.common.check()?;
        let source = self.source.unwrap_or(SitesSource::Tree);
        let workspace = Self::named(&self.workspace);
        let derivable = Self::named(&self.head_sha).zip(Self::named(&self.github));
        match (source, workspace, derivable) {
            (SitesSource::Tree, None, _) => {
                return Err(RuleViolation::new(
                    "workspace",
                    "is required when `source` is the tree (the default): config.github and config.head_sha derive a workspace for a diff only",
                ));
            }
            (SitesSource::Diff, None, None) => {
                return Err(RuleViolation::new(
                    "workspace",
                    "is required, or set both config.github and config.head_sha to derive one",
                ));
            }
            (SitesSource::Diff, None, Some((_, github))) if github_repo(github).is_none() => {
                return Err(RuleViolation::new(
                    "github",
                    format!("must be `owner/repo` or a GitHub URL, got {github:?}"),
                ));
            }
            _ => {}
        }
        if source == SitesSource::Diff && self.diff_file.is_none() {
            return Err(RuleViolation::new("diff_file", "is required when `source` is \"diff\""));
        }
        Ok(())
    }
}

/// The largest `draws` a `dispatch.unit` config may name.
///
/// A draw is a WHOLE extra dispatch of the same unit: its own container, its
/// own turn budget, its own tokens. So `draws` multiplies a run's cost
/// linearly with nothing else in the pipeline bounding it. A mistyped
/// `--param draws=80` is not a slightly more expensive run; it is an 80x one
/// whose first visible symptom is an operator watching the same unit go
/// round for an hour. Refused when the config is read rather than clamped:
/// a silently lowered value would make the run's own `draws` a lie.
///
/// 8 is the ceiling because the technique this ports (the retired funnel's
/// k-draw recall) never measured past a handful of draws.
pub const MAX_UNIT_DRAWS: usize = 8;

/// `dispatch.unit`: dispatch one planned unit.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DispatchUnitConfig {
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

impl ConfigRules for DispatchUnitConfig {
    fn check(&self) -> Result<(), RuleViolation> {
        require_text("plan", &self.plan)?;
        require_text("unit", &self.unit)?;
        if let Some(rule) = &self.rule {
            require_safe_rule("rule", rule)?;
        }
        if let Some(t) = self.timeout_seconds.and_then(|t| t.0).filter(|t| t.0 < 1) {
            return Err(RuleViolation::new(
                "timeout_seconds",
                format!(
                    "must be >= 1, got {}: `0` resolves to an already-expired inactivity deadline (an instant kill), \
                     not 'unbounded'. Omit it for the standing default, or set a real positive bound",
                    t.0
                ),
            ));
        }
        match self.draws {
            Some(n) if n.0 < 1 => Err(RuleViolation::new("draws", format!("must be >= 1, got {}", n.0))),
            Some(n) if n.0 > MAX_UNIT_DRAWS as u64 => Err(RuleViolation::new(
                "draws",
                format!(
                    "is {}, above the cap of {MAX_UNIT_DRAWS}: every draw is a whole extra dispatch of this same \
                     unit, so this run would cost {}x its own wall clock and tokens. Lower `draws`, or split \
                     the work across units",
                    n.0, n.0
                ),
            )),
            _ => Ok(()),
        }
    }
}

/// `dispatch.summary`, `mission.worktree` and `mission.verify` read no config.
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
