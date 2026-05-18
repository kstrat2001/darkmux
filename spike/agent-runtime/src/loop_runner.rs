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

/// Hard cap on tool-call turns inside a single dispatch. If the model
/// hits this without returning `stop`, the loop aborts with a loud
/// error — better than spinning forever. Tunable; raise it when
/// genuine multi-step workflows need more rounds.
const MAX_TURNS: u32 = 16;

/// Outcome of a completed loop run.
#[derive(Debug)]
pub struct LoopOutcome {
    /// Full conversation, in order, including system / user / assistant
    /// / tool messages. The final assistant message has the model's
    /// terminal response.
    pub messages: Vec<Message>,

    /// Number of model turns the loop took (each chat-completion call).
    pub turns: u32,

    /// Total prompt tokens summed across all calls. Useful for spike
    /// measurement; Phase 6 will use the per-call usage for compaction
    /// triggering.
    pub total_prompt_tokens: u32,

    /// Total completion tokens summed across all calls.
    pub total_completion_tokens: u32,

    /// Number of compaction events that fired during the loop.
    /// Phase 6: middle-replace via the companion compactor model.
    pub compactions: u32,
}

/// Run the tool-call loop to completion.
pub fn run(
    client: &LmStudioClient,
    model: &str,
    initial_messages: Vec<Message>,
    tools: &[Tool],
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
                // what each tool returned.
                for call in calls {
                    let result = dispatch(&call.function.name, &call.function.arguments);
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
                    compactions = compactions.saturating_add(1);
                    compaction::compact(client, &mut messages, compactions)?;
                }

                // Loop back and call chat() again.
                continue;
            }
            "length" => {
                return Err(anyhow!(
                    "model returned finish_reason=length — context overflow. \
                     Compaction fires before the next call when prompt_tokens \
                     crosses DARKMUX_AGENT_COMPACT_THRESHOLD_TOKENS, but the \
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
