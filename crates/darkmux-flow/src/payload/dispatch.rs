//! Payloads of a role execution's own records: `dispatch.*` (its bookends and the
//! events inside it) and `dispatch.route`.

use super::Attribution;
use serde::{Deserialize, Serialize};

/// How a dispatch ended, as its terminal record classes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum ResultClass {
    /// It ran to a clean end.
    Ok,
    /// It failed, or was cut before completion.
    Error,
    /// A value this build does not name, read from an archive written by another version.
    /// Never written.
    #[serde(other)]
    Unknown,
}

/// How a tool call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum ToolOutcome {
    /// Ran and reported success.
    Ok,
    /// Ran and reported a non-zero result (a red test is work).
    Reported,
    /// Did not run, or could not complete.
    Failed,
    /// A value this build does not name, read from an archive written by another version.
    /// Never written.
    #[serde(other)]
    Unknown,
}

/// What the model is doing mid-stream, when it is more than writing text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum StreamPhase {
    /// A tool call has been named and its arguments are being written.
    WritingToolCall,
    /// A value this build does not name, read from an archive written by another version.
    /// Never written.
    #[serde(other)]
    Unknown,
}

/// What a reasoning checkpoint decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum CheckpointVerdict {
    /// Let the turn keep going.
    Continue,
    /// Close the thought and ask for the answer.
    Conclude,
    /// A value this build does not name, read from an archive written by another version.
    /// Never written.
    #[serde(other)]
    Unknown,
}

/// The verdict of a `dispatch.route`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum RouteDecision {
    /// The operator named the machine.
    Pinned,
    /// No machine was named; it runs here.
    Local,
    /// A value this build does not name, read from an archive written by another version.
    /// Never written.
    #[serde(other)]
    Unknown,
}

/// Where a resolved runtime knob's value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum KnobSource {
    /// The environment variable.
    Env,
    /// `config.json`.
    Config,
    /// The built-in default.
    #[serde(rename = "built-in")]
    BuiltIn,
    /// The launcher's own per-run ceiling.
    Launcher,
    /// A command-line flag.
    Cli,
    /// Forced by an agentic-remote dispatch, which takes no rest.
    #[serde(rename = "forced-agentic-remote")]
    ForcedAgenticRemote,
    /// A value this build does not name, read from an archive written by another version.
    /// Never written.
    #[schemars(skip)]
    #[serde(other)]
    Unknown,
}

/// Which record store a brief ref's key addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum BriefRefKind {
    /// A finding: something an earlier dispatch observed.
    Finding,
    /// A mod: a change someone proposed.
    Mod,
    /// A value this build does not name, read from an archive written by another version.
    /// Never written.
    #[serde(other)]
    Unknown,
}

/// What a checkout's `.git` pointer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum GitdirKind {
    /// A linked worktree.
    Worktree,
    /// A submodule.
    Submodule,
    /// A separate git dir.
    Separate,
    /// A value this build does not name, read from an archive written by another version.
    /// Never written.
    #[serde(other)]
    Unknown,
}

/// One record a dispatch's brief carries: a kind plus the key its store answers to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct BriefRef {
    /// Which store the key addresses.
    pub kind: BriefRefKind,
    /// The key.
    pub key: String,
}

/// One resolved runtime knob, with where its value came from: the operator never has to wonder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct Knob {
    /// The resolved value (a number, boolean or string); `null` for an uncapped knob.
    #[cfg_attr(feature = "ts-export", ts(type = "unknown | null"))]
    pub value: Option<serde_json::Value>,
    /// The tier that resolved it.
    pub source: KnobSource,
    /// What the operator's own knob resolved to, when a forced override replaced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub configured_value: Option<Box<Knob>>,
}

/// The runtime knobs a dispatch ran under, each with its provenance. The same block rides the
/// mission envelope, from one producer, so a reader watching the flow stream and one reading the
/// finished envelope cannot disagree about what governed the run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct RuntimeBounds {
    pub max_tokens_per_call: Knob,
    pub inactivity_timeout_seconds: Knob,
    pub max_turns: Knob,
    pub max_tokens: Knob,
    // The knobs below were added over time, so an older archive's block lacks them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub reasoning_checkpoint_interval_tokens: Option<Knob>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub turn_delay_ms: Option<Knob>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub feedback_injection: Option<Knob>,
    /// The detection regime the run executed under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub detection_degeneracy_policy: Option<Knob>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub thermal_pacing_enabled: Option<Knob>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub battery_pause_enabled: Option<Knob>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub battery_pause_floor_pct: Option<Knob>,
}

/// A role execution began: the payload of `dispatch.start`. One type for every producer of the
/// record: the container path (image, tools, bounds), the hosted and local single-shot paths
/// (endpoint, prompt), and a step kind's per-call bookend (kind, item index). A field a producer
/// has no reading for is absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct DispatchStartPayload {
    /// The resolved runtime image: the environment the coder ran in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub image: Option<String>,
    /// The tool names the dispatch requested of the runtime, from the role's palette; absent when
    /// the role declares none and the runtime's full catalog applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub tools_requested: Option<Vec<String>>,
    /// The darkmux records `dispatch --finding` / `--mod` appended to the brief. Empty on every
    /// other dispatch: an absent key would be indistinguishable from a record written before the
    /// field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub brief_refs: Option<Vec<BriefRef>>,
    /// The full length of the prompt; `prompt` is capped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub prompt_chars: Option<u64>,
    /// The prompt text, capped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub prompt: Option<String>,
    /// The system prompt's length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub system_chars: Option<u64>,
    /// The workspace the dispatch ran in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub workspace: Option<String>,
    /// The resolved inter-turn rest the container was given; zero for an agentic-remote dispatch,
    /// which takes none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub turn_delay_ms: Option<u64>,
    /// The resolved runtime knobs with provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub bounds: Option<RuntimeBounds>,
    /// The flow schema this run's own records were written against, so a consumer can tell a run
    /// recorded before a forwarder fix from one with no findings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub flow_schema: Option<String>,
    /// The hosted endpoint the dispatch called. Its absence means local LMStudio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub endpoint: Option<String>,
    /// The prior dispatch's output directory this one resumed; absent when it started fresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub resumed_from: Option<String>,
    /// The step kind that ran the execution, on a step's per-call bookend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub kind: Option<String>,
    /// On a `dispatch.map` item's bookend: the item's position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub item_index: Option<u64>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied (the crawl launcher's workspace, source, sha,
    /// rule, unit), carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for DispatchStartPayload {
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

/// The live channel's own cost for one execution. The live samples themselves are never records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct LiveSummary {
    pub enabled: bool,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub cadence_ms: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub samples_sent: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub dropped_no_receiver: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub dropped_full: u64,
    pub sampler_ms: f64,
    pub forward_ms: f64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub bytes: u64,
}

/// How comfortable the host was over one execution: the compact host-pressure summary the
/// envelope's `host` block carries too. A field the sampler could not read is `null`, never zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct HostWindow {
    pub thermal_worst_state: Option<String>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub above_nominal_ms: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub min_cpu_speed_limit_pct: Option<u64>,
    /// How many times the machine reached `serious`; `null` when the sampler thread panicked and
    /// the ladder was never recovered.
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub thermal_serious_episodes: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub thermal_duty_delay_ms: Option<u64>,
    /// The scenario file that produced the thermal and battery numbers, `null` on a real run. An
    /// artifact is read by eye long after the run, and an explicit null says "these were real".
    pub simulated_host_source: Option<String>,
    pub power_mw_total: Option<super::machine::MetricWindow>,
    pub energy_mwh: Option<f64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub samples: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub span_ms: u64,
}

/// A role execution ended: the payload of `dispatch.complete` and `dispatch.error`. One type for
/// every producer of the two: the container path (full run accounting), the hosted and local
/// single-shot paths, a step kind's per-call bookend, and the guard that writes an error when a
/// dispatch ends before completing. A field a producer has no reading for is absent, except the
/// turn count: every producer states it (`new` takes it, and there is deliberately no `Default`),
/// so no path leaves a run's turns for a reader to guess at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct DispatchEndPayload {
    /// The runtime's own measure of the execution. "Wall stays wall": it includes any rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub wall_ms: Option<u64>,
    /// The sum of every inter-turn rest: routine cool-downs, thermal and battery pauses and
    /// operator holds all land here, so this is not a thermal-only figure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub rest_ms: Option<u64>,
    /// How many rests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub rests: Option<u64>,
    /// Of `rest_ms`, the part that was a paced rest (a manual pause or a governor), so a reader
    /// need not subtract the routine cool-down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub paced_rest_ms: Option<u64>,
    /// The post-clamp cadence the runtime applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub turn_delay_effective_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub stdout_chars: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub stderr_chars: Option<u64>,
    /// On the error path, a bounded stderr tail, so a failed dispatch is diagnosable from the flow
    /// stream alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub stderr_excerpt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub result_class: Option<ResultClass>,
    /// Why a dispatch that ended without a result did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub error: Option<String>,
    /// The model turns the execution took: a direct single-shot call or a map item is 1, a
    /// container loop its fold's count, an execution that ended before any call 0. Every
    /// producer states it: `new` takes it and there is no `Default`, so a struct built without
    /// it must spell out every field. `Option` only so an archived terminal written before that
    /// reads back as absent, not as a fabricated 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub total_turns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub total_tools: Option<u64>,
    /// Dispatched calls that came back `ok: false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub tool_calls_failed: Option<u64>,
    /// Structured calls that never ran because the name is no tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub tool_calls_invalid_name: Option<u64>,
    /// Structured calls that never ran because the role was not granted the tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub tool_calls_ungranted: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub total_compactions: Option<u64>,
    /// The live channel's own cost for this execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub live: Option<LiveSummary>,
    /// This execution's own tokens, never a prior resume's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub completion_tokens: Option<u64>,
    /// Each call's own total, summed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub total_tokens: Option<u64>,
    /// Absent when no call reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub reasoning_tokens: Option<u64>,
    /// Absent when no call reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub cached_tokens: Option<u64>,
    /// The hosted endpoint the dispatch called; its absence means local LMStudio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub endpoint: Option<String>,
    /// The host-pressure summary over the execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub host_window: Option<HostWindow>,
    /// The prior dispatch's output directory this one resumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub resumed_from: Option<String>,
    /// The step kind that ran the execution, on a step's per-call bookend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub kind: Option<String>,
    /// On a `dispatch.map` item's bookend: the item's position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub item_index: Option<u64>,
    /// What a hosted step's item spent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub remote_tokens: Option<u64>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for DispatchEndPayload {
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

/// The usage block a turn's model reply carried. A count the provider did not report is `null`,
/// never zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct TurnUsage {
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub prompt_tokens: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub completion_tokens: Option<u64>,
    /// The provider's own total; it can exceed prompt plus completion.
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub total_tokens: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub reasoning_tokens: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub cached_tokens: Option<u64>,
}

/// One logical turn ended: the payload of `dispatch.turn`. A `length` finish mid-checkpointing is a
/// continuation, not a turn boundary, and emits `dispatch.checkpoint` instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct DispatchTurnPayload {
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub turn_seq: u64,
    /// As the endpoint reported it, bounded.
    pub finish_reason: String,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub tool_calls_count: u64,
    /// `null` when the reply carried no usage block.
    pub usage: Option<TurnUsage>,
    /// The authoritative running turn count (monotonic, 1-based), so a viewer opened mid-dispatch
    /// reads the true count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub turns_so_far: Option<u64>,
    /// The turn's model time, request sent to stream end; absent when no stream was recorded, and
    /// reported once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub generation_ms: Option<u64>,
    /// Each running call's file, in order, so the viewer names the call running now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub tool_paths: Option<Vec<Option<String>>>,
    /// Each running call's tool name, in order; `null` for a name that is no runtime tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub tool_names: Option<Vec<Option<String>>>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for DispatchTurnPayload {
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

/// A streamed chunk or a writing tick: the payload of `dispatch.turn.heartbeat`, coalesced to one
/// per interval so topology edges stay animated without flooding the stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct DispatchHeartbeatPayload {
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub turn_seq: u64,
    /// The chunk's index; absent on a turn's opening heartbeat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub partial_index: Option<u64>,
    /// Answer text only: stays 0 while a separate-field-reasoning model reasons.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub cumulative_chars: u64,
    /// The runtime's own millisecond clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub sampled_at_ms: Option<u64>,
    /// Everything generated, reasoning and tool-call arguments included; `null` when the chunk did
    /// not say.
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub generated_chars: Option<u64>,
    /// On a turn's opening heartbeat: the size of the request about to be sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub prompt_chars: Option<u64>,
    /// Present while the model is writing a named tool call, absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub phase: Option<StreamPhase>,
    /// The tool being written, with `phase`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub tool_name: Option<String>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for DispatchHeartbeatPayload {
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

/// One executed tool call: the payload of `dispatch.tool`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct DispatchToolPayload {
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub tool_seq: u64,
    /// The authoritative running tool-call count (monotonic, 1-based).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub tool_calls_so_far: Option<u64>,
    pub tool_name: String,
    /// The arguments preview (search pattern, path, command), bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub args: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub args_chars: Option<u64>,
    /// An accepted `create_finding`'s emission, whole (bounded loudly): the model's own JSON,
    /// carried verbatim. `null` for every other call.
    #[cfg_attr(feature = "ts-export", ts(type = "unknown | null"))]
    pub emitted: Option<serde_json::Value>,
    /// The emission's 1-based ordinal; `null` for every other call.
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub emit_seq: Option<u64>,
    /// The true result length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub result_chars: Option<u64>,
    /// The result, bounded by eliding its middle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub result: Option<String>,
    /// Whether the tool did its job (true for a red test).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub ok: Option<bool>,
    /// The three-way outcome as classified.
    pub outcome: Option<ToolOutcome>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub exit_code: Option<i64>,
    pub failure_reason: Option<String>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for DispatchToolPayload {
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

/// A compaction was installed: the payload of `dispatch.compaction`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct DispatchCompactionPayload {
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub generation: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub before_messages: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub after_messages: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub summary_chars: u64,
    /// The utility model that did the work; `null` when none was bound.
    pub compactor_model: Option<String>,
    /// The specialist's model, which stays the record's `model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub parent_model: Option<String>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for DispatchCompactionPayload {
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

/// The request bound a record names, with its provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct BoundRef {
    /// Which bound: `reasoning_checkpoint_interval`, `max_tokens_per_call`, ...
    pub kind: String,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub value: Option<u64>,
    /// Which tier resolved it.
    pub source: Option<String>,
}

/// The harness checked in on a turn at the reasoning interval: the payload of
/// `dispatch.checkpoint`. `verdict` is the decision and `tail_ratio` its evidence; `policy` and
/// `would_conclude` tell an enforced conclusion from a recorded finding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct DispatchCheckpointPayload {
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub turn_seq: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub checkpoint: u64,
    /// `null` when the slice was too short to judge.
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub slice_tokens: Option<u64>,
    /// `null` when the slice was too short to judge, never 0.0, which would read as maximally
    /// repetitive.
    pub tail_ratio: Option<f64>,
    pub verdict: CheckpointVerdict,
    pub bound: Option<BoundRef>,
    /// The degeneracy policy in force, as recorded.
    pub policy: Option<String>,
    /// What the detector found, independent of whether it was allowed to act.
    pub would_conclude: Option<bool>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for DispatchCheckpointPayload {
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

/// A turn's reasoning text, bounded: the payload of `dispatch.reasoning`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct DispatchReasoningPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub turn_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub reasoning_chars: Option<u64>,
    pub reasoning_text: String,
    /// `inline-think-tags` or `separate-field`.
    pub reasoning_format: String,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for DispatchReasoningPayload {
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

/// The runtime delivered system messages to the model: the payload of `dispatch.feedback.injected`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct DispatchFeedbackPayload {
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub turn_seq: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub message_count: u64,
    pub signal_kinds: Vec<String>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for DispatchFeedbackPayload {
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

/// A run rested: the payload of `dispatch.rest`. Two producers: the runtime's own record of a rest
/// it took (`ms`, `turn`, `rest_ms`, `rests`), and a governor's decision to pause, resume or pace
/// (`pause`, and per tier `delay_ms` or `episode`). `reason` names why, `state` the reading behind
/// it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct DispatchRestPayload {
    /// Why the loop rested: `turn_delay`, or a paced reason (`thermal`, `thermal-critical`,
    /// `thermal-duty-cycle`, `thermal-episode-limit`, `battery`, `budget`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub reason: Option<String>,
    /// The pace file's own state, an OS thermal-state name when the governor wrote the pause;
    /// absent on a plain turn-delay rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub state: Option<String>,
    /// On the runtime's record: how long it rested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub ms: Option<u64>,
    /// On the runtime's record: the turn it rested after.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub turn: Option<u64>,
    /// On the runtime's record: the running total of rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub rest_ms: Option<u64>,
    /// On the runtime's record: the running count of rests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub rests: Option<u64>,
    /// On a governor's record: whether it paused the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub pause: Option<bool>,
    /// On a duty-cycle entry: the delay it instructs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub delay_ms: Option<u64>,
    /// On an episode-limit hold: how many times the machine reached `serious` this mission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub episode: Option<u64>,
    /// On an episode-limit hold: what to check, never a diagnosis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub checklist: Option<String>,
    /// On an episode-limit hold: the command that continues the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub resume_hint: Option<String>,
    /// The scenario file behind a decision made on scripted readings; absent on a real run, so its
    /// presence alone answers "were these readings real".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub simulated_host_source: Option<String>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for DispatchRestPayload {
    fn step_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.step_id)
    }
    fn step(&self) -> Option<&str> {
        self.step_id.as_deref()
    }
    fn context_slot(&mut self) -> Option<&mut Option<serde_json::Map<String, serde_json::Value>>> {
        Some(&mut self.context)
    }
    fn host_source_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.simulated_host_source)
    }
}

/// One `warn`-policy degeneracy finding: the payload of `dispatch.degeneracy.warning`. Nothing was
/// cut; the runtime never acts under `warn`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct DispatchDegeneracyWarningPayload {
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub turn_seq: u64,
    /// Which look found it: `checkpoint` or `stream_gate`.
    pub source: String,
    pub tail_ratio: Option<f64>,
    pub policy: String,
    pub acted: bool,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for DispatchDegeneracyWarningPayload {
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

/// One checkout whose `.git` pointer resolves outside the mounted workdir.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct GitCheckout {
    pub checkout: String,
    pub gitdir_target: String,
    pub kind: GitdirKind,
    pub superproject: Option<String>,
    /// Where the checkout appears inside the container.
    pub container_path: String,
}

/// The preflight found a workdir whose git metadata the container cannot reach: the payload of
/// `dispatch.workdir_git_unavailable`. Every checkout the scan found, not one arbitrary sibling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct DispatchWorkdirGitUnavailablePayload {
    /// The issue this finding is tracked under.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub issue: u64,
    pub workdir: String,
    pub checkouts: Vec<GitCheckout>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for DispatchWorkdirGitUnavailablePayload {
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

/// The routing decision of a dispatch: the payload of `dispatch.route`, emitted under the
/// dispatch's own session so the topology view pairs it with the dispatch's later records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct DispatchRoutePayload {
    /// The machine an explicit `--machine` named; `null` when the dispatch runs here.
    pub target_machine: Option<String>,
    /// The operator-visible verdict, the topology view's edge colour.
    pub decision: RouteDecision,
    /// On a routed dispatch: the `profile@machine` it was sent as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub profile_address: Option<String>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
}

impl Attribution for DispatchRoutePayload {
    fn step_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.step_id)
    }
    fn step(&self) -> Option<&str> {
        self.step_id.as_deref()
    }
}

impl From<darkmux_types::config_access::Source> for KnobSource {
    fn from(source: darkmux_types::config_access::Source) -> Self {
        use darkmux_types::config_access::Source;
        match source {
            Source::BuiltIn => KnobSource::BuiltIn,
            Source::Config => KnobSource::Config,
            Source::Env => KnobSource::Env,
        }
    }
}

impl Knob {
    /// A knob resolved to `value` by `source`.
    pub fn new(value: Option<serde_json::Value>, source: KnobSource) -> Self {
        Knob { value, source, configured_value: None }
    }
}

impl BriefRef {
    pub fn finding(key: impl Into<String>) -> Self {
        BriefRef { kind: BriefRefKind::Finding, key: key.into() }
    }

    pub fn mod_(key: impl Into<String>) -> Self {
        BriefRef { kind: BriefRefKind::Mod, key: key.into() }
    }
}

impl BriefRefKind {
    /// The wire word, used on the step config and the flow record alike.
    pub fn as_str(self) -> &'static str {
        match self {
            BriefRefKind::Finding => "finding",
            BriefRefKind::Mod => "mod",
            BriefRefKind::Unknown => "unknown",
        }
    }

    /// Parse the wire word. `None` for anything else: readers of a step
    /// config or a flow record stay lenient.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "finding" => Some(BriefRefKind::Finding),
            "mod" => Some(BriefRefKind::Mod),
            _ => None,
        }
    }
}

impl DispatchEndPayload {
    /// A terminal that states its turn count and nothing else: the one constructor, so a
    /// producer cannot leave `total_turns` out. Fill the rest with struct update.
    pub fn new(total_turns: u64) -> Self {
        Self {
            wall_ms: None,
            rest_ms: None,
            rests: None,
            paced_rest_ms: None,
            turn_delay_effective_ms: None,
            stdout_chars: None,
            stderr_chars: None,
            stderr_excerpt: None,
            exit_code: None,
            result_class: None,
            error: None,
            total_turns: Some(total_turns),
            total_tools: None,
            tool_calls_failed: None,
            tool_calls_invalid_name: None,
            tool_calls_ungranted: None,
            total_compactions: None,
            live: None,
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            reasoning_tokens: None,
            cached_tokens: None,
            endpoint: None,
            host_window: None,
            resumed_from: None,
            kind: None,
            item_index: None,
            remote_tokens: None,
            step_id: None,
            context: None,
        }
    }

    /// The terminal a bookend guard writes when a dispatch ends before it
    /// completed (an early return or a panic), so its `dispatch.start` is never
    /// left orphaned.
    pub fn aborted(endpoint: Option<String>) -> Self {
        Self {
            result_class: Some(ResultClass::Error),
            error: Some("dispatch terminated before completion (early return or panic)".to_string()),
            endpoint,
            ..Self::new(0)
        }
    }
}
