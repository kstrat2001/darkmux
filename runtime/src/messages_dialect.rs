//! (#3162) The `messages` dialect: Anthropic's native Messages API, spoken at
//! the client boundary so everything above it (the loop, the accumulator, the
//! trajectory) keeps reading the chat-completions shapes it already knows.
//!
//! Why it exists: the OpenAI-compatible layer at `api.anthropic.com/v1`
//! neither caches nor reports caching, even when asked (probed 2026-10-10 on
//! #3162: the same 17.6k-token prefix billed in full twice). The agent loop
//! resends its whole conversation every turn, so without caching a review
//! paid full input price for the same prefix again and again. The native API
//! takes a top-level `cache_control`, which places the breakpoint on the last
//! cacheable block and moves it forward as the conversation grows: exactly
//! the multi-turn case.
//!
//! The translation keeps the model-facing text of every message (contract 6);
//! the envelope changes, plus two documented exceptions below (a malformed
//! tool call's arguments, and the note after a cut turn):
//!
//! - Leading `system` messages become the top-level `system`. A `system`
//!   message later in the conversation (the runtime's `[darkmux-runtime]`
//!   feedback notes) becomes a user text block with the same text, in place,
//!   so the cached prefix ahead of it stays stable.
//! - A conversation ending on an assistant turn (the loop's checkpoint
//!   prefill) gets [`CONTINUE_CUT_TURN`] after it: Claude refuses a prefill.
//! - Tool-call arguments that are not a JSON object replay wrapped as
//!   `{"arguments": "<text>"}`, since `tool_use.input` must be an object.
//! - An assistant turn becomes `text` and `tool_use` blocks; a `tool` message
//!   becomes a `tool_result` block in a user turn. Consecutive turns of one
//!   role merge into one turn, as the Messages API requires alternation.
//! - `max_tokens` is the cap field; no `temperature` (Claude 5.x refuses a
//!   sampling value); `response_format` has no Messages equivalent and is
//!   not sent.
//! - Usage maps onto the OpenAI-shaped counts: prompt = uncached input +
//!   cache writes + cache reads, `cached` = cache reads. A budget counts every
//!   token the endpoint served, cached or not.

use crate::lmstudio::{
    ChatChunk, ChatRequest, ChatResponse, Choice, ChoiceDelta, Delta, FunctionCall, FunctionCallDelta,
    Message, ToolCall, ToolCallDelta,
};
use anyhow::{anyhow, Result};
use darkmux_trajectory::UsageCounts;
use serde_json::{json, Map, Value};

/// (#3162 review) Claude models refuse a conversation that ends on an
/// assistant message ("This model does not support assistant message
/// prefill", HTTP 400, probed on Haiku and Sonnet 5.5). The loop's
/// checkpoint hands a cut turn back as exactly that trailing message, so in
/// this dialect the turn stays an assistant turn and this user note follows
/// it: the model continues the turn instead of restarting it.
pub const CONTINUE_CUT_TURN: &str = "[darkmux-runtime] Your previous turn was cut off at the output limit. \
Continue it exactly where it stopped, without repeating anything already written.";

/// The API version header the Messages endpoint requires on every request.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The request body for `req` in the Messages shape. `stream` adds
/// `"stream": true`. Fails only when the request carries no `max_tokens`:
/// the Messages API requires a cap and darkmux does not guess one.
pub fn request_body(req: &ChatRequest, stream: bool) -> Result<Value> {
    let max_tokens = req
        .max_tokens
        .ok_or_else(|| anyhow!("the messages dialect needs a max_tokens cap on every request"))?;
    let (system, messages) = conversation(&req.messages);
    let mut body = Map::new();
    body.insert("model".into(), req.model.clone().into());
    body.insert("max_tokens".into(), max_tokens.into());
    if !system.is_empty() {
        body.insert("system".into(), system.into());
    }
    body.insert("messages".into(), Value::Array(messages));
    if !req.tools.is_empty() {
        let tools = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.function.name,
                    "description": t.function.description,
                    "input_schema": t.function.parameters,
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
        if let Some(choice) = req.tool_choice.as_deref().and_then(tool_choice) {
            body.insert("tool_choice".into(), choice);
        }
    }
    body.insert("cache_control".into(), json!({ "type": "ephemeral" }));
    if stream {
        body.insert("stream".into(), true.into());
    }
    Ok(Value::Object(body))
}

/// `auto` and `none` carry over; any other choice is not expressible and is
/// left to the API's default (`auto`).
fn tool_choice(choice: &str) -> Option<Value> {
    match choice {
        "auto" | "none" => Some(json!({ "type": choice })),
        _ => None,
    }
}

/// The top-level system text and the alternating message list.
fn conversation(messages: &[Message]) -> (String, Vec<Value>) {
    let leading = messages.iter().take_while(|m| m.role == "system").count();
    let system = messages[..leading]
        .iter()
        .filter_map(|m| m.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut turns: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for m in &messages[leading..] {
        let (role, blocks) = blocks_of(m);
        if blocks.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some((last, existing)) if *last == role => existing.extend(blocks),
            _ => turns.push((role, blocks)),
        }
    }
    if turns.last().is_some_and(|(role, _)| *role == "assistant") {
        turns.push(("user", vec![text_block(CONTINUE_CUT_TURN)]));
    }
    let out = turns
        .into_iter()
        .map(|(role, content)| json!({ "role": role, "content": content }))
        .collect();
    (system, out)
}

/// One chat-completions message as a Messages role and its content blocks.
/// Empty text is dropped (the API refuses an empty text block).
fn blocks_of(m: &Message) -> (&'static str, Vec<Value>) {
    let text = m.content.as_deref().filter(|t| !t.is_empty());
    match m.role.as_str() {
        "assistant" => {
            let mut blocks: Vec<Value> = text.map(text_block).into_iter().collect();
            for call in m.tool_calls.iter().flatten() {
                blocks.push(json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.function.name,
                    "input": tool_input(&call.function.arguments),
                }));
            }
            ("assistant", blocks)
        }
        "tool" => {
            let block = json!({
                "type": "tool_result",
                "tool_use_id": m.tool_call_id.as_deref().unwrap_or_default(),
                "content": m.content.as_deref().unwrap_or_default(),
            });
            ("user", vec![block])
        }
        // `user`, and a `system` note after the conversation began.
        _ => ("user", text.map(text_block).into_iter().collect()),
    }
}

fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

/// A tool call's arguments as the object `tool_use.input` must be. Empty
/// arguments (a call with no parameters streams none) are `{}`. Arguments
/// that are not a JSON object (a model's malformed call, already answered by
/// the loop with an error result) are carried as `{"arguments": <the text>}`
/// so the history still replays.
fn tool_input(arguments: &str) -> Value {
    if arguments.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(arguments) {
        Ok(v @ Value::Object(_)) => v,
        _ => json!({ "arguments": arguments }),
    }
}

/// The chat-completions finish reason for a Messages `stop_reason`.
fn finish_reason(stop_reason: &str) -> String {
    match stop_reason {
        "end_turn" | "stop_sequence" => "stop",
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        // The loop's spelling for an answer the provider withheld.
        "refusal" => "content_filter",
        other => other,
    }
    .to_string()
}

/// A Messages `usage` object as the OpenAI-shaped counts: the one mapping
/// the host shares ([`UsageCounts::from_messages_usage`]).
pub fn usage_counts(usage: &Value) -> UsageCounts {
    UsageCounts::from_messages_usage(usage)
}

/// A non-streamed Messages reply as a [`ChatResponse`].
pub fn response(reply: &Value) -> Result<ChatResponse> {
    let blocks = reply
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("messages reply has no content array: {reply}"))?;
    let mut text = String::new();
    let mut calls = Vec::new();
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(b.get("text").and_then(Value::as_str).unwrap_or_default()),
            Some("tool_use") => calls.push(ToolCall {
                id: str_of(b, "id"),
                kind: "function".into(),
                function: FunctionCall {
                    name: str_of(b, "name"),
                    arguments: b.get("input").map(Value::to_string).unwrap_or_else(|| "{}".into()),
                },
                extra_content: None,
            }),
            _ => {}
        }
    }
    let message = Message {
        role: "assistant".into(),
        content: (!text.is_empty()).then_some(text),
        tool_calls: (!calls.is_empty()).then_some(calls),
        tool_call_id: None,
        name: None,
        reasoning_content: None,
    };
    Ok(ChatResponse {
        id: str_of(reply, "id"),
        model: reply.get("model").and_then(Value::as_str).map(str::to_string),
        choices: vec![Choice {
            index: 0,
            message,
            finish_reason: finish_reason(reply.get("stop_reason").and_then(Value::as_str).unwrap_or("end_turn")),
        }],
        usage: reply.get("usage").map(usage_counts),
    })
}

fn str_of(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// What one Messages stream event means to the chunk stream.
#[derive(Debug)]
pub enum Translated {
    /// A chunk for the accumulator.
    Chunk(ChatChunk),
    /// Nothing to pass on (`ping`, a block's start or stop with no content).
    Skip,
    /// `message_stop`: the stream is over.
    Done,
}

/// The per-stream state a Messages event stream needs to become chunks: the
/// message's id and model (sent once, on `message_start`), its input usage
/// (also sent there, while output usage arrives on `message_delta`), and
/// which content blocks are tool calls.
#[derive(Debug, Default)]
pub struct StreamTranslator {
    id: String,
    model: Option<String>,
    start_usage: Value,
    /// Content-block index -> tool-call slot, for blocks that are `tool_use`.
    tool_slots: Vec<(u64, u32)>,
}

impl StreamTranslator {
    pub fn translate(&mut self, event: &Value) -> Result<Translated> {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or_default();
        Ok(match kind {
            "message_start" => {
                let m = event.get("message").cloned().unwrap_or_default();
                self.id = str_of(&m, "id");
                self.model = m.get("model").and_then(Value::as_str).map(str::to_string);
                self.start_usage = m.get("usage").cloned().unwrap_or_default();
                // The input side is known now: a stream cut before
                // `message_delta` still reports the prompt it was served
                // (output stays unreported, so the total is unknown).
                let mut input = self.start_usage.clone();
                if let Some(obj) = input.as_object_mut() {
                    obj.remove("output_tokens");
                }
                Translated::Chunk(self.chunk(Delta::default(), None, Some(usage_counts(&input))))
            }
            "content_block_start" => self.block_start(event),
            "content_block_delta" => self.block_delta(event),
            "message_delta" => {
                let stop = event.pointer("/delta/stop_reason").and_then(Value::as_str).map(finish_reason);
                let mut usage = self.start_usage.clone();
                if let (Some(obj), Some(Value::Object(late))) = (usage.as_object_mut(), event.get("usage")) {
                    for (k, v) in late {
                        if !v.is_null() {
                            obj.insert(k.clone(), v.clone());
                        }
                    }
                }
                Translated::Chunk(self.chunk(Delta::default(), stop, Some(usage_counts(&usage))))
            }
            "message_stop" => Translated::Done,
            "error" => return Err(anyhow!("endpoint sent an error event mid-stream: {event}")),
            _ => Translated::Skip,
        })
    }

    fn block_start(&mut self, event: &Value) -> Translated {
        let block = event.get("content_block").cloned().unwrap_or_default();
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            return Translated::Skip;
        }
        let index = event.get("index").and_then(Value::as_u64).unwrap_or_default();
        let slot = self.tool_slots.len() as u32;
        self.tool_slots.push((index, slot));
        let call = ToolCallDelta {
            index: Some(slot),
            id: Some(str_of(&block, "id")),
            kind: Some("function".into()),
            function: Some(FunctionCallDelta { name: Some(str_of(&block, "name")), arguments: None }),
            extra_content: None,
        };
        Translated::Chunk(self.chunk(Delta { tool_calls: Some(vec![call]), ..Delta::default() }, None, None))
    }

    fn block_delta(&mut self, event: &Value) -> Translated {
        let index = event.get("index").and_then(Value::as_u64).unwrap_or_default();
        let d = event.get("delta").cloned().unwrap_or_default();
        let text = |k: &str| d.get(k).and_then(Value::as_str).map(str::to_string);
        let delta = match d.get("type").and_then(Value::as_str) {
            Some("text_delta") => Delta { content: text("text"), ..Delta::default() },
            Some("thinking_delta") => Delta { reasoning_content: text("thinking"), ..Delta::default() },
            Some("input_json_delta") => {
                let Some(slot) = self.tool_slots.iter().find(|(i, _)| *i == index).map(|(_, s)| *s) else {
                    return Translated::Skip;
                };
                let call = ToolCallDelta {
                    index: Some(slot),
                    id: None,
                    kind: None,
                    function: Some(FunctionCallDelta { name: None, arguments: text("partial_json") }),
                    extra_content: None,
                };
                Delta { tool_calls: Some(vec![call]), ..Delta::default() }
            }
            _ => return Translated::Skip,
        };
        Translated::Chunk(self.chunk(delta, None, None))
    }

    fn chunk(&self, delta: Delta, finish_reason: Option<String>, usage: Option<UsageCounts>) -> ChatChunk {
        ChatChunk {
            id: self.id.clone(),
            model: self.model.clone(),
            choices: vec![ChoiceDelta { index: 0, delta, finish_reason }],
            usage,
        }
    }
}

#[cfg(test)]
#[path = "messages_dialect_tests.rs"]
mod tests;
