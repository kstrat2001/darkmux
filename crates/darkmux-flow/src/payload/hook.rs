//! Payloads of the hook engine's own records (`hook.fired`, `hook.failed`,
//! `hook.dry_run`).

use super::Attribution;
use serde::{Deserialize, Serialize};

/// One delivery attempt of a matched record to a hook rule's receiver: the payload of `hook.fired`
/// (delivered) and of the delivery form of `hook.failed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct HookDeliveryPayload {
    /// The rule this delivery belongs to, by its position in the configured list.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub rule_index: usize,
    /// The receiver's `host:port`.
    pub target_host: String,
    /// The delivered record's action, `null` when the delivered line named none (never an empty
    /// string: that would claim a blank action).
    pub delivered_action: Option<String>,
    /// Which attempt this was, from 1.
    pub attempt: u32,
    /// The id this attempt sent as `X-Darkmux-Delivery`, so a receiver can correlate the record
    /// with the request it saw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub delivery_id: Option<String>,
    /// The delivered line's chain hash, when it carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub delivered_hash: Option<String>,
    /// Why the attempt failed, on `hook.failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub error: Option<String>,
    /// How many items the receiver's own response body reported rejecting; never zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub receiver_rejected: Option<u64>,
    /// The receiver's own `results[].error` text, with `receiver_rejected` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub receiver_rejected_reasons: Option<Vec<String>>,
}

impl Attribution for HookDeliveryPayload {}

/// A rate-limited `hook.failed` notice about a rule as a whole rather than one delivery: the outbox
/// over its cap (`dropped_count`) or its transform backlogged (`orphaned_transforms`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct HookNoticePayload {
    /// The rule the notice is about.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub rule_index: usize,
    /// The rule's receiver `host:port`.
    pub target_host: String,
    /// The condition, in words.
    pub error: String,
    /// Writes dropped for this rule so far, on an outbox-over-cap notice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub dropped_count: Option<u64>,
    /// Timed-out transform evaluations still running, on a backlog notice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub orphaned_transforms: Option<u32>,
}

impl Attribution for HookNoticePayload {}

/// A `hook.failed` payload: a failed delivery, or a notice about the rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum HookFailedPayload {
    Delivery(HookDeliveryPayload),
    Notice(HookNoticePayload),
}

impl Attribution for HookFailedPayload {}

/// One `file`-transport write: what a `hook.fired` would have delivered, dumped to disk instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct HookDryRunPayload {
    /// The rule that wrote the dump.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub rule_index: usize,
    /// The dumped record's action, `null` when it named none.
    pub delivered_action: Option<String>,
    /// The delivery id the dump carries.
    pub delivery_id: String,
    /// Where the dump was written.
    pub dump_path: String,
}

impl Attribution for HookDryRunPayload {}
