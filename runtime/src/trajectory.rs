//! Trajectory + metrics recording.
//!
//! Gives post-dispatch visibility. Writes a line-per-event JSONL trace
//! to `<RUNTIME_OUT_BASE>/.darkmux-runtime/trajectory.jsonl` (i.e.
//! `/darkmux-out/.darkmux-runtime/` — the out-dir, SEPARATE from the
//! agent's `/workspace`) plus a top-line `metrics.json` at exit.
//! Operators inspect these after the container is gone (the `--rm`
//! mode otherwise loses everything except stderr).
//!
//! The shape of each event mirrors openclaw's trajectory format
//! closely enough that a side-by-side diff between the two runtimes
//! is feasible — same `type` field, same `seq`, same `usage` shape
//! on model.completed events.
//!
//! Failure mode: if the trajectory directory can't be created or the
//! file can't be opened, the recorder degrades to a silent no-op so
//! the dispatch itself isn't blocked by an instrumentation problem.
//! Operator-visible stderr line announces success/failure.

use anyhow::Result;
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::lmstudio::{ToolCall, Usage};

/// Container mount point for darkmux's OWN bookkeeping — SEPARATE from
/// /workspace so the runtime never writes its logs into the tree it's
/// operating on. dispatch_internal mounts a host tempdir here. An
/// unmounted manual `docker run` writes to the container-ephemeral path
/// (lost on --rm) but /workspace stays clean either way.
/// MUST match the mount point in darkmux-crew's dispatch_internal.rs.
pub const RUNTIME_OUT_BASE: &str = "/darkmux-out";

/// Subdir (under the out-base) where trajectory + metrics land. The
/// dot-prefix is a soft signal that this is runtime metadata rather
/// than agent content; agents that respect "don't muck with dotfiles"
/// conventions will leave it alone.
const TRAJECTORY_SUBDIR: &str = ".darkmux-runtime";
const TRAJECTORY_FILE: &str = "trajectory.jsonl";
const METRICS_FILE: &str = "metrics.json";

/// Cap on the recorded tool-argument string. A search pattern, file path, or
/// shell command is far under this; only a `write`/`edit` file-content arg
/// exceeds it, and a truncated head is enough to recall what was attempted.
const MAX_TOOL_ARGS_CHARS: usize = 512;

/// Truncate to at most `max` chars on a char boundary, appending an ellipsis
/// marker when truncation happened. Never splits a multi-byte char.
fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// The runtime's bookkeeping directory: `<RUNTIME_OUT_BASE>/<TRAJECTORY_SUBDIR>`.
/// Both runtime write sites (trajectory/metrics in `main.rs`, structured
/// compaction output in `loop_runner.rs`) consume this so the two can't
/// drift to different paths.
pub fn runtime_dir() -> std::path::PathBuf {
    std::path::Path::new(RUNTIME_OUT_BASE).join(TRAJECTORY_SUBDIR)
}

/// Trajectory + metrics recorder. Open at dispatch start; methods
/// append events as they occur; `save_metrics()` writes the final
/// summary at exit.
pub struct Trajectory {
    /// `None` when recording is disabled (open() failed; degraded
    /// silently). All append methods become no-ops in that case.
    file: Option<File>,
    metrics_path: Option<PathBuf>,
    started: Instant,
}

/// Top-line summary written to metrics.json at dispatch exit.
#[derive(Debug, Serialize)]
pub struct Metrics {
    pub runtime: &'static str,
    pub version: &'static str,
    pub model: String,
    pub started_at_unix_ms: u64,
    pub wall_ms: u128,
    pub result: String,
    pub turns: u32,
    pub compactions: u32,
    pub total_prompt_tokens: u32,
    pub total_completion_tokens: u32,
    pub total_messages: usize,
    pub max_turns_reached: bool,
    /// (#2094) Sum of every inter-turn rest this dispatch took, in
    /// milliseconds — the AFTER-clamp duration actually slept. `wall_ms`
    /// above INCLUDES this time (wall stays wall); a caller wanting
    /// model-only time subtracts `rest_ms` from `wall_ms` itself.
    pub rest_ms: u64,
    /// (#2094) How many inter-turn rests fired during this dispatch.
    pub rests: u32,
    /// (#2094 finding 8) The POST-CLAMP `turn_delay_ms` cadence this
    /// dispatch actually applied (`resolve_turn_delay_ms`'s output) — the
    /// effective knob, known even on a dispatch that took zero rests.
    /// Read back host-side (`dispatch_internal.rs`) and surfaced on the
    /// `dispatch.complete` flow payload as `turn_delay_effective_ms`.
    pub turn_delay_effective_ms: u64,
    /// First 400 chars of the final assistant message (for at-a-glance
    /// "what did the agent end up saying"). Truncated to keep
    /// metrics.json human-readable in a terminal.
    pub final_assistant_preview: String,
}

impl Trajectory {
    /// Open a trajectory file at `<base_dir>/.darkmux-runtime/`. In
    /// production `base_dir` is `RUNTIME_OUT_BASE` (`/darkmux-out`, the
    /// out-dir — SEPARATE from the agent's `/workspace`); tests pass a
    /// tempdir. If the directory can't be created (permission, missing
    /// path, etc.) returns a degraded no-op recorder rather than failing.
    pub fn open(base_dir: &Path) -> Self {
        let dir = base_dir.join(TRAJECTORY_SUBDIR);
        let trajectory_path = dir.join(TRAJECTORY_FILE);
        let metrics_path = dir.join(METRICS_FILE);

        match try_open(&dir, &trajectory_path) {
            Ok(file) => {
                eprintln!(
                    "darkmux-runtime: trajectory → {}",
                    trajectory_path.display()
                );
                Self {
                    file: Some(file),
                    metrics_path: Some(metrics_path),
                    started: Instant::now(),
                }
            }
            Err(e) => {
                eprintln!(
                    "darkmux-runtime: trajectory recording disabled ({e}); \
                     dispatch will continue without it"
                );
                Self {
                    file: None,
                    metrics_path: None,
                    started: Instant::now(),
                }
            }
        }
    }

    /// dispatch.start — first event in the trajectory.
    /// (#2268) `tools` is the ADVERTISED tool-name list — what the model was
    /// actually offered, after the allow-list — not what the host requested.
    pub fn append_dispatch_start(
        &mut self,
        model: &str,
        system_chars: usize,
        prompt_chars: usize,
        tools: &[&str],
    ) {
        self.write_event(&serde_json::json!({
            "type": "dispatch.start",
            "ts": unix_ms(),
            "model": model,
            "system_chars": system_chars,
            "prompt_chars": prompt_chars,
            "tools": tools,
        }));
    }

    /// model.completed — one per chat-completion response. Mirrors
    /// openclaw's `model.completed` event shape (seq + finish_reason +
    /// usage) so trajectories can be diffed cross-runtime.
    pub fn append_model_completed(
        &mut self,
        seq: u32,
        finish_reason: &str,
        usage: Option<&Usage>,
        tool_calls: Option<&[ToolCall]>,
    ) {
        let usage_json = usage.map(|u| {
            serde_json::json!({
                "prompt_tokens": u.prompt_tokens,
                "completion_tokens": u.completion_tokens,
                "total_tokens": u.total_tokens,
            })
        });
        let tool_calls_json = tool_calls.map(|calls| {
            calls
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "id": c.id,
                        "name": c.function.name,
                        "arguments_chars": c.function.arguments.len(),
                    })
                })
                .collect::<Vec<_>>()
        });
        self.write_event(&serde_json::json!({
            "type": "model.completed",
            "seq": seq,
            "ts": unix_ms(),
            "finish_reason": finish_reason,
            "usage": usage_json,
            "tool_calls": tool_calls_json,
        }));
    }

    /// model.reasoning — one per turn where the model emitted reasoning
    /// content (parsed from inline `<think>...</think>` blocks in the
    /// assistant message content, OR from a separate `reasoning_content`
    /// field when the model uses that pattern). Schema 1.6 addition for
    /// flow stream richer events (#204).
    ///
    /// Carries the FULL reasoning text — flow viewer renders as a
    /// collapse/expand block per operator-discretion design. Reasoning
    /// can be 5-10× the size of the actual response on hard problems;
    /// expect trajectory.jsonl sizes to grow proportionally when
    /// thinking-mode models are used.
    ///
    /// `format` is one of `"inline-think-tags"` (parsed from content)
    /// or `"separate-field"` (extracted from a `reasoning_content`
    /// field). Lets downstream consumers know how to interpret the
    /// reasoning's relationship to the rest of the assistant message.
    pub fn append_model_reasoning(
        &mut self,
        seq: u32,
        reasoning_text: &str,
        format: &str,
    ) {
        self.write_event(&serde_json::json!({
            "type": "model.reasoning",
            "seq": seq,
            "ts": unix_ms(),
            "reasoning_text": reasoning_text,
            "reasoning_chars": reasoning_text.chars().count(),
            "reasoning_format": format,
        }));
    }

    /// dispatch.tool.repeated_failure — fires (edge-triggered) when
    /// one tool+args signature's failure counter crosses the threshold
    /// (#419). Sibling to dispatch.cycle.suspected: that catches
    /// repeated SUCCESS patterns; this catches repeated FAILURE
    /// patterns. Observability-only in the MVP — no behavioral
    /// change. Pattern observed empirically Beat 45: agent kept
    /// retrying gcc inside dispatch sandbox where it doesn't exist;
    /// burned ~20 turns before MAX_TURNS bailed.
    pub fn append_tool_repeated_failure(
        &mut self,
        seq: u32,
        tool_name: &str,
        // (#1001) The failing tool's args (so the host can derive the file the
        // cascade is on) + the file's firing-time content hash for staleness.
        // `code_hash` is omitted when the tool has no file target.
        canonical_args: &str,
        code_hash: Option<&str>,
        failure_count: u32,
    ) {
        let mut event = serde_json::json!({
            "type": "dispatch.tool.repeated_failure",
            "seq": seq,
            "ts": unix_ms(),
            "tool_name": tool_name,
            "canonical_args": canonical_args,
            "failure_count": failure_count,
        });
        if let Some(h) = code_hash {
            event["code_hash"] = serde_json::Value::String(h.to_string());
        }
        self.write_event(&event);
    }

    /// dispatch.cycle.suspected — fires (edge-triggered) when the
    /// cycle detector observes the same tool_name+canonical_args
    /// hash appearing K times within the recent window of N tool
    /// calls (#418). Observability-only in the MVP — no behavioral
    /// change. The "model keeps reading the same file" pattern that
    /// MAX_TURNS catches LATE; this catches it EARLY for operator
    /// visibility. Hash collisions across compactions: yes, expected
    /// (the same file may legitimately be re-read after compaction
    /// evicts it from history); the warn is informational, not
    /// accusatory.
    /// (#414 PR A) dispatch.intra_turn_stall.recovered — recovery event
    /// when the loop caught a `finish_reason=length` response that had
    /// no content and no tool_calls (the runaway-reasoning shape from
    /// Beat 47 / N=5-post-#439 Run 1) and salvaged the dispatch by
    /// dropping the useless turn, injecting a nudge system message,
    /// and retrying. Records the per-turn completion-token count so
    /// trajectory replay can confirm the stall was per-call-cap-shaped
    /// (count ≈ MAX_TOKENS_PER_CALL) vs context-overflow-shaped (count
    /// well below cap), and the recovery budget consumption so
    /// repeated stalls in one dispatch are visible at a glance.
    /// (#1221) The harness checked in on a turn that hit the checkpoint
    /// interval, and either let it keep thinking or asked it to conclude.
    ///
    /// Its own event type on purpose. A checkpoint continuation is the SAME
    /// logical turn resuming, not a new one — but it IS a new API call, so
    /// without this the only trace was another `model.completed` and the
    /// stream read as N turns for what the operator watched as one unbroken
    /// thought. Twelve continuations of one turn produced twelve "turn"
    /// records and exactly one `model.reasoning`, because after a prefill the
    /// provider returns the continued thinking as ordinary `content` and no
    /// longer tags it as reasoning.
    ///
    /// This record says what the HARNESS did, which is a different fact from
    /// what the model did, and belongs in a different event.
    /// (#2165) `bound` names WHICH bound this checkpoint continuation
    /// judged against + its provenance — a checkpoint only ever fires on
    /// the reasoning check-in interval, so callers pass
    /// `BoundKind::ReasoningCheckpointInterval`.
    pub fn append_checkpoint(
        &mut self,
        seq: u32,
        checkpoint: u32,
        slice_tokens: Option<u32>,
        tail_ratio: Option<f32>,
        verdict: &str,
        bound: crate::bounds::BoundRef,
    ) {
        let slice = slice_tokens
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null);
        // `null` rather than a number when the slice was too short to judge —
        // a 0.0 would read as "maximally repetitive", the exact opposite.
        let ratio = tail_ratio
            .and_then(|r| serde_json::Number::from_f64(r as f64))
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null);
        self.write_event(&serde_json::json!({
            "type": "dispatch.checkpoint",
            "seq": seq,
            "ts": unix_ms(),
            "checkpoint": checkpoint,
            "slice_tokens": slice,
            "tail_ratio": ratio,
            "verdict": verdict,
            "bound": bound,
        }));
    }

    /// (#2165) `bound` names WHICH bound governed the request that stalled
    /// (the reasoning check-in interval or the per-call cap, whichever
    /// region was in force) + its provenance.
    pub fn append_intra_turn_stall_recovered(
        &mut self,
        seq: u32,
        completion_tokens: Option<u32>,
        recoveries_used: u32,
        recoveries_budget: u32,
        bound: crate::bounds::BoundRef,
    ) {
        // The event's analytic purpose is to discriminate per-call-cap
        // stalls (completion_tokens ≈ MAX_TOKENS_PER_CALL) from
        // context-overflow stalls (count well below cap). When the
        // upstream response omits `usage` (rare but possible), emit
        // the field as null so consumers see "unknown" rather than a
        // misleading 0 that reads identical to a real small count.
        let completion_tokens_value = completion_tokens
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null);
        self.write_event(&serde_json::json!({
            "type": "dispatch.intra_turn_stall.recovered",
            "seq": seq,
            "ts": unix_ms(),
            "completion_tokens": completion_tokens_value,
            "recoveries_used": recoveries_used,
            "recoveries_budget": recoveries_budget,
            "bound": bound,
        }));
    }

    /// (#2190) dispatch.empty_tool_calls.recovered — sibling of
    /// `append_intra_turn_stall_recovered` above, for the shape that is NOT
    /// runaway reasoning: a turn returned `finish_reason=tool_calls` with an
    /// EMPTY `tool_calls` array. This is a protocol-shaped failure (the
    /// model claimed a tool call and produced none), so it gets its own
    /// event type rather than sharing the runaway-reasoning one — conflating
    /// the two sent a live diagnosis down the wrong path twice (measured:
    /// the dropped turns were 286-648 completion tokens, nowhere near any
    /// configured bound, so "runaway-reasoning turn dropped" was factually
    /// wrong for this shape).
    pub fn append_empty_tool_calls_recovered(
        &mut self,
        seq: u32,
        completion_tokens: Option<u32>,
        recoveries_used: u32,
        recoveries_budget: u32,
        bound: crate::bounds::BoundRef,
    ) {
        let completion_tokens_value = completion_tokens
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null);
        self.write_event(&serde_json::json!({
            "type": "dispatch.empty_tool_calls.recovered",
            "seq": seq,
            "ts": unix_ms(),
            "completion_tokens": completion_tokens_value,
            "recoveries_used": recoveries_used,
            "recoveries_budget": recoveries_budget,
            "bound": bound,
        }));
    }

    /// (#2190) dispatch.escalation.triggered — fires once, at the exact
    /// moment a dispatch terminates via `TerminalReason::EscalationTriggered`,
    /// for ANY escalation reason. Stamps `model` and the prompt-token count
    /// AT THAT MOMENT directly onto the record, so "which model, at what
    /// context, stopped producing calls" is answerable from this one line
    /// instead of joining `telemetry.lms` (model) and `telemetry.context`
    /// (token count) by hand — same shape as #2188's model/locality stamp.
    /// `reason` is the exact same snake_case string `main.rs` emits as the
    /// JSON envelope's `result` field for this escalation (e.g.
    /// `"escalation_empty_tool_calls"`) — see [`escalation_reason_str`],
    /// the single source of truth both call sites read from, so the
    /// trajectory event and the envelope can never name the same
    /// termination two different ways.
    pub fn append_escalation_triggered(
        &mut self,
        seq: u32,
        reason: &str,
        model: &str,
        prompt_tokens: u32,
    ) {
        self.write_event(&serde_json::json!({
            "type": "dispatch.escalation.triggered",
            "seq": seq,
            "ts": unix_ms(),
            "reason": reason,
            "model": model,
            "prompt_tokens": prompt_tokens,
        }));
    }

    /// (#2094) One event per turn-delay rest the loop took — harness-owned
    /// idle time between inference turns, never a stall. `ms` is the actual
    /// sleep duration AFTER clamping (see `loop_runner.rs`'s clamp logic),
    /// so a consumer summing `ms` across every `runtime.rest` event gets
    /// exactly `Metrics.rest_ms`.
    ///
    /// (2026-08-30 fleet-observability finding) `reason` is always
    /// `"turn_delay"` — every OTHER cause of a `runtime.rest` event routes
    /// through [`Self::append_paced_rest`] instead, which carries its own
    /// operator/governor-supplied reason. Before this, a plain turn-delay
    /// rest and a manual pace pause were indistinguishable on the fleet
    /// flow stream except by cadence (this one polls once per rest, at the
    /// configured `turn_delay_ms`; a paced rest polls every 2000ms while
    /// held) — a fragile, undocumented signal for a remote reader to have
    /// to reverse-engineer.
    pub fn append_rest(&mut self, seq: u32, ms: u64) {
        self.write_event(&serde_json::json!({
            "type": "runtime.rest",
            "seq": seq,
            "ts": unix_ms(),
            "ms": ms,
            "reason": "turn_delay",
        }));
    }

    /// (#2114) Same `runtime.rest` event `append_rest` writes, but with the
    /// operator/governor-supplied `reason` (`.darkmux/pace.json`'s own
    /// `reason` field, or `PaceFile::reason_or_default`'s `"paused"`
    /// fallback) instead of the fixed `"turn_delay"` — one event per bounded
    /// sleep increment the loop takes while the pace file holds `pause:
    /// true`. Kept as a distinct method (not an `Option<&str>` param bolted
    /// onto `append_rest`) so the #2094 turn-delay call site's signature
    /// never has to change for a feature it doesn't use.
    ///
    /// (2026-08-30 fleet-observability finding) `state` is the pace file's
    /// own `state` field (`PaceFile::state`, e.g. an OS thermal-state name
    /// when the thermal governor wrote the pause) — forwarded alongside
    /// `reason` so a remote reader can distinguish "the operator paused
    /// this by hand" from "the thermal governor paused this, and here is
    /// which state tripped it" without cross-referencing pace.json, which
    /// the flow stream has no access to. `None` (a hand-written pace file,
    /// or a writer that never set `state`) serializes to JSON `null` rather
    /// than omitting the key — the reader should never have to distinguish
    /// "not asked" from "genuinely unknown."
    pub fn append_paced_rest(&mut self, seq: u32, ms: u64, reason: &str, state: Option<&str>) {
        self.write_event(&serde_json::json!({
            "type": "runtime.rest",
            "seq": seq,
            "ts": unix_ms(),
            "ms": ms,
            "reason": reason,
            "state": state,
        }));
    }

    pub fn append_cycle_suspected(
        &mut self,
        seq: u32,
        tool_name: &str,
        canonical_args: &str,
        // (#1001) Firing-time content hash of the tool's target file, for
        // staleness ranking. Omitted when the tool has no file target.
        code_hash: Option<&str>,
        count: usize,
        window_size: usize,
    ) {
        let mut event = serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "seq": seq,
            "ts": unix_ms(),
            "tool_name": tool_name,
            "canonical_args": canonical_args,
            "count": count,
            "window_size": window_size,
        });
        if let Some(h) = code_hash {
            event["code_hash"] = serde_json::Value::String(h.to_string());
        }
        self.write_event(&event);
    }

    /// dispatch.per_turn_cap.salvaged — fires when the runtime
    /// salvages tool call(s) on a `finish_reason=length` turn where
    /// `completion_tokens` hit `MAX_TOKENS_PER_CALL` but the tool
    /// call args were well-formed JSON. The truncated content is
    /// discarded; the tool call is dispatched as if `finish_reason`
    /// had been `tool_calls`. Companion to the feedback nudge so the
    /// model knows what happened. (#479)
    /// (#2165) `bound` names WHICH bound the cap that got salvaged was —
    /// the reasoning check-in interval or `max_tokens_per_call`, whichever
    /// region the salvaged turn was writing in — + its provenance. `cap`
    /// stays the numeric value already carried above; `bound.value` mirrors
    /// it so a consumer reading only `bound` still has the number.
    ///
    /// (#2169 merge-gate CONSIDER 6) `salvaged_tool_calls` counts JSON
    /// well-formedness ONLY (#479's own filter) — it does NOT mean that
    /// many calls actually reached `tools::dispatch`. A salvaged batch
    /// also passes through the SAME name-allowlist partition every other
    /// turn's calls do; a call counted here can still turn out to be
    /// invalid-name or ungranted. Reconcile against the same turn's
    /// `dispatch.tool.malformed_names` event(s) (matching `seq`):
    /// `salvaged_tool_calls` minus the sum of that turn's malformed
    /// `count` fields is what was actually dispatched.
    pub fn append_per_turn_cap_salvaged(
        &mut self,
        seq: u32,
        completion_tokens: u32,
        cap: u32,
        salvaged_tool_calls: usize,
        bound: crate::bounds::BoundRef,
    ) {
        self.write_event(&serde_json::json!({
            "type": "dispatch.per_turn_cap.salvaged",
            "seq": seq,
            "ts": unix_ms(),
            "completion_tokens": completion_tokens,
            "cap": cap,
            "salvaged_tool_calls": salvaged_tool_calls,
            "bound": bound,
        }));
    }

    /// dispatch.tool.malformed_names — fires ONCE per turn where the
    /// model's structured `tool_calls` carried one or more names that
    /// are not in the runtime's allowlist (#2169). Observed live:
    /// Devstral 2 quoting its own generated code around LM Studio's
    /// Mistral `[TOOL_CALLS]` marker, which the parser slices such
    /// that the preceding text becomes the call's `name` — 48 in one
    /// turn, each of which pre-#2169 was dispatched, failed with
    /// "tool doesn't exist", and burned a tool message. Post-#2169
    /// none of them are dispatched; this event is the ONLY trace of
    /// the whole turn's malformed batch, so `count` names how many
    /// were coalesced.
    ///
    /// `model` is the dispatch's model id, forwarded so the run record
    /// names the MODEL (not the tool layer) as the source of the
    /// pattern — the whole point of #2169 is that this reads as a
    /// model finding, not a broken-tools finding.
    ///
    /// `sample_name_prefix` is one representative offending name,
    /// already sanitized by `loop_runner::sanitize_sample_name_prefix`
    /// (≤ 40 chars, header-safe printable ASCII, no newlines) — this
    /// event rides into flow records and eventually an HTTP-header-
    /// bearing hook delivery (#2178's sanitizer covers that transport;
    /// this event carries an already-clean value into it).
    ///
    /// (merge-gate MUST FIX 1) `reason` discriminates the TWO distinct
    /// causes a bucket of these can have — `"not_a_tool"` (no darkmux tool
    /// is named this) or `"real_tool_not_granted"` (a real darkmux tool
    /// this dispatch's role wasn't granted). One event fires per
    /// reason-bucket per turn, never merged — see
    /// `loop_runner::handle_invalid_tool_calls`'s doc for why.
    pub fn append_malformed_tool_names(
        &mut self,
        seq: u32,
        count: u32,
        model: &str,
        sample_name_prefix: &str,
        reason: &str,
    ) {
        self.write_event(&serde_json::json!({
            "type": "dispatch.tool.malformed_names",
            "seq": seq,
            "ts": unix_ms(),
            "count": count,
            "model": model,
            "sample_name_prefix": sample_name_prefix,
            "reason": reason,
        }));
    }

    /// dispatch.reasoning_loop.suspected — fires when the runtime's
    /// reasoning-loop detector (#461) flags that the same normalized
    /// reasoning content has appeared `count` times in a sliding window
    /// of `window_size` recent turns. Sibling of
    /// `dispatch.cycle.suspected` — same shape applied to reasoning
    /// instead of tools. Observability + feedback-injection (no bail
    /// in the MVP).
    pub fn append_reasoning_loop_suspected(
        &mut self,
        seq: u32,
        count: usize,
        window_size: usize,
    ) {
        self.write_event(&serde_json::json!({
            "type": "dispatch.reasoning_loop.suspected",
            "seq": seq,
            "ts": unix_ms(),
            "count": count,
            "window_size": window_size,
        }));
    }

    /// dispatch.reasoning_bound.not_applied — fires ONCE per dispatch, the
    /// first time a call that carried real dispatchable output (an answer
    /// or tool calls) produces no reasoning region at all, cumulative over
    /// every turn so far. (#2164) A fresh turn's first call carries the
    /// `REASONING_CHECKPOINT_INTERVAL` bound only once the dispatch has
    /// proven, on some earlier call, that the model reasons — this event
    /// is the run record's explanation for why that stopped happening (or
    /// never started): the model this dispatch is talking to does not
    /// appear to emit a thinking region, so the reasoning check-in interval
    /// is not being applied to fresh turns' first calls; only the answer
    /// bound (`max_tokens_per_call`) is. Observability-only — nothing
    /// about the dispatch's behavior changes when this fires; it explains a
    /// decision the runtime already made.
    pub fn append_reasoning_bound_not_applied(&mut self, seq: u32) {
        self.write_event(&serde_json::json!({
            "type": "dispatch.reasoning_bound.not_applied",
            "seq": seq,
            "ts": unix_ms(),
        }));
    }

    /// dispatch.feedback.injected — fires when the runtime injects one
    /// or more synthetic system messages into the next-turn prompt as
    /// model-facing telemetry (cycle warnings, tool-failure cascades,
    /// future signal kinds). Companion to the existing per-signal
    /// trajectory events (`dispatch.cycle.suspected`,
    /// `dispatch.tool.repeated_failure`) which record that the SIGNAL
    /// fired; this event records that the message was DELIVERED to
    /// the model's prompt. Step-1 scaffold of the feedback-injection
    /// primitive — see `feedback.rs`.
    pub fn append_feedback_injected(
        &mut self,
        seq: u32,
        message_count: usize,
        signal_kinds: &[&str],
    ) {
        self.write_event(&serde_json::json!({
            "type": "dispatch.feedback.injected",
            "seq": seq,
            "ts": unix_ms(),
            "message_count": message_count,
            "signal_kinds": signal_kinds,
        }));
    }

    /// dispatch.context.stale_tokens — fires when the runtime detects the
    /// endpoint's reported `usage.prompt_tokens` has been frozen at
    /// `frozen_value` for `frozen_turns` consecutive turns while the message
    /// thread kept growing (#854). A healthy conversation strictly increases
    /// prompt_tokens turn-over-turn, so a frozen count is an endpoint misreport
    /// that can't gate compaction — the runtime substitutes the local size
    /// `estimate` for the compaction decision. Pure observability: surfaces WHY
    /// a compaction can fire (or occupancy be read) without the reported count
    /// crossing the threshold. eureka-detection.
    pub fn append_stale_context_tokens(
        &mut self,
        seq: u32,
        frozen_value: u32,
        frozen_turns: u32,
        estimate: u32,
        message_count: usize,
    ) {
        self.write_event(&serde_json::json!({
            "type": "dispatch.context.stale_tokens",
            "seq": seq,
            "ts": unix_ms(),
            "frozen_value": frozen_value,
            "frozen_turns": frozen_turns,
            "estimate": estimate,
            "message_count": message_count,
        }));
    }

    /// tool_call.promoted — recovery event when the runtime promoted
    /// plain-text tool-call markup back into structured tool_calls
    /// (#406). `source` is either `"content"` or `"reasoning"`
    /// indicating which message channel the markup was found in;
    /// `format` is one of `"bracket"`, `"harmony"`, or `"xml"`.
    /// Observability matters: every promotion is a model wire-format
    /// failure the runtime caught — operators monitoring bail rates
    /// want this rate visible alongside `dispatch.complete`.
    ///
    /// (#2230) `xml_openers_skipped_as_fenced` counts the `<tool_call>`
    /// openers the XML scan declined because they sat inside a markdown fence
    /// (quoted markup, not an emission). It counts OPENERS, not regions — one
    /// quoted block holding ten examples reports 10 — so the number bounds how
    /// much a wrong fence verdict cost.
    ///
    /// Non-zero HERE means PARTIAL suppression: this turn promoted some calls
    /// and dropped others. Two shapes produce it — a fence left unbalanced
    /// inside an earlier call's parameter value, which swallows every later
    /// call in the same emission; and the hull rule (three or more fence lines
    /// make the nesting ambiguous, so the span between the first and the last
    /// is read as quoted), which additionally declines a real call sandwiched
    /// between two separate quoted blocks. Either way a `promoted_call_count`
    /// of 1 beside a skip count of 1 is a turn that emitted two calls and ran
    /// one. Without the field that asymmetry is invisible.
    pub fn append_tool_call_promoted(
        &mut self,
        seq: u32,
        source: &str,
        format: &str,
        promoted_call_count: usize,
        xml_openers_skipped_as_fenced: usize,
    ) {
        self.write_event(&serde_json::json!({
            "type": "tool_call.promoted",
            "seq": seq,
            "ts": unix_ms(),
            "source": source,
            "format": format,
            "promoted_call_count": promoted_call_count,
            "xml_openers_skipped_as_fenced": xml_openers_skipped_as_fenced,
        }));
    }

    /// tool_call.promotion_suppressed — (#2230) the turn promoted NOTHING and
    /// the reason was the fence rule: every `<tool_call>` opener the XML scan
    /// found sat inside a markdown fence and was read as quoted markup.
    ///
    /// This is the event that exists purely so a wrong suppression is
    /// DIAGNOSABLE. `tool_call.promoted` cannot carry it — there is no
    /// promotion to hang it on — and without it a genuine call dropped as a
    /// false quotation is byte-for-byte indistinguishable in the trajectory
    /// from a model that simply emitted no call at all. Measured against this
    /// repo's own corpus (2,840 real `model.reasoning` emissions) the joint
    /// event is rare: 8.8% contain a fence, 0.035% have odd fence parity, and
    /// none would have had a call suppressed. Rare is not never, and the
    /// corpus contains ZERO emissions carrying both a fence and a tool call,
    /// so that measurement bounds the FREQUENCY of the failure and says
    /// nothing about its cost when it happens. Hence a record, not a silence.
    pub fn append_tool_call_promotion_suppressed(
        &mut self,
        seq: u32,
        xml_openers_skipped_as_fenced: usize,
    ) {
        self.write_event(&serde_json::json!({
            "type": "tool_call.promotion_suppressed",
            "seq": seq,
            "ts": unix_ms(),
            "xml_openers_skipped_as_fenced": xml_openers_skipped_as_fenced,
        }));
    }

    /// tool.completed — one per executed tool call. Records the tool name, the
    /// ARGUMENTS (capped by `MAX_TOOL_ARGS_CHARS`), and the RESULT.
    ///
    /// (#2007) The result used to be a size only, on the rationale that "a
    /// file read is large and re-derivable". That holds for `read` and not for
    /// `bash`: a test run's output cannot be re-derived later because the state
    /// it observed has moved on. The cost of that asymmetry was that a failed
    /// dispatch could not be diagnosed after the fact — the tool-failure
    /// cascade detector fired on three `bash` failures in the 3.0.0 release
    /// dogfood and the evidence for WHY was already discarded. Meanwhile
    /// `model.reasoning` persisted the model's thinking verbatim, so the
    /// trajectory kept one side of a two-sided conversation.
    ///
    /// Stored in FULL, deliberately (operator call): measured across 455 real
    /// tool calls the median result is 951 chars and the whole day's output is
    /// 1.2 MB, which is the "record exhaustively, display selectively" trade —
    /// a consumer can always truncate, and a consumer cannot un-discard.
    /// `result_chars` remains the TRUE length so that if a cap is ever
    /// introduced its truncation is visible rather than silent.
    // (#2272) Two more than the lint likes: `emitted` and `emit_seq` are one
    // event's worth of fields and folding them into a struct would only move
    // the count, not the shape. Same call as the 40-odd siblings in this crate.
    #[allow(clippy::too_many_arguments)]
    pub fn append_tool_completed(
        &mut self,
        seq: u32,
        tool_seq: u32,
        tool_name: &str,
        args: &str,
        result: &str,
        outcome: &crate::failure_rate::ToolOutcome,
        emitted: Option<&serde_json::Value>,
        emit_seq: Option<usize>,
    ) {
        // `ok` discriminates success from failure (#469). Additive,
        // backward-compatible field: consumers predating it treat a
        // missing `ok` as success. The host-side watchdog reads it so a
        // model fast-failing with varying tool calls can't keep the
        // inactivity deadline alive with a stream of failed calls.
        let args_chars = args.chars().count();
        // The TRUE length, computed from the result itself — never passed in.
        // A caller-supplied length can drift from the text beside it, and this
        // field's whole job (#2007) is to make a future cap's truncation
        // visible rather than silent.
        let result_chars = result.chars().count();
        self.write_event(&serde_json::json!({
            "type": "tool.completed",
            "seq": seq,
            "tool_seq": tool_seq,
            "tool_name": tool_name,
            // The actual arguments, char-boundary-safe truncation to keep a
            // pathological write/edit payload from bloating the trajectory
            // (search/read/exec args are tiny; only file-content args hit this).
            // A viewer PREVIEW, and only that: cut at MAX_TOOL_ARGS_CHARS.
            "args": cap_chars(args, MAX_TOOL_ARGS_CHARS),
            "args_chars": args_chars,
            // (#2272) An accepted `report_finding`'s emission — the model's
            // arguments verbatim, an opaque value darkmux never interprets —
            // and its 1-based ordinal in this dispatch. `null` for every
            // other tool and every rejected report. The crawl's product
            // never rides the preview above.
            "emitted": emitted,
            "emit_seq": emit_seq,
            "result_chars": result_chars,
            "result": result,
            // (#2008) The three-way outcome beside the boolean. `ok` answers
            // "did the tool work" (true for a red test); `outcome`
            // distinguishes a clean run from one that reported non-zero from
            // one that never ran, which are three different things three
            // different consumers need to tell apart.
            "outcome": outcome.as_str(),
            // Flat additive keys rather than a nested tagged enum: every
            // reader of this file is lenient-on-read, and a flat key is the
            // shape they already tolerate. `exit_code` is what lets a viewer
            // render "exit 1" instead of a bare cross.
            "exit_code": match outcome {
                crate::failure_rate::ToolOutcome::Reported { exit_code } => {
                    serde_json::json!(exit_code)
                }
                _ => serde_json::Value::Null,
            },
            "failure_reason": match outcome {
                crate::failure_rate::ToolOutcome::Failed { reason } => serde_json::json!(reason),
                _ => serde_json::Value::Null,
            },
            "ok": outcome.tool_worked(),
            "ts": unix_ms(),
        }));
    }

    /// dispatch.context — per-turn context-window occupancy. `used` is the EXACT
    /// prompt-token count from the LMStudio API response (usage.prompt_tokens);
    /// `max` is the configured context window (n_ctx), None when unconfigured.
    /// The #557 Slice-3 sawtooth: occupancy climbs each turn, drops at compaction.
    pub fn append_context_window(&mut self, seq: u32, used: u32, max: Option<u32>) {
        self.write_event(&serde_json::json!({
            "type": "dispatch.context",
            "seq": seq,
            "ts": unix_ms(),
            "used": used,
            "max": max,   // serde_json renders None as null; the viewer treats null max as "unknown window"
        }));
    }

    /// compaction — fires when middle-replace compaction runs. Records
    /// the size delta so the operator can verify compaction is actually
    /// shrinking the conversation.
    ///
    /// `tokens_before` / `tokens_after` (#557 Slice-3) carry the token
    /// occupancy across the compaction drop so the sawtooth's fall is
    /// quantified in tokens (not just message counts). `tokens_before`
    /// is the EXACT prompt-token count that triggered the compaction
    /// (`usage.prompt_tokens` from the prior turn); `tokens_after` is a
    /// chars/4 ESTIMATE of the compacted buffer (the runtime has no
    /// tokenizer) — the EXACT post-compaction count lands on the next
    /// turn's `dispatch.context` `used`.
    pub fn append_compaction(
        &mut self,
        generation: u32,
        before_message_count: usize,
        after_message_count: usize,
        summary_chars: usize,
        tokens_before: u32,
        tokens_after: u32,
    ) {
        self.write_event(&serde_json::json!({
            "type": "compaction",
            "generation": generation,
            "ts": unix_ms(),
            "before_messages": before_message_count,
            "after_messages": after_message_count,
            "summary_chars": summary_chars,
            "tokens_before": tokens_before,
            "tokens_after": tokens_after,
        }));
    }

    /// dispatch.complete — last event in the trajectory. Records the
    /// terminal outcome + wall time.
    pub fn append_dispatch_complete(&mut self, result: &str, wall_ms: u128) {
        self.write_event(&serde_json::json!({
            "type": "dispatch.complete",
            "ts": unix_ms(),
            "result": result,
            "wall_ms": wall_ms,
        }));
    }

    /// model.streaming.start — fires when an SSE-streamed turn begins,
    /// before any partial chunks arrive. (#205, #361)
    ///
    /// `system_chars` is the total character length of system-role
    /// messages in the request; `prompt_chars` is the total length of
    /// all non-system messages (the accumulated conversation context).
    /// Together they give per-turn context-size telemetry from the
    /// trajectory alone, independent of whether LMStudio's `usage`
    /// field came through on the SSE close. Surfaced as a Phase B
    /// dogfood finding (#361) — pre-fix the fields existed in the
    /// schema but the producer never populated them.
    pub fn append_model_streaming_start(
        &mut self,
        seq: u32,
        system_chars: usize,
        prompt_chars: usize,
    ) {
        self.write_event(&serde_json::json!({
            "type": "model.streaming.start",
            "seq": seq,
            "ts": unix_ms(),
            "system_chars": system_chars,
            "prompt_chars": prompt_chars,
        }));
    }

    /// model.partial — fires per SSE chunk during a streamed turn.
    /// Carries STATS ONLY, never the chunk content itself: a streaming
    /// 10K-token response would otherwise blow up `trajectory.jsonl` by
    /// orders of magnitude. Operators tailing the file get a steady
    /// line cadence (= dispatch is alive) plus a running byte count
    /// (= roughly how much has been produced so far). (#205)
    pub fn append_model_partial(
        &mut self,
        seq: u32,
        partial_index: u32,
        delta_chars: usize,
        cumulative_chars: usize,
        tool_calls_present: bool,
    ) {
        self.write_event(&serde_json::json!({
            "type": "model.partial",
            "seq": seq,
            "partial_index": partial_index,
            "delta_chars": delta_chars,
            "cumulative_chars": cumulative_chars,
            "tool_calls_present": tool_calls_present,
            "ts": unix_ms(),
        }));
    }

    /// model.streaming.end — fires when the SSE stream terminates
    /// (either via `data: [DONE]` or EOF), before the `model.completed`
    /// summary event for the same turn. Records totals collected during
    /// the stream so the operator sees a one-line summary in
    /// `trajectory.jsonl` even without parsing all the partials. (#205)
    pub fn append_model_streaming_end(
        &mut self,
        seq: u32,
        partial_count: u32,
        total_content_chars: usize,
        tool_calls_count: usize,
    ) {
        self.write_event(&serde_json::json!({
            "type": "model.streaming.end",
            "seq": seq,
            "partial_count": partial_count,
            "total_content_chars": total_content_chars,
            "tool_calls_count": tool_calls_count,
            "ts": unix_ms(),
        }));
    }

    /// Save the metrics.json summary. Called once at dispatch exit.
    pub fn save_metrics(&mut self, metrics: &Metrics) -> Result<()> {
        let Some(path) = self.metrics_path.as_ref() else {
            return Ok(());
        };
        let json = serde_json::to_string_pretty(metrics)?;
        fs::write(path, json)?;
        eprintln!("darkmux-runtime: metrics → {}", path.display());
        Ok(())
    }

    /// Wall time since the trajectory was opened. Useful for the
    /// dispatch.complete event.
    pub fn elapsed_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }

    /// Internal: append one event to the JSONL file. Silently drops
    /// the write if the file isn't open (recorder degraded to no-op).
    /// Append errors are emitted to stderr but don't propagate up —
    /// the dispatch shouldn't fail because of an instrumentation
    /// problem.
    fn write_event(&mut self, event: &serde_json::Value) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let mut line = serde_json::to_string(event).unwrap_or_default();
        line.push('\n');
        if let Err(e) = file.write_all(line.as_bytes()) {
            eprintln!("darkmux-runtime: trajectory write failed: {e}");
        }
    }
}

fn try_open(dir: &Path, path: &Path) -> std::io::Result<File> {
    fs::create_dir_all(dir)?;
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
}

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure_rate::ToolOutcome;

    #[test]
    fn open_creates_dot_dir_and_file() {
        let ws = tempfile::Builder::new().prefix("traj-test").tempdir().unwrap();
        let _t = Trajectory::open(ws.path());
        let traj_file = ws
            .path()
            .join(TRAJECTORY_SUBDIR)
            .join(TRAJECTORY_FILE);
        assert!(traj_file.exists(), "trajectory file should be created");
    }

    // (#2268) `dispatch.start` records the ADVERTISED tool names, so the
    // "the model never saw report_finding" class is a grep of the artifact.
    #[test]
    fn dispatch_start_records_the_advertised_tools() {
        let ws = tempfile::Builder::new().prefix("traj-test-tools").tempdir().unwrap();
        let mut t = Trajectory::open(ws.path());
        t.append_dispatch_start("m", 1, 1, &["search", "read", "bash", "report_finding"]);
        drop(t);
        let traj_file = ws.path().join(TRAJECTORY_SUBDIR).join(TRAJECTORY_FILE);
        let body = fs::read_to_string(&traj_file).unwrap();
        let first: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(first["type"], "dispatch.start");
        assert_eq!(first["tools"], serde_json::json!(["search", "read", "bash", "report_finding"]));
    }

    #[test]
    fn append_events_writes_jsonl() {
        let ws = tempfile::Builder::new().prefix("traj-test-2").tempdir().unwrap();
        let mut t = Trajectory::open(ws.path());
        t.append_dispatch_start("test-model", 100, 50, &["read", "search"]);
        t.append_model_completed(1, "stop", None, None);
        drop(t);

        let traj_file = ws
            .path()
            .join(TRAJECTORY_SUBDIR)
            .join(TRAJECTORY_FILE);
        let body = fs::read_to_string(&traj_file).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        // Each line should parse as JSON
        for line in &lines {
            let parsed: serde_json::Value =
                serde_json::from_str(line).expect("each line is valid JSON");
            assert!(parsed["type"].is_string());
            assert!(parsed["ts"].is_number());
        }
    }

    #[test]
    fn append_rest_writes_runtime_rest_event_with_ms() {
        // (#2094) One event per turn-delay rest; `ms` is the actual
        // (post-clamp) sleep duration.
        let ws = tempfile::Builder::new().prefix("traj-rest").tempdir().unwrap();
        let mut t = Trajectory::open(ws.path());
        t.append_rest(1, 500);
        t.append_rest(2, 500);
        drop(t);

        let traj_file = ws.path().join(TRAJECTORY_SUBDIR).join(TRAJECTORY_FILE);
        let body = fs::read_to_string(&traj_file).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "one runtime.rest event per rest");
        for line in &lines {
            let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(parsed["type"], "runtime.rest");
            assert_eq!(parsed["ms"], 500);
            // (2026-08-30 fleet-observability finding) A plain turn-delay
            // rest must name itself as such — before this, it carried no
            // `reason` at all, indistinguishable on the flow stream from a
            // paced rest except by cadence.
            assert_eq!(parsed["reason"], "turn_delay");
        }
    }

    /// (2026-08-30 fleet-observability finding) `append_paced_rest` forwards
    /// BOTH `reason` and `state` — the pace file's own governor-supplied
    /// fields — verbatim onto the event, and `state` serializes to JSON
    /// `null` (not an absent key) when the pace file never set one.
    #[test]
    fn append_paced_rest_forwards_reason_and_state() {
        let ws = tempfile::Builder::new().prefix("traj-paced-rest").tempdir().unwrap();
        let mut t = Trajectory::open(ws.path());
        t.append_paced_rest(1, 2000, "thermal", Some("critical"));
        t.append_paced_rest(2, 2000, "paused", None);
        drop(t);

        let traj_file = ws.path().join(TRAJECTORY_SUBDIR).join(TRAJECTORY_FILE);
        let body = fs::read_to_string(&traj_file).unwrap();
        let lines: Vec<serde_json::Value> =
            body.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["reason"], "thermal");
        assert_eq!(lines[0]["state"], "critical");
        assert_eq!(lines[1]["reason"], "paused");
        assert_eq!(lines[1]["state"], serde_json::Value::Null, "no state -> JSON null, not an absent key");
    }

    #[test]
    fn tool_completed_persists_the_result_not_just_its_length() {
        // (#2007) The result used to be recorded as a SIZE only. That made a
        // failed dispatch undiagnosable after the fact: the tool-failure
        // cascade detector would fire on three `bash` failures and the
        // evidence for WHY was already gone.
        //
        // The original rationale — "a file read is large and re-derivable" —
        // holds for `read` and not for `bash`: a test run's output cannot be
        // re-derived later, because the state it observed has moved on.
        let ws = tempfile::Builder::new().prefix("traj-test-result").tempdir().unwrap();
        let mut t = Trajectory::open(ws.path());
        let failure = "exit: 1\n--- stdout ---\nTests: 2 failed, 86 passed\n--- stderr ---\n";
        t.append_tool_completed(1, 0, "bash", "{\"command\":\"npm test\"}", failure, &ToolOutcome::Reported { exit_code: 1 }, None, None);
        drop(t);

        let body = fs::read_to_string(
            ws.path().join(TRAJECTORY_SUBDIR).join(TRAJECTORY_FILE),
        )
        .unwrap();
        let line: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();

        assert_eq!(
            line["result"], serde_json::json!(failure),
            "the result text must be persisted verbatim, not summarized"
        );
        assert_eq!(
            line["result_chars"],
            serde_json::json!(failure.chars().count()),
            "result_chars stays the TRUE length, so a future cap is visible rather than silent"
        );
        assert!(
            line["result"].as_str().unwrap().contains("2 failed"),
            "the diagnosis has to survive: {}", line["result"]
        );
    }

    #[test]
    fn tool_completed_carries_the_emission_whole_while_args_stays_a_preview() {
        // (#2272) `args` is a 512-char VIEWER PREVIEW and always was. A
        // `report_finding` call's arguments ARE the crawl's product, and
        // nine of nine findings on 2026-09-02 were lost because the only
        // wire copy was that preview, cut mid-JSON. The accepted emission
        // now rides the event verbatim as `emitted`, complete, beside the
        // preview it never should have depended on.
        let ws = tempfile::Builder::new().prefix("traj-test-finding").tempdir().unwrap();
        let mut t = Trajectory::open(ws.path());
        let why = "w".repeat(2_000);
        let raw_args = serde_json::json!({
            "file": "/workspace/x/src/a.ts", "line": 82, "pattern": "unnamed-predicate",
            "evidence": "  enabled: !a && b !== null && c,", "why": why,
        })
        .to_string();
        let emitted: serde_json::Value = serde_json::from_str(&raw_args).unwrap();
        t.append_tool_completed(
            2, 0, "report_finding", &raw_args,
            "Recorded. 1 finding(s) so far, 39 remaining in this run's budget.",
            &ToolOutcome::Ok, Some(&emitted), Some(1),
        );
        t.append_tool_completed(
            2, 1, "read", "{\"path\":\"/workspace/x/src/a.ts\"}", "line one\n",
            &ToolOutcome::Ok, None, None,
        );
        drop(t);

        let body = fs::read_to_string(ws.path().join(TRAJECTORY_SUBDIR).join(TRAJECTORY_FILE)).unwrap();
        let mut lines = body.lines().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap());
        let reported = lines.next().unwrap();
        let read = lines.next().unwrap();

        assert_eq!(reported["emitted"], emitted, "the emission rides the event whole, verbatim");
        assert_eq!(reported["emitted"]["why"].as_str().unwrap().len(), 2_000);
        assert_eq!(reported["emit_seq"], serde_json::json!(1));
        assert!(
            reported["args"].as_str().unwrap().chars().count() <= MAX_TOOL_ARGS_CHARS + 1,
            "args stays the capped preview it always was"
        );
        assert_eq!(reported["args_chars"], serde_json::json!(raw_args.chars().count()));
        let read = read.as_object().unwrap();
        assert!(
            read.contains_key("emitted") && read["emitted"].is_null()
                && read.contains_key("emit_seq") && read["emit_seq"].is_null(),
            "every other tool emits nothing — as an EXPLICIT null, key present: the host \
             reads a missing key as \"this runtime predates the field\": {read:?}"
        );
    }

    #[test]
    fn tool_completed_emits_ok_discriminator() {
        // (#469) tool.completed carries `ok` so the host watchdog can
        // distinguish a successful tool call (proof-of-work, resets the
        // deadline) from a failed one (does not).
        let ws = tempfile::Builder::new().prefix("traj-test-ok").tempdir().unwrap();
        let mut t = Trajectory::open(ws.path());
        t.append_tool_completed(1, 0, "bash", "{\"command\":\"ls\"}", "a.txt\nb.txt\n", &ToolOutcome::Ok, None, None);
        t.append_tool_completed(2, 1, "bash", "{\"command\":\"cat x\"}", "exit: 1\n--- stderr ---\nno such file\n", &ToolOutcome::Failed { reason: "no such file".into() }, None, None);
        drop(t);

        let body = fs::read_to_string(
            ws.path().join(TRAJECTORY_SUBDIR).join(TRAJECTORY_FILE),
        )
        .unwrap();
        let lines: Vec<serde_json::Value> = body
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["type"], "tool.completed");
        assert_eq!(lines[0]["ok"], serde_json::json!(true));
        assert_eq!(lines[1]["ok"], serde_json::json!(false));
        // (args capture) the actual arguments are recorded so the operator can
        // recall WHAT the tool did, plus the char count.
        assert_eq!(lines[0]["args"], "{\"command\":\"ls\"}");
        assert_eq!(lines[0]["args_chars"], 16);
    }

    #[test]
    fn tool_completed_caps_oversized_args() {
        // A pathological write/edit file-content arg is truncated to the cap +
        // an ellipsis marker; search/read/exec args are far under the cap and
        // pass through whole.
        let ws = tempfile::Builder::new().prefix("traj-test-cap").tempdir().unwrap();
        let mut t = Trajectory::open(ws.path());
        let big = "x".repeat(MAX_TOOL_ARGS_CHARS + 200);
        t.append_tool_completed(1, 0, "write", &big, "", &ToolOutcome::Ok, None, None);
        drop(t);
        let line: serde_json::Value = serde_json::from_str(
            fs::read_to_string(ws.path().join(TRAJECTORY_SUBDIR).join(TRAJECTORY_FILE))
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        let recorded = line["args"].as_str().unwrap();
        assert_eq!(recorded.chars().count(), MAX_TOOL_ARGS_CHARS + 1); // cap + '…'
        assert!(recorded.ends_with('…'));
        // args_chars reflects the TRUE length, not the truncated one.
        assert_eq!(line["args_chars"], (MAX_TOOL_ARGS_CHARS + 200) as u64);
    }

    #[test]
    fn append_context_window_writes_used_and_max() {
        // (#557 Slice-3) The per-turn sawtooth event carries the EXACT
        // prompt-token count (`used`) + the configured n_ctx (`max`).
        let ws = tempfile::Builder::new().prefix("traj-test-ctx").tempdir().unwrap();
        let mut t = Trajectory::open(ws.path());
        t.append_context_window(3, 42000, Some(101000));
        drop(t);

        let body = fs::read_to_string(
            ws.path().join(TRAJECTORY_SUBDIR).join(TRAJECTORY_FILE),
        )
        .unwrap();
        let line: serde_json::Value =
            serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(line["type"], "dispatch.context");
        assert_eq!(line["seq"], 3);
        assert_eq!(line["used"], 42000);
        assert_eq!(line["max"], 101000);
        assert!(line["ts"].is_number());
    }

    #[test]
    fn append_context_window_renders_none_max_as_json_null() {
        // (#557 Slice-3) An unconfigured context window (None) must
        // render as JSON null — the viewer treats null max as "unknown
        // window", distinct from a real numeric cap.
        let ws = tempfile::Builder::new().prefix("traj-test-ctx-null").tempdir().unwrap();
        let mut t = Trajectory::open(ws.path());
        t.append_context_window(1, 5000, None);
        drop(t);

        let body = fs::read_to_string(
            ws.path().join(TRAJECTORY_SUBDIR).join(TRAJECTORY_FILE),
        )
        .unwrap();
        let line: serde_json::Value =
            serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(line["used"], 5000);
        assert!(line["max"].is_null(), "None max must serialize as JSON null");
    }

    /// (#2094 finding 8) The runtime writes `turn_delay_effective_ms` into
    /// metrics.json under exactly that key — the host-side
    /// `read_turn_delay_effective_ms` (`dispatch_internal.rs`) reads it
    /// back by this literal name, so a rename here silently breaks that
    /// reader without either side's own compiler catching it.
    #[test]
    fn save_metrics_writes_turn_delay_effective_ms_under_its_own_key() {
        let ws = tempfile::Builder::new().prefix("traj-metrics-tdem").tempdir().unwrap();
        let mut t = Trajectory::open(ws.path());
        let m = Metrics {
            runtime: "darkmux-runtime",
            version: "0.1.0",
            model: "test".into(),
            started_at_unix_ms: 0,
            wall_ms: 3000,
            result: "stop".into(),
            turns: 3,
            compactions: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_messages: 0,
            max_turns_reached: false,
            rest_ms: 1000,
            rests: 2,
            turn_delay_effective_ms: 500,
            final_assistant_preview: "".into(),
        };
        t.save_metrics(&m).unwrap();
        drop(t);

        let body = fs::read_to_string(ws.path().join(TRAJECTORY_SUBDIR).join(METRICS_FILE))
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["turn_delay_effective_ms"], 500);
        assert_eq!(parsed["rest_ms"], 1000);
        assert_eq!(parsed["rests"], 2);
    }

    #[test]
    fn open_no_op_when_dir_unwritable() {
        // Point at a path under root that we can't create (assuming
        // tests don't run as root). Recorder should degrade silently.
        let bad = Path::new("/proc/cannot-create-this/please");
        let mut t = Trajectory::open(bad);
        // This shouldn't panic or fail:
        t.append_dispatch_start("model", 0, 0, &[]);
        // metrics save should also be a no-op:
        let m = Metrics {
            runtime: "darkmux-runtime",
            version: "0.1.0",
            model: "test".into(),
            started_at_unix_ms: 0,
            wall_ms: 0,
            result: "stop".into(),
            turns: 0,
            compactions: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_messages: 0,
            max_turns_reached: false,
            rest_ms: 0,
            rests: 0,
            turn_delay_effective_ms: 0,
            final_assistant_preview: "".into(),
        };
        t.save_metrics(&m).unwrap();
    }
}
