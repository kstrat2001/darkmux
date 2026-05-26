//! The tool-call loop.
//!
//! Sends the conversation to LMStudio. If the model returns a
//! `tool_calls` finish_reason, dispatches each tool, appends results,
//! checks whether the context budget needs compaction, and re-sends.
//! Loops until `stop`, or fails loudly on `length` / unexpected outcomes.
//!
//! Phase 6 added compaction via `crate::compaction`: token-count-aware
//! middle-replace strategy that summarizes via a companion model.
//!
//! Phase 7 (#205) added SSE streaming for the main turn chat() call:
//! delta chunks accumulate into the same `ChatResponse` shape the loop
//! used to receive from non-streaming, and per-chunk `model.partial`
//! events land in the trajectory so a second observer can `tail -F`
//! and see the dispatch making progress mid-turn. The companion
//! compactor model (`compaction::compact`) stays non-streaming — it's a
//! short fire-and-forget summarization call where mid-turn observability
//! doesn't matter.
//!
//! Still omitted (Phase 8+ if measurements show they're needed):
//!
//! - No retries on transient failures. A network blip aborts the loop.
//! - No per-profile threshold derivation (compaction threshold is env-
//!   tunable but global, not derived from active darkmux profile).

use std::collections::HashSet;

use anyhow::{anyhow, Result};

use crate::compaction;
use crate::lmstudio::{ChatRequest, ChunkAccumulator, LmStudioClient, Message};
use crate::plain_text_tool_calls::promote_plain_text_tool_calls;
use crate::tools::{dispatch, Tool};
use crate::trajectory::Trajectory;

/// Hard cap on tool-call turns inside a single dispatch. If the model
/// hits this without returning `stop`, the loop aborts with a loud
/// error — better than spinning forever. 100 was picked after the
/// Phase 6d e2e found 16 was too tight for long-agentic-shape work
/// (real coding workloads with ~3 files of test edits need 30-50+
/// granular tool calls with the current tool palette).
const MAX_TURNS: u32 = 100;

/// How the loop terminated. Distinguishes "model said stop" from
/// "loop hit the safety cap and gave up" — semantically different
/// outcomes for downstream consumers (a max_turns hit means the
/// reply is partial/wedged and a re-dispatch with a fresh session
/// might be the right move; a stop means use the reply).
///
/// Pre-fix the MAX_TURNS path was an `Err(...)` indistinguishable
/// from infrastructure failures (Docker died, LMStudio went away).
/// Operators reading the JSON envelope's `result` field saw `error`
/// for both cases; structured terminal reason lets the runtime emit
/// `result: "max_turns"` instead. (#325)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalReason {
    /// Model returned finish_reason=stop.
    Stop,
    /// Loop hit MAX_TURNS without reaching a stop. Reply is whatever
    /// the last assistant message produced — likely partial.
    MaxTurns,
    /// (#377) Operator-set bound was hit and the dispatch escalated
    /// out of local-tier rather than continuing. The bound + the
    /// specific condition that fired live in [`EscalationReason`].
    /// Salvageable state (final messages, partial work, completed
    /// turns) is in the rest of [`LoopOutcome`] so the frontier-tier
    /// handoff skill can pick up where local-tier left off. KISS-
    /// doubled (Beat 44 closure): bound the cost, don't optimize it.
    EscalationTriggered(EscalationReason),
}

/// (#377) Which operator-set bound was crossed when an
/// [`TerminalReason::EscalationTriggered`] terminal fires. Designed
/// as an enum (not a single variant on TerminalReason) so future
/// escalation conditions — token-budget exhaustion, hang-timeout,
/// role-explicit bail — can join under the same terminal without
/// fragmenting the JSON envelope's `result` field. v0.1 ships
/// `CompactionLimitReached` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationReason {
    /// Compaction count reached the operator-configured
    /// `bail_after_compactions` threshold (typed field
    /// `profile.runtime.compaction.reserve.bail_after_compactions`,
    /// schema landed in #357, consumer in #377).
    CompactionLimitReached,
}

/// Outcome of a completed loop run.
#[derive(Debug)]
pub struct LoopOutcome {
    /// Why the loop terminated. See [`TerminalReason`].
    pub terminal_reason: TerminalReason,
    /// Full conversation, in order, including system / user / assistant
    /// / tool messages. The final assistant message has the model's
    /// terminal response.
    pub messages: Vec<Message>,

    /// Number of model turns the loop took (each chat-completion call).
    pub turns: u32,

    /// Total prompt tokens summed across all calls. Used for cumulative
    /// cost reporting; per-call usage drives compaction triggering.
    pub total_prompt_tokens: u32,

    /// Total completion tokens summed across all calls.
    pub total_completion_tokens: u32,

    /// Number of compaction events that fired during the loop.
    /// Phase 6: middle-replace via the companion compactor model.
    pub compactions: u32,
}

/// Run the tool-call loop to completion.
///
/// `trajectory` records each significant event (model.completed,
/// tool.completed, compaction). When the recorder was opened against
/// an unwritable path, its methods are no-ops — the loop runs the same
/// either way.
///
/// `streaming` switches the per-turn chat call between SSE-streamed
/// (default; emits model.partial trajectory events as chunks arrive)
/// and single-shot non-streaming (opt-out for tests/benchmarks where
/// determinism or simpler trajectory size matters). The accumulated
/// final response is identical either way; the rest of the loop
/// (tool dispatch, compaction triggering, finish_reason handling)
/// doesn't change.
pub fn run(
    client: &LmStudioClient,
    model: &str,
    initial_messages: Vec<Message>,
    tools: &[Tool],
    trajectory: &mut Trajectory,
    streaming: bool,
    compaction_cfg: &compaction::CompactionConfig,
) -> Result<LoopOutcome> {
    let mut messages = initial_messages;
    let tool_defs: Vec<_> = tools.iter().map(|t| t.to_tool_def()).collect();
    // Set of tool names the model is allowed to call. Drives the
    // plain-text-tool-call promoter (#406): any tool name in the
    // promoted markup that isn't here is rejected so adversarial /
    // malformed output can't smuggle arbitrary tool names into the
    // dispatch pipeline.
    let allowed_tool_names: HashSet<String> = tools.iter().map(|t| t.name().to_string()).collect();

    let mut turns: u32 = 0;
    let mut total_prompt_tokens: u32 = 0;
    let mut total_completion_tokens: u32 = 0;
    let mut compactions: u32 = 0;
    let mut latest_prompt_tokens: u32 = 0;

    loop {
        if turns >= MAX_TURNS {
            // #325: structured terminal reason instead of an opaque
            // Err. The JSON envelope's `result` field can now be
            // "max_turns" (the runtime tried; the model didn't stop)
            // rather than "error" (indistinguishable from Docker /
            // LMStudio failures). Reply is whatever the last
            // assistant message produced — likely partial.
            eprintln!(
                "darkmux-runtime: loop hit MAX_TURNS={MAX_TURNS} without reaching stop; \
                 returning partial outcome"
            );
            return Ok(LoopOutcome {
                terminal_reason: TerminalReason::MaxTurns,
                messages,
                turns,
                total_prompt_tokens,
                total_completion_tokens,
                compactions,
            });
        }

        let request = ChatRequest {
            model: model.to_string(),
            messages: messages.clone(),
            tools: tool_defs.clone(),
            tool_choice: Some("auto".into()),
            temperature: 0.2,
            max_tokens: None,
            response_format: None,
        };

        let next_seq = turns + 1;
        let mut response = if streaming {
            run_streaming_turn(client, &request, next_seq, trajectory)?
        } else {
            client.chat(&request)?
        };
        turns += 1;

        // (#406) Recover plain-text tool calls the model emitted in
        // `content` or `reasoning_content` instead of the structured
        // `tool_calls` field. Three formats recognized: bracket /
        // harmony (mirrors openclaw's `promoteLmstudioPlainTextToolCalls`)
        // and XML (the Qwen 3.x thinking-mode case openclaw doesn't
        // handle today). When promotion fires we flip `finish_reason`
        // to `"tool_calls"` regardless of its incoming value so the
        // downstream match below routes into the dispatch branch — the
        // model intended to call a tool, it just emitted the markup
        // in the wrong field. This catches both `"stop"` (the V4 N=5
        // bail shape) and the rarer `"length"` (which would otherwise
        // hit the context-overflow Err path and throw away a perfectly
        // good recovered call).
        let promotion = response
            .choices
            .first_mut()
            .and_then(|choice| {
                let info = promote_plain_text_tool_calls(&mut choice.message, &allowed_tool_names)?;
                choice.finish_reason = "tool_calls".to_string();
                Some(info)
            });
        if let Some(info) = promotion {
            trajectory.append_tool_call_promoted(
                turns,
                info.source.as_str(),
                info.format.as_str(),
                info.call_count,
            );
        }

        // (#406) Clear `reasoning_content` from the response message
        // now that the promoter has had its chance to scan it. The
        // Message struct's `reasoning_content` field carries a
        // documented invariant (`runtime/src/lmstudio.rs` Message
        // doc): "skip-serialize so outgoing request messages never
        // emit it (always None on the request side)". The streaming
        // path used to enforce this by stripping reasoning via
        // `accumulator.take_reasoning_content()` BEFORE building the
        // response; #406 re-attached it so the promoter could scan
        // it. The original invariant must hold from this point on —
        // the response message is about to be cloned into the
        // conversation history (`messages.push(assistant_message)`)
        // and shipped back to LMStudio on the next request. Carrying
        // reasoning_content into request-side history caused a
        // recursive-feedback regression (Beat 47 attempt 2: run 2
        // hit MAX_TURNS with 100 thinking-mode entries; run 3 went
        // 1235s before runtime exit). Clearing here restores the
        // pre-#406 behavior for the conversation history while
        // preserving the promoter's ability to scan reasoning above.
        // (Promotion-from-reasoning path also clears reasoning_content
        // inside `apply_promotion`, so this is idempotent on that
        // path.)
        if let Some(choice) = response.choices.first_mut() {
            choice.message.reasoning_content = None;
        }

        if let Some(usage) = &response.usage {
            total_prompt_tokens = total_prompt_tokens.saturating_add(usage.prompt_tokens);
            total_completion_tokens =
                total_completion_tokens.saturating_add(usage.completion_tokens);
            latest_prompt_tokens = usage.prompt_tokens;
        }

        // Record model.completed for trajectory. We grab the first
        // choice's finish_reason + tool_calls below; mirror it here.
        let trajectory_finish_reason = response
            .choices
            .first()
            .map(|c| c.finish_reason.clone())
            .unwrap_or_default();
        let trajectory_tool_calls = response
            .choices
            .first()
            .and_then(|c| c.message.tool_calls.as_ref())
            .cloned();
        trajectory.append_model_completed(
            turns,
            &trajectory_finish_reason,
            response.usage.as_ref(),
            trajectory_tool_calls.as_deref(),
        );

        // Take the first choice — LMStudio's OpenAI-compatible endpoint
        // returns exactly one for non-streaming requests, but we don't
        // assume that.
        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("LMStudio returned no choices"))?;

        let assistant_message = choice.message;
        let finish_reason = choice.finish_reason;

        // Extract reasoning content from `<think>...</think>` blocks in
        // the assistant message content (#204). Thinking-mode models
        // (qwen 3.x line, in particular) emit reasoning inline; we
        // surface it as a separate trajectory event so the flow
        // stream + viewer can render it as a collapse/expand block
        // (operator discretion to expand). The original content stays
        // unchanged in `assistant_message` — downstream consumers
        // (compaction, conversation history) see everything as-was.
        if let Some(content) = assistant_message.content.as_deref() {
            for reasoning_text in extract_think_blocks(content) {
                trajectory.append_model_reasoning(
                    turns,
                    &reasoning_text,
                    "inline-think-tags",
                );
            }
        }

        // Append the assistant's message to the conversation before we
        // process its tool calls — that's the order the next request
        // needs to see things in.
        messages.push(assistant_message.clone());

        match finish_reason.as_str() {
            "stop" => {
                return Ok(LoopOutcome {
                    terminal_reason: TerminalReason::Stop,
                    messages,
                    turns,
                    total_prompt_tokens,
                    total_completion_tokens,
                    compactions,
                });
            }
            "tool_calls" => {
                let calls = assistant_message
                    .tool_calls
                    .clone()
                    .unwrap_or_default();

                if calls.is_empty() {
                    return Err(anyhow!(
                        "finish_reason=tool_calls but assistant message had no tool_calls"
                    ));
                }

                // Dispatch each call; append a `tool` message per
                // result so the next request shows the model exactly
                // what each tool returned. Trajectory records each
                // tool.completed event so the operator can see what
                // ran post-dispatch.
                for (tool_seq, call) in calls.into_iter().enumerate() {
                    let result = dispatch(&call.function.name, &call.function.arguments);
                    trajectory.append_tool_completed(
                        turns,
                        tool_seq as u32,
                        &call.function.name,
                        call.function.arguments.len(),
                        result.len(),
                    );
                    messages.push(Message::tool_result(
                        call.id,
                        call.function.name,
                        result,
                    ));
                }

                // Phase 6: check whether the most recent prompt's
                // token count crossed the compaction threshold, AND
                // whether the conversation is long enough to compact.
                // If so, compact BEFORE the next chat() call so the
                // next request sees a smaller message thread.
                if compaction::needs_compaction(
                    latest_prompt_tokens,
                    messages.len(),
                    compaction_cfg,
                ) {
                    let before_count = messages.len();
                    compactions = compactions.saturating_add(1);
                    // (#372 T2-C) Route by strategy. Narrative is
                    // today's default (prose summary as synthetic
                    // USER message). StructuredSlot is tier-2 (typed
                    // schema + JSON mode + SYSTEM message); on
                    // success the parsed output is persisted to
                    // `/workspace/.darkmux-runtime/compaction-<gen>.json`
                    // per #352 Step 5 "persistence falls out for free."
                    match compaction_cfg.strategy {
                        compaction::CompactionStrategy::Narrative => {
                            compaction::compact(
                                client,
                                &mut messages,
                                compactions,
                                compaction_cfg,
                            )?;
                        }
                        compaction::CompactionStrategy::StructuredSlot => {
                            let parsed = compaction::structured_compact(
                                client,
                                &mut messages,
                                compactions,
                                compaction_cfg,
                            )?;
                            // Persist the JSON for downstream
                            // consumers (replay, methodology
                            // research, cross-sprint memory). Best-
                            // effort: a write failure logs but does
                            // NOT fail the dispatch — observability,
                            // not correctness.
                            persist_structured_compaction_output(
                                std::path::Path::new("/workspace/.darkmux-runtime"),
                                compactions,
                                &parsed,
                            );
                        }
                    }
                    let after_count = messages.len();
                    // Approximate summary_chars from the inserted
                    // synthetic message at PRESERVE_HEAD position
                    // (works for both Narrative=user + StructuredSlot=system).
                    let summary_chars = messages
                        .get(2)
                        .and_then(|m| m.content.as_ref())
                        .map(|c| c.len())
                        .unwrap_or(0);
                    trajectory.append_compaction(
                        compactions,
                        before_count,
                        after_count,
                        summary_chars,
                    );

                    // (#377) Escalation bound check. After persisting
                    // this compaction's trajectory entry, see whether
                    // we've crossed the operator-configured
                    // `bail_after_compactions`. If yes, bail with
                    // EscalationTriggered so the frontier-tier handoff
                    // skill picks up the salvageable state instead of
                    // burning more local-tier cycles. KISS-doubled
                    // (Beat 44 closure): bound the cost, escalate past
                    // the bound. The check is AFTER the trajectory
                    // append so the bound-crossing compaction is still
                    // observable + persisted; only the next chat()
                    // call is skipped.
                    if let Some(bail) = compaction_cfg.bail_after_compactions {
                        if compactions >= bail {
                            eprintln!(
                                "darkmux-runtime: escalation_triggered — \
                                 compactions ({compactions}) reached bail_after_compactions ({bail}); \
                                 emitting EscalationTriggered terminal for frontier handoff"
                            );
                            return Ok(LoopOutcome {
                                terminal_reason: TerminalReason::EscalationTriggered(
                                    EscalationReason::CompactionLimitReached,
                                ),
                                messages,
                                turns,
                                total_prompt_tokens,
                                total_completion_tokens,
                                compactions,
                            });
                        }
                    }
                }

                // Loop back and call chat() again.
                continue;
            }
            "length" => {
                return Err(anyhow!(
                    "model returned finish_reason=length — context overflow. \
                     Compaction fires before the next call when prompt_tokens \
                     crosses DARKMUX_RUNTIME_COMPACT_THRESHOLD_TOKENS, but the \
                     current response itself was already cut off mid-generation. \
                     Workload may need a smaller threshold or a larger n_ctx."
                ));
            }
            other => {
                return Err(anyhow!(
                    "unexpected finish_reason: {other} — runtime doesn't know \
                     how to handle this. Aborting."
                ));
            }
        }
    }
}

/// Run one SSE-streamed turn: consume the chunk iterator, emit a
/// `model.partial` trajectory event per chunk (stats only — no content
/// in the events to keep `trajectory.jsonl` bounded), and return the
/// accumulated `ChatResponse` shaped identically to a non-streaming
/// response. (#205)
///
/// Reasoning content delivered via the separate-field stream
/// (`Delta.reasoning_content`, the Qwen 3 / DeepSeek pattern) is
/// extracted from the accumulator and emitted as a `model.reasoning`
/// trajectory event with `format=separate-field`, mirroring the
/// inline-`<think>`-tag path that the caller handles post-turn.
fn run_streaming_turn(
    client: &LmStudioClient,
    request: &ChatRequest,
    seq: u32,
    trajectory: &mut Trajectory,
) -> Result<crate::lmstudio::ChatResponse> {
    let (system_chars, prompt_chars) = measure_request_context(&request.messages);
    trajectory.append_model_streaming_start(seq, system_chars, prompt_chars);
    let mut accumulator = ChunkAccumulator::new();
    let mut last_content_bytes: usize = 0;
    let stream = client.chat_streaming(request)?;
    for chunk_result in stream {
        let chunk = chunk_result?;
        let partial_index = accumulator.ingest(&chunk);
        let cumulative = accumulator.content_bytes();
        let delta_bytes = cumulative.saturating_sub(last_content_bytes);
        last_content_bytes = cumulative;
        trajectory.append_model_partial(
            seq,
            partial_index,
            delta_bytes,
            cumulative,
            accumulator.has_tool_calls(),
        );
    }
    let partial_count = accumulator.partial_count();
    let total_content = accumulator.content_bytes();
    let reasoning_content = accumulator.take_reasoning_content();
    let mut response = accumulator.into_response();
    let tool_calls_count = response
        .choices
        .first()
        .and_then(|c| c.message.tool_calls.as_ref())
        .map(|tc| tc.len())
        .unwrap_or(0);
    trajectory.append_model_streaming_end(seq, partial_count, total_content, tool_calls_count);
    if let Some(reasoning) = reasoning_content {
        trajectory.append_model_reasoning(seq, &reasoning, "separate-field");
        // (#406) Surface reasoning_content on the response message so
        // the caller's plain-text-tool-call promoter can scan it.
        // Without this the streaming path loses the reasoning field
        // before promotion runs — and Qwen 3.x thinking-mode bails
        // ride in reasoning, not content.
        if let Some(choice) = response.choices.first_mut() {
            choice.message.reasoning_content = Some(reasoning);
        }
    }
    Ok(response)
}

/// Measure per-turn context size: returns `(system_chars, prompt_chars)`.
/// `system_chars` is the total length of system-role message content;
/// `prompt_chars` is the total length of every other message — user
/// content, assistant text, assistant tool-call args (function name +
/// arguments JSON string), tool-result content. Stamped on
/// `model.streaming.start` (#361) so operators can read per-turn
/// context growth straight from the trajectory, independent of
/// whether LMStudio's `usage` field arrived (#360).
///
/// **Counting choice**: we measure what the MODEL ATTENDS TO, not the
/// wire-framing bytes. `tool_call.id` / `tool_call.kind` (always
/// `"function"`) / message-envelope fields are excluded — those are
/// transport-shape that doesn't carry semantic information the model
/// reasons over. Future telemetry layers that need wire bytes (for
/// API-cost calculations) should compute that separately rather than
/// extend this function.
fn measure_request_context(messages: &[Message]) -> (usize, usize) {
    let mut system_chars = 0usize;
    let mut prompt_chars = 0usize;
    for m in messages {
        let content_len = m.content.as_ref().map(|s| s.len()).unwrap_or(0);
        let tool_args_len: usize = m
            .tool_calls
            .as_ref()
            .map(|tcs| {
                tcs.iter()
                    .map(|tc| tc.function.name.len() + tc.function.arguments.len())
                    .sum()
            })
            .unwrap_or(0);
        let total = content_len + tool_args_len;
        if m.role == "system" {
            system_chars += total;
        } else {
            prompt_chars += total;
        }
    }
    (system_chars, prompt_chars)
}

/// (#372 T2-C) Best-effort write of the parsed structured-compaction
/// output to `<runtime_dir>/compaction-<generation>.json`. Creates
/// the parent directory if needed. Write failures log to stderr but
/// do NOT propagate — persistence is observability (replay,
/// methodology research, cross-sprint memory) not correctness, per
/// #352 "persistence falls out for free" framing.
fn persist_structured_compaction_output(
    runtime_dir: &std::path::Path,
    generation: u32,
    output: &compaction::StructuredCompactionOutput,
) {
    if let Err(e) = std::fs::create_dir_all(runtime_dir) {
        eprintln!(
            "darkmux-runtime: persist compaction #{generation} — create dir failed: {e}"
        );
        return;
    }
    let path = runtime_dir.join(format!("compaction-{generation}.json"));
    let json = match serde_json::to_string_pretty(output) {
        Ok(j) => j,
        Err(e) => {
            eprintln!(
                "darkmux-runtime: persist compaction #{generation} — serialize failed: {e}"
            );
            return;
        }
    };
    if let Err(e) = std::fs::write(&path, json) {
        eprintln!(
            "darkmux-runtime: persist compaction #{generation} — write to {} failed: {e}",
            path.display()
        );
    }
}

/// Extract `<think>...</think>` block contents from a string. Returns
/// each block's inner text (without the tags) in order. Returns empty
/// vec when no blocks are present.
///
/// Used to surface reasoning content as separate trajectory events
/// (#204). qwen 3.x thinking-mode models emit reasoning inline in the
/// assistant message content wrapped in these tags; we extract for the
/// flow stream + viewer but leave the original content untouched.
///
/// Implementation is a tag-scan, not a regex — keeps the runtime free
/// of regex deps and handles nested tags by treating the outermost
/// pairs as the boundary. Malformed (unclosed) tags are ignored.
fn extract_think_blocks(content: &str) -> Vec<String> {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";
    let mut blocks = Vec::new();
    let mut cursor = 0;
    while let Some(open_at) = content[cursor..].find(OPEN) {
        let start = cursor + open_at + OPEN.len();
        if let Some(close_offset) = content[start..].find(CLOSE) {
            blocks.push(content[start..start + close_offset].to_string());
            cursor = start + close_offset + CLOSE.len();
        } else {
            // Unclosed tag — stop scanning to avoid runaway capture.
            break;
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lmstudio::{FunctionCall, ToolCall};

    // ─── #372 T2-C: persist_structured_compaction_output ──────────

    use crate::compaction::{CompactionMetadata, CurrentTruth, StructuredCompactionOutput};

    fn dummy_structured_output(generation: u32) -> StructuredCompactionOutput {
        StructuredCompactionOutput {
            objective: "test obj".into(),
            current_truth: CurrentTruth::default(),
            compaction_metadata: CompactionMetadata {
                schema_version: "0.1".into(),
                generation,
                source_message_count: 5,
            },
            completed_decisions: None,
            errors_to_preserve: None,
            next_concrete_actions: None,
            verify_criteria: None,
            sprint_id: None,
        }
    }

    #[test]
    fn persist_writes_compaction_json_to_runtime_dir() {
        let tmp = tempdir::TempDir::new("persist-compaction").unwrap();
        let runtime_dir = tmp.path().join(".darkmux-runtime");
        let out = dummy_structured_output(3);
        persist_structured_compaction_output(&runtime_dir, 3, &out);
        let written = runtime_dir.join("compaction-3.json");
        assert!(
            written.exists(),
            "expected compaction-3.json at {}",
            written.display()
        );
        let body = std::fs::read_to_string(&written).unwrap();
        let parsed: StructuredCompactionOutput =
            serde_json::from_str(&body).expect("written JSON round-trips");
        assert_eq!(parsed.compaction_metadata.generation, 3);
        assert_eq!(parsed.objective, "test obj");
    }

    #[test]
    fn persist_creates_runtime_dir_if_missing() {
        let tmp = tempdir::TempDir::new("persist-mkdir").unwrap();
        // Subdir that doesn't exist yet — persist must create it.
        let runtime_dir = tmp.path().join("nested").join("not-yet").join(".darkmux-runtime");
        let out = dummy_structured_output(1);
        persist_structured_compaction_output(&runtime_dir, 1, &out);
        assert!(runtime_dir.join("compaction-1.json").exists());
    }

    #[test]
    fn persist_silently_skips_when_dir_unwritable() {
        // Path under a regular file (can't be a dir) — write should
        // fail silently, NOT panic or propagate. Persistence is
        // observability, not correctness.
        let tmp = tempdir::TempDir::new("persist-unwritable").unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"i am a file not a dir").unwrap();
        let runtime_dir = blocker.join("under-a-file");
        let out = dummy_structured_output(2);
        // Should NOT panic.
        persist_structured_compaction_output(&runtime_dir, 2, &out);
    }

    // ─── measure_request_context (#361 fix) ─────────────────────────

    #[test]
    fn measure_empty_messages_returns_zero_zero() {
        let (s, p) = measure_request_context(&[]);
        assert_eq!(s, 0);
        assert_eq!(p, 0);
    }

    #[test]
    fn measure_system_and_user_routes_to_correct_bucket() {
        let messages = vec![Message::system("sys prompt"), Message::user("hello")];
        let (system, prompt) = measure_request_context(&messages);
        assert_eq!(system, "sys prompt".len());
        assert_eq!(prompt, "hello".len());
    }

    #[test]
    fn measure_counts_assistant_tool_calls_into_prompt() {
        let assistant_with_tools = Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: r#"{"path":"/workspace/file.py"}"#.into(),
                },
            }]),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        };
        let (system, prompt) = measure_request_context(&[assistant_with_tools]);
        assert_eq!(system, 0);
        // name + arguments lengths — sanity-check the sum.
        assert_eq!(
            prompt,
            "read".len() + r#"{"path":"/workspace/file.py"}"#.len()
        );
    }

    #[test]
    fn measure_counts_tool_result_into_prompt() {
        let messages = vec![Message {
            role: "tool".into(),
            content: Some("file contents".into()),
            tool_calls: None,
            tool_call_id: Some("call_1".into()),
            name: Some("read".into()),
            reasoning_content: None,
        }];
        let (system, prompt) = measure_request_context(&messages);
        assert_eq!(system, 0);
        assert_eq!(prompt, "file contents".len());
    }

    #[test]
    fn measure_typical_turn_buckets_correctly() {
        // System + user + assistant (with content + tool calls) + tool result.
        let messages = vec![
            Message::system("you are coder"),
            Message::user("fix the bug"),
            Message {
                role: "assistant".into(),
                content: Some("I'll read the file first.".into()),
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".into(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: "read".into(),
                        arguments: r#"{"path":"/x"}"#.into(),
                    },
                }]),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
            },
            Message {
                role: "tool".into(),
                content: Some("def foo():\n    pass".into()),
                tool_calls: None,
                tool_call_id: Some("call_1".into()),
                name: Some("read".into()),
                reasoning_content: None,
            },
        ];
        let (system, prompt) = measure_request_context(&messages);
        assert_eq!(system, "you are coder".len());
        let expected_prompt = "fix the bug".len()
            + "I'll read the file first.".len()
            + "read".len()
            + r#"{"path":"/x"}"#.len()
            + "def foo():\n    pass".len();
        assert_eq!(prompt, expected_prompt);
    }

    #[test]
    fn extract_think_blocks_none() {
        assert_eq!(extract_think_blocks("just plain content"), Vec::<String>::new());
    }

    #[test]
    fn extract_think_blocks_single() {
        let content = "Before <think>my reasoning here</think> after.";
        assert_eq!(extract_think_blocks(content), vec!["my reasoning here"]);
    }

    #[test]
    fn extract_think_blocks_multiple() {
        let content =
            "<think>first thought</think>\nresponse\n<think>second thought</think>";
        assert_eq!(
            extract_think_blocks(content),
            vec!["first thought", "second thought"]
        );
    }

    #[test]
    fn extract_think_blocks_multiline() {
        let content = "<think>line one\nline two\nline three</think>";
        assert_eq!(
            extract_think_blocks(content),
            vec!["line one\nline two\nline three"]
        );
    }

    #[test]
    fn extract_think_blocks_unclosed_tag_skipped() {
        // Unclosed tag mid-content — return whatever closed blocks came
        // before, ignore the unclosed one.
        let content = "<think>closed</think> middle <think>unclosed forever";
        assert_eq!(extract_think_blocks(content), vec!["closed"]);
    }

    #[test]
    fn extract_think_blocks_empty_inside() {
        let content = "<think></think>";
        assert_eq!(extract_think_blocks(content), vec![""]);
    }

    // ─── compaction loop integration (against mock LMStudio) ──────────
    //
    // These tests verify the end-to-end loop behavior — the predicate
    // tests in compaction.rs cover "should compaction fire?"; these
    // cover "does the runtime actually invoke the compactor model when
    // the predicate trips?" That's the layer-boundary gap pre-fix
    // didn't have coverage for.
    //
    // The mock LMStudio (httpmock) lets the test:
    //   - drive a deterministic sequence of chat responses
    //   - inspect which `model` each request used (primary vs compactor)
    //   - assert the compactor was called the expected number of times
    //
    // No real LMStudio + no Docker required. The non-streaming code
    // path is exercised (streaming=false) — the streaming path's
    // compaction behavior is structurally identical (uses the same
    // `needs_compaction` + `compact` calls) and is covered by the
    // companion test below.

    use crate::lmstudio::{LmStudioClient, Message};
    use crate::tools::Tool;
    use crate::trajectory::Trajectory;
    use httpmock::prelude::*;

    /// Build a non-streaming chat-completion response body the way
    /// LMStudio would return it. Tests use this to construct
    /// deterministic turn-by-turn responses.
    fn chat_response_json(
        content: Option<&str>,
        tool_calls: Option<serde_json::Value>,
        finish_reason: &str,
        prompt_tokens: u32,
        completion_tokens: u32,
    ) -> serde_json::Value {
        let mut message = serde_json::json!({ "role": "assistant" });
        if let Some(c) = content {
            message["content"] = serde_json::json!(c);
        } else {
            message["content"] = serde_json::Value::Null;
        }
        if let Some(tc) = tool_calls {
            message["tool_calls"] = tc;
        }
        serde_json::json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "ignored-by-test",
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": finish_reason,
            }],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens,
            },
        })
    }

    /// #325: terminal_reason discriminates loop outcomes. A finish_reason=
    /// stop response from the model produces TerminalReason::Stop;
    /// a loop that runs out the MAX_TURNS clock produces
    /// TerminalReason::MaxTurns (NOT an Err — that path was reserved
    /// for infrastructure failures).
    ///
    /// This test pairs with the existing
    /// `loop_runs_against_mock_and_terminates_on_stop` (Stop case)
    /// to lock both terminal reasons. MaxTurns specifically asserts
    /// the loop returns Ok(outcome) — the JSON envelope path in
    /// main.rs reads outcome.terminal_reason and emits result=max_turns.
    #[test]
    #[serial_test::serial]
    fn loop_returns_maxturns_terminal_reason_when_cap_hit() {
        let server = MockServer::start();
        // Primary mock: every call returns finish_reason=tool_calls.
        // The loop will never see stop; will run MAX_TURNS=100 turns
        // and bail with the structured terminal_reason.
        let _primary = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"path\":\"/workspace/missing.txt\",\"offset\":1,\"limit\":0}",
                    },
                }])),
                "tool_calls",
                100,
                10,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("maxturns").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("loop forever")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg)
            .expect("MAX_TURNS path returns Ok(outcome), not Err");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::MaxTurns,
            "expected MaxTurns terminal_reason after exhausting the loop"
        );
        // Sanity: hit the cap.
        assert!(
            outcome.turns >= 100,
            "expected >= MAX_TURNS turns; got {}",
            outcome.turns
        );
    }

    /// (#406) The 20% silent-bail scenario: model returned
    /// `finish_reason=stop` with `content` containing an XML-format
    /// tool call but EMPTY `tool_calls` field. The promoter must
    /// recover the call from content, flip finish_reason to
    /// `tool_calls`, and the loop must continue (NOT exit after one
    /// turn). Asserts:
    ///   - outcome.turns > 1 (the bail was promoted, not exited)
    ///   - terminal_reason is MaxTurns (mock keeps returning bail
    ///     shape; we run out the clock — that's fine, what matters
    ///     is the first turn didn't terminate as Stop)
    ///
    /// Before #406 this test would assert turns==1 + Stop, which is
    /// the silent-bail behavior that compounded across multi-dispatch
    /// dogfood to 67% chance of seeing at least one bail per
    /// five-dispatch workflow.
    #[test]
    #[serial_test::serial]
    fn loop_recovers_tool_call_from_xml_in_content_when_finish_reason_is_stop() {
        let server = MockServer::start();
        // Every call returns the bail shape: finish=stop, content has
        // an XML tool_call, tool_calls field is null. Without the
        // promoter, loop exits at turn 1. With the promoter, loop
        // promotes the call, dispatches `read` (will fail on missing
        // /workspace/x.txt — that's fine, a failed tool dispatch is
        // still a successful loop iteration), and loops back.
        let _bail_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some(
                    "Let me read the file:\n\
                    <tool_call>\
                    <function=read>\
                    <parameter=path>/workspace/x.txt</parameter>\
                    <parameter=offset>1</parameter>\
                    <parameter=limit>50</parameter>\
                    </function>\
                    </tool_call>",
                ),
                None,
                "stop",
                100,
                10,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("xml-promote").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg)
            .expect("promoted XML tool call should drive the loop, not error");

        assert!(
            outcome.turns > 1,
            "promotion must continue the loop past turn 1; got turns={} (pre-#406 silent bail at turn 1)",
            outcome.turns
        );
        // The mock keeps returning the bail shape, so the loop runs
        // until MAX_TURNS. That's the right outcome for this synthetic
        // test — the load-bearing assertion is the turns>1 above.
        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::MaxTurns,
            "expected MaxTurns after the promoter kept the loop alive past MAX_TURNS"
        );
    }

    /// (#406) Promotion also recovers calls when `finish_reason` is
    /// `"length"`. Pre-fix the downstream match treated `"length"` as
    /// a hard context-overflow error and threw away the recovered
    /// call. Asserts the loop continues past turn 1 just like the
    /// finish_reason=stop case.
    #[test]
    #[serial_test::serial]
    fn loop_recovers_tool_call_from_xml_when_finish_reason_is_length() {
        let server = MockServer::start();
        let _bail_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some(
                    "<tool_call>\
                    <function=read>\
                    <parameter=path>/workspace/x.txt</parameter>\
                    <parameter=offset>1</parameter>\
                    <parameter=limit>50</parameter>\
                    </function>\
                    </tool_call>",
                ),
                None,
                "length", // Pre-fix: hard error. Post-fix: promotion flips to tool_calls.
                100,
                10,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("xml-promote-length").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg)
            .expect("recovered call from length-truncated response should drive the loop");

        assert!(
            outcome.turns > 1,
            "promotion must continue the loop even when finish_reason=length; got turns={}",
            outcome.turns
        );
    }

    /// (#406) Reasoning-channel variant of the bail scenario: the XML
    /// tool call lands in `reasoning_content` rather than `content`
    /// (the Qwen 3.x thinking-mode case from V4 N=5 Run 2). The
    /// promoter must fall back from content to reasoning_content.
    #[test]
    #[serial_test::serial]
    fn loop_recovers_tool_call_from_xml_in_reasoning_when_finish_reason_is_stop() {
        let server = MockServer::start();
        let _bail_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            // content is null; reasoning_content carries the call —
            // exactly the V4 N=5 Run 2 bail shape.
            then.status(200).json_body(serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "created": 1700000000,
                "model": "ignored-by-test",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "reasoning_content":
                            "Now I should read the file:\n\
                            <tool_call>\
                            <function=read>\
                            <parameter=path>/workspace/x.txt</parameter>\
                            <parameter=offset>1</parameter>\
                            <parameter=limit>50</parameter>\
                            </function>\
                            </tool_call>",
                    },
                    "finish_reason": "stop",
                }],
                "usage": { "prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110 },
            }));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("xml-promote-reason").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg)
            .expect("promoted XML tool call from reasoning_content should drive the loop");

        assert!(
            outcome.turns > 1,
            "reasoning-channel promotion must keep the loop alive past turn 1; got turns={}",
            outcome.turns
        );
        assert_eq!(outcome.terminal_reason, TerminalReason::MaxTurns);
    }

    /// (#406 regression guard, Beat 47) The streaming path used to
    /// strip reasoning_content via `accumulator.take_reasoning_content`
    /// before building the response, enforcing the documented Message
    /// invariant ("outgoing request messages never emit
    /// reasoning_content"). PR #407 re-attached reasoning so the
    /// promoter could scan it; that re-attachment must be cleared
    /// BEFORE the response message gets pushed into conversation
    /// history. Otherwise the next turn's request carries the model's
    /// prior reasoning text — recursive feedback that caused 100-turn
    /// MAX_TURNS bails in attempt 2 of the validation.
    ///
    /// This test pins the invariant: an assistant message in the
    /// returned conversation MUST have reasoning_content=None,
    /// regardless of whether the model emitted reasoning.
    #[test]
    #[serial_test::serial]
    fn assistant_messages_in_history_never_carry_reasoning_content() {
        let server = MockServer::start();
        // First call: model emits reasoning + a structured tool call
        // (promotion does NOT fire — tool_calls field is populated).
        // The reasoning is set on the response; without the post-
        // promoter clear, it would leak into the next request.
        let _turn1 = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"role\":\"user\"");
            then.status(200).json_body(serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "created": 1700000000,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "reasoning_content": "Let me think about this and call a tool",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "read",
                                "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":1}",
                            },
                        }],
                    },
                    "finish_reason": "tool_calls",
                }],
                "usage": { "prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150 },
            }));
        });
        // Second call (after tool result): model finishes with stop.
        let _turn2 = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"role\":\"tool\"");
            then.status(200).json_body(chat_response_json(
                Some("done"),
                None,
                "stop",
                200,
                10,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("reasoning-invariant").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![Message::system("test"), Message::user("read x.txt")];
        let tools = [Tool::Read];

        let cfg = compaction::CompactionConfig::default();
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg)
            .expect("clean two-turn dispatch");

        // The first assistant message in the conversation must have
        // reasoning_content stripped — even though the model emitted
        // reasoning. The promoter scanned it; the conversation
        // history does not retain it.
        let assistant_msgs: Vec<&Message> = outcome
            .messages
            .iter()
            .filter(|m| m.role == "assistant")
            .collect();
        assert!(
            !assistant_msgs.is_empty(),
            "expected at least one assistant message in history"
        );
        for (idx, m) in assistant_msgs.iter().enumerate() {
            assert!(
                m.reasoning_content.is_none(),
                "assistant message #{idx} in history must have reasoning_content=None \
                 (invariant: lmstudio.rs Message doc — request-side never emits it). \
                 Got: {:?}",
                m.reasoning_content
            );
        }
    }

    /// Smoke: mock returns finish_reason=stop on first call. Loop
    /// terminates cleanly, no compaction. Proves the mock + LmStudioClient
    /// + loop_runner integration plumbing works.
    #[test]
    #[serial_test::serial]
    fn loop_runs_against_mock_and_terminates_on_stop() {
        let server = MockServer::start();
        let stop_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(chat_response_json(
                Some("done"),
                None,
                "stop",
                1234,
                10,
            ));
        });

        // LmStudioClient expects the base_url to include the /v1
        // prefix (matches the production default); httpmock's
        // server.base_url() is just the host:port. Compose the path
        // here so the mock's /v1/chat/completions matcher hits.
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("compaction-smoke").unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let initial = vec![
            Message::system("you are a test assistant"),
            Message::user("hi"),
        ];
        let tools = [Tool::Read, Tool::Edit, Tool::Bash];

        let cfg = compaction::CompactionConfig::default();
        let outcome = run(&client, "test-model", initial, &tools, &mut traj, false, &cfg)
            .expect("loop should terminate cleanly on first-turn stop");

        stop_mock.assert();
        assert_eq!(outcome.turns, 1);
        assert_eq!(outcome.compactions, 0);
        assert_eq!(outcome.total_prompt_tokens, 1234);
        // #325: pin the Stop terminal_reason on this clean-exit path.
        assert_eq!(outcome.terminal_reason, TerminalReason::Stop);
    }

    /// The real signal: drive the loop into a compaction by escalating
    /// prompt_tokens past threshold. Mock sequence:
    ///   1. primary returns tool_calls + above-threshold prompt_tokens
    ///   2. (tools execute → messages grow past PRESERVE_HEAD+1+PRESERVE_TAIL=7)
    ///   3. needs_compaction fires → runtime calls compactor model
    ///   4. compactor returns summary
    ///   5. primary returns stop
    ///
    /// We pass an explicit `CompactionConfig { threshold_tokens: 1000,
    /// compactor_model: "test-compactor" }` so the mock doesn't have
    /// to fake huge prompt sizes. Distinguishes the compactor call
    /// from primary calls by inspecting the request's `model` field —
    /// they differ.
    ///
    /// Pre-#368 this test set/unset
    /// `DARKMUX_RUNTIME_COMPACT_THRESHOLD_TOKENS` env var with a 40-
    /// line EnvGuard for restore-on-drop and required serial
    /// execution. Post-#368 the runtime reads compaction config from
    /// explicit params (no env), so this is just a struct literal.
    ///
    /// Asserts:
    ///   - outcome.compactions == 1
    ///   - compactor mock was hit exactly once
    ///   - primary mock was hit at least twice (before + after compaction)
    #[test]
    fn loop_triggers_compaction_when_threshold_crossed() {
        let cfg = compaction::CompactionConfig {
            threshold_tokens: 1000,
            compactor_model: "test-compactor".to_string(),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: None,
            custom_instructions: None,
        };

        let server = MockServer::start();

        // Primary model: every call returns tool_calls with above-
        // threshold prompt_tokens. The loop will keep calling until
        // MAX_TURNS, but we only need the FIRST primary call to fire
        // and the compactor to be invoked once before MAX_TURNS hits.
        // The mock-hits assertion below is what verifies the
        // layer-boundary signal — outcome itself is not needed here.
        let _primary_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-primary\"");
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":0}",
                    },
                }])),
                "tool_calls",
                5000, // above 1000-token threshold
                50,
            ));
        });

        let compactor_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-compactor\"");
            then.status(200).json_body(chat_response_json(
                Some("Summary: assistant read a file."),
                None,
                "stop",
                500,
                30,
            ));
        });

        // LmStudioClient expects the base_url to include the /v1
        // prefix (matches the production default); httpmock's
        // server.base_url() is just the host:port. Compose the path
        // here so the mock's /v1/chat/completions matcher hits.
        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("compaction-fire").unwrap();
        // Pre-populate /workspace dir + the file the mock's tool_call
        // will try to read. Real dispatches mount /workspace as a
        // tempdir; here we just give read a target that resolves.
        std::fs::create_dir_all(tmp.path()).unwrap();
        // The runtime's `read` tool will validate paths under
        // /workspace; for the integration test we don't actually need
        // the tool to succeed — failed reads still append a `tool`
        // message and the loop continues. The key invariant: the
        // primary's escalated usage trips needs_compaction(...) on
        // the next iteration.

        let mut traj = Trajectory::open(tmp.path());
        // Pad initial messages so that after the first turn's
        // assistant-message + tool-result, we have >= 7 messages
        // (PRESERVE_HEAD=2 + 1 + PRESERVE_TAIL=4) — the second
        // condition for needs_compaction. Adding 5 extra user/assistant
        // pairs gets us there.
        let mut initial = vec![Message::system("test system"), Message::user("seed")];
        for i in 0..3 {
            initial.push(Message::user(format!("padding user {i}")));
            initial.push(Message::assistant(format!("padding assistant {i}")));
        }
        let tools = [Tool::Read];

        // Run. Expected to error eventually (mock loops forever on
        // tool_calls; will hit MAX_TURNS); we don't care about the
        // outcome's Ok/Err — just whether the compactor was invoked
        // along the way. The result IS the side-effect assertion below.
        let outcome = run(&client, "test-primary", initial, &tools, &mut traj, false, &cfg);

        // Core assertion: compactor was invoked at least once. This is
        // the layer-boundary signal — the runtime's loop translated
        // a threshold-crossing into a compactor model call.
        assert!(
            compactor_mock.hits() >= 1,
            "compactor model was never invoked despite threshold being crossed; \
             compactor hits={}",
            compactor_mock.hits()
        );

        // QA FLAG 2 — also assert the runtime's own compactions counter
        // incremented. Catches the future-regression class where the
        // loop calls the compactor but forgets to bump the telemetry
        // (drift between observable side-effect and reported counter).
        // The loop hits MAX_TURNS so `outcome` is Err; we still want
        // to read its inner state. The Err path doesn't expose the
        // partial LoopOutcome, but the runtime emits compaction events
        // to trajectory which is the more durable signal anyway.
        // For now: if the loop ever returns Ok (would require the
        // mock to drive a stop after compaction), enforce counter
        // parity; otherwise rely on the mock-hit assertion above.
        if let Ok(o) = outcome {
            assert!(
                o.compactions >= 1,
                "runtime returned Ok but compactions counter is 0 \
                 despite mock recording {} compactor hit(s) — \
                 telemetry drift",
                compactor_mock.hits()
            );
        }
    }

    /// (#377) When `bail_after_compactions = N` is set and N
    /// compactions have fired, the loop must exit with
    /// `TerminalReason::EscalationTriggered(CompactionLimitReached)`
    /// rather than continuing to MAX_TURNS. Same mock setup as the
    /// preceding test except: bail=1 so the FIRST compaction trips
    /// the bound + the loop bails immediately after persisting the
    /// trajectory entry.
    ///
    /// This is the load-bearing chunk-3 invariant: the bound is
    /// observed, the salvageable state ships in LoopOutcome, and the
    /// terminal reason is the specific escalation variant (NOT a
    /// generic timeout or Err). Frontier handoff skill branches on
    /// the variant.
    #[test]
    fn loop_bails_with_escalation_when_compaction_limit_reached() {
        let cfg = compaction::CompactionConfig {
            threshold_tokens: 1000,
            compactor_model: "test-compactor".to_string(),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: Some(1),
            custom_instructions: None,
        };

        let server = MockServer::start();

        let _primary_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-primary\"");
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":0}",
                    },
                }])),
                "tool_calls",
                5000,
                50,
            ));
        });

        let compactor_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-compactor\"");
            then.status(200).json_body(chat_response_json(
                Some("Summary: assistant read a file."),
                None,
                "stop",
                500,
                30,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("compaction-bail").unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let mut initial = vec![Message::system("test system"), Message::user("seed")];
        for i in 0..3 {
            initial.push(Message::user(format!("padding user {i}")));
            initial.push(Message::assistant(format!("padding assistant {i}")));
        }
        let tools = [Tool::Read];

        let outcome = run(&client, "test-primary", initial, &tools, &mut traj, false, &cfg)
            .expect("bail should produce Ok with EscalationTriggered, not Err");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::EscalationTriggered(EscalationReason::CompactionLimitReached),
            "bail must produce the specific escalation variant, not a generic terminal"
        );
        assert_eq!(
            outcome.compactions, 1,
            "the bound-crossing compaction is counted"
        );
        assert_eq!(
            compactor_mock.hits(),
            1,
            "exactly one compactor call before the bail"
        );
        // Salvageable state: messages vec must be non-empty so the
        // frontier handoff can pick up where local-tier left off.
        assert!(
            !outcome.messages.is_empty(),
            "LoopOutcome.messages must carry salvageable state for frontier handoff"
        );
    }

    /// (#377) When `bail_after_compactions = None` is set (operator
    /// hasn't configured a bound), the loop must NOT bail — it
    /// continues through subsequent compactions as before. Catches
    /// the regression class where the bail check fires on the
    /// default None case.
    #[test]
    fn loop_does_not_bail_when_bail_after_compactions_is_none() {
        let cfg = compaction::CompactionConfig {
            threshold_tokens: 1000,
            compactor_model: "test-compactor".to_string(),
            threshold_ratio: None,
            context_window: None,
            strategy: compaction::CompactionStrategy::Narrative,
            bail_after_compactions: None,
            custom_instructions: None,
        };

        let server = MockServer::start();
        let _primary_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-primary\"");
            then.status(200).json_body(chat_response_json(
                None,
                Some(serde_json::json!([{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":0}",
                    },
                }])),
                "tool_calls",
                5000,
                50,
            ));
        });
        let _compactor_mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"model\":\"test-compactor\"");
            then.status(200).json_body(chat_response_json(
                Some("Summary: assistant read a file."),
                None,
                "stop",
                500,
                30,
            ));
        });

        let client = LmStudioClient::with_base_url(format!("{}/v1", server.base_url()));
        let tmp = tempdir::TempDir::new("compaction-no-bail").unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        let mut traj = Trajectory::open(tmp.path());
        let mut initial = vec![Message::system("test system"), Message::user("seed")];
        for i in 0..3 {
            initial.push(Message::user(format!("padding user {i}")));
            initial.push(Message::assistant(format!("padding assistant {i}")));
        }
        let tools = [Tool::Read];

        // Loop hits MAX_TURNS (mock loops forever). The key
        // assertion: terminal_reason must be MaxTurns, NOT
        // EscalationTriggered, even though compactions fired.
        let outcome = run(&client, "test-primary", initial, &tools, &mut traj, false, &cfg)
            .expect("loop should hit MAX_TURNS, not error");

        assert_eq!(
            outcome.terminal_reason,
            TerminalReason::MaxTurns,
            "with bail_after_compactions=None, MAX_TURNS is the only bound that fires"
        );
        assert!(
            outcome.compactions >= 1,
            "compaction still fires; bail just doesn't kick in"
        );
    }
}
