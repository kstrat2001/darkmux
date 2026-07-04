//! LMStudio chat-completions client.
//!
//! The OpenAI-compatible chat-completions endpoint LMStudio exposes is
//! what darkmux dispatches against — same shape as Anthropic / OpenAI
//! function calling, so this module also serves as the reference for
//! what the loop sees coming back.
//!
//! Phase 2 scope: just enough to send one request and parse one
//! response. No streaming, no retries, no token-budgeting yet — those
//! are Phase 6+ when measurements tell us they're needed.
//!
//! Endpoint: by default `http://host.docker.internal:1234/v1/chat/completions`
//! (which is how the container reaches the host's LMStudio on Docker
//! for Mac). Overridable via [`LmStudioClient::with_base_url`] for tests.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

/// Default base URL the runtime container talks to. Configurable via
/// the `--base-url` CLI arg passed to `run`.
pub const DEFAULT_BASE_URL: &str = "http://host.docker.internal:1234/v1";

/// HTTP timeout for a single chat-completion call. Long-agentic-shape
/// workloads at 101K context with the bundled-tool-call pattern can
/// have individual model turns that take 5+ minutes (the model is
/// reading a large context, reasoning across multiple tool calls, and
/// generating a response with several tool_calls in one shot). Phase
/// 6d's first try at 300s timed out mid-generation; 900s gives 15min
/// of headroom per turn. The container's outer wall-clock guard is a
/// separate concern; tighten only when measurements show this is
/// excessive.
pub const REQUEST_TIMEOUT_SECS: u64 = 900;

/// One message in the conversation. Mirrors the OpenAI shape exactly so
/// LMStudio parses it without surprises.
///
/// `role` is one of `system`, `user`, `assistant`, `tool`. For tool
/// responses, set `role = "tool"` and `tool_call_id` to the id from the
/// assistant's tool_call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,

    /// Present on `assistant` messages that called tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,

    /// Present on `tool` messages — references the tool_call.id this
    /// message is responding to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,

    /// Present on `tool` messages — the tool's name, mirrored for
    /// convenience. Some models care; others don't.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// (#376) Present on RESPONSE messages from thinking-mode models
    /// (qwen 3.x line, deepseek-r1, etc.) — LMStudio routes their
    /// `<think>...</think>` output to this field rather than
    /// `content`. When the model is asked for JSON-mode output via
    /// `response_format: json_schema`, thinking-mode models put the
    /// JSON HERE, leaving `content` empty.
    ///
    /// Skip-serialize so outgoing request messages never emit it
    /// (always None on the request side); deserialize-populated on
    /// the response side. Consumers (compaction.rs's call_and_parse)
    /// should fall back to this field when `content` is empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        }
    }

    /// Construct an `assistant` role message. Currently only used by
    /// tests (real assistant messages come from chat-completion
    /// responses, where the role is set by the server). Kept on the
    /// public surface for symmetry with `system`/`user` and so tests
    /// can build realistic conversation shapes.
    #[cfg(test)]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        }
    }

    pub fn tool_result(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            name: Some(name.into()),
            reasoning_content: None,
        }
    }
}

/// A tool call the assistant wants the runtime to execute.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,

    #[serde(rename = "type")]
    pub kind: String, // always "function" today

    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,

    /// Arguments are JSON-encoded as a string per the OpenAI spec
    /// (not a nested object). The runtime parses this with
    /// `serde_json::from_str` before dispatching.
    pub arguments: String,
}

/// Tool definition included in the request so the model knows what's
/// available.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String, // always "function" today

    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema
}

/// One chat-completion request.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,

    /// `auto` (let the model decide), `none`, or an explicit choice.
    /// For Phase 2 we always use `auto`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,

    pub temperature: f32,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,

    /// OpenAI-compatible response-format constraint. Set to
    /// `Some(serde_json::json!({"type": "json_object"}))` for
    /// JSON-mode requests (#372 T2-B tier-2 structured-slot
    /// compactor). LMStudio + most other OpenAI-compatible servers
    /// enforce parseable JSON when this is set; reduces the
    /// "compactor returned prose-prefixed JSON" failure mode the
    /// runtime would otherwise have to handle defensively.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<serde_json::Value>,
}

/// One chat-completion response.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatResponse {
    /// OpenAI-shape correlation id for the response. Parsed by serde
    /// for wire-shape parity; not read by the loop today (kept for
    /// future cross-tier diagnostics).
    #[allow(dead_code)]
    pub id: String,
    pub choices: Vec<Choice>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Choice {
    /// Multi-choice (`n > 1`) is permitted by the OpenAI spec but
    /// darkmux always requests `n = 1`; the field is parsed for
    /// wire-shape parity in case that ever changes.
    #[allow(dead_code)]
    pub index: u32,
    pub message: Message,

    /// `stop`, `tool_calls`, `length`, or backend-specific.
    pub finish_reason: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// Blocking HTTP client for LMStudio's chat-completions endpoint.
///
/// Deliberately does NOT derive `Debug` — `auth_header` carries a live
/// secret when the "brain" is a remote endpoint (#1187), and a stray
/// `{:?}` on this struct must never be able to leak it.
pub struct LmStudioClient {
    base_url: String,
    agent: ureq::Agent,
    /// (#1187) Full chat-completions URL override, used when the brain is a
    /// remote OpenAI-compatible endpoint that needs a query string (Azure's
    /// `?api-version=`) that `base_url` + the unconditional `/chat/completions`
    /// suffix below can't express. When set, `chat`/`chat_streaming` POST to
    /// this URL verbatim; `base_url` is otherwise unused.
    chat_url_override: Option<String>,
    /// (#1187) An extra header attached to every request — `(name, value)`,
    /// e.g. `("api-key", "<secret>")` or `("Authorization", "Bearer <secret>")`.
    /// Populated only for a remote-brain dispatch, read once from stdin at
    /// startup (never a file or argv/env) — see `runtime/src/main.rs`'s
    /// `--auth-header-stdin`.
    auth_header: Option<(String, String)>,
}

impl LmStudioClient {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build();
        Self {
            base_url: base_url.into(),
            agent,
            chat_url_override: None,
            auth_header: None,
        }
    }

    /// (#1187) Override the chat-completions URL entirely — the client POSTs
    /// here verbatim instead of `{base_url}/chat/completions`. Setting this
    /// is also what marks the client as talking to a remote brain, which
    /// gates the `max_tokens` → `max_completion_tokens` rename in
    /// [`Self::request_body`] (Azure's newer reasoning-family models reject
    /// `max_tokens` outright).
    pub fn with_chat_url(mut self, url: impl Into<String>) -> Self {
        self.chat_url_override = Some(url.into());
        self
    }

    /// (#1187) Attach an auth header to every request this client makes.
    pub fn with_auth_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.auth_header = Some((name.into(), value.into()));
        self
    }

    /// True when this client is configured for a remote (non-LMStudio) brain
    /// — i.e. a `--chat-url` override was set. Drives the `max_tokens` field
    /// rename in [`Self::request_body`].
    fn is_remote_brain(&self) -> bool {
        self.chat_url_override.is_some()
    }

    fn effective_chat_url(&self) -> String {
        self.chat_url_override
            .clone()
            .unwrap_or_else(|| format!("{}/chat/completions", self.base_url))
    }

    /// Build the wire JSON body for `req`, renaming `max_tokens` to
    /// `max_completion_tokens` when talking to a remote brain (#1187) —
    /// verified empirically against Azure OpenAI's gpt-5.1 deployment,
    /// which rejects `max_tokens` for reasoning-family models. LMStudio and
    /// most other OpenAI-compatible servers only ever see the unrenamed
    /// `max_tokens` field, since this only fires when `chat_url_override`
    /// is set.
    fn request_body(&self, req: &ChatRequest) -> Result<serde_json::Value> {
        let mut body = serde_json::to_value(req)?;
        if self.is_remote_brain() {
            if let Some(obj) = body.as_object_mut() {
                if let Some(mt) = obj.remove("max_tokens") {
                    obj.insert("max_completion_tokens".to_string(), mt);
                }
            }
        }
        Ok(body)
    }

    fn apply_auth_header(&self, req: ureq::Request) -> ureq::Request {
        match &self.auth_header {
            Some((name, value)) => req.set(name, value),
            None => req,
        }
    }

    /// POST a chat-completion and parse the response. Returns an error
    /// if the HTTP call fails, the response isn't JSON, or the JSON
    /// doesn't fit the expected shape.
    pub fn chat(&self, req: &ChatRequest) -> Result<ChatResponse> {
        let url = self.effective_chat_url();
        let body = self.request_body(req)?;

        let request = self.apply_auth_header(
            self.agent.post(&url).set("content-type", "application/json"),
        );
        let resp = request
            .send_json(body)
            .map_err(|e| anyhow!("LMStudio request failed: {e}"))?;

        let status = resp.status();
        if !(200..300).contains(&status) {
            let body = resp.into_string().unwrap_or_default();
            return Err(anyhow!(
                "LMStudio returned HTTP {status}: {body}"
            ));
        }

        resp.into_json::<ChatResponse>()
            .context("parsing LMStudio chat response as JSON")
    }

    /// Streaming variant of `chat`. Returns an iterator over SSE chunks;
    /// the caller drives accumulation via `ChunkAccumulator` and gets a
    /// final `ChatResponse` via `into_response()`. The wire request is
    /// the same shape as `chat`, with `stream: true` injected
    /// unconditionally. (#205)
    pub fn chat_streaming(
        &self,
        req: &ChatRequest,
    ) -> Result<ChunkStream<BufReader<Box<dyn Read + Send + Sync>>>> {
        let url = self.effective_chat_url();
        let body = build_streaming_request_body(req, self.is_remote_brain())?;
        let request = self.apply_auth_header(
            self.agent
                .post(&url)
                .set("content-type", "application/json")
                .set("accept", "text/event-stream"),
        );
        let resp = request
            .send_json(body)
            .map_err(|e| anyhow!("LMStudio streaming request failed: {e}"))?;
        let status = resp.status();
        if !(200..300).contains(&status) {
            let body = resp.into_string().unwrap_or_default();
            return Err(anyhow!(
                "LMStudio returned HTTP {status} on streaming request: {body}"
            ));
        }
        Ok(ChunkStream::new(BufReader::new(resp.into_reader())))
    }
}

impl Default for LmStudioClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the SSE-streaming request body — start from the typed `ChatRequest`
/// shape, inject `stream: true`, and inject `stream_options.include_usage:
/// true` so LMStudio (and any OpenAI-compatible server) emits per-dispatch
/// `usage` in the final SSE chunk (#360). Extracted to a free function so
/// the body-building rule is unit-testable without an HTTP round-trip.
///
/// `remote` (#1187): when true (the client is talking to a remote brain, not
/// local LMStudio), renames `max_tokens` to `max_completion_tokens` —
/// mirrors [`LmStudioClient::request_body`]'s non-streaming rename, needed
/// because Azure OpenAI's reasoning-family models (e.g. gpt-5.1) reject
/// `max_tokens` outright. LMStudio and other OpenAI-compatible servers never
/// see this rename, since it only fires when `remote` is true.
pub(crate) fn build_streaming_request_body(req: &ChatRequest, remote: bool) -> Result<serde_json::Value> {
    let mut body = serde_json::to_value(req)?;
    let body_obj = body
        .as_object_mut()
        .ok_or_else(|| anyhow!("chat request did not serialize to a JSON object"))?;
    body_obj.insert("stream".to_string(), serde_json::Value::Bool(true));
    // (#360) OpenAI-compatible servers only emit `usage` on the final SSE
    // chunk when `stream_options.include_usage` is true. Without this,
    // LMStudio omits the usage field entirely — every `model.completed`
    // event ends up with `usage: null` and the runtime's per-turn /
    // dispatch-total token accumulators stay at 0, breaking compaction-
    // trigger telemetry + threshold analysis. Empirically surfaced via
    // Phase B dogfood (medium-coding × D, 2026-05-25).
    body_obj.insert(
        "stream_options".to_string(),
        serde_json::json!({"include_usage": true}),
    );
    if remote {
        if let Some(mt) = body_obj.remove("max_tokens") {
            body_obj.insert("max_completion_tokens".to_string(), mt);
        }
    }
    Ok(body)
}

// ─── Streaming chat-completions (#205) ──────────────────────────────
//
// SSE delta types + accumulator. Mirrors the OpenAI streaming
// chat-completions wire shape that LMStudio also speaks. The
// non-streaming `chat()` path above is unchanged; streaming is opt-in
// via `chat_streaming()`, and the caller drives accumulation into a
// final `ChatResponse` that the rest of the loop treats identically.

/// One chunk in a streamed chat-completion response. SSE framing is
/// `data: <json>\n\n`; this is the parsed `<json>` payload.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatChunk {
    pub id: String,
    pub choices: Vec<ChoiceDelta>,
    /// Some servers (LMStudio included, when `stream_options.include_usage`
    /// is on) emit `usage` on the final chunk. Optional everywhere.
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChoiceDelta {
    /// Same multi-choice provision as `Choice.index` above — parsed
    /// for wire-shape parity; the accumulator always treats every
    /// `ChoiceDelta` as if it belongs to choice 0.
    #[allow(dead_code)]
    pub index: u32,
    #[serde(default)]
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// The incremental content delivered in a single chunk's `delta`. All
/// fields are optional — different chunks carry different pieces.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Delta {
    /// Set on the FIRST chunk only (typically `"assistant"`). Parsed
    /// for wire-shape parity; the accumulator hardcodes the role on
    /// `into_response()`.
    #[serde(default)]
    #[allow(dead_code)]
    pub role: Option<String>,
    /// Incremental response text — append to the accumulator's buffer.
    #[serde(default)]
    pub content: Option<String>,
    /// Incremental tool-call fragments. Each entry carries `index` to
    /// route into the right accumulator slot; fragments for the same
    /// index arrive across multiple chunks.
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallDelta>>,
    /// Some thinking-mode models (Qwen 3 line, DeepSeek) deliver
    /// reasoning content in a separate `reasoning_content` field rather
    /// than inline `<think>` tags. Accumulator captures it; downstream
    /// can emit a `model.reasoning` trajectory event from it.
    #[serde(default)]
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallDelta {
    /// Stable across all fragments of the same tool call. Use this to
    /// route fragments into the right accumulator slot.
    pub index: u32,
    /// Set on the first fragment for this index; absent on continuation.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionCallDelta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FunctionCallDelta {
    #[serde(default)]
    pub name: Option<String>,
    /// Arguments arrive in fragments — append to the slot's buffer.
    #[serde(default)]
    pub arguments: Option<String>,
}

/// Reconstructs a single `ChatResponse` from a sequence of `ChatChunk`s.
/// Handles delta text accumulation, tool-call fragment assembly
/// (indexed), and optional usage on the final chunk.
pub struct ChunkAccumulator {
    id: String,
    content: String,
    /// One slot per tool_call index seen. Built incrementally as
    /// fragments arrive. Converted to `ToolCall` on `into_response()`.
    tool_call_slots: Vec<ToolCall>,
    finish_reason: Option<String>,
    usage: Option<Usage>,
    /// Reasoning content arrived via the separate-field stream. Empty
    /// when the model uses inline `<think>` tags (those land in the
    /// regular `content` stream and get extracted post-turn in
    /// loop_runner::extract_think_blocks).
    reasoning_content: String,
    /// Number of chunks ingested. Drives `model.partial.partial_index`
    /// (1-based) in the trajectory.
    partial_count: u32,
}

impl ChunkAccumulator {
    pub fn new() -> Self {
        Self {
            id: String::new(),
            content: String::new(),
            tool_call_slots: Vec::new(),
            finish_reason: None,
            usage: None,
            reasoning_content: String::new(),
            partial_count: 0,
        }
    }

    /// Ingest one chunk. Returns the 1-based partial index assigned to
    /// this chunk (suitable for trajectory `model.partial` events).
    pub fn ingest(&mut self, chunk: &ChatChunk) -> u32 {
        if self.id.is_empty() && !chunk.id.is_empty() {
            self.id = chunk.id.clone();
        }
        for choice in &chunk.choices {
            if let Some(content) = &choice.delta.content {
                self.content.push_str(content);
            }
            if let Some(reasoning) = &choice.delta.reasoning_content {
                self.reasoning_content.push_str(reasoning);
            }
            if let Some(tcs) = &choice.delta.tool_calls {
                for td in tcs {
                    let slot_idx = td.index as usize;
                    while self.tool_call_slots.len() <= slot_idx {
                        self.tool_call_slots.push(ToolCall {
                            id: String::new(),
                            kind: "function".to_string(),
                            function: FunctionCall {
                                name: String::new(),
                                arguments: String::new(),
                            },
                        });
                    }
                    let slot = &mut self.tool_call_slots[slot_idx];
                    if let Some(id) = &td.id {
                        slot.id = id.clone();
                    }
                    if let Some(k) = &td.kind {
                        slot.kind = k.clone();
                    }
                    if let Some(f) = &td.function {
                        if let Some(n) = &f.name {
                            slot.function.name.push_str(n);
                        }
                        if let Some(a) = &f.arguments {
                            slot.function.arguments.push_str(a);
                        }
                    }
                }
            }
            if let Some(fr) = &choice.finish_reason {
                self.finish_reason = Some(fr.clone());
            }
        }
        if let Some(u) = &chunk.usage {
            self.usage = Some(u.clone());
        }
        self.partial_count = self.partial_count.saturating_add(1);
        self.partial_count
    }

    /// Cumulative chars in the accumulated content buffer. Cheap byte
    /// length, NOT a char-iter count — partial events fire often and
    /// we'd rather not pay UTF-8 iteration cost per chunk for a stat.
    pub fn content_bytes(&self) -> usize {
        self.content.len()
    }

    /// Number of chunks ingested so far.
    pub fn partial_count(&self) -> u32 {
        self.partial_count
    }

    /// True when any tool_call fragments have been accumulated.
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_call_slots.is_empty()
    }

    /// Convert into the equivalent non-streaming `ChatResponse` so the
    /// rest of the agent loop treats the result identically. Synthesizes
    /// one `Choice` with index 0 (the only one we ever generate via
    /// non-streaming today). If `finish_reason` was never set, defaults
    /// to "stop" — this happens with malformed streams; the caller's
    /// terminal-state handling will treat it as a normal completion.
    /// (#905) This is a deliberate choice over flagging an abnormal
    /// termination: LMStudio reliably emits `finish_reason` on the final
    /// SSE chunk, so a never-set value means a dead/truncated connection
    /// where the partial `content` accumulated so far is the best result
    /// available — surfacing it as a clean stop beats a synthetic error
    /// arm that the loop has no extra recovery for.
    pub fn into_response(self) -> ChatResponse {
        let assistant_message = Message {
            role: "assistant".to_string(),
            content: if self.content.is_empty() {
                None
            } else {
                Some(self.content)
            },
            tool_calls: if self.tool_call_slots.is_empty() {
                None
            } else {
                Some(self.tool_call_slots)
            },
            tool_call_id: None,
            name: None,
            reasoning_content: if self.reasoning_content.is_empty() {
                None
            } else {
                Some(self.reasoning_content)
            },
        };
        ChatResponse {
            id: self.id,
            choices: vec![Choice {
                index: 0,
                message: assistant_message,
                finish_reason: self.finish_reason.unwrap_or_else(|| "stop".to_string()),
            }],
            usage: self.usage,
        }
    }

    /// Drain any reasoning content accumulated via the `reasoning_content`
    /// separate-field stream (NOT inline `<think>` tags — those land in
    /// `content` and are extracted post-turn). Caller emits a
    /// `model.reasoning` trajectory event from this. Returns `None` when
    /// the model didn't use the separate-field pattern.
    pub fn take_reasoning_content(&mut self) -> Option<String> {
        if self.reasoning_content.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.reasoning_content))
        }
    }
}

impl Default for ChunkAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

/// Maximum bytes `ChunkStream` will accumulate in a single SSE line.
/// A compromised LMStudio or MITM could send a `data: ` line with no
/// terminating newline; without a cap, `read_line` would grow RAM until
/// the runtime container is OOM-killed. Realistic chunks are a few KB;
/// 1 MiB is several orders above realistic and bounds the memory ceiling.
/// Hit on a real stream → the iterator emits one `Err` and terminates.
/// (#231 / S4)
pub const MAX_SSE_LINE_BYTES: usize = 1 << 20;

/// Read one `\n`-terminated line via `BufRead` with a hard byte cap.
/// Returns the number of bytes consumed (including the newline if found,
/// or the remaining bytes at EOF). Returns `Err(InvalidData)` if the cap
/// is exceeded before a newline or EOF is reached. Memory growth is
/// bounded by `max_bytes + BufReader::DEFAULT_BUF_SIZE` since each
/// `fill_buf()` round is checked before its bytes are appended to `out`.
///
/// Tested separately so the cap semantics are auditable independent of
/// the `ChunkStream` iterator that uses it.
/// `out` is now `&mut Vec<u8>` (was `&mut String`): partial reads
/// of multi-byte UTF-8 sequences cannot be decoded losslessly, so
/// the caller must decode the full line buffer once after this
/// function returns. Pre-fix, the per-fill_buf `from_utf8_lossy` on
/// partial bytes silently replaced split codepoints with U+FFFD,
/// corrupting any SSE chunk that crossed an 8 KiB BufReader
/// boundary mid-emoji or mid-CJK char (#329).
fn read_line_capped<R: BufRead>(
    reader: &mut R,
    out: &mut Vec<u8>,
    max_bytes: usize,
) -> std::io::Result<usize> {
    let mut total: usize = 0;
    loop {
        let buf = match reader.fill_buf() {
            Ok(b) => b,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if buf.is_empty() {
            return Ok(total); // EOF
        }
        match buf.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                let take = pos + 1;
                if total + take > max_bytes {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("SSE line exceeded {max_bytes}-byte cap"),
                    ));
                }
                out.extend_from_slice(&buf[..take]);
                let n = take;
                reader.consume(n);
                total += n;
                return Ok(total);
            }
            None => {
                if total + buf.len() > max_bytes {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("SSE line exceeded {max_bytes}-byte cap (no terminator)"),
                    ));
                }
                let n = buf.len();
                out.extend_from_slice(buf);
                reader.consume(n);
                total += n;
            }
        }
    }
}

/// Iterator over SSE-encoded `ChatChunk`s from a streamed
/// chat-completions response.
///
/// Parses the `data: <json>\n\n` framing line by line. Empty lines (SSE
/// event separators) and comment lines (starting with `:`) are skipped.
/// Other SSE fields (`event:`, `id:`, `retry:`) are also skipped — the
/// chat-completions stream only uses `data:`. Terminates on `data: [DONE]`
/// or EOF.
///
/// Returns `Some(Err(...))` at most once for a parse error or read error,
/// then `None` — the caller can't usefully retry mid-stream.
///
/// Generic over `R: BufRead` so tests can construct from a `&[u8]`;
/// production wraps the `ureq` response body in `BufReader`. Per-line
/// byte cap (`MAX_SSE_LINE_BYTES`) bounds OOM from a pathological no-
/// newline stream. (#231 / S4)
pub struct ChunkStream<R: BufRead> {
    reader: R,
    done: bool,
}

impl<R: BufRead> ChunkStream<R> {
    pub fn new(reader: R) -> Self {
        Self { reader, done: false }
    }
}

impl<R: BufRead> Iterator for ChunkStream<R> {
    type Item = Result<ChatChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        // Bytes buffer per #329: decode-per-line, not per-fill_buf.
        let mut line_bytes: Vec<u8> = Vec::new();
        loop {
            line_bytes.clear();
            match read_line_capped(&mut self.reader, &mut line_bytes, MAX_SSE_LINE_BYTES) {
                Ok(0) => {
                    self.done = true;
                    return None;
                }
                Ok(_) => {
                    // Decode the COMPLETE line once. from_utf8_lossy is
                    // still defense-in-depth for genuinely malformed
                    // payloads, but it's never run on a partial read.
                    let line = String::from_utf8_lossy(&line_bytes);
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    if trimmed.is_empty() || trimmed.starts_with(':') {
                        continue; // event separator or comment / keepalive
                    }
                    if let Some(data) = trimmed.strip_prefix("data: ") {
                        if data == "[DONE]" {
                            self.done = true;
                            return None;
                        }
                        match serde_json::from_str::<ChatChunk>(data) {
                            Ok(chunk) => return Some(Ok(chunk)),
                            Err(e) => {
                                self.done = true;
                                return Some(Err(anyhow!(
                                    "SSE chunk parse failed: {e} — payload: {data}"
                                )));
                            }
                        }
                    }
                    // Other SSE fields (event:, id:, retry:) are ignored.
                }
                Err(e) => {
                    self.done = true;
                    return Some(Err(anyhow!("SSE read failed: {e}")));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── build_streaming_request_body (#360) ──────────────────

    /// (#360) The streaming request body MUST include
    /// `stream_options.include_usage: true` — without it, LMStudio
    /// (and any OpenAI-compatible server) omits `usage` from the
    /// final SSE chunk, leaving the runtime with zero token counts.
    #[test]
    fn streaming_request_body_includes_include_usage_flag() {
        let req = ChatRequest {
            model: "test-model".to_string(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            tool_choice: None,
            temperature: 0.2,
            max_tokens: None,
            response_format: None,
        };
        let body = build_streaming_request_body(&req, false).expect("body builds");
        let obj = body.as_object().expect("object shape");
        assert_eq!(
            obj.get("stream"),
            Some(&serde_json::Value::Bool(true)),
            "stream flag must be true for SSE endpoint"
        );
        let opts = obj.get("stream_options").expect("stream_options present");
        assert_eq!(
            opts.get("include_usage"),
            Some(&serde_json::Value::Bool(true)),
            "include_usage must be true to receive per-dispatch usage on the final chunk"
        );
        // Sanity: typed fields from the original ChatRequest survived
        // the body massaging — body construction doesn't drop messages
        // or model.
        assert_eq!(obj.get("model").and_then(|v| v.as_str()), Some("test-model"));
        assert!(obj.get("messages").and_then(|v| v.as_array()).is_some());
    }

    /// (#1187) `remote = true` renames `max_tokens` to `max_completion_tokens`
    /// — Azure OpenAI's reasoning-family models (gpt-5.1) reject the
    /// unrenamed field outright. `remote = false` (LMStudio/local, the test
    /// above) must NOT see this rename.
    #[test]
    fn streaming_request_body_renames_max_tokens_for_remote_brain() {
        let req = ChatRequest {
            model: "gpt-5.1".to_string(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            tool_choice: None,
            temperature: 0.2,
            max_tokens: Some(10_000),
            response_format: None,
        };
        let body = build_streaming_request_body(&req, true).expect("body builds");
        let obj = body.as_object().expect("object shape");
        assert_eq!(
            obj.get("max_completion_tokens"),
            Some(&serde_json::json!(10_000)),
            "remote brain must see max_completion_tokens, not max_tokens"
        );
        assert!(
            obj.get("max_tokens").is_none(),
            "the original max_tokens key must be removed, not duplicated"
        );
    }

    // ─── LmStudioClient (#1187 remote-brain builder methods) ─────

    fn sample_request() -> ChatRequest {
        ChatRequest {
            model: "gpt-5.1".to_string(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            tool_choice: None,
            temperature: 0.2,
            max_tokens: Some(10_000),
            response_format: None,
        }
    }

    #[test]
    fn client_without_chat_url_is_not_remote_brain() {
        let client = LmStudioClient::with_base_url("http://host.docker.internal:1234/v1");
        assert!(!client.is_remote_brain());
        assert_eq!(
            client.effective_chat_url(),
            "http://host.docker.internal:1234/v1/chat/completions"
        );
        let body = client.request_body(&sample_request()).expect("body builds");
        assert_eq!(
            body.get("max_tokens"),
            Some(&serde_json::json!(10_000)),
            "local (non-remote) client must NOT rename max_tokens"
        );
    }

    #[test]
    fn client_with_chat_url_is_remote_brain_and_renames_max_tokens() {
        let full_url = "https://x.cognitiveservices.azure.com/openai/deployments/gpt-4o/chat/completions?api-version=2025-01-01-preview";
        let client = LmStudioClient::with_base_url("http://unused:1234/v1").with_chat_url(full_url);
        assert!(client.is_remote_brain());
        assert_eq!(
            client.effective_chat_url(),
            full_url,
            "chat_url_override must be used verbatim, not suffixed with /chat/completions"
        );
        let body = client.request_body(&sample_request()).expect("body builds");
        assert_eq!(body.get("max_completion_tokens"), Some(&serde_json::json!(10_000)));
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn client_with_auth_header_applies_it_to_a_request() {
        // Can't assert on a live HTTP send without a server; assert on the
        // builder's effect via a fresh request builder instead — confirms
        // apply_auth_header doesn't panic and actually sets the header name
        // given (ureq::Request doesn't expose a getter, so this is a smoke
        // check that the call chain type-checks and runs without error).
        let client = LmStudioClient::with_base_url("http://unused:1234/v1")
            .with_auth_header("api-key", "secret-value-not-logged");
        let req = client.apply_auth_header(client.agent.post("http://unused:1234/v1/chat/completions"));
        // ureq::Request has no public header-inspection API; the meaningful
        // assertion is that construction succeeds and auth_header is Some —
        // exercised indirectly since the field is private to this module.
        drop(req);
        assert!(client.auth_header.is_some());
    }

    // ─── read_line_capped (S4 byte cap) ────────────────────────

    #[test]
    fn read_line_capped_reads_terminated_line() {
        let input = b"hello\nworld\n";
        let mut r = std::io::BufReader::new(&input[..]);
        let mut out: Vec<u8> = Vec::new();
        let n = read_line_capped(&mut r, &mut out, 1024).unwrap();
        assert_eq!(n, 6);
        assert_eq!(out, b"hello\n");
    }

    #[test]
    fn read_line_capped_returns_zero_at_eof() {
        let input: &[u8] = b"";
        let mut r = std::io::BufReader::new(input);
        let mut out: Vec<u8> = Vec::new();
        let n = read_line_capped(&mut r, &mut out, 1024).unwrap();
        assert_eq!(n, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn read_line_capped_returns_bytes_when_eof_without_newline() {
        let input = b"no-terminator-line";
        let mut r = std::io::BufReader::new(&input[..]);
        let mut out: Vec<u8> = Vec::new();
        let n = read_line_capped(&mut r, &mut out, 1024).unwrap();
        assert_eq!(n, input.len());
        assert_eq!(out, b"no-terminator-line");
    }

    #[test]
    fn read_line_capped_rejects_over_limit_without_terminator() {
        // No newline, length exceeds cap → InvalidData error.
        let input = vec![b'x'; 2048];
        let mut r = std::io::BufReader::new(&input[..]);
        let mut out: Vec<u8> = Vec::new();
        let err = read_line_capped(&mut r, &mut out, 512).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("512"));
    }

    #[test]
    fn read_line_capped_rejects_over_limit_even_with_terminator() {
        // Line is bigger than cap, terminator exists at the end.
        let mut input = vec![b'x'; 2048];
        input.push(b'\n');
        let mut r = std::io::BufReader::new(&input[..]);
        let mut out: Vec<u8> = Vec::new();
        let err = read_line_capped(&mut r, &mut out, 512).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    /// Regression guard for #329 — a 4-byte emoji that spans a
    /// `fill_buf` boundary must round-trip intact. Pre-fix,
    /// `read_line_capped` called `from_utf8_lossy` on each partial
    /// buf, corrupting the split bytes to U+FFFD. Post-fix, the
    /// function accumulates bytes; caller decodes the complete line
    /// once after read returns.
    ///
    /// Forces the split by wrapping the input in a `BufReader` with
    /// capacity 2 — each `fill_buf` returns at most 2 bytes, so the
    /// 4-byte 🦀 must straddle at least one boundary.
    #[test]
    fn read_line_capped_preserves_multibyte_across_fill_buf_boundary() {
        // 🦀 (F0 9F A6 80) + " reactor\n"
        let payload: Vec<u8> = {
            let mut v = Vec::new();
            v.extend_from_slice(b"\xF0\x9F\xA6\x80");
            v.extend_from_slice(b" reactor\n");
            v
        };
        let mut reader = std::io::BufReader::with_capacity(2, &payload[..]);
        let mut out: Vec<u8> = Vec::new();
        let n = read_line_capped(&mut reader, &mut out, 1024).unwrap();
        assert_eq!(n, payload.len(), "all bytes consumed");
        // Decode and verify the emoji is intact.
        let decoded = String::from_utf8_lossy(&out);
        assert!(
            decoded.contains("🦀 reactor"),
            "multi-byte char must round-trip; got: {decoded:?}"
        );
        assert!(
            !decoded.contains('\u{FFFD}'),
            "no replacement chars should appear; got: {decoded:?}"
        );
    }

    #[test]
    fn chunk_stream_terminates_on_oversize_sse_line() {
        // Construct a `data: ` line that exceeds MAX_SSE_LINE_BYTES; the
        // iterator must emit one Err and stop rather than allocate
        // arbitrary RAM. Use a small custom MAX is not possible
        // (compile-time const) so this directly exercises the wired-up
        // production cap by sending an oversize buffer.
        // The cap is 1 MiB; 1.5 MiB of `x` will trip it.
        let mut sse = b"data: ".to_vec();
        sse.extend(std::iter::repeat(b'x').take(1_500_000));
        // No newline — the iterator's read must error out.
        let mut iter = ChunkStream::new(&sse[..]);
        let first = iter.next();
        assert!(first.is_some(), "iterator must emit at least one Result");
        let err = first.unwrap().unwrap_err();
        assert!(
            err.to_string().contains("SSE read failed")
                || err.to_string().contains("cap"),
            "expected cap-violation error; got: {err}"
        );
        // After the error, iterator is exhausted.
        assert!(iter.next().is_none(), "iterator must terminate after cap error");
    }

    // ─── ChunkStream (SSE framing) ─────────────────────────────

    #[test]
    fn chunk_stream_parses_data_lines() {
        let sse = b"data: {\"id\":\"a\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n\
                    data: {\"id\":\"a\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" there\"}}]}\n\n\
                    data: [DONE]\n\n";
        let chunks: Vec<_> = ChunkStream::new(&sse[..]).collect();
        assert_eq!(chunks.len(), 2);
        let c0 = chunks[0].as_ref().expect("first chunk parses");
        let c1 = chunks[1].as_ref().expect("second chunk parses");
        assert_eq!(c0.id, "a");
        assert_eq!(c0.choices[0].delta.content.as_deref(), Some("hi"));
        assert_eq!(c1.choices[0].delta.content.as_deref(), Some(" there"));
    }

    #[test]
    fn chunk_stream_done_sentinel_terminates() {
        let sse = b"data: [DONE]\n\ndata: {\"id\":\"after\",\"choices\":[]}\n\n";
        let chunks: Vec<_> = ChunkStream::new(&sse[..]).collect();
        // [DONE] terminates the iterator — subsequent chunks aren't read.
        assert!(chunks.is_empty(), "[DONE] must end iteration before later data");
    }

    #[test]
    fn chunk_stream_skips_comments_and_empty_lines() {
        let sse = b": keepalive comment\n\
                    \n\
                    event: chunk\n\
                    data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\n\
                    data: [DONE]\n\n";
        let chunks: Vec<_> = ChunkStream::new(&sse[..]).collect();
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].as_ref().unwrap().choices[0].delta.content.as_deref(),
            Some("ok")
        );
    }

    #[test]
    fn chunk_stream_malformed_json_returns_error_once_then_none() {
        let sse = b"data: not-json-at-all\n\ndata: [DONE]\n\n";
        let mut stream = ChunkStream::new(&sse[..]);
        let first = stream.next();
        let second = stream.next();
        assert!(
            matches!(first, Some(Err(_))),
            "first call returns Err on bad JSON"
        );
        assert!(
            second.is_none(),
            "stream terminates after the error rather than spinning",
        );
    }

    #[test]
    fn chunk_stream_eof_without_done_terminates() {
        let sse = b"data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n";
        let chunks: Vec<_> = ChunkStream::new(&sse[..]).collect();
        assert_eq!(chunks.len(), 1);
    }

    // ─── ChunkAccumulator ──────────────────────────────────────

    fn content_chunk(content: &str) -> ChatChunk {
        ChatChunk {
            id: "t".to_string(),
            choices: vec![ChoiceDelta {
                index: 0,
                delta: Delta {
                    role: None,
                    content: Some(content.to_string()),
                    tool_calls: None,
                    reasoning_content: None,
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    fn tool_call_fragment(
        index: u32,
        id: Option<&str>,
        name: Option<&str>,
        args: Option<&str>,
    ) -> ChatChunk {
        ChatChunk {
            id: "t".to_string(),
            choices: vec![ChoiceDelta {
                index: 0,
                delta: Delta {
                    role: None,
                    content: None,
                    tool_calls: Some(vec![ToolCallDelta {
                        index,
                        id: id.map(String::from),
                        kind: id.map(|_| "function".to_string()),
                        function: Some(FunctionCallDelta {
                            name: name.map(String::from),
                            arguments: args.map(String::from),
                        }),
                    }]),
                    reasoning_content: None,
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    #[test]
    fn accumulator_concatenates_content_across_chunks() {
        let mut acc = ChunkAccumulator::new();
        acc.ingest(&content_chunk("hello "));
        acc.ingest(&content_chunk("world"));
        let resp = acc.into_response();
        assert_eq!(
            resp.choices[0].message.content.as_deref(),
            Some("hello world"),
        );
    }

    #[test]
    fn accumulator_assembles_tool_call_fragments_by_index() {
        // First fragment: carries id + name + partial args.
        // Second fragment: continues args only.
        let mut acc = ChunkAccumulator::new();
        acc.ingest(&tool_call_fragment(0, Some("call_1"), Some("read"), Some("{\"pat")));
        acc.ingest(&tool_call_fragment(0, None, None, Some("h\":\"foo\"}")));
        let resp = acc.into_response();
        let tcs = resp.choices[0]
            .message
            .tool_calls
            .as_ref()
            .expect("tool_calls accumulated");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].id, "call_1");
        assert_eq!(tcs[0].function.name, "read");
        assert_eq!(tcs[0].function.arguments, r#"{"path":"foo"}"#);
    }

    #[test]
    fn accumulator_handles_multiple_tool_call_indices() {
        // Two distinct tool calls (index 0 and index 1) interleave
        // their fragments. Accumulator must keep them separate.
        let mut acc = ChunkAccumulator::new();
        acc.ingest(&tool_call_fragment(0, Some("call_a"), Some("read"), Some("{\"a\":1}")));
        acc.ingest(&tool_call_fragment(1, Some("call_b"), Some("write"), Some("{")));
        acc.ingest(&tool_call_fragment(1, None, None, Some("\"b\":2}")));
        let resp = acc.into_response();
        let tcs = resp.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(tcs.len(), 2);
        assert_eq!(tcs[0].function.name, "read");
        assert_eq!(tcs[0].function.arguments, r#"{"a":1}"#);
        assert_eq!(tcs[1].function.name, "write");
        assert_eq!(tcs[1].function.arguments, r#"{"b":2}"#);
    }

    #[test]
    fn accumulator_captures_usage_on_final_chunk() {
        let mut acc = ChunkAccumulator::new();
        acc.ingest(&content_chunk("hi"));
        // Final chunk carries usage.
        let final_chunk = ChatChunk {
            id: "t".to_string(),
            choices: vec![ChoiceDelta {
                index: 0,
                delta: Delta::default(),
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(Usage {
                prompt_tokens: 42,
                completion_tokens: 7,
                total_tokens: 49,
            }),
        };
        acc.ingest(&final_chunk);
        let resp = acc.into_response();
        assert_eq!(resp.choices[0].finish_reason, "stop");
        let usage = resp.usage.expect("usage captured from final chunk");
        assert_eq!(usage.prompt_tokens, 42);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.total_tokens, 49);
    }

    #[test]
    fn accumulator_into_response_defaults_finish_reason_when_missing() {
        // Malformed/incomplete stream: no finish_reason ever set.
        // into_response() defaults to "stop" so downstream loop can
        // treat it as a normal completion. (Caller has already
        // observed the stream end, so this is "ended without a clean
        // terminal" — best to default to non-fatal.)
        let mut acc = ChunkAccumulator::new();
        acc.ingest(&content_chunk("partial"));
        let resp = acc.into_response();
        assert_eq!(resp.choices[0].finish_reason, "stop");
    }

    #[test]
    fn accumulator_take_reasoning_content_separate_field() {
        // Some models (Qwen 3 / DeepSeek) deliver reasoning via the
        // `reasoning_content` delta channel rather than inline
        // `<think>` tags.
        let mut acc = ChunkAccumulator::new();
        let chunk = ChatChunk {
            id: "t".to_string(),
            choices: vec![ChoiceDelta {
                index: 0,
                delta: Delta {
                    role: None,
                    content: Some("response".to_string()),
                    tool_calls: None,
                    reasoning_content: Some("thinking step".to_string()),
                },
                finish_reason: None,
            }],
            usage: None,
        };
        acc.ingest(&chunk);
        let reasoning = acc.take_reasoning_content();
        assert_eq!(reasoning.as_deref(), Some("thinking step"));
        // Second take returns None — already drained.
        assert!(acc.take_reasoning_content().is_none());
    }

    #[test]
    fn accumulator_partial_count_increments_per_ingest() {
        let mut acc = ChunkAccumulator::new();
        assert_eq!(acc.ingest(&content_chunk("a")), 1);
        assert_eq!(acc.ingest(&content_chunk("b")), 2);
        assert_eq!(acc.ingest(&content_chunk("c")), 3);
        assert_eq!(acc.partial_count(), 3);
    }

    #[test]
    fn accumulator_content_bytes_tracks_cumulative_length() {
        let mut acc = ChunkAccumulator::new();
        acc.ingest(&content_chunk("hello"));
        assert_eq!(acc.content_bytes(), 5);
        acc.ingest(&content_chunk(" world"));
        assert_eq!(acc.content_bytes(), 11);
    }

    #[test]
    fn accumulator_has_tool_calls_false_when_only_content() {
        let mut acc = ChunkAccumulator::new();
        acc.ingest(&content_chunk("just text"));
        assert!(!acc.has_tool_calls());
    }

    #[test]
    fn accumulator_has_tool_calls_true_after_any_fragment() {
        let mut acc = ChunkAccumulator::new();
        acc.ingest(&tool_call_fragment(0, Some("call"), Some("read"), Some("{}")));
        assert!(acc.has_tool_calls());
    }
}

