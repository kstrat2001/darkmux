//! Local single-shot chat primitive (#1222 Phase B packet 2).
//!
//! A container-free, single-turn call to an LMStudio-loaded model. Reuses
//! the hardened curl machinery in `dispatch_internal` (0600 secret-bearing
//! curl config, 429 backoff) but skips the container path entirely — this
//! is for callers that just need ONE chat completion, not an agentic
//! dispatch (a review-funnel probe/judge invocation is the first consumer).
//!
//! **Dialect note:** this is the LOCAL LMStudio dialect — `"max_tokens"` +
//! `"temperature"` + `"stream": false`. `dispatch_internal::single_shot_body`
//! builds the HOSTED (Azure/OpenAI) dialect (`"max_completion_tokens"`,
//! optional `reasoning_effort`) for `dispatch_remote`. The two are separate
//! request shapes for separate targets — do not merge them.

use crate::dispatch_internal::remote_chat_completion;
use anyhow::Result;

/// One local single-shot chat request. `base_url` defaults to
/// `config_access::lmstudio_url()` when `None` — the same
/// `env(DARKMUX_LMSTUDIO_URL) > config.lmstudio_url > http://localhost:1234`
/// precedence every other LMStudio caller in darkmux uses.
pub struct SingleShotRequest<'a> {
    pub base_url: Option<&'a str>,
    /// The LOADED LMStudio identifier (may be darkmux-namespaced, e.g.
    /// `darkmux:qwen3.6-35b-a3b`) — not necessarily the profile's bare
    /// model id. Callers resolve which identifier is actually loaded
    /// (`lms ps`) before calling in.
    pub model: &'a str,
    pub system: &'a str,
    pub user: &'a str,
    pub temperature: f32,
    pub max_tokens: u32,
    pub timeout_seconds: u32,
}

/// The extracted reply. `content` is `""` on an empty-but-successful
/// response — degeneracy (e.g. a reasoning model that spent its whole
/// budget thinking) is the CALLER's judgment call, not this primitive's.
pub struct SingleShotReply {
    pub content: String,
    pub total_tokens: Option<u64>,
    pub model: Option<String>,
}

/// The local LMStudio chat-completions request body. Pure — unit-testable.
/// LOCAL dialect: `"max_tokens"` (not the hosted `"max_completion_tokens"`
/// form built by `dispatch_internal::single_shot_body`), `"temperature"`,
/// and `"stream": false` (this primitive is single-shot, not streamed).
pub fn local_chat_body(
    model: &str,
    system: &str,
    user: &str,
    temperature: f32,
    max_tokens: u32,
) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user },
        ],
        "temperature": temperature,
        "max_tokens": max_tokens,
        "stream": false,
    })
}

/// The chat-completions URL for a local LMStudio base:
/// `{base}/v1/chat/completions`. `base` already has any trailing slash
/// trimmed by `config_access::lmstudio_url()`; an explicit `base_url`
/// override is trimmed the same way so `/v1/...` can't double up.
fn local_chat_url(base_url: Option<&str>) -> String {
    let base = base_url
        .map(str::to_string)
        .unwrap_or_else(darkmux_types::config_access::lmstudio_url);
    let base = base.trim_end_matches('/');
    format!("{base}/v1/chat/completions")
}

/// Container-free single-shot chat call against a local LMStudio endpoint.
/// Builds the local-dialect body, POSTs via the same hardened curl path
/// `dispatch_remote` uses (0600 secret-bearing config file — moot here
/// since local calls carry no auth header, but it's the SAME machinery,
/// so the 429-backoff ladder applies uniformly), and extracts
/// `choices[0].message.content` + `usage.total_tokens` + `model`.
pub fn single_shot_chat(req: &SingleShotRequest) -> Result<SingleShotReply> {
    let url = local_chat_url(req.base_url);
    let body = local_chat_body(
        req.model,
        req.system,
        req.user,
        req.temperature,
        req.max_tokens,
    );
    let resp = remote_chat_completion(&url, None, &body, req.timeout_seconds)?;
    Ok(extract_reply(&resp))
}

/// Pull the caller-facing fields out of an already-classified response
/// body (`remote_chat_completion` / `parse_hosted_response` have already
/// ruled out the error-in-body shapes — this only runs on a body that
/// passed that classification). Empty/missing content is `Ok("")`, never
/// an error — degeneracy is the caller's call.
fn extract_reply(resp: &serde_json::Value) -> SingleShotReply {
    let content = resp
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let total_tokens = resp.pointer("/usage/total_tokens").and_then(|v| v.as_u64());
    let model = resp
        .get("model")
        .and_then(|m| m.as_str())
        .map(str::to_string);
    SingleShotReply {
        content,
        total_tokens,
        model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch_internal::{parse_hosted_response, HostedCallError};

    // ─── local_chat_body: LOCAL dialect shape ───────────────────────────

    #[test]
    fn local_chat_body_has_local_dialect_keys() {
        let body = local_chat_body("darkmux:qwen3.6-35b-a3b", "sys", "user msg", 0.2, 512);
        assert_eq!(body["model"], "darkmux:qwen3.6-35b-a3b");
        // f32 -> f64 widening loses exactness (0.2f32 as f64 != 0.2f64) —
        // compare within tolerance rather than exact equality.
        assert!((body["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6);
        assert_eq!(body["max_tokens"], 512);
        assert_eq!(body["stream"], false);
        assert!(
            body.get("max_completion_tokens").is_none(),
            "local dialect must use \"max_tokens\", never the hosted \"max_completion_tokens\" form"
        );
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "sys");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "user msg");
    }

    #[test]
    fn local_chat_url_joins_base_and_v1_without_doubling() {
        assert_eq!(
            local_chat_url(Some("http://localhost:1234")),
            "http://localhost:1234/v1/chat/completions"
        );
        // Trailing slash on an explicit override doesn't double the `/v1`.
        assert_eq!(
            local_chat_url(Some("http://localhost:1234/")),
            "http://localhost:1234/v1/chat/completions"
        );
    }

    #[test]
    fn local_chat_url_defaults_to_config_access_lmstudio_url() {
        let prev = std::env::var("DARKMUX_LMSTUDIO_URL").ok();
        unsafe {
            std::env::set_var("DARKMUX_LMSTUDIO_URL", "http://192.168.1.5:1234");
        }
        assert_eq!(
            local_chat_url(None),
            "http://192.168.1.5:1234/v1/chat/completions"
        );
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DARKMUX_LMSTUDIO_URL", v),
                None => std::env::remove_var("DARKMUX_LMSTUDIO_URL"),
            }
        }
    }

    // ─── extract_reply: over the SAME error-shape corpus parse_hosted_response
    // classifies (#1177), reused rather than re-derived ──────────────────

    #[test]
    fn extract_reply_pulls_content_tokens_and_model() {
        let resp = parse_hosted_response(
            br#"{"model":"darkmux:qwen3.6-35b-a3b","choices":[{"message":{"content":"ok"}}],"usage":{"total_tokens":42}}"#,
        )
        .expect("well-formed success body classifies as Ok");
        let reply = extract_reply(&resp);
        assert_eq!(reply.content, "ok");
        assert_eq!(reply.total_tokens, Some(42));
        assert_eq!(reply.model.as_deref(), Some("darkmux:qwen3.6-35b-a3b"));
    }

    #[test]
    fn extract_reply_empty_content_is_ok_empty_string() {
        // A well-formed body whose content is the empty string still
        // classifies as Ok by parse_hosted_response (it only rejects a
        // MISSING content pointer, not an empty one) — extraction then
        // hands the caller "" rather than failing. Degeneracy judgment
        // is the caller's, not this primitive's.
        let resp = parse_hosted_response(br#"{"choices":[{"message":{"content":""}}]}"#)
            .expect("empty-but-present content still classifies as Ok");
        let reply = extract_reply(&resp);
        assert_eq!(reply.content, "");
        assert_eq!(reply.total_tokens, None);
        assert_eq!(reply.model, None);
    }

    #[test]
    fn extract_reply_missing_usage_and_model_are_none() {
        let resp = parse_hosted_response(br#"{"choices":[{"message":{"content":"hi"}}]}"#)
            .expect("content-only body classifies as Ok");
        let reply = extract_reply(&resp);
        assert_eq!(reply.content, "hi");
        assert_eq!(reply.total_tokens, None);
        assert_eq!(reply.model, None);
    }

    // ─── parse_hosted_response reuse: confirm the error-in-body shapes this
    // primitive inherits (via remote_chat_completion) are still classified
    // the same way — no local-dialect drift on the shared error path ─────

    #[test]
    fn shared_error_classification_still_applies_to_local_calls() {
        match parse_hosted_response(br#"{"error":{"code":401,"message":"bad key"}}"#) {
            Err(HostedCallError::Other(e)) => {
                assert!(e.to_string().contains("bad key"));
            }
            _ => panic!("expected a terminal Other error"),
        }
        match parse_hosted_response(br#"{"error":{"code":429,"message":"quota"}}"#) {
            Err(HostedCallError::RateLimited(msg)) => assert_eq!(msg, "quota"),
            _ => panic!("expected a retryable RateLimited error"),
        }
    }
}
