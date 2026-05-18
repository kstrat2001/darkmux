//! The tool-call loop.
//!
//! Sends the conversation to LMStudio. If the model returns a
//! `tool_calls` finish_reason, dispatches each tool, appends results,
//! and re-sends. Loops until `stop`, or fails loudly on `length` /
//! unexpected outcomes.
//!
//! Deliberately omitted in Phase 2 (will be designed in Phase 6+ if the
//! spike validates):
//!
//! - No compaction. If a dispatch exceeds the model's `n_ctx` we fail
//!   loudly; the runtime doesn't quietly summarize-and-retry.
//! - No streaming. Single-shot completions only — the loop reads the
//!   whole response before deciding what to do.
//! - No retries on transient failures. A network blip aborts the loop.
//!   Operator-visible failure is the right answer for spike work.
//! - No turn budget cap. A model that ping-pongs in tool-calls forever
//!   would loop forever; Phase 4 adds a `max_turns` guard.

use anyhow::{anyhow, Result};

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

                // Loop back and call chat() again.
                continue;
            }
            "length" => {
                return Err(anyhow!(
                    "model returned finish_reason=length — context overflow. \
                     Phase 2 fails loudly here; Phase 6+ will add compaction."
                ));
            }
            other => {
                return Err(anyhow!(
                    "unexpected finish_reason: {other}. \
                     Phase 2 doesn't know how to handle this; aborting."
                ));
            }
        }
    }
}
