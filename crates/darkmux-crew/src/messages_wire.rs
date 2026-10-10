//! (#3162) The host side of the `messages` dialect: the request body a
//! single-shot call posts to a Messages endpoint, and its reply read back as
//! the chat-completions shape every host-side reader already parses
//! (`parse_hosted_response`, `single_shot::extract_reply`, the usage fold).
//! The agent loop's twin lives in the runtime crate (`messages_dialect`),
//! which cannot depend on this one; both read usage through
//! `UsageCounts::from_messages_usage`.

use serde_json::{json, Map, Value};

/// The version header every Messages request carries.
pub(crate) const ANTHROPIC_VERSION_HEADER: (&str, &str) = ("anthropic-version", "2023-06-01");

/// True when `url` is a Messages endpoint. The URL is built in one place
/// from the endpoint's resolved dialect (`ModelEndpoint::chat_url`), so its
/// path is the dialect: `{url}/messages` exactly when it is `messages`.
pub(crate) fn speaks_messages(url: &str) -> bool {
    url.split(['?', '#']).next().is_some_and(|path| path.trim_end_matches('/').ends_with("/messages"))
}

/// The Messages body for a chat-completions message list: leading `system`
/// messages become the top-level `system`, the rest keep their role and
/// text. Model-facing text is byte-identical to the other dialects
/// (contract 6). No `cache_control`: a single-shot call's prefix is not
/// guaranteed to repeat, and a cache write costs more than an uncached read.
pub(crate) fn body(model: &str, messages: &Value, max_tokens: u32) -> Value {
    let all = messages.as_array().map(Vec::as_slice).unwrap_or_default();
    let leading = all.iter().take_while(|m| m["role"] == "system").count();
    let system: Vec<&str> = all[..leading].iter().filter_map(|m| m["content"].as_str()).collect();
    let mut b = Map::new();
    b.insert("model".into(), model.into());
    b.insert("max_tokens".into(), max_tokens.into());
    if !system.is_empty() {
        b.insert("system".into(), system.join("\n\n").into());
    }
    let rest: Vec<Value> = all[leading..]
        .iter()
        .map(|m| {
            let role = if m["role"] == "assistant" { "assistant" } else { "user" };
            json!({ "role": role, "content": m["content"] })
        })
        .collect();
    b.insert("messages".into(), Value::Array(rest));
    Value::Object(b)
}

/// The curl-config line carrying the version header on a Messages call,
/// empty on any other.
pub(crate) fn version_header_line(messages: bool) -> String {
    let (h, v) = ANTHROPIC_VERSION_HEADER;
    if messages { format!("header = \"{h}: {v}\"\n") } else { String::new() }
}

/// A reply body read as the chat-completions shape when the call was a
/// Messages call; a body that is not JSON is left for the classifier to
/// report.
pub(crate) fn reply_bytes_as_chat(messages: bool, body: Vec<u8>) -> Vec<u8> {
    match serde_json::from_slice::<Value>(&body) {
        Ok(reply) if messages => reply_as_chat(reply).to_string().into_bytes(),
        _ => body,
    }
}

/// A Messages reply as a chat-completions reply. An error body (`"error"`)
/// passes through unchanged for `parse_hosted_response` to classify.
pub(crate) fn reply_as_chat(reply: Value) -> Value {
    if reply.get("error").is_some() || reply.get("content").is_none() {
        return reply;
    }
    let text: String = reply["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    let finish = match reply["stop_reason"].as_str().unwrap_or("end_turn") {
        "end_turn" | "stop_sequence" => "stop",
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        other => other,
    };
    let mut out = json!({
        "id": reply["id"],
        "choices": [{ "index": 0, "message": { "role": "assistant", "content": text }, "finish_reason": finish }],
    });
    if let Some(model) = reply.get("model") {
        out["model"] = model.clone();
    }
    if let Some(u) = reply.get("usage") {
        out["usage"] = usage_as_chat(&darkmux_trajectory::UsageCounts::from_messages_usage(u));
    }
    out
}

/// The chat-completions `usage` object carrying exactly the reported counts.
fn usage_as_chat(c: &darkmux_trajectory::UsageCounts) -> Value {
    let mut u = Map::new();
    for (k, v) in [("prompt_tokens", c.prompt), ("completion_tokens", c.completion), ("total_tokens", c.total)] {
        if let Some(v) = v {
            u.insert(k.into(), v.into());
        }
    }
    if let Some(cached) = c.cached {
        u.insert("prompt_tokens_details".into(), json!({ "cached_tokens": cached }));
    }
    Value::Object(u)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_messages_path_speaks_messages() {
        assert!(speaks_messages("https://api.anthropic.com/v1/messages"));
        assert!(speaks_messages("https://api.anthropic.com/v1/messages?beta=true"));
        assert!(!speaks_messages("https://api.anthropic.com/v1/chat/completions"));
        assert!(!speaks_messages("https://r.example/openai/deployments/messages-bot/chat/completions?api-version=1"));
    }

    #[test]
    fn a_single_shot_body_moves_system_to_the_top_and_keeps_the_text() {
        let msgs = crate::single_shot::chat_messages("PERSONA", "judge this");
        assert_eq!(
            body("claude-sonnet-5-5", &msgs, 512),
            json!({
                "model": "claude-sonnet-5-5",
                "max_tokens": 512,
                "system": "PERSONA",
                "messages": [{"role": "user", "content": "judge this"}],
            })
        );
        let bare = body("m", &crate::single_shot::chat_messages("  ", "u"), 8);
        assert!(bare.get("system").is_none(), "{bare}");
    }

    #[test]
    fn a_messages_reply_reads_as_the_chat_shape_with_its_cache_counts() {
        let chat = reply_as_chat(json!({
            "id": "msg_1", "type": "message", "model": "claude-sonnet-5-5", "stop_reason": "max_tokens",
            "content": [{"type": "text", "text": "par"}, {"type": "text", "text": "tial"}],
            "usage": {"input_tokens": 2, "cache_creation_input_tokens": 10, "cache_read_input_tokens": 30, "output_tokens": 5},
        }));
        assert_eq!(chat["choices"][0]["message"]["content"], "partial");
        assert_eq!(chat["choices"][0]["finish_reason"], "length");
        assert_eq!(chat["model"], "claude-sonnet-5-5");
        let counts = darkmux_trajectory::UsageCounts::of_reply(&chat);
        assert_eq!((counts.prompt, counts.completion, counts.total, counts.cached), (Some(42), Some(5), Some(47), Some(30)));
        let parsed = crate::dispatch_internal::parse_hosted_response(chat.to_string().as_bytes());
        assert!(parsed.is_ok(), "the translated reply passes the shape check");
    }

    #[test]
    fn an_error_reply_passes_through_for_classification() {
        let err = json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}});
        assert_eq!(reply_as_chat(err.clone()), err);
        assert!(crate::dispatch_internal::parse_hosted_response(err.to_string().as_bytes()).is_err());
    }

    /// The whole host path against a local server: the version header goes
    /// out on a Messages URL, and the reply comes back in the chat shape.
    #[test]
    fn a_hosted_call_to_a_messages_url_sends_the_version_and_reads_the_reply() {
        let server = httpmock::MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/messages")
                .header("anthropic-version", "2023-06-01")
                .json_body_partial(r#"{"system": "PERSONA", "max_tokens": 64}"#);
            then.status(200).json_body(json!({
                "id": "msg_h", "type": "message", "model": "claude-sonnet-5-5", "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "verdict"}],
                "usage": {"input_tokens": 9, "output_tokens": 2},
            }));
        });
        let req = body("claude-sonnet-5-5", &crate::single_shot::chat_messages("PERSONA", "judge"), 64);
        let reply = crate::dispatch_internal::remote_chat_completion(&server.url("/v1/messages"), None, &req, 30).unwrap();
        mock.assert();
        let r = crate::single_shot::extract_reply(&reply);
        assert_eq!(r.content, "verdict");
        assert_eq!(r.counts.total, Some(11));
    }

    /// And the other dialects never send it.
    #[test]
    fn a_hosted_call_to_a_chat_completions_url_sends_no_version() {
        let server = httpmock::MockServer::start();
        let with_version = server.mock(|when, then| {
            when.method(httpmock::Method::POST).header_exists("anthropic-version");
            then.status(500);
        });
        let plain = server.mock(|when, then| {
            when.method(httpmock::Method::POST).path("/v1/chat/completions");
            then.status(200).json_body(json!({"choices": [{"message": {"content": "ok"}}]}));
        });
        let reply = crate::dispatch_internal::remote_chat_completion(&server.url("/v1/chat/completions"), None, &json!({}), 30).unwrap();
        assert_eq!(crate::single_shot::extract_reply(&reply).content, "ok");
        with_version.assert_hits(0);
        plain.assert();
    }
}
