//! Payloads of the usage accounting records: `telemetry.tokens` (one per model
//! call) and the utility-job markers `utility.start` / `utility.error`.

use super::{Attribution, RecordContext};
use serde::{Deserialize, Serialize};

/// Which kind of model call a usage record accounts for. Serialized into the
/// payload's `call_kind` through serde (the variant names ARE the wire
/// spelling), and exported to the viewer as a generated TS type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum CallKind {
    /// One agent-loop turn of the container runtime.
    Turn,
    /// One container-free single-shot chat completion.
    SingleShot,
    /// One `dispatch.map` item.
    MapItem,
    /// One runtime compactor call: a sub-execution of the utility role,
    /// attributed to it, never to the specialist.
    Compaction,
    /// A value this build does not name, read from an archive written by another version.
    /// Never written.
    #[serde(other)]
    Unknown,
}

/// WHOSE job a model call was: the operator's WORK, or one of darkmux's own
/// UTILITY jobs. Stamped on every usage record as `purpose`. The viewer's hero
/// shows utility as its own chip, and an execution's own numbers (run page
/// tiles, mission-graph step meter) exclude it (CLAUDE.md contract 8:
/// sub-executions are never blended into the primary).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum UsagePurpose {
    /// The operator's work: every call that is not a utility job.
    Work,
    /// darkmux's own job, run on the machine's utility model.
    Utility,
    /// A value this build does not name, read from an archive written by another version.
    /// Never written.
    #[serde(other)]
    Unknown,
}

/// WHICH of darkmux's utility jobs a call (or a `utility.start`) belongs to.
/// Exported to the viewer as a generated TS union: the viewer keys each job's
/// own visual by it, and gives a job it has no visual for a generic utility
/// indicator, so a new variant here is never silent there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum UtilityJobKind {
    /// A runtime compactor call (inside the container, serving the
    /// execution it compacts).
    Compaction,
    /// A radio routing call (host-side, serving no execution).
    RadioRouting,
    /// A value this build does not name, read from an archive written by another version.
    /// Never written.
    #[serde(other)]
    Unknown,
}

/// Whether the provider's reply carried a usage block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum TokenSource {
    /// The reply carried a usage block; its counts are the provider's.
    Provider,
    /// The reply carried none; the record has no counts.
    Absent,
    /// A value this build does not name, read from an archive written by another version.
    /// Never written.
    #[serde(other)]
    Unknown,
}

/// One model call's usage: the payload of `telemetry.tokens`. The model CALL is darkmux's unit of
/// accounting, and a total anywhere is a plain sum of these records. Counts are never fabricated:
/// one the provider did not report is absent, and a reply with no usage block carries no count keys
/// and `token_source: "absent"`. A field older archives lack is optional here, so an archive still
/// reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct UsagePayload {
    /// What kind of call this accounts for. Absent before flow 1.57.0.
    pub call_kind: Option<CallKind>,
    /// Whose job the call was. Absent before flow 1.59.0; a reader then decides by `call_kind`: a
    /// compactor call is utility, anything else work.
    pub purpose: Option<UsagePurpose>,
    /// The model id darkmux put on the wire.
    pub requested_model: Option<String>,
    /// The endpoint darkmux called, as a fact: the bookends' hosted label for a hosted call, the
    /// resolved LMStudio base URL for a local one. Never a classification.
    pub endpoint: Option<String>,
    /// The utility job a utility record belongs to; absent on work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub job: Option<UtilityJobKind>,
    /// The response's own `model` field; absent when it had none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub reported_model: Option<String>,
    /// The `endpoints` registry id the call was made through; absent for an unnamed endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub endpoint_id: Option<String>,
    /// Whether the counts below are the provider's.
    pub token_source: Option<TokenSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub completion_tokens: Option<u64>,
    /// The provider's own total, else prompt plus completion when it reported a split without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub total_tokens: Option<u64>,
    /// Provider-scoped: whether it sits inside `completion_tokens` varies by provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub cached_tokens: Option<u64>,
    /// The runtime turn, on a `turn` record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub turn_seq: Option<u64>,
    /// On a `dispatch.map` per-call record: whether the item ran on a hosted endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub remote: Option<bool>,
    /// On a `dispatch.map` per-call record: the item's position. An item that retried emits one
    /// record per attempt, all with this index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub index: Option<u64>,
    /// On a compaction call: the compaction it served.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub generation: Option<u64>,
    /// On a compaction call: the role of the specialist execution it ran inside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub parent_role_id: Option<String>,
    /// On a compaction call: the model of that execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub parent_model: Option<String>,
    /// On a utility record: the id its `utility.start` minted, so an end pairs with ITS start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub job_id: Option<String>,
    /// On a utility record: when the job ended, epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub ended_at_ms: Option<u64>,
    /// On a utility record: from the start's `started_at_ms` to `ended_at_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub duration_ms: Option<u64>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied (the crawl launcher's workspace, source, sha,
    /// rule, unit), a [`RecordContext`].
    #[serde(default, deserialize_with = "super::context::lenient", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub context: Option<RecordContext>,
}

impl Attribution for UsagePayload {
    fn step_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.step_id)
    }
    fn step(&self) -> Option<&str> {
        self.step_id.as_deref()
    }
    fn context_slot(&mut self) -> Option<&mut Option<RecordContext>> {
        Some(&mut self.context)
    }
}

/// A utility job began: the payload of `utility.start`. The viewer shows the job from here until
/// its usage record (or `utility.error`) lands; a start with no end after `stall_after_ms` reads as
/// stalled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct UtilityStartPayload {
    /// Which utility job.
    pub job: UtilityJobKind,
    /// A fresh id echoed by the job's end, so the viewer pairs an end with ITS start; a start
    /// orphaned by a killed process never absorbs a later job's end.
    pub job_id: String,
    /// The model the job runs on, as its wire id.
    pub model: String,
    /// The job's own bound: the inactivity window for a compaction, the call timeout for routing.
    /// Recorded so the viewer never guesses a knob the host already knows.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub stall_after_ms: u64,
    /// When the job started, epoch milliseconds (the record's `ts` is whole-second).
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub started_at_ms: u64,
    /// The session id of the execution the job serves; absent when it serves none, as routing does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub serves: Option<String>,
    /// On a compaction start: the compaction it serves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub generation: Option<u64>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, a [`RecordContext`].
    #[serde(default, deserialize_with = "super::context::lenient", skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub context: Option<RecordContext>,
}

impl Attribution for UtilityStartPayload {
    fn step_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.step_id)
    }
    fn step(&self) -> Option<&str> {
        self.step_id.as_deref()
    }
    fn context_slot(&mut self) -> Option<&mut Option<RecordContext>> {
        Some(&mut self.context)
    }
}

/// A started utility job ended without a usage record: the payload of `utility.error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct UtilityErrorPayload {
    /// Which utility job.
    pub job: UtilityJobKind,
    /// The model the job ran on, as its wire id.
    pub model: String,
    /// The id its `utility.start` minted.
    pub job_id: String,
    /// When the job ended, epoch milliseconds.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub ended_at_ms: u64,
    /// From the start's `started_at_ms` to `ended_at_ms`.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub duration_ms: u64,
}

impl Attribution for UtilityErrorPayload {}
