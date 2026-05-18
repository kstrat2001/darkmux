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
use std::time::Duration;

/// Default base URL the spike container talks to. Phase 4 will make this
/// configurable via env var or CLI arg.
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
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
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
}

/// One chat-completion response.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatResponse {
    pub id: String,
    pub choices: Vec<Choice>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Choice {
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
pub struct LmStudioClient {
    base_url: String,
    agent: ureq::Agent,
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
        }
    }

    /// POST a chat-completion and parse the response. Returns an error
    /// if the HTTP call fails, the response isn't JSON, or the JSON
    /// doesn't fit the expected shape.
    pub fn chat(&self, req: &ChatRequest) -> Result<ChatResponse> {
        let url = format!("{}/chat/completions", self.base_url);

        let resp = self
            .agent
            .post(&url)
            .set("content-type", "application/json")
            .send_json(serde_json::to_value(req)?)
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
}

impl Default for LmStudioClient {
    fn default() -> Self {
        Self::new()
    }
}
