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
//! A struct also owns the kind's VALUE rules ([`ConfigRules`]): the bounds and
//! required-together keys the kind refuses at run time although the keys have
//! the right types (a `draws` of 0, a diff plan with no diff file). The kind's
//! own reader calls the same rule function the gate does, so a launch refuses
//! such a config before anything runs: on the document itself where no
//! `{{param}}` is involved (`gate`), and again once a launch's params are
//! substituted (`gate::check_resolved`). What no config can decide
//! alone stays a run-time refusal: a role named by neither the task nor the
//! config, a profile-registry endpoint id, a directory or file that must
//! exist, and a collection read from a dependency's output.
//!
//! [`ConfigKind`] is the closed set of kinds with a config struct. Its ids
//! are THE ids: each kind's `id()` and its `*_KIND` constant come from
//! [`ConfigKind::id`], and the registry conformance test proves every
//! registered kind is here and every one here is registered.

pub mod gate;
pub mod kinds;
#[cfg(any(test, feature = "test-support"))]
#[path = "sweep_tests.rs"]
pub mod sweep;

pub use kinds::*;

use crate::types::Step;
use anyhow::{anyhow, Result};
use darkmux_types::param_scalar::is_placeholder;
use darkmux_types::user_files::{key_issues_at, no_retired, top_level_keys, Issue, KeyIssue};
use std::fmt;
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
    DispatchUnit,
    DispatchSummary,
    MissionWorktree,
    MissionCoder,
    MissionVerify,
}

/// A step kind id as this build spells it: a retired id (see
/// [`ConfigKind::replacing`]) maps to its replacement, any other id is
/// returned unchanged. Applied where a step record is READ from disk, so an
/// archived mission still opens; nothing writes the old id back.
pub fn current_kind_id(id: &str) -> &str {
    match ConfigKind::replacing(id) {
        Some(kind) => kind.id(),
        None => id,
    }
}

/// A value a step kind refuses although its type is right: the key (dotted,
/// under `config`) and the rule it breaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleViolation {
    pub key: String,
    pub rule: String,
}

impl RuleViolation {
    pub fn new(key: &str, rule: impl Into<String>) -> Self {
        Self { key: key.to_string(), rule: rule.into() }
    }
}

impl fmt::Display for RuleViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "config.{} {}", self.key, self.rule)
    }
}

/// The value rules of one kind's config, checked by its own reader and by
/// the gate. The default is "no rules beyond the types".
pub trait ConfigRules {
    fn check(&self) -> Result<(), RuleViolation> {
        Ok(())
    }
}

/// A text key that must say something: not empty, not blank.
pub fn require_text(key: &str, value: &str) -> Result<(), RuleViolation> {
    non_blank(Some(value.to_string())).map(|_| ()).ok_or_else(|| RuleViolation::new(key, "must not be blank"))
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
            ConfigKind::DispatchUnit => { type $t = DispatchUnitConfig; $body }
            ConfigKind::MissionCoder => { type $t = MissionCoderConfig; $body }
            ConfigKind::DispatchSummary | ConfigKind::MissionWorktree | ConfigKind::MissionVerify => {
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
        Self::DispatchUnit,
        Self::DispatchSummary,
        Self::MissionWorktree,
        Self::MissionCoder,
        Self::MissionVerify,
    ];

    /// The kind's registry id, the string a mission config's `kind` holds.
    pub const fn id(self) -> &'static str {
        Self::IDS[self as usize]
    }

    /// Registry ids in declaration order, so `id` is one index instead of a
    /// fifteen-arm match. `ids_are_in_declaration_order` pins the order
    /// against [`Self::ALL`].
    const IDS: [&'static str; 15] = [
        "dispatch.internal",
        "dispatch.single_shot",
        "dispatch.map",
        "procedural.shell",
        "procedural.noop",
        "mods.gate",
        "records.gather",
        "deliver.github_review",
        "crawl.plan",
        "plan.sites",
        "dispatch.unit",
        "dispatch.summary",
        "mission.worktree",
        "mission.coder",
        "mission.verify",
    ];

    /// The kind a registry id names, parsed once here.
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.id() == id)
    }

    /// The kind that replaced a retired registry id (#2430: `crawl.unit` and
    /// `crawl.summary` became `dispatch.unit` and `dispatch.summary`), or
    /// `None` for an id that was never retired. The ONE place the old
    /// spellings live. A config naming one is refused, naming the new id
    /// (`MissionConfig::validate_with`); a record an old run left on disk is
    /// read through [`current_kind_id`] and never written back.
    pub fn replacing(retired_id: &str) -> Option<Self> {
        Self::RETIRED.iter().find(|(old, _)| *old == retired_id).map(|(_, new)| *new)
    }

    /// Every retired registry id and the kind that replaced it (#2430).
    pub const RETIRED: [(&'static str, ConfigKind); 2] =
        [("crawl.unit", Self::DispatchUnit), ("crawl.summary", Self::DispatchSummary)];

    /// The exact edit that upgrades a config file naming retired ids, for a
    /// refusal or a doctor row to print: every old id and its replacement.
    pub fn retired_fix() -> String {
        let pairs: Vec<String> = Self::RETIRED.iter().map(|(old, new)| format!("`{old}` to `{}`", new.id())).collect();
        format!("rename {}", pairs.join(" and "))
    }

    /// Every schema issue in `config` (`Null` reads as an empty object)
    /// against this kind's struct, with paths written under `prefix`. A
    /// config that is neither an object nor null is one wrong-type issue at
    /// `prefix`.
    pub fn issues(self, config: &Value, prefix: &str) -> Vec<KeyIssue> {
        match object_config(config) {
            Ok(object) => with_config_type!(self, T => key_issues_at::<T>(&object, &no_retired, prefix)),
            Err(_) => vec![KeyIssue {
                path: prefix.to_string(),
                issue: Issue::WrongType { expected: "an object".to_string(), got: json_type(config).to_string() },
            }],
        }
    }

    /// The value rule `config` breaks, if any: what the kind's own reader
    /// refuses although the types are right. `None` also for a config that
    /// does not load at all ([`Self::issues`] names that).
    ///
    /// A `{{param}}` still in `config` is a value not known yet: a whole-string
    /// one is read as one that satisfies its key, and a rule broken AT a key
    /// whose text holds one (whole or embedded) is not reported. Every other key is checked as written, so a launch's
    /// `--param` values are checked again once substituted.
    pub fn violation(self, config: &Value) -> Option<RuleViolation> {
        let assumed = assume_params(config);
        let violation = with_config_type!(self, T => T::deserialize(&*object_config(&assumed).ok()?).ok()?.check().err())?;
        let top = violation.key.split('.').next().unwrap_or_default();
        let unresolved = config.get(top).and_then(Value::as_str).is_some_and(|text| text.contains("{{"));
        (!unresolved).then_some(violation)
    }

    /// Every problem of `config`: its schema issues, else the value rule it
    /// breaks, written at `prefix`.
    pub fn problems(self, config: &Value, prefix: &str) -> Vec<KeyIssue> {
        let issues = self.issues(config, prefix);
        if !issues.is_empty() {
            return issues;
        }
        self.violation(config)
            .map(|v| KeyIssue { path: format!("{prefix}.{}", v.key), issue: Issue::Rule(v.to_string()) })
            .into_iter()
            .collect()
    }

    /// How a message names the step of this kind it is about.
    pub fn step_label(self, step_id: &str) -> String {
        format!("step `{step_id}` (`{}`)", self.id())
    }

    /// The top-level keys this kind's config names.
    pub fn keys(self) -> Vec<String> {
        with_config_type!(self, T => top_level_keys::<T>())
    }

    /// Whether `config` is one the kind's own reader accepts, rules
    /// included: what its `load` decides, without running anything.
    pub fn loads(self, config: &Value) -> Result<(), String> {
        with_config_type!(self, T => {
            let typed = T::deserialize(&*object_config(config)?).map_err(|e| e.to_string())?;
            typed.check().map_err(|v| v.to_string())
        })
    }
}

/// `value` with every whole-string `{{param}}` replaced by the text `1`,
/// which reads as a string, a count, a decimal and a flag.
fn assume_params(value: &Value) -> Value {
    match value {
        Value::String(text) if is_placeholder(text) => Value::String("1".to_string()),
        Value::Array(items) => Value::Array(items.iter().map(assume_params).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), assume_params(v))).collect()),
        other => other.clone(),
    }
}

/// `config` as a JSON object: `Null` reads as an empty one, anything else that
/// is not an object is refused (a JSON array would otherwise load through
/// serde's sequence visitor as a struct's fields in order).
fn object_config(config: &Value) -> Result<Cow<'_, Value>, String> {
    match config {
        Value::Null => Ok(Cow::Owned(Value::Object(Default::default()))),
        Value::Object(_) => Ok(Cow::Borrowed(config)),
        other => Err(format!("a step's config must be an object, got {}", json_type(other))),
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// `step`'s config as `T`, `Null` reading as an empty object. The one way a
/// step kind reads its config. A load that fails says which keys are wrong or
/// missing (the gate's own wording, so a run and a preflight name them the
/// same), else the parse error.
pub fn load<T: DeserializeOwned>(step: &Step, kind: ConfigKind) -> Result<T> {
    let config = object_config(&step.config).map_err(|why| anyhow!("step `{}`: `{}` config: {why}", step.id, kind.id()))?;
    T::deserialize(&*config).map_err(|parse| {
        let named: Vec<String> = kind
            .issues(&config, "config")
            .into_iter()
            .filter(|i| matches!(i.issue, Issue::WrongType { .. } | Issue::Missing { .. }))
            .map(|i| i.to_string())
            .collect();
        let why = if named.is_empty() { parse.to_string() } else { named.join("; ") };
        anyhow!("step `{}`: `{}` config: {why}", step.id, kind.id())
    })
}

/// [`load`], then the kind's value rules ([`ConfigRules`]): the reader for a
/// kind whose config the gate also checks by value.
pub fn load_checked<T: DeserializeOwned + ConfigRules>(step: &Step, kind: ConfigKind) -> Result<T> {
    let config: T = load(step, kind)?;
    config.check().map_err(|v| anyhow!("step `{}`: `{}` {v}", step.id, kind.id()))?;
    Ok(config)
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

/// The rule and unit a `crawl.plan` / `plan.sites` / `dispatch.unit` step names.
pub fn crawl_identity(kind: &str, config: &Value) -> CrawlIdentity {
    match ConfigKind::from_id(kind) {
        Some(ConfigKind::CrawlPlan | ConfigKind::PlanSites | ConfigKind::DispatchUnit) => {
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
