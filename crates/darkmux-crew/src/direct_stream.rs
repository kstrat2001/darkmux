//! (#3211) The live evidence of a dispatch that runs host-side
//! (`dispatch_unmanaged`, `runtime=direct`): its request streamed, so the
//! fleet card's scope reads PROMPT, then GEN, then finished, the way a
//! container turn does, instead of PROMPT for the whole call.
//!
//! The transport stays the one curl call (`remote_chat_attempt`): its secret
//! handling, retries and status classification are unchanged. Streaming adds
//! `"stream": true` to a chat-completions body, and the caller reads the
//! response file while curl writes it ([`SseTail`]); when curl is done the
//! SSE body is put back together into the reply a non-streamed call returns
//! ([`assemble`]), so every reader of the reply is untouched. A Messages
//! dialect call is not streamed here (its events are another shape); it
//! still writes the turn's opening heartbeat and its `dispatch.turn`.

use darkmux_flow::payload::{DispatchHeartbeatPayload, DispatchTurnPayload, TurnUsage};
use serde_json::Value;

/// What has streamed so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SseProgress {
    /// When this attempt's request was sent: a retried call's model time
    /// starts at its last attempt, never at the first (backoff excluded).
    pub started: Option<std::time::Instant>,
    /// Answer text only (`delta.content`).
    pub content_chars: u64,
    /// Everything generated: answer and reasoning text.
    pub generated_chars: u64,
    /// `data:` events read, the end marker excluded.
    pub chunks: u64,
}

/// A chat-completions body asked to stream, with the usage block on its last
/// event (`stream_options.include_usage`), so the reply's counts survive.
pub(crate) fn stream_body(body: &Value) -> Value {
    let mut b = body.clone();
    if let Some(obj) = b.as_object_mut() {
        obj.insert("stream".into(), true.into());
        obj.insert("stream_options".into(), serde_json::json!({ "include_usage": true }));
    }
    b
}

/// One event's delta text: (answer, reasoning). Reasoning arrives as
/// `reasoning_content` (LM Studio, vLLM, DeepSeek) or `reasoning` (some
/// OpenAI-compatible servers); a server that sends both sends one text
/// twice, so the first present is read, never their sum.
fn delta_text(event: &Value) -> (&str, &str) {
    let delta = event.pointer("/choices/0/delta");
    let text = |key: &str| delta.and_then(|d| d.get(key)).and_then(Value::as_str);
    (text("content").unwrap_or(""), text("reasoning_content").or_else(|| text("reasoning")).unwrap_or(""))
}

/// The characters one event's delta adds: (answer, everything generated).
fn delta_chars(event: &Value) -> (u64, u64) {
    let (content, reasoning) = delta_text(event);
    let content = content.chars().count() as u64;
    (content, content + reasoning.chars().count() as u64)
}

/// Whether `text` is an SSE body: its first line is an SSE field (`data:`,
/// `event:`, `id:`, `retry:`) or a comment (`:`, which OpenRouter sends
/// before its events). A JSON body (an error, or a server that ignored
/// `stream`) is not.
fn is_sse(text: &str) -> bool {
    let first = text.lines().map(str::trim_start).find(|l| !l.is_empty()).unwrap_or("");
    ["data:", "event:", "id:", "retry:", ":"].iter().any(|p| first.starts_with(p))
}

/// Whether the stream said it was done: the `[DONE]` marker, or a choice's
/// `finish_reason`. A stream without either was cut off.
fn finished(text: &str) -> bool {
    text.lines().filter_map(|l| l.strip_prefix("data:")).any(|d| d.trim() == "[DONE]")
        || data_lines(text)
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .any(|e| e.pointer("/choices/0/finish_reason").is_some_and(|f| !f.is_null()))
}

/// The `data:` payloads of the complete lines in `text`, the end marker
/// (`[DONE]`) excluded.
fn data_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(str::trim)
        .filter(|d| !d.is_empty() && *d != "[DONE]")
}

/// Reads a growing SSE response a piece at a time, keeping a partial last
/// line for the next piece.
#[derive(Debug, Default)]
pub(crate) struct SseTail {
    pending: Vec<u8>,
    pub progress: SseProgress,
}

impl SseTail {
    /// Read `bytes`, the response's next piece; `true` when it completed at
    /// least one event.
    /// A tail for an attempt sent at `started`.
    pub(crate) fn sent_at(started: std::time::Instant) -> Self {
        SseTail { progress: SseProgress { started: Some(started), ..SseProgress::default() }, ..SseTail::default() }
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) -> bool {
        self.pending.extend_from_slice(bytes);
        let Some(end) = self.pending.iter().rposition(|b| *b == b'\n') else {
            return false;
        };
        let complete: Vec<u8> = self.pending.drain(..=end).collect();
        let before = self.progress.chunks;
        for data in data_lines(&String::from_utf8_lossy(&complete)) {
            let Ok(event) = serde_json::from_str::<Value>(data) else { continue };
            let (content, generated) = delta_chars(&event);
            self.progress.content_chars += content;
            self.progress.generated_chars += generated;
            self.progress.chunks += 1;
        }
        self.progress.chunks > before
    }
}

/// An SSE chat-completions response put back together as the reply a
/// non-streamed call returns: `choices[0].message` (content, and reasoning
/// when any streamed), its `finish_reason`, the `usage` block and `model`.
/// `None` when `body` is not an SSE response (an error body, or a server
/// that ignored `stream`): the caller reads it as it is. An error event in
/// the stream is returned as that error object, so the classification that
/// reads every reply finds it. `Some(Err)` when the stream was cut off
/// before it finished: a partial reply is never read as a whole one.
pub(crate) fn assemble(body: &[u8]) -> Option<Result<Vec<u8>, String>> {
    let text = String::from_utf8_lossy(body);
    if !is_sse(&text) {
        return None;
    }
    if let Some(err) = data_lines(&text).filter_map(|d| serde_json::from_str::<Value>(d).ok()).find(|e| e.get("error").is_some()) {
        return Some(Ok(err.to_string().into_bytes()));
    }
    if !finished(&text) {
        return Some(Err("the streamed reply ended with no [DONE] and no finish_reason".to_string()));
    }
    Some(Ok(reassemble(&text)))
}

/// The events of a finished stream as one reply.
fn reassemble(text: &str) -> Vec<u8> {
    let mut content = String::new();
    let mut reasoning = String::new();
    let (mut finish, mut usage, mut model) = (Value::Null, Value::Null, Value::Null);
    for data in data_lines(text) {
        let Ok(event) = serde_json::from_str::<Value>(data) else { continue };
        let (c, r) = delta_text(&event);
        content.push_str(c);
        reasoning.push_str(r);
        if let Some(f) = event.pointer("/choices/0/finish_reason").filter(|f| !f.is_null()) {
            finish = f.clone();
        }
        if let Some(u) = event.get("usage").filter(|u| !u.is_null()) {
            usage = u.clone();
        }
        if let Some(m) = event.get("model").filter(|m| !m.is_null()) {
            model = m.clone();
        }
    }
    let mut message = serde_json::json!({ "role": "assistant", "content": content });
    if !reasoning.is_empty() {
        message["reasoning_content"] = reasoning.into();
    }
    let mut reply = serde_json::json!({ "choices": [{ "message": message, "finish_reason": finish }] });
    if !usage.is_null() {
        reply["usage"] = usage;
    }
    if !model.is_null() {
        reply["model"] = model;
    }
    reply.to_string().into_bytes()
}

/// When a streamed call's next heartbeat is due: the first event at once
/// (it is the first proof the model is writing), then one per
/// `interval`, the container path's coalescing.
#[derive(Debug)]
pub(crate) struct HeartbeatGate {
    interval: std::time::Duration,
    last: Option<std::time::Instant>,
}

impl HeartbeatGate {
    pub(crate) fn new(interval: std::time::Duration) -> Self {
        HeartbeatGate { interval, last: None }
    }

    pub(crate) fn due(&mut self, now: std::time::Instant) -> bool {
        let due = self.last.is_none_or(|prev| now.duration_since(prev) >= self.interval);
        if due {
            self.last = Some(now);
        }
        due
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The one turn of a direct dispatch.
const TURN: u64 = 1;

/// The turn's opening heartbeat, before the request is sent: the request's
/// size (system and user text, as the container path counts it) and nothing
/// generated yet. The scope reads PROMPT from it.
pub(crate) fn opener(prompt_chars: u64) -> DispatchHeartbeatPayload {
    DispatchHeartbeatPayload {
        turn_seq: TURN,
        partial_index: None,
        cumulative_chars: 0,
        sampled_at_ms: Some(now_ms()),
        generated_chars: Some(0),
        prompt_chars: Some(prompt_chars),
        phase: None,
        tool_name: None,
        step_id: None,
        context: None,
    }
}

/// A heartbeat for what has streamed so far. The scope reads GEN from it.
pub(crate) fn chunk_beat(p: &SseProgress) -> DispatchHeartbeatPayload {
    DispatchHeartbeatPayload {
        partial_index: Some(p.chunks),
        cumulative_chars: p.content_chars,
        generated_chars: Some(p.generated_chars),
        prompt_chars: None,
        ..opener(0)
    }
}

/// The turn's record, once the reply is in: its finish reason as the reply
/// gives it, its usage from the reply's counts (`null` when it reported
/// none), and its model time when the reply streamed (absent otherwise, as
/// on a container turn with no stream).
pub(crate) fn turn(reply: &Value, counts: &darkmux_trajectory::UsageCounts, generation_ms: Option<u64>) -> DispatchTurnPayload {
    let finish = reply.pointer("/choices/0/finish_reason").and_then(Value::as_str).unwrap_or("unknown");
    let reported = [counts.prompt, counts.completion, counts.total, counts.reasoning, counts.cached];
    DispatchTurnPayload {
        turn_seq: TURN,
        finish_reason: finish.chars().take(64).collect(),
        tool_calls_count: 0,
        usage: reported.iter().any(Option::is_some).then(|| TurnUsage {
            prompt_tokens: counts.prompt,
            completion_tokens: counts.completion,
            total_tokens: counts.total,
            reasoning_tokens: counts.reasoning,
            cached_tokens: counts.cached,
        }),
        turns_so_far: Some(TURN),
        generation_ms,
        tool_paths: None,
        tool_names: None,
        step_id: None,
        context: None,
    }
}

/// A direct dispatch's turn records, written through `emit` (the caller's
/// record builder): the opener at [`TurnEvidence::open`], coalesced
/// heartbeats while a streamed reply arrives, and the turn's record at
/// [`TurnEvidence::ended`]. `streams` is whether the call streams: a
/// chat-completions dialect does, a Messages one does not (yet).
pub(crate) struct TurnEvidence<F: Fn(darkmux_flow::Payload)> {
    emit: F,
    gate: HeartbeatGate,
    pub streams: bool,
    /// When the attempt that streamed was sent (its model time's start).
    started: Option<std::time::Instant>,
}

impl<F: Fn(darkmux_flow::Payload)> TurnEvidence<F> {
    pub(crate) fn open(dialect: darkmux_types::Dialect, prompt_chars: u64, interval: std::time::Duration, emit: F) -> Self {
        emit(darkmux_flow::Payload::DispatchTurnHeartbeat(opener(prompt_chars)));
        TurnEvidence {
            emit,
            gate: HeartbeatGate::new(interval),
            streams: dialect != darkmux_types::Dialect::Messages,
            started: None,
        }
    }

    /// Send `plain`, the unstreamed request body, through `send`: asked to
    /// stream when this call streams, with the progress callback. An endpoint
    /// that refuses the streamed request itself (`refused_streaming`: an
    /// older API version rejecting `stream_options`) is asked once more,
    /// unstreamed, and the turn is then recorded as not streamed. Returns the
    /// reply and the body that was sent (what a charge is estimated from).
    pub(crate) fn send(
        &mut self,
        plain: Value,
        mut send: impl FnMut(&Value, Option<&mut dyn FnMut(&SseProgress)>) -> anyhow::Result<Value>,
        refused_streaming: impl Fn(&anyhow::Error) -> bool,
    ) -> (anyhow::Result<Value>, Value) {
        if !self.streams {
            return (send(&plain, None), plain);
        }
        let streamed = stream_body(&plain);
        let reply = self.with_progress(|on| send(&streamed, Some(on)));
        match reply {
            Err(e) if refused_streaming(&e) => {
                self.streams = false;
                (send(&plain, None), plain)
            }
            reply => (reply, streamed),
        }
    }

    /// Run `call` with the progress callback: heartbeats through the gate,
    /// and the streaming attempt's start noted for the model time.
    fn with_progress<R>(&mut self, call: impl FnOnce(&mut dyn FnMut(&SseProgress)) -> R) -> R {
        let (gate, emit, started) = (&mut self.gate, &self.emit, &mut self.started);
        let mut on = |p: &SseProgress| {
            *started = p.started.or(*started);
            if gate.due(std::time::Instant::now()) {
                emit(darkmux_flow::Payload::DispatchTurnHeartbeat(chunk_beat(p)));
            }
        };
        call(&mut on)
    }

    /// The reply is in: the turn's record. Its model time runs from the
    /// streaming attempt's send to now; absent when nothing streamed.
    pub(crate) fn ended(&self, reply: &Value, counts: &darkmux_trajectory::UsageCounts) {
        let generation_ms = self.started.filter(|_| self.streams).map(|t| t.elapsed().as_millis() as u64);
        (self.emit)(darkmux_flow::Payload::DispatchTurn(turn(reply, counts, generation_ms)));
    }
}

#[cfg(test)]
#[path = "direct_stream_tests.rs"]
mod tests;
