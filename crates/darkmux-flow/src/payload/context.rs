//! The provenance a dispatch caller stamps on every record its dispatch writes
//! (`payload.context`), typed (#3035).
//!
//! One producer writes it today: the crawl launcher, which names the workspace,
//! source, sha, rule and unit a unit dispatch ran against, plus the seat that
//! staffed it. A finding record carries the same block, and gains `site` when
//! its emission falls inside a planned span.
//!
//! It reads leniently, so every record already in an archive still parses:
//! every field is optional (a record written before `confirm`, `rules` or the
//! seat fields lacks them), `rule` reads either spelling it has had, and a key
//! this type does not name lands in `extras` and re-serializes flat. A field
//! that is `None` is omitted, so a context re-serializes as the keys it was
//! read with. Each field reads leniently ([`lenient`]): a key of the wrong type is `None`
//! and the rest of the context, and of the record around it, still reads.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

/// A field that reads as `T` or, when it is not one, as `None`: leniency per FIELD, so one
/// wrong-typed key costs that key and never the record around it (contract 7). The bad raw
/// value is dropped, not kept: a typed field has nowhere to hold it, and a record this build
/// rewrites would otherwise carry a value its own type cannot read back.
pub fn lenient<'de, D: Deserializer<'de>, T: serde::de::DeserializeOwned>(d: D) -> Result<Option<T>, D::Error> {
    Ok(serde_json::from_value(serde_json::Value::deserialize(d)?).ok())
}

/// The rule a unit dispatch ran under: a string, or (a record written when a
/// unit could carry several) a list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum RuleRef {
    One(String),
    Many(Vec<String>),
}

/// The planned span that holds a finding, stamped onto its context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct ContextSite {
    pub file: String,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub start: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub end: u64,
}

/// The provenance a dispatch caller supplied, carried on every record the
/// dispatch writes.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct RecordContext {
    /// The crawl workspace (manifest) name.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub workspace: Option<String>,
    /// The workspace source id the unit ran against.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub source: Option<String>,
    /// The source's commit.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub sha: Option<String>,
    /// The rule the unit ran under, when it ran under exactly one.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub rule: Option<RuleRef>,
    /// Every rule the unit ran under.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub rules: Option<Vec<String>>,
    /// The confirmation form of the unit's one rule (`mod`, `search`, `question`).
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub confirm: Option<String>,
    /// The planned unit's id.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub unit: Option<String>,
    /// The model that staffed the seat.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub model: Option<String>,
    /// Where the seat ran (`local`, `endpoint`, `unknown`).
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub locality: Option<String>,
    /// The profile that staffed the seat.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub profile: Option<String>,
    /// On a finding record: the planned span that holds the finding.
    #[serde(default, deserialize_with = "lenient", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub site: Option<ContextSite>,
    /// A key this type does not name (a newer producer's), kept and re-serialized flat.
    #[serde(flatten)]
    #[schemars(skip)]
    #[cfg_attr(feature = "ts-export", ts(skip))]
    pub extras: serde_json::Map<String, serde_json::Value>,
}
