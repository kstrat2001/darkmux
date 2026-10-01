//! Every event a role execution writes to `trajectory.jsonl`, as one type.
//!
//! The runtime builds these and serializes them; the host and the lab parse
//! the same type back. The `type` string of each event is its serde rename
//! below and nowhere else, so a writer and a reader cannot spell an event
//! differently.
//!
//! Reading is lenient, because the file is an append-only archive: every
//! struct takes `#[serde(default)]`, so a field an older runtime did not
//! write reads as its default, and an event type this build does not know
//! reads as [`TrajectoryEvent::Unknown`]. A line that is not JSON, or whose
//! known fields have the wrong JSON type, is skipped by [`parse_line`].

use serde::{Deserialize, Serialize};

use crate::usage::Usage;

/// One trajectory line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TrajectoryEvent {
    /// The first line of a trajectory (#3035): the file's data-shape
    /// version. Not an execution event: the fold records the version and
    /// counts nothing for it, so an otherwise empty trajectory still folds
    /// empty.
    #[serde(rename = "trajectory.header")]
    Header(Header),
    #[serde(rename = "dispatch.start")]
    DispatchStart(DispatchStart),
    #[serde(rename = "dispatch.complete")]
    DispatchComplete(DispatchComplete),
    #[serde(rename = "model.streaming.start")]
    StreamingStart(StreamingStart),
    #[serde(rename = "model.partial")]
    Partial(Partial),
    #[serde(rename = "model.tool_call.writing")]
    ToolCallWriting(ToolCallWriting),
    #[serde(rename = "model.streaming.end")]
    StreamingEnd(StreamingEnd),
    #[serde(rename = "model.completed")]
    ModelCompleted(ModelCompleted),
    #[serde(rename = "model.reasoning")]
    Reasoning(Reasoning),
    #[serde(rename = "tool.completed")]
    ToolCompleted(ToolCompleted),
    #[serde(rename = "tool_call.promoted")]
    ToolCallPromoted(ToolCallPromoted),
    #[serde(rename = "tool_call.promotion_suppressed")]
    PromotionSuppressed(PromotionSuppressed),
    #[serde(rename = "runtime.rest")]
    Rest(Rest),
    #[serde(rename = "compaction.start")]
    CompactionStart(CompactionStart),
    #[serde(rename = "compaction.call")]
    CompactionCall(CompactionCall),
    #[serde(rename = "compaction")]
    Compaction(Compaction),
    #[serde(rename = "compaction.skipped")]
    CompactionSkipped(CompactionSkipped),
    #[serde(rename = "compaction.unproductive")]
    CompactionUnproductive(CompactionUnproductive),
    #[serde(rename = "dispatch.context")]
    Context(Context),
    #[serde(rename = "dispatch.context.stale_tokens")]
    StaleContextTokens(StaleContextTokens),
    #[serde(rename = "dispatch.pre_send_bound")]
    PreSendBound(PreSendBound),
    #[serde(rename = "dispatch.checkpoint")]
    Checkpoint(Checkpoint),
    #[serde(rename = "dispatch.gate.observation")]
    GateObservation(GateObservation),
    #[serde(rename = "dispatch.gate.abort")]
    GateAbort(GateAbort),
    #[serde(rename = "dispatch.cycle.suspected")]
    CycleSuspected(CycleSuspected),
    #[serde(rename = "dispatch.reasoning_loop.suspected")]
    ReasoningLoopSuspected(ReasoningLoopSuspected),
    #[serde(rename = "dispatch.reasoning_bound.not_applied")]
    ReasoningBoundNotApplied(ReasoningBoundNotApplied),
    #[serde(rename = "dispatch.tool.repeated_failure")]
    RepeatedToolFailure(RepeatedToolFailure),
    #[serde(rename = "dispatch.intra_turn_stall.recovered")]
    IntraTurnStallRecovered(StallRecovered),
    #[serde(rename = "dispatch.empty_tool_calls.recovered")]
    EmptyToolCallsRecovered(StallRecovered),
    #[serde(rename = "dispatch.per_turn_cap.salvaged")]
    PerTurnCapSalvaged(PerTurnCapSalvaged),
    #[serde(rename = "dispatch.tool_call.discarded")]
    ToolCallDiscarded(ToolCallDiscarded),
    #[serde(rename = "dispatch.tool.malformed_names")]
    MalformedToolNames(MalformedToolNames),
    #[serde(rename = "dispatch.escalation.triggered")]
    EscalationTriggered(EscalationTriggered),
    #[serde(rename = "dispatch.feedback.injected")]
    FeedbackInjected(FeedbackInjected),
    /// An event type this build does not know. Never written.
    #[serde(other)]
    Unknown,
}

/// Parse one trajectory line. `None` for a blank line, a line that is not
/// JSON (a run killed mid-write ends in a partial line), or a known event
/// whose `type`, `seq` or a number or flag field has the wrong JSON type:
/// every count is built from those, so a wrong one is skipped, not
/// guessed. A TEXT or opaque field of the wrong type (an older runtime's
/// non-string `args`, say) reads as absent instead of dropping the event
/// and every count it feeds. The token counts inside a `usage` block are
/// read one by one on every path, the strict one included: a count that is
/// not a whole number reads as unreported ([`crate::Usage`]), never as a
/// reason to drop the event.
pub fn parse_line(line: &str) -> Option<TrajectoryEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if line.contains(OPENCLAW_TRACE_SCHEMA) && is_openclaw_line(line) {
        return Some(TrajectoryEvent::Unknown);
    }
    serde_json::from_str(line).ok().or_else(|| parse_tolerant(line))
}

/// The `traceSchema` every line of the retired openclaw runtime named (#1405).
/// Nothing reads that format any more (#3036), and its lines share event
/// types with current ones (`model.completed`), so they are told apart here
/// and read as unknown rather than as a current event.
const OPENCLAW_TRACE_SCHEMA: &str = "openclaw-trajectory";

fn is_openclaw_line(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .is_some_and(|v| v.get("traceSchema").and_then(serde_json::Value::as_str) == Some(OPENCLAW_TRACE_SCHEMA))
}

/// The fields that identify an event: never dropped to rescue it.
const IDENTIFYING_FIELDS: [&str; 2] = ["type", "seq"];

/// The strict parse failed. Each field is judged on its own, beside the
/// identifying fields (every event struct is flat and defaults whatever is
/// absent): one that reads is kept; one that does not is dropped only when
/// the event would accept text there, i.e. it is a text field. A mistyped
/// number or flag leaves the event unreadable. Runs only on a line that
/// already failed, so the ordinary path pays nothing for it.
fn parse_tolerant(line: &str) -> Option<TrajectoryEvent> {
    let serde_json::Value::Object(obj) = serde_json::from_str::<serde_json::Value>(line).ok()? else {
        return None;
    };
    let identity: serde_json::Map<String, serde_json::Value> =
        obj.iter().filter(|(k, _)| IDENTIFYING_FIELDS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    let reads = |k: &str, v: serde_json::Value| {
        let mut probe = identity.clone();
        probe.insert(k.to_string(), v);
        serde_json::from_value::<TrajectoryEvent>(serde_json::Value::Object(probe)).is_ok()
    };
    let mut kept = identity.clone();
    for (k, v) in obj.iter().filter(|(k, _)| !IDENTIFYING_FIELDS.contains(&k.as_str())) {
        if reads(k, v.clone()) {
            kept.insert(k.clone(), v.clone());
        } else if !reads(k, serde_json::Value::String(String::new())) {
            return None;
        }
    }
    serde_json::from_value(serde_json::Value::Object(kept)).ok()
}

/// `trajectory.header`: the version of the shape the rest of the file is in.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Header {
    pub schema_version: String,
}

/// `dispatch.start`: the first execution event.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DispatchStart {
    pub ts: u64,
    pub model: String,
    pub system_chars: u64,
    pub prompt_chars: u64,
    /// The ADVERTISED tool names: what the model was offered after the
    /// allow-list, not what the host requested.
    pub tools: Vec<String>,
}

/// The `dispatch.complete` result of an execution a signal ended (the
/// runtime's SIGTERM/SIGINT path, or a host that found no terminal record
/// after the execution died). Never a reason the loop chose.
pub const RESULT_INTERRUPTED: &str = "interrupted";

/// The prefix every runtime escalation reason carries (`escalation_*`).
const ESCALATION_PREFIX: &str = "escalation";

/// A `dispatch.complete` / envelope `result`, typed (F2). The one parse of
/// that string, so no surface compares `result` text itself and a deliberate
/// escalation can never be read as an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalResult {
    Stop,
    MaxTurns,
    /// The runtime stopped on purpose, at an operator-configured bound or a
    /// detected loop, and handed the work to a higher tier. Not a failure.
    Escalated,
    Interrupted,
    Error,
    /// A result this reader does not know (a newer runtime).
    Other,
}

impl TerminalResult {
    pub fn parse(result: &str) -> Self {
        match result {
            "stop" => Self::Stop,
            "max_turns" => Self::MaxTurns,
            RESULT_INTERRUPTED => Self::Interrupted,
            "error" => Self::Error,
            r if r.starts_with(ESCALATION_PREFIX) => Self::Escalated,
            _ => Self::Other,
        }
    }
}

/// `dispatch.complete`: the last event, written on every exit the runtime
/// reaches. A SIGTERM or SIGINT writes it with `result: "interrupted"`
/// ([`RESULT_INTERRUPTED`]); SIGKILL is uncatchable and writes none.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DispatchComplete {
    pub ts: u64,
    /// The terminal reason: `stop`, `max_turns`, an `escalation_*` reason,
    /// `error`, or `interrupted` ([`RESULT_INTERRUPTED`]).
    pub result: String,
    /// The runtime's own wall clock for the execution, rests included.
    pub wall_ms: u64,
    /// The POST-CLAMP turn delay the loop applied. `None` when the loop
    /// returned an error (nothing to report) or the runtime predates it.
    pub turn_delay_effective_ms: Option<u64>,
}

/// `model.streaming.start`: a streamed call begins, before any chunk.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamingStart {
    pub seq: u64,
    pub ts: u64,
    /// Total length of the system-role messages in the request.
    pub system_chars: u64,
    /// Total length of every other message in the request.
    pub prompt_chars: u64,
}

/// What the model is doing mid-stream, when it is more than writing text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamPhase {
    /// A tool call has been named and its arguments are being written. The
    /// viewer matches this spelling (`ui/src/lib/tokenRate.ts`).
    WritingToolCall,
}

/// `model.partial`: one streamed chunk. Stats only, never the chunk text.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Partial {
    pub seq: u64,
    pub ts: u64,
    pub partial_index: u64,
    pub delta_chars: u64,
    /// Answer text so far in THIS stream (reasoning excluded).
    pub cumulative_chars: u64,
    pub tool_calls_present: bool,
    /// Generated chars so far including separate-field reasoning.
    pub generated_chars: Option<u64>,
    /// Absent while no tool call is named.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<StreamPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

/// `model.tool_call.writing`: a tick while the endpoint is silent and a tool
/// call has been named. Not a chunk: its counts are the last chunk's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolCallWriting {
    pub seq: u64,
    pub ts: u64,
    pub partial_index: u64,
    pub cumulative_chars: u64,
    pub generated_chars: Option<u64>,
    pub phase: StreamPhase,
    pub tool_name: String,
}

impl Default for ToolCallWriting {
    fn default() -> Self {
        Self {
            seq: 0,
            ts: 0,
            partial_index: 0,
            cumulative_chars: 0,
            generated_chars: None,
            phase: StreamPhase::WritingToolCall,
            tool_name: String::new(),
        }
    }
}

/// `model.streaming.end`: the stream terminated, before `model.completed`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamingEnd {
    pub seq: u64,
    pub ts: u64,
    pub partial_count: u64,
    pub total_content_chars: u64,
    pub tool_calls_count: u64,
    /// How many times the runtime looked at this call's output mid-stream.
    pub observations: Option<u64>,
    /// Generated chars over the endpoint's `completion_tokens`; `None` when
    /// the endpoint reported no usage.
    pub chars_per_token: Option<f64>,
}

/// One tool call a `model.completed` reported.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolCallEntry {
    pub id: String,
    pub name: String,
    pub arguments_chars: u64,
    /// The call's `path` argument, for a tool that takes one. Never the
    /// content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `Some(false)` on a call that will not run; absent on one that will.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runs: Option<bool>,
}

/// `model.completed`: one chat-completion response. A checkpoint
/// continuation reports its own, under the SAME `seq` as the turn it
/// resumes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelCompleted {
    pub seq: u64,
    pub ts: u64,
    pub finish_reason: String,
    /// `None` when the reply carried no usage block (a cut stream is never
    /// billed).
    pub usage: Option<Usage>,
    pub tool_calls: Option<Vec<ToolCallEntry>>,
    /// The model the server says answered. Absent when it named none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reported_model: Option<String>,
    /// True when `tool_calls` carry their `runs` marks.
    #[serde(default, skip_serializing_if = "is_false")]
    pub calls_planned: bool,
    /// The runtime's own estimate of the completion tokens of a call IT cut
    /// (the degeneracy gate, a silent stream). Such a call has `usage: None`
    /// because the endpoint never sent one; this is what streamed past
    /// before the cut, never a reported count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_estimate: Option<u64>,
}

impl ModelCompleted {
    /// A `length` finish is a continuation of the same logical turn, not its
    /// end. The loop decides whether to run its terminal fold on exactly
    /// this test.
    pub fn ends_turn(&self) -> bool {
        self.finish_reason != "length"
    }
}

/// `model.reasoning`: the thinking text of one turn, in full.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Reasoning {
    pub seq: u64,
    pub ts: u64,
    pub reasoning_text: String,
    pub reasoning_chars: u64,
    /// `inline-think-tags` or `separate-field`. Absent on the oldest
    /// runtimes, which only parsed inline tags.
    pub reasoning_format: Option<String>,
}

/// How a tool call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcomeKind {
    /// Ran and reported success.
    Ok,
    /// Ran and reported a non-zero result (a red test is work).
    Reported,
    /// Did not run, or could not complete.
    Failed,
}

/// `tool.completed`: one executed tool call, with its arguments preview and
/// its result in full.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolCompleted {
    pub seq: u64,
    pub ts: u64,
    pub tool_seq: u64,
    pub tool_name: String,
    /// A preview, capped by the runtime.
    pub args: String,
    pub args_chars: u64,
    /// An accepted `create_finding`'s emission, verbatim; `None` for every
    /// other call.
    pub emitted: Option<serde_json::Value>,
    /// The emission's 1-based ordinal in this execution.
    pub emit_seq: Option<u64>,
    /// The TRUE result length, so a future cap's truncation is visible.
    pub result_chars: u64,
    pub result: String,
    pub outcome: Option<ToolOutcomeKind>,
    pub exit_code: Option<i64>,
    pub failure_reason: Option<String>,
    /// Did the tool do its job (true for a red test). Absent on runtimes
    /// older than #469, which counted every call as a success.
    pub ok: bool,
}

impl Default for ToolCompleted {
    fn default() -> Self {
        Self {
            seq: 0,
            ts: 0,
            tool_seq: 0,
            tool_name: String::new(),
            args: String::new(),
            args_chars: 0,
            emitted: None,
            emit_seq: None,
            result_chars: 0,
            result: String::new(),
            outcome: None,
            exit_code: None,
            failure_reason: None,
            ok: true,
        }
    }
}

/// `tool_call.promoted`: plain-text tool-call markup the runtime turned back
/// into structured calls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolCallPromoted {
    pub seq: u64,
    pub ts: u64,
    /// `content` or `reasoning`.
    pub source: String,
    /// `bracket`, `harmony` or `xml`.
    pub format: String,
    pub promoted_call_count: u64,
    pub xml_openers_skipped_as_fenced: u64,
}

impl Default for ToolCallPromoted {
    fn default() -> Self {
        Self {
            seq: 0,
            ts: 0,
            source: String::new(),
            format: String::new(),
            // A record older than the count promoted one call.
            promoted_call_count: 1,
            xml_openers_skipped_as_fenced: 0,
        }
    }
}

/// `tool_call.promotion_suppressed`: nothing promoted because every opener
/// sat inside a markdown fence.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PromotionSuppressed {
    pub seq: u64,
    pub ts: u64,
    pub xml_openers_skipped_as_fenced: u64,
}

/// Why the loop slept.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum RestReason {
    /// Routine turn-to-turn cool-down (`turn_delay_ms`). The reading of a
    /// rest recorded before reasons existed.
    #[default]
    TurnDelay,
    /// A paced rest: the pace file's own reason (a manual pause, or the
    /// thermal governor's `thermal-duty-cycle`).
    Paced(String),
}

const TURN_DELAY: &str = "turn_delay";

impl From<String> for RestReason {
    fn from(s: String) -> Self {
        if s == TURN_DELAY {
            Self::TurnDelay
        } else {
            Self::Paced(s)
        }
    }
}

impl From<RestReason> for String {
    fn from(r: RestReason) -> Self {
        match r {
            RestReason::TurnDelay => TURN_DELAY.to_string(),
            RestReason::Paced(s) => s,
        }
    }
}

/// `runtime.rest`: one sleep between turns. `ms` is the duration actually
/// slept.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rest {
    pub seq: u64,
    pub ts: u64,
    pub ms: u64,
    pub reason: RestReason,
    /// The pace file's `state` (e.g. the thermal state that tripped it).
    pub state: Option<String>,
}

/// `compaction.start`: a compaction is about to call its compactor.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionStart {
    pub generation: u64,
    pub ts: u64,
    /// The compactor model id; absent when the runtime was given none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_model: Option<String>,
}

/// `compaction.call`: one compactor model call that got a reply. A
/// sub-execution of the utility role, never a turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionCall {
    pub generation: u64,
    pub ts: u64,
    pub requested_model: String,
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reported_model: Option<String>,
}

/// `compaction`: a compaction was installed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Compaction {
    pub generation: u64,
    pub ts: u64,
    pub before_messages: u64,
    pub after_messages: u64,
    pub summary_chars: u64,
    /// The exact prompt-token count that triggered it.
    pub tokens_before: Option<u64>,
    /// A chars/4 estimate of the compacted buffer.
    pub tokens_after: Option<u64>,
}

/// `compaction.skipped`: triggered and refused; nothing was installed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionSkipped {
    pub turn: u64,
    pub attempted_generation: u64,
    pub ts: u64,
    pub messages: u64,
    pub reason: String,
}

/// `compaction.unproductive`: compaction keeps leaving the thread above its
/// own trigger.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionUnproductive {
    pub turn: u64,
    pub ts: u64,
    pub consecutive: u64,
    pub tokens_after: u64,
    pub trigger_tokens: u64,
}

/// `dispatch.context`: per-turn context occupancy.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Context {
    pub seq: u64,
    pub ts: u64,
    /// The exact prompt-token count the endpoint reported.
    pub used: u64,
    /// The configured context window; `None` when unconfigured.
    pub max: Option<u64>,
}

/// `dispatch.context.stale_tokens`: the endpoint's prompt count froze while
/// the thread grew.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StaleContextTokens {
    pub seq: u64,
    pub ts: u64,
    pub frozen_value: u64,
    pub frozen_turns: u64,
    pub estimate: u64,
    pub message_count: u64,
}

/// `dispatch.pre_send_bound`: the assembled prompt exceeded the declared
/// window, and what was trimmed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PreSendBound {
    pub turn: u64,
    pub ts: u64,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub declared_window: u64,
    pub results_trimmed: u64,
    pub fits: bool,
}

/// What a checkpoint decided.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Let the turn keep going.
    #[default]
    Continue,
    /// Close the thought and ask for the answer.
    Conclude,
}

/// `dispatch.checkpoint`: the harness checked in on a turn at the reasoning
/// interval.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Checkpoint {
    pub seq: u64,
    pub ts: u64,
    pub checkpoint: u64,
    /// `None` when the slice was too short to judge.
    pub slice_tokens: Option<u64>,
    /// `None` when the slice was too short to judge (never a 0.0, which
    /// would read as maximally repetitive).
    pub tail_ratio: Option<f64>,
    pub verdict: Verdict,
    pub judged_chars: Option<u64>,
    /// The degeneracy policy in force, as recorded (`off`/`record`/`warn`/
    /// `conclude`, or `enforce`/`observe` before 4.0).
    pub policy: Option<String>,
    /// What the detector FOUND, independent of whether it was allowed to
    /// act. Absent on records older than the field.
    pub would_conclude: Option<bool>,
    /// The bound judged against, with its provenance. Opaque here: the host
    /// forwards it verbatim.
    pub bound: Option<serde_json::Value>,
}

impl Checkpoint {
    /// Whether the detector judged this slice degenerate. A record older
    /// than `would_conclude` only knows what it did, so a conclusion counts.
    pub fn judged_degenerate(&self) -> bool {
        self.would_conclude.unwrap_or(self.verdict == Verdict::Conclude)
    }
}

/// `dispatch.gate.observation`: one look at the stream.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GateObservation {
    pub seq: u64,
    pub ts: u64,
    pub observation: u64,
    pub slice_chars: u64,
    pub tail_ratio: Option<f64>,
    pub interval_tokens: u64,
    pub degenerate: bool,
    pub policy: Option<String>,
    /// Whether THIS verdict ended the call.
    pub acted: Option<bool>,
}

/// `dispatch.gate.abort`: the in-stream observer ended a call itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GateAbort {
    pub seq: u64,
    pub ts: u64,
    pub observation: u64,
    pub slice_chars: u64,
    pub generated_chars: u64,
    pub interval_tokens: u64,
    /// Always false for a degeneracy abort; a `true` is a bug announcing
    /// itself.
    pub tool_call_in_flight: bool,
    pub policy: Option<String>,
    pub acted: bool,
}

/// `dispatch.cycle.suspected`: the same tool call K times in a window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CycleSuspected {
    pub seq: u64,
    pub ts: u64,
    pub tool_name: String,
    pub canonical_args: String,
    /// The target file's firing-time content hash; absent without a target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_hash: Option<String>,
    pub count: u64,
    pub window_size: u64,
}

/// `dispatch.reasoning_loop.suspected`: the same reasoning K times in a
/// window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReasoningLoopSuspected {
    pub seq: u64,
    pub ts: u64,
    pub count: u64,
    pub window_size: u64,
}

/// `dispatch.reasoning_bound.not_applied`: the model does not appear to
/// reason, so the check-in interval is not applied to fresh turns.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReasoningBoundNotApplied {
    pub seq: u64,
    pub ts: u64,
}

/// `dispatch.tool.repeated_failure`: one call signature kept failing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepeatedToolFailure {
    pub seq: u64,
    pub ts: u64,
    pub tool_name: String,
    pub canonical_args: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_hash: Option<String>,
    pub failure_count: u64,
}

/// `dispatch.intra_turn_stall.recovered` and
/// `dispatch.empty_tool_calls.recovered`: a useless turn was dropped and
/// retried.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StallRecovered {
    pub seq: u64,
    pub ts: u64,
    /// `None` when the response carried no usage ("unknown", not 0).
    pub completion_tokens: Option<u64>,
    pub recoveries_used: u64,
    pub recoveries_budget: u64,
    pub bound: Option<serde_json::Value>,
}

/// `dispatch.per_turn_cap.salvaged`: well-formed tool calls dispatched from
/// a turn cut at its cap.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PerTurnCapSalvaged {
    pub seq: u64,
    pub ts: u64,
    pub completion_tokens: u64,
    pub cap: u64,
    pub salvaged_tool_calls: u64,
    pub bound: Option<serde_json::Value>,
}

/// `dispatch.tool_call.discarded`: a call thrown away because the cut
/// landed inside its arguments.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolCallDiscarded {
    pub seq: u64,
    pub ts: u64,
    pub name: String,
    pub arguments_chars: u64,
    /// Who ended the call (`server_length`, `runtime_abort:*`).
    pub cut: String,
}

/// (#2169) Why a structured tool call is not dispatchable. Two different
/// causes, never one bucket: telling a model it called a fictional tool
/// when it named `bash` correctly without the grant misleads it, and a
/// permission refusal mixed into the "garbage tool-call names" count would
/// corrupt the signal that count exists to surface.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MalformedReason {
    /// No darkmux tool is named this (e.g. model content sliced around a
    /// `[TOOL_CALLS]` marker became the name). The reading of a record
    /// older than the field.
    #[default]
    NotATool,
    /// A real darkmux tool this execution's role was not granted.
    RealToolNotGranted,
}

/// `dispatch.tool.malformed_names`: one turn's calls whose names the
/// runtime refused, coalesced per reason.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MalformedToolNames {
    pub seq: u64,
    pub ts: u64,
    pub count: u64,
    pub model: String,
    /// One representative name, already sanitized by the runtime.
    pub sample_name_prefix: String,
    pub reason: MalformedReason,
}

/// `dispatch.escalation.triggered`: the execution terminated by escalation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EscalationTriggered {
    pub seq: u64,
    pub ts: u64,
    /// The same string the envelope's `result` carries for it.
    pub reason: String,
    pub model: String,
    pub prompt_tokens: u64,
}

/// `dispatch.feedback.injected`: system messages delivered to the model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedbackInjected {
    pub seq: u64,
    pub ts: u64,
    pub message_count: u64,
    pub signal_kinds: Vec<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl TrajectoryEvent {
    /// The turn an event belongs to, for the events that carry one.
    pub fn seq(&self) -> Option<u64> {
        use TrajectoryEvent as E;
        match self {
            E::StreamingStart(e) => Some(e.seq),
            E::Partial(e) => Some(e.seq),
            E::ToolCallWriting(e) => Some(e.seq),
            E::StreamingEnd(e) => Some(e.seq),
            E::ModelCompleted(e) => Some(e.seq),
            E::Reasoning(e) => Some(e.seq),
            E::ToolCompleted(e) => Some(e.seq),
            E::ToolCallPromoted(e) => Some(e.seq),
            E::PromotionSuppressed(e) => Some(e.seq),
            E::Rest(e) => Some(e.seq),
            E::Context(e) => Some(e.seq),
            E::StaleContextTokens(e) => Some(e.seq),
            E::Checkpoint(e) => Some(e.seq),
            E::GateObservation(e) => Some(e.seq),
            E::GateAbort(e) => Some(e.seq),
            E::CycleSuspected(e) => Some(e.seq),
            E::ReasoningLoopSuspected(e) => Some(e.seq),
            E::ReasoningBoundNotApplied(e) => Some(e.seq),
            E::RepeatedToolFailure(e) => Some(e.seq),
            E::IntraTurnStallRecovered(e) | E::EmptyToolCallsRecovered(e) => Some(e.seq),
            E::PerTurnCapSalvaged(e) => Some(e.seq),
            E::ToolCallDiscarded(e) => Some(e.seq),
            E::MalformedToolNames(e) => Some(e.seq),
            E::EscalationTriggered(e) => Some(e.seq),
            E::FeedbackInjected(e) => Some(e.seq),
            E::DispatchStart(_)
            | E::DispatchComplete(_)
            | E::CompactionStart(_)
            | E::CompactionCall(_)
            | E::Compaction(_)
            | E::CompactionSkipped(_)
            | E::CompactionUnproductive(_)
            | E::PreSendBound(_)
            | E::Header(_)
                        | E::Unknown => None,
        }
    }

    /// When the runtime wrote the event, in unix milliseconds. `None` for
    /// the events that carry no clock.
    pub fn ts(&self) -> Option<u64> {
        use TrajectoryEvent as E;
        match self {
            E::DispatchStart(e) => Some(e.ts),
            E::DispatchComplete(e) => Some(e.ts),
            E::StreamingStart(e) => Some(e.ts),
            E::Partial(e) => Some(e.ts),
            E::ToolCallWriting(e) => Some(e.ts),
            E::StreamingEnd(e) => Some(e.ts),
            E::ModelCompleted(e) => Some(e.ts),
            E::Reasoning(e) => Some(e.ts),
            E::ToolCompleted(e) => Some(e.ts),
            E::ToolCallPromoted(e) => Some(e.ts),
            E::PromotionSuppressed(e) => Some(e.ts),
            E::Rest(e) => Some(e.ts),
            E::CompactionStart(e) => Some(e.ts),
            E::CompactionCall(e) => Some(e.ts),
            E::Compaction(e) => Some(e.ts),
            E::CompactionSkipped(e) => Some(e.ts),
            E::CompactionUnproductive(e) => Some(e.ts),
            E::Context(e) => Some(e.ts),
            E::StaleContextTokens(e) => Some(e.ts),
            E::PreSendBound(e) => Some(e.ts),
            E::Checkpoint(e) => Some(e.ts),
            E::GateObservation(e) => Some(e.ts),
            E::GateAbort(e) => Some(e.ts),
            E::CycleSuspected(e) => Some(e.ts),
            E::ReasoningLoopSuspected(e) => Some(e.ts),
            E::ReasoningBoundNotApplied(e) => Some(e.ts),
            E::RepeatedToolFailure(e) => Some(e.ts),
            E::IntraTurnStallRecovered(e) | E::EmptyToolCallsRecovered(e) => Some(e.ts),
            E::PerTurnCapSalvaged(e) => Some(e.ts),
            E::ToolCallDiscarded(e) => Some(e.ts),
            E::MalformedToolNames(e) => Some(e.ts),
            E::EscalationTriggered(e) => Some(e.ts),
            E::FeedbackInjected(e) => Some(e.ts),
            E::Header(_) | E::Unknown => None,
        }
    }
}
