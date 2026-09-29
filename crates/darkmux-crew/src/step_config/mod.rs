//! Typed config per step kind.
//!
//! A mission step's `config` is a JSON object whose keys belong to its
//! `kind`. Each shipped kind has ONE struct ([`kinds`]) that is both its
//! schema and its only reader: the unknown-key gate ([`gate`]) checks a
//! mission config's step configs against the struct its `kind` selects, and
//! the kind loads its config through [`load`]. A typo, a wrong type or a
//! missing required key inside a step's config is refused before anything
//! runs, naming the file, the key path and the closest valid key.
//!
//! [`ConfigKind`] is the closed set of kinds with a config struct. Its ids
//! are THE ids: each kind's `id()` and its `*_KIND` constant come from
//! [`ConfigKind::id`], and the registry conformance test proves every
//! registered kind is here and every one here is registered.

pub mod gate;
pub mod kinds;

pub use kinds::*;

use crate::types::Step;
use anyhow::{anyhow, Result};
use darkmux_types::user_files::{key_issues_at, no_retired, top_level_keys, Issue, KeyIssue};
use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};
use std::borrow::Cow;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

/// Every step kind that has a config struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfigKind {
    DispatchInternal,
    DispatchSingleShot,
    DispatchMap,
    ProceduralShell,
    ProceduralNoop,
    ModsGate,
    RecordsGather,
    DeliverGithubReview,
    CrawlPlan,
    PlanSites,
    CrawlUnit,
    CrawlSummary,
    MissionWorktree,
    MissionCoder,
    MissionVerify,
}

/// Run `$body` with `$t` bound to the config struct of `$kind`. The one place
/// that pairs a [`ConfigKind`] with its struct.
macro_rules! with_config_type {
    ($kind:expr, $t:ident => $body:expr) => {
        match $kind {
            ConfigKind::DispatchInternal => { type $t = DispatchInternalConfig; $body }
            ConfigKind::DispatchSingleShot => { type $t = SingleShotConfig; $body }
            ConfigKind::DispatchMap => { type $t = MapConfig; $body }
            ConfigKind::ProceduralShell => { type $t = ShellConfig; $body }
            ConfigKind::ProceduralNoop => { type $t = NoopConfig; $body }
            ConfigKind::ModsGate => { type $t = ModsGateConfig; $body }
            ConfigKind::RecordsGather => { type $t = RecordsGatherConfig; $body }
            ConfigKind::DeliverGithubReview => { type $t = DeliverGithubReviewConfig; $body }
            ConfigKind::CrawlPlan => { type $t = CrawlPlanConfig; $body }
            ConfigKind::PlanSites => { type $t = PlanSitesConfig; $body }
            ConfigKind::CrawlUnit => { type $t = CrawlUnitConfig; $body }
            ConfigKind::MissionCoder => { type $t = MissionCoderConfig; $body }
            ConfigKind::CrawlSummary | ConfigKind::MissionWorktree | ConfigKind::MissionVerify => {
                type $t = NoConfig;
                $body
            }
        }
    };
}

impl ConfigKind {
    pub const ALL: [ConfigKind; 15] = [
        Self::DispatchInternal,
        Self::DispatchSingleShot,
        Self::DispatchMap,
        Self::ProceduralShell,
        Self::ProceduralNoop,
        Self::ModsGate,
        Self::RecordsGather,
        Self::DeliverGithubReview,
        Self::CrawlPlan,
        Self::PlanSites,
        Self::CrawlUnit,
        Self::CrawlSummary,
        Self::MissionWorktree,
        Self::MissionCoder,
        Self::MissionVerify,
    ];

    /// The kind's registry id, the string a mission config's `kind` holds.
    pub const fn id(self) -> &'static str {
        match self {
            Self::DispatchInternal => "dispatch.internal",
            Self::DispatchSingleShot => "dispatch.single_shot",
            Self::DispatchMap => "dispatch.map",
            Self::ProceduralShell => "procedural.shell",
            Self::ProceduralNoop => "procedural.noop",
            Self::ModsGate => "mods.gate",
            Self::RecordsGather => "records.gather",
            Self::DeliverGithubReview => "deliver.github_review",
            Self::CrawlPlan => "crawl.plan",
            Self::PlanSites => "plan.sites",
            Self::CrawlUnit => "crawl.unit",
            Self::CrawlSummary => "crawl.summary",
            Self::MissionWorktree => "mission.worktree",
            Self::MissionCoder => "mission.coder",
            Self::MissionVerify => "mission.verify",
        }
    }

    /// The kind a registry id names, parsed once here.
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.id() == id)
    }

    /// Every issue in `config` (`Null` reads as an empty object) against this
    /// kind's struct, with paths written under `prefix`.
    pub fn issues(self, config: &Value, prefix: &str) -> Vec<KeyIssue> {
        let empty = Value::Object(Default::default());
        let config = if config.is_null() { &empty } else { config };
        with_config_type!(self, T => key_issues_at::<T>(config, &no_retired, prefix))
    }

    /// The top-level keys this kind's config names.
    pub fn keys(self) -> Vec<String> {
        with_config_type!(self, T => top_level_keys::<T>())
    }

    /// Whether `config` loads as this kind's struct: what the kind's own
    /// `load` will decide, without running anything.
    pub fn loads(self, config: &Value) -> Result<(), String> {
        let empty = Value::Object(Default::default());
        let config = if config.is_null() { &empty } else { config };
        with_config_type!(self, T => serde_json::from_value::<T>(config.clone()).map(|_| ()).map_err(|e| e.to_string()))
    }
}

/// `step`'s config as `T`, `Null` reading as an empty object. The one way a
/// step kind reads its config. A load that fails says which keys are wrong or
/// missing (the gate's own wording, so a run and a preflight name them the
/// same), else the parse error.
pub fn load<T: DeserializeOwned>(step: &Step, kind: ConfigKind) -> Result<T> {
    let empty = Value::Object(Default::default());
    let config = if step.config.is_null() { &empty } else { &step.config };
    T::deserialize(config).map_err(|parse| {
        let named: Vec<String> = kind
            .issues(config, "config")
            .into_iter()
            .filter(|i| matches!(i.issue, Issue::WrongType { .. } | Issue::Missing { .. }))
            .map(|i| i.to_string())
            .collect();
        let why = if named.is_empty() { parse.to_string() } else { named.join("; ") };
        anyhow!("step `{}`: `{}` config: {why}", step.id, kind.id())
    })
}

/// `text` without surrounding blanks, `None` when nothing is left: a
/// `{{param}}` nothing supplied renders as an empty string, which every
/// optional text key reads as absent.
pub fn non_blank(text: Option<String>) -> Option<String> {
    text.filter(|t| !t.trim().is_empty())
}

/// The schema of a step's `kind`: exactly the ids in [`ConfigKind::ALL`].
impl JsonSchema for ConfigKind {
    fn schema_name() -> Cow<'static, str> {
        "ConfigKind".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "string", "enum": Self::ALL.map(Self::id)})
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;

/// The token allowance group a `dispatch.map` step names, with the budget a
/// launcher stamped for it. Only that kind meters through a group. Read
/// tolerantly: the scheduler resolves the group before the kind loads (and
/// refuses) the rest of the config.
pub fn bucket_of(kind: &str, config: &Value) -> Option<(String, Option<u64>)> {
    if ConfigKind::from_id(kind)? != ConfigKind::DispatchMap {
        return None;
    }
    let spec = BucketSpec::deserialize(config).ok()?;
    Some((spec.bucket_group?, spec.bucket_budget.map(|c| c.0)))
}

/// The model-call keys of a `dispatch.single_shot` / `dispatch.map` step.
pub fn model_call(kind: &str, config: &Value) -> Option<ModelCallConfig> {
    match ConfigKind::from_id(kind)? {
        ConfigKind::DispatchSingleShot => SingleShotConfig::deserialize(config).ok().map(|c| c.call),
        ConfigKind::DispatchMap => MapConfig::deserialize(config).ok().map(|c| c.call),
        _ => None,
    }
}

/// The workspace spec a planning step names. Read tolerantly, because the
/// launch preflight asks of a document whose other keys still hold
/// `{{param}}` references.
pub fn workspace_of(kind: &str, config: &Value) -> Option<String> {
    #[derive(Deserialize)]
    struct WorkspaceRef {
        workspace: Option<String>,
    }
    match ConfigKind::from_id(kind)? {
        ConfigKind::CrawlPlan | ConfigKind::PlanSites => WorkspaceRef::deserialize(config).ok()?.workspace,
        _ => None,
    }
}

/// The role and profile a `dispatch.internal` step's own config names (a
/// task's own assignment outranks these). Read tolerantly: a gate that asks
/// who staffs a step must not stop at a bad value elsewhere in its config,
/// which the step's own load refuses.
pub fn dispatch_staffing(kind: &str, config: &Value) -> (Option<String>, Option<String>) {
    #[derive(Deserialize)]
    struct StaffingRef {
        role_id: Option<String>,
        profile_name: Option<String>,
    }
    match ConfigKind::from_id(kind) {
        Some(ConfigKind::DispatchInternal) => {
            StaffingRef::deserialize(config).map(|c| (c.role_id, c.profile_name)).unwrap_or_default()
        }
        _ => (None, None),
    }
}

/// What a run's records call a crawl step's work by. Read tolerantly (a bad
/// value elsewhere in the config does not hide it): it labels a step that
/// may have failed BECAUSE its config was bad.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CrawlIdentity {
    pub rule: Option<String>,
    pub unit: Option<String>,
}

/// The rule and unit a `crawl.plan` / `plan.sites` / `crawl.unit` step names.
pub fn crawl_identity(kind: &str, config: &Value) -> CrawlIdentity {
    match ConfigKind::from_id(kind) {
        Some(ConfigKind::CrawlPlan | ConfigKind::PlanSites | ConfigKind::CrawlUnit) => {
            CrawlIdentity::deserialize(config).unwrap_or_default()
        }
        _ => CrawlIdentity::default(),
    }
}

/// Whether a `procedural.shell` step's config names its own directory
/// (`cwd` or `workdir`, an empty string included: an authored key is an
/// authored key).
pub fn shell_names_a_directory(config: &Value) -> bool {
    #[derive(Deserialize)]
    struct Directories {
        cwd: Option<String>,
        workdir: Option<String>,
    }
    Directories::deserialize(config).is_ok_and(|d| d.cwd.is_some() || d.workdir.is_some())
}
