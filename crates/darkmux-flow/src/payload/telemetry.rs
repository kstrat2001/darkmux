//! Payloads of the `telemetry.*` records an execution's sampler and tailer write
//! (context occupancy, compaction, runtime turns, model residency, detector
//! findings). Model-call usage is in [`super::usage`].

use super::dispatch::BoundRef;
use super::Attribution;
use serde::{Deserialize, Serialize};

/// A model became resident or stopped being.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum LmsEvent {
    /// It was loaded.
    Load,
    /// It was unloaded.
    Unload,
    /// A value this build does not name, read from an archive written by another version.
    /// Never written.
    #[serde(other)]
    Unknown,
}

/// Which seat a resident model holds relative to the dispatch's own staffing, so the viewer can
/// tell "the primary changed" from "the utility model went resident, exactly as staffed".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum LmsRole {
    /// The dispatch's own model.
    Primary,
    /// The machine's utility model. Archives from before the utility seat was named spell it
    /// `compactor`.
    #[serde(alias = "compactor")]
    Utility,
    /// A model the dispatch did not declare: a leftover, or the operator's own use.
    Resident,
    /// A value this build does not name, read from an archive written by another version.
    /// Never written.
    #[serde(other)]
    Unknown,
}

/// Which detector fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum DetectorKind {
    /// A repeated-tool-call cycle.
    #[serde(rename = "cycle")]
    Cycle,
    /// The same reasoning repeated.
    #[serde(rename = "reasoning-loop")]
    ReasoningLoop,
    /// Repeated failures of one tool.
    #[serde(rename = "tool-failure")]
    ToolFailure,
    /// A runaway-reasoning turn was dropped and recovered.
    #[serde(rename = "intra-turn-stall")]
    IntraTurnStall,
    /// The model finished with tool calls and returned none.
    #[serde(rename = "empty_tool_calls")]
    EmptyToolCalls,
    /// Tool calls were salvaged at the per-turn cap.
    #[serde(rename = "per-turn-cap")]
    PerTurnCap,
    /// A tool call was cut mid-arguments and never dispatched.
    #[serde(rename = "discarded_tool_call")]
    DiscardedToolCall,
    /// Calls named no real tool, or one the role is not granted.
    #[serde(rename = "malformed_tool_names")]
    MalformedToolNames,
    /// The dispatch escalated out of the local tier.
    #[serde(rename = "escalation")]
    Escalation,
    /// The degeneracy gate judged the stream repeating.
    #[serde(rename = "repetition")]
    Repetition,
    /// A value this build does not name, read from an archive written by another version.
    /// Never written.
    #[serde(other)]
    Unknown,
}

/// How loudly a detector firing reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum DetectorSeverity {
    /// Worth attention.
    Warn,
    /// Recovered; recorded for the run's account.
    Info,
    /// A value this build does not name, read from an archive written by another version.
    /// Never written.
    #[serde(other)]
    Unknown,
}

/// Per-turn context-window occupancy: the payload of `telemetry.context`, the sawtooth the viewer
/// draws.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct TelemetryContextPayload {
    /// The exact prompt-token count the endpoint reported.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub used: u64,
    /// The configured context window; `null` when unconfigured.
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub max: Option<u64>,
    /// The compaction threshold, `null` when none applies.
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub threshold: Option<u64>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for TelemetryContextPayload {
    fn step_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.step_id)
    }
    fn step(&self) -> Option<&str> {
        self.step_id.as_deref()
    }
    fn context_slot(&mut self) -> Option<&mut Option<serde_json::Map<String, serde_json::Value>>> {
        Some(&mut self.context)
    }
}

/// The drop a compaction made in the context-occupancy sawtooth: the payload of
/// `telemetry.compaction`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct TelemetryCompactionPayload {
    /// The exact prompt-token count that triggered it.
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub from: Option<u64>,
    /// A chars/4 estimate of the compacted buffer.
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub to: Option<u64>,
    /// The utility model that did the work; `null` when none was bound.
    pub compactor_model: Option<String>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for TelemetryCompactionPayload {
    fn step_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.step_id)
    }
    fn step(&self) -> Option<&str> {
        self.step_id.as_deref()
    }
    fn context_slot(&mut self) -> Option<&mut Option<serde_json::Map<String, serde_json::Value>>> {
        Some(&mut self.context)
    }
}

/// The per-dispatch runtime turn count: the payload of `telemetry.runtime`, so the viewer can
/// render runtime turns without parsing a terminal payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct TelemetryRuntimePayload {
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub turns: u64,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for TelemetryRuntimePayload {
    fn step_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.step_id)
    }
    fn step(&self) -> Option<&str> {
        self.step_id.as_deref()
    }
    fn context_slot(&mut self) -> Option<&mut Option<serde_json::Map<String, serde_json::Value>>> {
        Some(&mut self.context)
    }
}

/// A model load or unload seen by the residency probe: the payload of `telemetry.lms`. The first
/// probe reports every already-resident model as a load with `baseline: true`, the starting lineup,
/// never an event this attempt caused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct TelemetryLmsPayload {
    pub event: LmsEvent,
    pub model: String,
    /// The model's size in whole gigabytes, on a load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub gb: Option<u64>,
    /// The seat the model holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub role: Option<LmsRole>,
    /// `true` on the first probe's loads only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub baseline: Option<bool>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for TelemetryLmsPayload {
    fn step_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.step_id)
    }
    fn step(&self) -> Option<&str> {
        self.step_id.as_deref()
    }
    fn context_slot(&mut self) -> Option<&mut Option<serde_json::Map<String, serde_json::Value>>> {
        Some(&mut self.context)
    }
}

/// The file a detector firing is about, for cautions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct DetectorArea {
    pub files: Vec<String>,
    /// The file's content hash at firing time, for staleness ranking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub code_hash: Option<String>,
}

/// A detector finding: the payload of `telemetry.detector`. The same finding goes to the envelope,
/// so the viewer and the orchestrator cannot disagree about what fired. `kind`, `severity` and
/// `detail` are always there; the rest belongs to the detector that fired.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct TelemetryDetectorPayload {
    pub kind: DetectorKind,
    pub severity: DetectorSeverity,
    /// What happened, in words, bounded.
    pub detail: String,
    /// The turn the degeneracy gate looked at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub turn_seq: Option<u64>,
    /// Which look at the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub observation: Option<u64>,
    /// How repetitive the tail was; absent when the gate ended the call before judging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub tail_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub slice_chars: Option<u64>,
    /// How much the call had generated when the gate ended it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub generated_chars: Option<u64>,
    /// The degeneracy policy in force.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub policy: Option<String>,
    /// Whether the gate ended the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub acted: Option<bool>,
    /// On malformed tool names: how many calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub count: Option<u64>,
    /// On malformed tool names or an escalation: the model concerned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub model: Option<String>,
    /// On malformed tool names: a bounded sample of the offending name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub sample_name_prefix: Option<String>,
    /// On malformed tool names or an escalation: why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub reason: Option<String>,
    /// On an escalation: the prompt size that tripped it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub prompt_tokens: Option<u64>,
    /// On a discarded tool call: the tool named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub name: Option<String>,
    /// On a discarded tool call: how much of its arguments arrived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub arguments_chars: Option<u64>,
    /// On a discarded tool call: where the arguments were cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub cut: Option<String>,
    /// The file the firing is about, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub area: Option<DetectorArea>,
    /// The request bound the firing names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub bound: Option<BoundRef>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for TelemetryDetectorPayload {
    fn step_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.step_id)
    }
    fn step(&self) -> Option<&str> {
        self.step_id.as_deref()
    }
    fn context_slot(&mut self) -> Option<&mut Option<serde_json::Map<String, serde_json::Value>>> {
        Some(&mut self.context)
    }
}

impl DetectorKind {
    /// The wire word.
    pub fn as_str(self) -> &'static str {
        match self {
            DetectorKind::Cycle => "cycle",
            DetectorKind::ReasoningLoop => "reasoning-loop",
            DetectorKind::ToolFailure => "tool-failure",
            DetectorKind::IntraTurnStall => "intra-turn-stall",
            DetectorKind::EmptyToolCalls => "empty_tool_calls",
            DetectorKind::PerTurnCap => "per-turn-cap",
            DetectorKind::DiscardedToolCall => "discarded_tool_call",
            DetectorKind::MalformedToolNames => "malformed_tool_names",
            DetectorKind::Escalation => "escalation",
            DetectorKind::Repetition => "repetition",
            DetectorKind::Unknown => "unknown",
        }
    }
}

impl DetectorSeverity {
    /// The wire word.
    pub fn as_str(self) -> &'static str {
        match self {
            DetectorSeverity::Warn => "warn",
            DetectorSeverity::Info => "info",
            DetectorSeverity::Unknown => "unknown",
        }
    }
}

impl TelemetryDetectorPayload {
    /// A finding of `kind`, with none of the detector-specific fields set.
    pub fn new(kind: DetectorKind, severity: DetectorSeverity, detail: String) -> Self {
        Self {
            kind,
            severity,
            detail,
            turn_seq: None,
            observation: None,
            tail_ratio: None,
            slice_chars: None,
            generated_chars: None,
            policy: None,
            acted: None,
            count: None,
            model: None,
            sample_name_prefix: None,
            reason: None,
            prompt_tokens: None,
            name: None,
            arguments_chars: None,
            cut: None,
            area: None,
            bound: None,
            step_id: None,
            context: None,
        }
    }
}
