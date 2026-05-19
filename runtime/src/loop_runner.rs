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
//! Still omitted (Phase 7+ if measurements show they're needed):
//!
//! - No streaming. Single-shot completions only.
//! - No retries on transient failures. A network blip aborts the loop.
//! - No per-profile threshold derivation (compaction threshold is env-
//!   tunable but global, not derived from active darkmux profile).

use anyhow::{anyhow, Result};

use crate::compaction;
use crate::lmstudio::{ChatRequest, LmStudioClient, Message};
use crate::tools::{dispatch, Tool};
use crate::trajectory::Trajectory;

/// Hard cap on tool-call turns inside a single dispatch. If the model
/// hits this without returning `stop`, the loop aborts with a loud
/// error — better than spinning forever. 100 was picked after the
/// Phase 6d e2e found 16 was too tight for long-agentic-shape work
/// (real coding workloads with ~3 files of test edits need 30-50+
/// granular tool calls with the current tool palette).
const MAX_TURNS: u32 = 100;

/// Outcome of a completed loop run.
#[derive(Debug)]
pub struct LoopOutcome {
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
pub fn run(
    client: &LmStudioClient,
    model: &str,
    initial_messages: Vec<Message>,
    tools: &[Tool],
    trajectory: &mut Trajectory,
) -> Result<LoopOutcome> {
    let mut messages = initial_messages;
    let tool_defs: Vec<_> = tools.iter().map(|t| t.to_tool_def()).collect();

    let mut turns: u32 = 0;
    let mut total_prompt_tokens: u32 = 0;
    let mut total_completion_tokens: u32 = 0;
    let mut compactions: u32 = 0;
    let mut latest_prompt_tokens: u32 = 0;

    loop {
        if turns >= MAX_TURNS {
            return Err(anyhow!(
                "loop exceeded max turns ({MAX_TURNS}) without reaching stop; \
                 last assistant message did not produce a terminal response"
            ));
        }

        let request = ChatRequest {
            model: model.to_string(),
            messages: messages.clone(),
            tools: tool_defs.clone(),
            tool_choice: Some("auto".into()),
            temperature: 0.2,
            max_tokens: None,
        };

        let response = client.chat(&request)?;
        turns += 1;

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

        // Append the assistant's message to the conversation before we
        // process its tool calls — that's the order the next request
        // needs to see things in.
        messages.push(assistant_message.clone());

        match finish_reason.as_str() {
            "stop" => {
                return Ok(LoopOutcome {
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
                if compaction::needs_compaction(latest_prompt_tokens, messages.len()) {
                    let before_count = messages.len();
                    compactions = compactions.saturating_add(1);
                    compaction::compact(client, &mut messages, compactions)?;
                    let after_count = messages.len();
                    // Approximate summary_chars from the inserted
                    // synthetic user message (the last 5-from-end
                    // position; head + 1 synthetic + tail-4).
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
