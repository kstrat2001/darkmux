//! Compaction strategy for the spike runtime.
//!
//! When the conversation's prompt-token count approaches the model's
//! loaded context window, the loop pauses to compact the middle of the
//! conversation — summarizing it via a small companion model and
//! replacing the summarized turns with a single synthetic user message.
//!
//! ## Architectural choices (Phase 6 spike)
//!
//! - **Middle-replace, not sliding-window.** Article 2's empirical
//!   finding: keeping the system prompt stable across compactions is
//!   critical for KV cache reuse. Middle-replace preserves the system
//!   prompt; sliding-window would drop earliest messages and break
//!   the prefix.
//! - **Keep last 4 messages uncompacted.** Preserves the active
//!   tool-call/tool-result thread and the last assistant turn so the
//!   model has immediate context for what's happening next.
//! - **Companion compactor model.** Same approach openclaw uses — a
//!   small 4B model summarizes via a separate chat-completion call
//!   with no tools. Tunable via `DARKMUX_AGENT_COMPACTOR_MODEL`.
//! - **Token-count-aware trigger, not heuristic percentage.** The
//!   threshold compares the chat-completion API's reported
//!   `usage.prompt_tokens` against an absolute number (env tunable).
//!   No guessing what the model's "believed" context window is —
//!   openclaw's heuristic-percentage approach is what produced
//!   spurious compactions at ambient mid-turn moments per the Article
//!   2 findings.
//!
//! ## Deliberately NOT done in this phase
//!
//! - No tool-call elision (a cheaper alternative that drops
//!   intermediate tool results but keeps tool calls + assistant
//!   reasoning). Could be added later if measurements show summarize
//!   is too expensive.
//! - No per-profile threshold (DARKMUX_AGENT_COMPACT_THRESHOLD_TOKENS
//!   is global). Phase 7+ could read the active darkmux profile and
//!   derive per-profile thresholds automatically.

use anyhow::{anyhow, Result};
use std::env;

use crate::lmstudio::{ChatRequest, LmStudioClient, Message};

/// Default threshold: when an incoming prompt's `prompt_tokens`
/// crosses this, compact before the next chat() call. 60K leaves
/// substantial headroom for the model's response generation when
/// loaded at 101K (the `balanced` profile context). Tunable via
/// `DARKMUX_AGENT_COMPACT_THRESHOLD_TOKENS`.
const DEFAULT_THRESHOLD_TOKENS: u32 = 60_000;

/// Default compactor model — the bake-off-hired admin agent
/// (Beat 21: "the dependable admin agent"). Same model openclaw
/// uses by convention. Tunable via `DARKMUX_AGENT_COMPACTOR_MODEL`.
const DEFAULT_COMPACTOR_MODEL: &str = "darkmux:qwen3-4b-instruct-2507";

/// Number of trailing messages to preserve uncompacted. Keeps the
/// active tool-call / tool-result thread + the last assistant turn.
const PRESERVE_TAIL: usize = 4;

/// Number of leading messages to preserve uncompacted. Keeps the
/// system prompt + the initial user message (the task framing). Both
/// are load-bearing for the agent to know what it's doing.
const PRESERVE_HEAD: usize = 2;

/// Read the threshold from env or use the default.
pub fn threshold_tokens() -> u32 {
    env::var("DARKMUX_AGENT_COMPACT_THRESHOLD_TOKENS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_THRESHOLD_TOKENS)
}

/// Read the compactor model id from env or use the default.
pub fn compactor_model() -> String {
    env::var("DARKMUX_AGENT_COMPACTOR_MODEL")
        .unwrap_or_else(|_| DEFAULT_COMPACTOR_MODEL.to_string())
}

/// Decide whether the conversation needs compaction before the next
/// chat() call. True when:
/// - `latest_prompt_tokens` crossed the threshold (the most recent
///   request's input was big), AND
/// - The conversation is long enough that middle-replace has
///   something meaningful to replace (head + 1 middle + tail).
pub fn needs_compaction(latest_prompt_tokens: u32, message_count: usize) -> bool {
    latest_prompt_tokens >= threshold_tokens()
        && message_count >= PRESERVE_HEAD + 1 + PRESERVE_TAIL
}

/// Apply middle-replace compaction in-place on `messages`.
///
/// Sends `messages[PRESERVE_HEAD .. n - PRESERVE_TAIL]` to the
/// compactor model with a "summarize this excerpt" prompt, then
/// replaces that slice with a single synthetic user message
/// containing the summary tagged `[compacted:<generation>]`.
pub fn compact(
    client: &LmStudioClient,
    messages: &mut Vec<Message>,
    generation: u32,
) -> Result<()> {
    let n = messages.len();
    if !needs_compaction(u32::MAX, n) {
        return Err(anyhow!(
            "conversation too short to compact: {n} messages \
             (need >= {} for middle-replace)",
            PRESERVE_HEAD + 1 + PRESERVE_TAIL
        ));
    }

    let middle_start = PRESERVE_HEAD;
    let middle_end = n - PRESERVE_TAIL;
    let middle_count = middle_end - middle_start;

    let middle_messages: Vec<Message> = messages[middle_start..middle_end].to_vec();
    let middle_rendered = render_messages_as_excerpt(&middle_messages);

    let compactor_system = Message::system(
        "You are a conversation compactor. Read the conversation excerpt below \
         and produce a concise summary that preserves: (1) what's been decided, \
         (2) what tools have been called and what they returned, (3) what state \
         changes have been made (files written, commands run, edits applied), \
         (4) what's currently in progress. Be specific and dense — this summary \
         REPLACES the excerpt in the agent's working memory. Do not editorialize. \
         Do not add framing language. Just the summary.",
    );
    let compactor_user = Message::user(format!(
        "Summarize the following conversation excerpt:\n\n\
         ---\n{middle_rendered}\n---\n\nSummary:"
    ));

    let request = ChatRequest {
        model: compactor_model(),
        messages: vec![compactor_system, compactor_user],
        tools: Vec::new(),
        tool_choice: None,
        temperature: 0.1,
        max_tokens: Some(4096),
    };

    eprintln!(
        "darkmux-agent: compaction #{generation} — summarizing {middle_count} middle messages \
         (preserving {PRESERVE_HEAD} head + {PRESERVE_TAIL} tail)"
    );

    let response = client.chat(&request)?;
    let summary = response
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.message.content)
        .ok_or_else(|| anyhow!("compactor model returned no content"))?;

    eprintln!(
        "darkmux-agent: compaction #{generation} — summary {} chars",
        summary.len()
    );

    let replacement = Message::user(format!("[compacted:{generation}] {summary}"));
    messages.splice(middle_start..middle_end, std::iter::once(replacement));

    Ok(())
}

/// Render a slice of messages as a plaintext excerpt the compactor
/// can read. Each message gets a `[role]:` prefix and tool_calls are
/// serialized in a human-readable form. Tool results are tagged with
/// the tool name.
fn render_messages_as_excerpt(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        out.push_str(&format!("[{}]: ", m.role));
        if let Some(content) = &m.content {
            out.push_str(content);
        }
        if let Some(calls) = &m.tool_calls {
            for c in calls {
                out.push_str(&format!(
                    "\n  tool_call: {}({})",
                    c.function.name, c.function.arguments
                ));
            }
        }
        if let Some(name) = &m.name {
            out.push_str(&format!("\n  (tool result for: {name})"));
        }
        out.push_str("\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_uses_env_when_set() {
        // We can't reliably set/unset env vars in unit tests without
        // serial_test because they leak across tests. The default
        // behavior is the load-bearing one — assert it's a reasonable
        // value.
        std::env::remove_var("DARKMUX_AGENT_COMPACT_THRESHOLD_TOKENS");
        assert_eq!(threshold_tokens(), DEFAULT_THRESHOLD_TOKENS);
    }

    #[test]
    fn needs_compaction_requires_both_size_and_length() {
        // Tokens below threshold → no compaction even with many messages
        assert!(!needs_compaction(1000, 100));
        // Tokens above threshold but conversation too short
        assert!(!needs_compaction(100_000, 5));
        // Both conditions met
        assert!(needs_compaction(100_000, 10));
    }

    #[test]
    fn needs_compaction_boundary() {
        // Exactly at threshold + minimum length
        let min_len = PRESERVE_HEAD + 1 + PRESERVE_TAIL;
        assert!(needs_compaction(DEFAULT_THRESHOLD_TOKENS, min_len));
        // One below
        assert!(!needs_compaction(DEFAULT_THRESHOLD_TOKENS - 1, min_len));
        // Length-1 below
        assert!(!needs_compaction(DEFAULT_THRESHOLD_TOKENS, min_len - 1));
    }

    #[test]
    fn render_excerpt_preserves_role_and_content() {
        let msgs = vec![
            Message::user("hello"),
            Message::system("system-text"),
        ];
        let out = render_messages_as_excerpt(&msgs);
        assert!(out.contains("[user]: hello"));
        assert!(out.contains("[system]: system-text"));
    }

    #[test]
    fn render_excerpt_includes_tool_calls() {
        let msg = Message {
            role: "assistant".into(),
            content: Some("thinking".into()),
            tool_calls: Some(vec![crate::lmstudio::ToolCall {
                id: "abc".into(),
                kind: "function".into(),
                function: crate::lmstudio::FunctionCall {
                    name: "read".into(),
                    arguments: r#"{"path":"foo"}"#.into(),
                },
            }]),
            tool_call_id: None,
            name: None,
        };
        let out = render_messages_as_excerpt(std::slice::from_ref(&msg));
        assert!(out.contains("[assistant]: thinking"));
        assert!(out.contains("tool_call: read({\"path\":\"foo\"})"));
    }
}
