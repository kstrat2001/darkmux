//! Local single-shot chat primitive (#1222 Phase B packet 2).
//!
//! A container-free, single-turn call to an LMStudio-loaded model. Reuses
//! the hardened curl machinery in `dispatch_internal` (0600 secret-bearing
//! curl config, 429 backoff) but skips the container path entirely — this
//! is for callers that just need ONE chat completion, not an agentic
//! dispatch (a review-funnel probe/judge invocation is the first consumer).
//!
//! **Dialect note:** the LOCAL LMStudio dialect is `"max_tokens"` +
//! `"temperature"` + `"stream": false`. The HOSTED (Azure/OpenAI) dialect
//! (`"max_completion_tokens"`, optional `reasoning_effort`, no temperature)
//! exists in two single-shot shapes: `dispatch_internal::single_shot_body`
//! for `dispatch_remote` (a full role dispatch), and this module's
//! [`hosted_chat_body`] + [`single_shot_chat_hosted`] (#1260) for
//! endpoint-staffed crew seats (the review funnel's remote probe/judge/
//! verify seats). Local vs hosted are separate request shapes for separate
//! targets — do not merge them; local vs hosted here differ ONLY in the
//! transport dialect, never in message assembly (contract 6 — frozen
//! model-facing text: [`chat_messages`] is the single assembly both bodies
//! share, so the frozen seat texts reach a remote endpoint byte-identical
//! to what a local seat receives).

use crate::dispatch_internal::{remote_auth_header, remote_chat_completion, remote_chat_url};
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

/// The extracted reply. `content` is `""` when the response's content is
/// empty OR the field is absent entirely (some OpenAI-compat reasoning
/// backends omit it on length-truncation) — degeneracy (e.g. a reasoning
/// model that spent its whole budget thinking) is the CALLER's judgment
/// call, not this primitive's.
pub struct SingleShotReply {
    pub content: String,
    pub total_tokens: Option<u64>,
    pub model: Option<String>,
}

/// The local LMStudio chat-completions request body. Pure — unit-testable.
/// LOCAL dialect: `"max_tokens"` (not the hosted `"max_completion_tokens"`
/// form built by `dispatch_internal::single_shot_body`), `"temperature"`,
/// and `"stream": false` (this primitive is single-shot, not streamed).
///
/// `system` empty (after trimming) omits the system message ENTIRELY —
/// `"messages"` carries only the user turn, matching the Phase A probe
/// protocol byte-for-byte (`probe-runner.py`'s `call_model` sends
/// `messages: [{"role": "user", ...}]`, no system role at all; the darkmux
/// review-probe seat, #1256, is this primitive's first caller with a
/// genuinely empty system). A non-empty `system` (e.g. the judge seat's
/// PERSONA) still gets its own leading system message, unchanged.
pub fn local_chat_body(
    model: &str,
    system: &str,
    user: &str,
    temperature: f32,
    max_tokens: u32,
) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": chat_messages(system, user),
        "temperature": temperature,
        "max_tokens": max_tokens,
        "stream": false,
    })
}

/// (#1260, contract 6 — frozen model-facing text) The message-array
/// assembly BOTH dialects share: an empty (after trimming) `system` omits
/// the system message entirely (one user-role message — the Phase A probe
/// protocol, see [`local_chat_body`]'s doc); a non-empty `system` leads as
/// its own system-role message. Extracted so [`local_chat_body`] and
/// [`hosted_chat_body`] cannot drift: a remote seat's model sees exactly
/// the messages a local seat's model sees — only the surrounding transport
/// dialect differs.
fn chat_messages(system: &str, user: &str) -> serde_json::Value {
    if system.trim().is_empty() {
        serde_json::json!([{ "role": "user", "content": user }])
    } else {
        serde_json::json!([
            { "role": "system", "content": system },
            { "role": "user", "content": user },
        ])
    }
}

/// (#1260) The HOSTED (Azure/OpenAI) chat-completions request body for an
/// endpoint-staffed crew seat. Pure — unit-testable, with a golden
/// field-set test mirroring the local body's. Message assembly is
/// [`chat_messages`], byte-identical to the local dialect (contract 6);
/// everything else is the hosted dialect `dispatch_remote` proved in the
/// 1.17 cycle:
///
/// - `"max_completion_tokens"` (the Azure/OpenAI cap form), never the
///   local `"max_tokens"`;
/// - optional `"reasoning_effort"` when the endpoint declares one;
/// - NO `"temperature"` — hosted reasoning models (the o-series /
///   GPT-5-class deployments these seats target) reject non-default
///   temperature, so the local dialect's temperature knob is deliberately
///   not sent (same as `dispatch_internal::single_shot_body`);
/// - NO `"stream"` — single-shot, and the hosted dialect's proven body
///   omits it.
pub fn hosted_chat_body(
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
    reasoning_effort: Option<&str>,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "messages": chat_messages(system, user),
        "max_completion_tokens": max_tokens,
    });
    if let Some(effort) = reasoning_effort {
        body["reasoning_effort"] = serde_json::Value::String(effort.to_string());
    }
    body
}

/// The chat-completions URL for a local LMStudio base:
/// `{base}/v1/chat/completions`. `base` already has any trailing slash
/// trimmed by `config_access::lmstudio_url()`; an explicit `base_url`
/// override is trimmed the same way so `/v1/...` can't double up. A base
/// that already ends in `/v1` (operators carrying the pre-#661 full-URL
/// habit) is tolerated too — the suffix is trimmed before this appends
/// its own.
fn local_chat_url(base_url: Option<&str>) -> String {
    let base = base_url
        .map(str::to_string)
        .unwrap_or_else(darkmux_types::config_access::lmstudio_url);
    let base = base.trim_end_matches('/').trim_end_matches("/v1");
    format!("{base}/v1/chat/completions")
}

/// Container-free single-shot chat call against a local LMStudio endpoint.
/// Builds the local-dialect body, POSTs via the same hardened curl path
/// `dispatch_remote` uses (0600 secret-bearing config file — moot here
/// since local calls carry no auth header, but it's the SAME machinery),
/// and extracts `choices[0].message.content` + `usage.total_tokens` +
/// `model`.
///
/// Blocking-behavior note for callers: this inherits the shared 429
/// backoff ladder — a rate-limited endpoint is retried after 30s, 60s,
/// then 120s before failing, so a single call can block for up to ~3.5
/// minutes plus timeouts in the worst case (unlikely against a local
/// LMStudio, which doesn't rate-limit, but true of the shared path).
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

/// (#1260) One HOSTED single-shot chat request — an endpoint-staffed crew
/// seat's call. `model` is the profile's model id (the deployment name for
/// Azure); nothing is loaded in LMStudio, so there is no darkmux-namespaced
/// identifier to resolve.
pub struct HostedSingleShotRequest<'a> {
    pub endpoint: &'a darkmux_types::ModelEndpoint,
    pub model: &'a str,
    pub system: &'a str,
    pub user: &'a str,
    pub max_tokens: u32,
    pub timeout_seconds: u32,
}

/// (#1260) Container-free single-shot chat call against a REMOTE
/// OpenAI-compatible endpoint — the hosted twin of [`single_shot_chat`],
/// through the EXACT URL/auth/POST chain `dispatch_remote` and
/// `doctor --probe` use (`remote_chat_url` + `remote_auth_header` +
/// `remote_chat_completion`): Azure `?api-version=`, Keychain-read auth
/// header (0600 curl config, never on argv, never logged), and the shared
/// 429/503 backoff ladder (bounded — 3 retries at 30s/60s/120s), so one
/// call can block for minutes against a rate-limited endpoint before
/// failing loud. Message assembly is byte-identical to the local path
/// (contract 6 — see [`hosted_chat_body`]); only the transport dialect
/// differs.
pub fn single_shot_chat_hosted(req: &HostedSingleShotRequest) -> Result<SingleShotReply> {
    let url = remote_chat_url(req.endpoint);
    let auth = remote_auth_header(req.endpoint)?;
    let body = hosted_chat_body(
        req.model,
        req.system,
        req.user,
        req.max_tokens,
        req.endpoint.reasoning_effort.as_deref(),
    );
    let resp = remote_chat_completion(&url, auth.as_ref(), &body, req.timeout_seconds)?;
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
    use serial_test::serial;

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
    fn local_chat_body_omits_system_message_when_system_is_empty() {
        // The Phase A probe parity fix (#1256): an empty system means NO
        // system message on the wire at all — a single user-role message,
        // matching probe-runner.py's `call_model` exactly. A blank/
        // whitespace-only system is treated the same as fully empty.
        let body = local_chat_body("m", "", "user msg", 0.2, 512);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1, "empty system -> exactly one message, the user turn");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "user msg");

        let whitespace_only = local_chat_body("m", "   \n", "user msg", 0.2, 512);
        assert_eq!(whitespace_only["messages"].as_array().unwrap().len(), 1);
    }

    // ─── Phase A request-BODY parity (#1256 scope extension) ────────────
    //
    // Prompt-string parity (`darkmux-lab`'s funnel golden tests) proves the
    // TEXT matches; these prove the FULL serialized request shape matches
    // too — field set, message-array structure, and every non-content
    // value — so a stray field (a `tools` array, a `top_p`, an accidental
    // extra message) fails CI the same as prompt-text drift would. Golden
    // field sets below are transcribed from the exact python dicts
    // `probe-runner.py`'s `call_model` and `judge-runner.py`'s
    // `call_judge` build (both read 2026-07 for #1256) — `json.dumps({...})`
    // over those literal dicts round-trips to exactly the `serde_json::json!`
    // literals asserted against here (verified by hand during development;
    // see funnel.rs's golden-harness comment for the sibling prompt-text
    // provenance note).

    /// `body`'s `"temperature"` compared within tolerance (the f32->f64
    /// widening every other test in this module already documents — 0.2f32
    /// as f64 != 0.2f64 exactly), every other top-level field compared for
    /// exact equality, AND the top-level field COUNT asserted so a stray
    /// extra field (a `tools` array, a `top_p`) fails loudly even though
    /// it'd otherwise be invisible to a "contains these fields" check.
    fn assert_body_matches_golden_shape(body: &serde_json::Value, expected_temperature: f64, mut expected: serde_json::Value) {
        expected.as_object_mut().expect("golden fixture is an object").remove("temperature");
        let mut body_sans_temp = body.clone();
        body_sans_temp.as_object_mut().unwrap().remove("temperature");
        assert_eq!(
            body_sans_temp, expected,
            "every field but temperature must match Phase A's shape exactly"
        );
        assert!(
            (body["temperature"].as_f64().unwrap() - expected_temperature).abs() < 1e-6,
            "temperature: got {:?}, want ~{expected_temperature}",
            body["temperature"]
        );
        assert_eq!(
            body.as_object().unwrap().len(),
            5,
            "exactly 5 top-level fields (model, messages, temperature, max_tokens, stream) — \
             no extra field (tools, top_p, ...) beyond Phase A's shape"
        );
    }

    #[test]
    fn local_chat_body_matches_phase_a_probe_golden_request_body() {
        // probe-runner.py's `call_model`, MODELS["devstral"]: {"id":
        // "mistralai/devstral-small-2-2512", "max_tokens": 3000}, TEMP=0.2,
        // messages=[{"role": "user", "content": prompt}] — no system
        // message, no "tools"/"top_p"/any other field.
        let body = local_chat_body("mistralai/devstral-small-2-2512", "", "PROMPT_PLACEHOLDER", 0.2, 3000);
        let expected = serde_json::json!({
            "model": "mistralai/devstral-small-2-2512",
            "messages": [
                { "role": "user", "content": "PROMPT_PLACEHOLDER" },
            ],
            "temperature": 0.2,
            "max_tokens": 3000,
            "stream": false,
        });
        assert_body_matches_golden_shape(&body, 0.2, expected);
    }

    #[test]
    fn local_chat_body_matches_phase_a_judge_golden_request_body() {
        // judge-runner.py's `call_judge`: MODEL="qwen3.6-35b-a3b-turboquant-mlx",
        // messages=[{"role": "system", "content": PERSONA}, {"role": "user",
        // "content": user}], temperature=0.2, max_tokens=20000, stream=False.
        let body = local_chat_body(
            "qwen3.6-35b-a3b-turboquant-mlx",
            "PERSONA_PLACEHOLDER",
            "USER_PLACEHOLDER",
            0.2,
            20_000,
        );
        let expected = serde_json::json!({
            "model": "qwen3.6-35b-a3b-turboquant-mlx",
            "messages": [
                { "role": "system", "content": "PERSONA_PLACEHOLDER" },
                { "role": "user", "content": "USER_PLACEHOLDER" },
            ],
            "temperature": 0.2,
            "max_tokens": 20000,
            "stream": false,
        });
        assert_body_matches_golden_shape(&body, 0.2, expected);
    }

    #[test]
    fn local_chat_body_round_trips_extreme_values() {
        // Pure value construction — no range validation happens here (the
        // caller / model server owns rejecting out-of-range values). Confirm
        // serde_json carries extreme f32/u32 inputs through unchanged rather
        // than silently clamping or losing precision.
        let zero = local_chat_body("m", "sys", "user", 0.0, 0);
        assert_eq!(zero["temperature"].as_f64().unwrap(), 0.0);
        assert_eq!(zero["max_tokens"], 0);

        let negative_temp = local_chat_body("m", "sys", "user", -1.0, 1);
        assert_eq!(negative_temp["temperature"].as_f64().unwrap(), -1.0);

        let large_temp = local_chat_body("m", "sys", "user", 100.0, 1);
        assert_eq!(large_temp["temperature"].as_f64().unwrap(), 100.0);

        let max_tokens = local_chat_body("m", "sys", "user", 1.0, u32::MAX);
        assert_eq!(max_tokens["max_tokens"], u32::MAX);

        // f32::NAN serializes to `null` under serde_json's default (non-
        // `arbitrary_precision`) float handling — characterize rather than
        // assume, since a NaN temperature silently becoming `null` is a
        // surprising wire shape a caller could hit via a bad upstream calc.
        let nan_temp = local_chat_body("m", "sys", "user", f32::NAN, 1);
        assert!(
            nan_temp["temperature"].is_null(),
            "NaN temperature serializes to JSON null (serde_json has no NaN literal), got: {:?}",
            nan_temp["temperature"]
        );
    }

    #[test]
    fn local_chat_body_round_trips_json_breaking_content() {
        // serde_json handles escaping internally — this asserts the
        // round-trip actually holds for content that would break naive
        // string concatenation: embedded quotes, backslashes, newlines,
        // and non-ASCII (CJK + emoji, astral-plane).
        let system = "sys with \"quotes\", a \\backslash\\, and\nnewlines";
        let user = "user says 你好 🎉 and \"nested 'quotes'\"";
        let body = local_chat_body("m", system, user, 0.5, 10);

        // Round-trip through a full serialize/parse cycle, not just the
        // in-memory Value — proves the wire format (what curl actually
        // sends) preserves the content, not just the Value tree.
        let wire = serde_json::to_string(&body).expect("body must serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&wire).expect("serialized body must re-parse");
        assert_eq!(parsed["messages"][0]["content"], system);
        assert_eq!(parsed["messages"][1]["content"], user);
        // Sanity: the raw wire bytes actually escaped the dangerous
        // characters (a naive `format!` build would have produced invalid
        // JSON here) — the round-trip above already proves this, but assert
        // the wire string itself contains an escaped quote and newline as a
        // second signal.
        assert!(wire.contains("\\\""), "embedded quote must be escaped on the wire");
        assert!(wire.contains("\\n"), "embedded newline must be escaped on the wire");
    }

    // ─── hosted_chat_body: HOSTED dialect golden shape (#1260, contract 6) ──

    /// The hosted request body's EXACT field set — the golden mirror of
    /// `local_chat_body_matches_phase_a_*`: assert exactly which fields are
    /// present, so a stray addition (a `temperature` an Azure reasoning
    /// deployment would reject, a `tools` array, a `stream`) fails CI the
    /// same way prompt-text drift would.
    #[test]
    fn hosted_chat_body_matches_hosted_dialect_golden_shape() {
        let body = hosted_chat_body("gpt-5.1", "PERSONA_PLACEHOLDER", "USER_PLACEHOLDER", 20_000, None);
        let expected = serde_json::json!({
            "model": "gpt-5.1",
            "messages": [
                { "role": "system", "content": "PERSONA_PLACEHOLDER" },
                { "role": "user", "content": "USER_PLACEHOLDER" },
            ],
            "max_completion_tokens": 20000,
        });
        assert_eq!(body, expected, "hosted body must match the proven dispatch_remote dialect exactly");
        assert_eq!(
            body.as_object().unwrap().len(),
            3,
            "exactly 3 top-level fields (model, messages, max_completion_tokens) — \
             no temperature (hosted reasoning models reject it), no stream, no local max_tokens"
        );
    }

    #[test]
    fn hosted_chat_body_reasoning_effort_is_the_only_optional_field() {
        let body = hosted_chat_body("gpt-5.1", "sys", "user", 16_384, Some("high"));
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(
            body.as_object().unwrap().len(),
            4,
            "with an endpoint-declared reasoning_effort: exactly 4 top-level fields"
        );
        assert!(body.get("max_tokens").is_none(), "hosted dialect never carries the local cap form");
        assert!(body.get("temperature").is_none());
    }

    /// Contract 6 conformance: the message ARRAY a remote seat sends is
    /// byte-identical to the local seat's — including the empty-system
    /// omission rule (the Phase A probe protocol sends ONE user-role
    /// message, no system message at all). Compared against the local
    /// body's own messages so the two dialects structurally cannot drift.
    #[test]
    fn hosted_and_local_bodies_share_identical_message_assembly() {
        for (system, user) in [
            ("", "probe user message"),
            ("   \n", "probe user message"),
            ("PERSONA", "judge user message"),
        ] {
            let local = local_chat_body("m", system, user, 0.2, 3000);
            let hosted = hosted_chat_body("m", system, user, 3000, Some("high"));
            assert_eq!(
                local["messages"], hosted["messages"],
                "message assembly must be byte-identical across dialects (system={system:?})"
            );
        }
        // And the empty-system case is the single-user-message wire shape.
        let hosted = hosted_chat_body("m", "", "u", 100, None);
        assert_eq!(hosted["messages"].as_array().unwrap().len(), 1);
        assert_eq!(hosted["messages"][0]["role"], "user");
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
        // A base already carrying `/v1` (pre-#661 full-URL habit) doesn't
        // double it either — with or without the trailing slash.
        assert_eq!(
            local_chat_url(Some("http://localhost:1234/v1")),
            "http://localhost:1234/v1/chat/completions"
        );
        assert_eq!(
            local_chat_url(Some("http://localhost:1234/v1/")),
            "http://localhost:1234/v1/chat/completions"
        );
    }

    #[test]
    #[serial]
    fn local_chat_url_defaults_to_config_access_lmstudio_url() {
        // SAFETY: serialized via `#[serial]`; restored below.
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

    #[test]
    fn local_chat_url_full_chat_completions_base_is_a_documented_operator_error() {
        // A base that ALREADY carries the full `/v1/chat/completions` path
        // (a plausible operator mistake — pasting the endpoint instead of
        // the base URL) is NOT covered by the `/v1`-suffix trim: the trim
        // only strips a LITERAL trailing "/v1", and this string ends in
        // "chat/completions", not "/v1". Characterize the actual result
        // (a doubled path) rather than assume — this is a documented
        // operator-error shape, not a claim that the doubling is desired.
        assert_eq!(
            local_chat_url(Some("http://localhost:1234/v1/chat/completions")),
            "http://localhost:1234/v1/chat/completions/v1/chat/completions"
        );
    }

    #[test]
    fn local_chat_url_preserves_https_scheme() {
        assert_eq!(
            local_chat_url(Some("https://models.example.com:8443")),
            "https://models.example.com:8443/v1/chat/completions"
        );
    }

    #[test]
    fn local_chat_url_handles_portless_base() {
        assert_eq!(
            local_chat_url(Some("https://models.example.com")),
            "https://models.example.com/v1/chat/completions"
        );
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
        // classifies as Ok by parse_hosted_response — extraction then
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
    fn extract_reply_absent_content_is_ok_empty_string() {
        // Some OpenAI-compat reasoning backends omit `content` ENTIRELY on
        // length-truncation (the message object carries only the role).
        // The shared classification accepts a present message object, and
        // extraction reads absent content as "" — same contract as empty
        // content: the caller owns degeneracy.
        let resp = parse_hosted_response(
            br#"{"model":"m","choices":[{"message":{"role":"assistant"},"finish_reason":"length"}],"usage":{"total_tokens":128}}"#,
        )
        .expect("message object without a content field classifies as Ok");
        let reply = extract_reply(&resp);
        assert_eq!(reply.content, "");
        assert_eq!(reply.total_tokens, Some(128));
        assert_eq!(reply.model.as_deref(), Some("m"));
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

    #[test]
    fn extract_reply_empty_choices_array_does_not_panic() {
        // `extract_reply` only ever runs on a body `parse_hosted_response`
        // already classified Ok (which rejects a choices-less body), so an
        // empty `choices: []` array should never reach it in practice. But
        // `extract_reply` itself is called directly here (bypassing the
        // classifier) to characterize its OWN defensiveness: `.pointer()`
        // on an out-of-bounds array index returns None, not a panic, so
        // extraction degrades to the same "" contract as absent content.
        let resp = serde_json::json!({ "choices": [] });
        let reply = extract_reply(&resp);
        assert_eq!(reply.content, "");
        assert_eq!(reply.total_tokens, None);
        assert_eq!(reply.model, None);
    }

    #[test]
    fn extract_reply_total_tokens_float_shape_is_none() {
        // serde_json's `Number::as_u64()` returns None for a value parsed
        // as a float (JSON `42.0` carries a decimal point, so it's stored
        // as an f64 internally) regardless of whether the value would fit
        // in a u64 — characterize this rather than assume total_tokens
        // survives a backend that emits a float-shaped usage count.
        let resp = parse_hosted_response(
            br#"{"choices":[{"message":{"content":"hi"}}],"usage":{"total_tokens":42.0}}"#,
        )
        .expect("well-formed body with a float usage count still classifies as Ok");
        let reply = extract_reply(&resp);
        assert_eq!(
            reply.total_tokens, None,
            "a float-shaped total_tokens does not extract as a u64"
        );
    }

    #[test]
    fn extract_reply_total_tokens_string_shape_is_none() {
        // Same characterization for a string-shaped usage count (a backend
        // quoting the number) — `.as_u64()` only matches the Number variant.
        let resp = parse_hosted_response(
            br#"{"choices":[{"message":{"content":"hi"}}],"usage":{"total_tokens":"42"}}"#,
        )
        .expect("well-formed body with a string usage count still classifies as Ok");
        let reply = extract_reply(&resp);
        assert_eq!(
            reply.total_tokens, None,
            "a string-shaped total_tokens does not extract as a u64"
        );
    }

    #[test]
    fn extract_reply_model_reflects_the_response_not_the_request() {
        // A caller's SingleShotRequest.model names the identifier they
        // ASKED for (e.g. a darkmux-namespaced loaded identifier); the
        // reply's `model` field is whatever the SERVER reports it actually
        // served, which can legitimately differ (a router/proxy resolving
        // an alias, or a backend that echoes its own internal name). This
        // primitive does no reconciliation — it just reports what the
        // response body said.
        let requested_model = "darkmux:qwen3.6-35b-a3b";
        let resp = parse_hosted_response(
            br#"{"model":"actual-served-model-v2","choices":[{"message":{"content":"hi"}}]}"#,
        )
        .expect("well-formed body classifies as Ok");
        let reply = extract_reply(&resp);
        assert_eq!(reply.model.as_deref(), Some("actual-served-model-v2"));
        assert_ne!(
            reply.model.as_deref(),
            Some(requested_model),
            "reply.model reports the server's actual value, not an echo of the request"
        );
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
        // 5xx (503, transient capacity shedding) still classifies retryable
        // through this primitive's shared reuse path, same as dispatch_internal's
        // own corpus — confirmed here rather than assumed, since the
        // #1222 packet-2 relaxation touched the SAME function's final
        // guard and a regression there would silently widen or narrow
        // what single_shot_chat's callers see as retryable.
        match parse_hosted_response(br#"{"error":{"code":503,"message":"overloaded"}}"#) {
            Err(HostedCallError::RateLimited(msg)) => assert_eq!(msg, "overloaded"),
            _ => panic!("expected a retryable RateLimited error for 503"),
        }
        // Array-shaped errors (Google's OpenAI-compat layer) classify the
        // same way through the shared function — not re-derived, reused.
        match parse_hosted_response(
            br#"[{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"quota exceeded"}}]"#,
        ) {
            Err(HostedCallError::RateLimited(msg)) => assert!(msg.contains("quota exceeded")),
            _ => panic!("expected a retryable RateLimited error for an array-shaped 429"),
        }
        match parse_hosted_response(br#"[{"error":{"code":400,"message":"bad request"}}]"#) {
            Err(HostedCallError::Other(e)) => assert!(e.to_string().contains("bad request")),
            _ => panic!("expected a terminal Other error for an array-shaped non-429"),
        }
    }
}
