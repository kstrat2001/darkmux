//! Trajectory + metrics recording.
//!
//! Gives post-dispatch visibility. Writes a line-per-event JSONL trace
//! to `/workspace/.darkmux-runtime/trajectory.jsonl` plus a top-line
//! `metrics.json` at exit. Operators inspect these after the container
//! is gone (the `--rm` mode otherwise loses everything except stderr).
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
    /// First 400 chars of the final assistant message (for at-a-glance
    /// "what did the agent end up saying"). Truncated to keep
    /// metrics.json human-readable in a terminal.
    pub final_assistant_preview: String,
}

impl Trajectory {
    /// Open a trajectory file at `<workspace>/.darkmux-runtime/`. If
    /// the directory can't be created (permission, missing workspace,
    /// etc.) returns a degraded no-op recorder rather than failing.
    pub fn open(workspace: &Path) -> Self {
        let dir = workspace.join(TRAJECTORY_SUBDIR);
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
    pub fn append_dispatch_start(
        &mut self,
        model: &str,
        system_chars: usize,
        prompt_chars: usize,
    ) {
        self.write_event(&serde_json::json!({
            "type": "dispatch.start",
            "ts": unix_ms(),
            "model": model,
            "system_chars": system_chars,
            "prompt_chars": prompt_chars,
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
    /// one tool's consecutive-failure counter crosses the threshold
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
        consecutive_failures: u32,
    ) {
        self.write_event(&serde_json::json!({
            "type": "dispatch.tool.repeated_failure",
            "seq": seq,
            "ts": unix_ms(),
            "tool_name": tool_name,
            "consecutive_failures": consecutive_failures,
        }));
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
    pub fn append_intra_turn_stall_recovered(
        &mut self,
        seq: u32,
        completion_tokens: Option<u32>,
        recoveries_used: u32,
        recoveries_budget: u32,
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
        }));
    }

    pub fn append_cycle_suspected(
        &mut self,
        seq: u32,
        tool_name: &str,
        canonical_args: &str,
        count: usize,
        window_size: usize,
    ) {
        self.write_event(&serde_json::json!({
            "type": "dispatch.cycle.suspected",
            "seq": seq,
            "ts": unix_ms(),
            "tool_name": tool_name,
            "canonical_args": canonical_args,
            "count": count,
            "window_size": window_size,
        }));
    }

    /// dispatch.per_turn_cap.salvaged — fires when the runtime
    /// salvages tool call(s) on a `finish_reason=length` turn where
    /// `completion_tokens` hit `MAX_TOKENS_PER_CALL` but the tool
    /// call args were well-formed JSON. The truncated content is
    /// discarded; the tool call is dispatched as if `finish_reason`
    /// had been `tool_calls`. Companion to the feedback nudge so the
    /// model knows what happened. (#479)
    pub fn append_per_turn_cap_salvaged(
        &mut self,
        seq: u32,
        completion_tokens: u32,
        cap: u32,
        salvaged_tool_calls: usize,
    ) {
        self.write_event(&serde_json::json!({
            "type": "dispatch.per_turn_cap.salvaged",
            "seq": seq,
            "ts": unix_ms(),
            "completion_tokens": completion_tokens,
            "cap": cap,
            "salvaged_tool_calls": salvaged_tool_calls,
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

    /// tool_call.promoted — recovery event when the runtime promoted
    /// plain-text tool-call markup back into structured tool_calls
    /// (#406). `source` is either `"content"` or `"reasoning"`
    /// indicating which message channel the markup was found in;
    /// `format` is one of `"bracket"`, `"harmony"`, or `"xml"`.
    /// Observability matters: every promotion is a model wire-format
    /// failure the runtime caught — operators monitoring bail rates
    /// want this rate visible alongside `dispatch.complete`.
    pub fn append_tool_call_promoted(
        &mut self,
        seq: u32,
        source: &str,
        format: &str,
        promoted_call_count: usize,
    ) {
        self.write_event(&serde_json::json!({
            "type": "tool_call.promoted",
            "seq": seq,
            "ts": unix_ms(),
            "source": source,
            "format": format,
            "promoted_call_count": promoted_call_count,
        }));
    }

    /// tool.completed — one per executed tool call. Records the
    /// tool name + arg/result sizes (not the content — that can be
    /// large; the trajectory is for shape, not for full payload).
    pub fn append_tool_completed(
        &mut self,
        seq: u32,
        tool_seq: u32,
        tool_name: &str,
        args_chars: usize,
        result_chars: usize,
        ok: bool,
    ) {
        // `ok` discriminates success from failure (#469). Additive,
        // backward-compatible field: consumers predating it treat a
        // missing `ok` as success. The host-side watchdog reads it so a
        // model fast-failing with varying tool calls can't keep the
        // inactivity deadline alive with a stream of failed calls.
        self.write_event(&serde_json::json!({
            "type": "tool.completed",
            "seq": seq,
            "tool_seq": tool_seq,
            "tool_name": tool_name,
            "args_chars": args_chars,
            "result_chars": result_chars,
            "ok": ok,
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

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempdir::TempDir;

    #[test]
    fn open_creates_dot_dir_and_file() {
        let ws = TempDir::new("traj-test").unwrap();
        let _t = Trajectory::open(ws.path());
        let traj_file = ws
            .path()
            .join(TRAJECTORY_SUBDIR)
            .join(TRAJECTORY_FILE);
        assert!(traj_file.exists(), "trajectory file should be created");
    }

    #[test]
    fn append_events_writes_jsonl() {
        let ws = TempDir::new("traj-test-2").unwrap();
        let mut t = Trajectory::open(ws.path());
        t.append_dispatch_start("test-model", 100, 50);
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
    fn tool_completed_emits_ok_discriminator() {
        // (#469) tool.completed carries `ok` so the host watchdog can
        // distinguish a successful tool call (proof-of-work, resets the
        // deadline) from a failed one (does not).
        let ws = TempDir::new("traj-test-ok").unwrap();
        let mut t = Trajectory::open(ws.path());
        t.append_tool_completed(1, 0, "bash", 40, 1000, true);
        t.append_tool_completed(2, 1, "bash", 40, 80, false);
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
    }

    #[test]
    fn append_context_window_writes_used_and_max() {
        // (#557 Slice-3) The per-turn sawtooth event carries the EXACT
        // prompt-token count (`used`) + the configured n_ctx (`max`).
        let ws = TempDir::new("traj-test-ctx").unwrap();
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
        let ws = TempDir::new("traj-test-ctx-null").unwrap();
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

    #[test]
    fn open_no_op_when_dir_unwritable() {
        // Point at a path under root that we can't create (assuming
        // tests don't run as root). Recorder should degrade silently.
        let bad = Path::new("/proc/cannot-create-this/please");
        let mut t = Trajectory::open(bad);
        // This shouldn't panic or fail:
        t.append_dispatch_start("model", 0, 0);
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
            final_assistant_preview: "".into(),
        };
        t.save_metrics(&m).unwrap();
    }
}
