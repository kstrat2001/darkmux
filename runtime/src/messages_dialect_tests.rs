use super::*;
use crate::lmstudio::{ChunkAccumulator, FunctionDef, ToolDef};

fn call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        kind: "function".into(),
        function: FunctionCall { name: name.into(), arguments: args.into() },
        extra_content: None,
    }
}

fn assistant_calling(text: Option<&str>, calls: Vec<ToolCall>) -> Message {
    Message { tool_calls: Some(calls), content: text.map(str::to_string), ..Message::assistant("") }
}

fn request(messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model: "claude-sonnet-5-5".into(),
        messages,
        tools: vec![ToolDef {
            kind: "function".into(),
            function: FunctionDef {
                name: "read".into(),
                description: "Read a file".into(),
                parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            },
        }],
        tool_choice: Some("auto".into()),
        temperature: 0.2,
        max_tokens: Some(4096),
        response_format: Some(json!({"type": "json_object"})),
    }
}

#[test]
fn a_conversation_becomes_the_messages_shape_with_caching_and_no_sampling_fields() {
    let req = request(vec![
        Message::system("You review diffs."),
        Message::user("Review this."),
        assistant_calling(Some("Reading."), vec![call("toolu_1", "read", r#"{"path":"a.rs"}"#)]),
        Message::tool_result("toolu_1", "read", "fn a() {}"),
        Message::system("[darkmux-runtime] 3 turns left"),
        assistant_calling(None, vec![call("toolu_2", "read", "")]),
        Message::tool_result("toolu_2", "read", "fn b() {}"),
    ]);
    let body = request_body(&req, true).unwrap();
    assert_eq!(
        body,
        json!({
            "model": "claude-sonnet-5-5",
            "max_tokens": 4096,
            "system": [{"type": "text", "text": "You review diffs.", "cache_control": {"type": "ephemeral"}}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Review this."}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Reading."},
                    {"type": "tool_use", "id": "toolu_1", "name": "read", "input": {"path": "a.rs"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "fn a() {}"},
                    {"type": "text", "text": "[darkmux-runtime] 3 turns left"},
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_2", "name": "read", "input": {}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_2", "content": "fn b() {}"},
                ]},
            ],
            "tools": [{
                "name": "read",
                "description": "Read a file",
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}},
            }],
            "tool_choice": {"type": "auto"},
            "cache_control": {"type": "ephemeral"},
            "stream": true,
        })
    );
}

#[test]
fn a_request_without_a_cap_is_refused_rather_than_given_a_guessed_one() {
    let mut req = request(vec![Message::user("hi")]);
    req.max_tokens = None;
    assert!(request_body(&req, false).unwrap_err().to_string().contains("max_tokens"));
}

#[test]
fn malformed_tool_arguments_still_replay_as_an_object() {
    assert_eq!(tool_input("not json"), json!({"arguments": "not json"}));
    assert_eq!(tool_input("[1]"), json!({"arguments": "[1]"}));
    assert_eq!(tool_input("  "), json!({}));
}

#[test]
fn usage_counts_cache_writes_and_reads_as_prompt_tokens_and_reads_as_cached() {
    let u = usage_counts(&json!({
        "input_tokens": 4, "cache_creation_input_tokens": 100, "cache_read_input_tokens": 17_610, "output_tokens": 9,
    }));
    assert_eq!(u, UsageCounts { prompt: Some(17_714), completion: Some(9), total: Some(17_723), reasoning: None, cached: Some(17_610) });
    let bare = usage_counts(&json!({"output_tokens": 9}));
    assert_eq!((bare.prompt, bare.total, bare.cached), (None, None, None));
}

#[test]
fn a_non_streamed_reply_reads_as_a_chat_response() {
    let r = response(&json!({
        "id": "msg_1", "model": "claude-sonnet-5-5", "stop_reason": "tool_use",
        "content": [
            {"type": "text", "text": "Checking."},
            {"type": "tool_use", "id": "toolu_9", "name": "read", "input": {"path": "b.rs"}},
        ],
        "usage": {"input_tokens": 10, "output_tokens": 3},
    }))
    .unwrap();
    let c = &r.choices[0];
    assert_eq!(c.finish_reason, "tool_calls");
    assert_eq!(c.message.content.as_deref(), Some("Checking."));
    let tc = &c.message.tool_calls.as_ref().unwrap()[0];
    assert_eq!((tc.id.as_str(), tc.function.name.as_str()), ("toolu_9", "read"));
    assert_eq!(serde_json::from_str::<Value>(&tc.function.arguments).unwrap(), json!({"path": "b.rs"}));
    assert_eq!(r.served_model(), Some("claude-sonnet-5-5"));
    assert_eq!(r.usage.unwrap().total, Some(13));
}

#[test]
fn finish_reasons_map_onto_the_chat_completions_spellings() {
    assert_eq!(finish_reason("end_turn"), "stop");
    assert_eq!(finish_reason("stop_sequence"), "stop");
    assert_eq!(finish_reason("tool_use"), "tool_calls");
    assert_eq!(finish_reason("max_tokens"), "length");
    assert_eq!(finish_reason("refusal"), "content_filter");
    assert_eq!(finish_reason("pause_turn"), "pause_turn");
}

/// A recorded-shape Messages stream (text, a tool call split across two
/// JSON deltas, the late usage) accumulates to the same response the
/// chat-completions path would build.
#[test]
fn a_streamed_reply_accumulates_text_tool_calls_and_combined_usage() {
    let events = [
        json!({"type": "message_start", "message": {"id": "msg_7", "model": "claude-sonnet-5-5",
            "usage": {"input_tokens": 5, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 900, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "ping"}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Let me "}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "look."}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_3", "name": "read", "input": {}}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "\"c.rs\"}"}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 40}}),
    ];
    let mut t = StreamTranslator::default();
    let mut acc = ChunkAccumulator::new();
    for e in &events {
        if let Translated::Chunk(c) = t.translate(e).unwrap() {
            acc.ingest(&c);
        }
    }
    assert!(matches!(t.translate(&json!({"type": "message_stop"})).unwrap(), Translated::Done));
    let r = acc.into_response();
    let c = &r.choices[0];
    assert_eq!(c.message.content.as_deref(), Some("Let me look."));
    assert_eq!(c.finish_reason, "tool_calls");
    let tc = &c.message.tool_calls.as_ref().unwrap()[0];
    assert_eq!((tc.id.as_str(), tc.function.name.as_str(), tc.function.arguments.as_str()), ("toolu_3", "read", r#"{"path":"c.rs"}"#));
    assert_eq!(r.id, "msg_7");
    let u = r.usage.unwrap();
    assert_eq!((u.prompt, u.completion, u.cached), (Some(905), Some(40), Some(900)));
}

#[test]
fn an_error_event_mid_stream_is_an_error_not_a_quiet_end() {
    let mut t = StreamTranslator::default();
    let e = t.translate(&json!({"type": "error", "error": {"type": "overloaded_error"}})).unwrap_err();
    assert!(e.to_string().contains("overloaded_error"), "{e}");
}

/// Serve one HTTP response on loopback and hand back the request it read
/// (head and body), so a test sees exactly what the client sent.
fn serve_once(response: &'static [u8]) -> (String, std::sync::mpsc::Receiver<String>) {
    use std::io::{BufRead, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut reader = std::io::BufReader::new(sock.try_clone().unwrap());
        let (mut request, mut len) = (String::new(), 0usize);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
            request.push_str(&line);
        }
        let mut body = vec![0u8; len];
        let _ = reader.read_exact(&mut body);
        request.push_str(&String::from_utf8_lossy(&body));
        let _ = sock.write_all(response);
        let _ = tx.send(request);
    });
    (format!("http://{addr}/v1/messages"), rx)
}

fn messages_client(url: &str) -> crate::lmstudio::LmStudioClient {
    crate::lmstudio::LmStudioClient::with_base_url("http://unused.invalid")
        .with_chat_url(url)
        .with_dialect(crate::lmstudio::Dialect::Messages)
        .with_auth_header("Authorization", "Bearer test-key")
}

/// The whole streaming path on real SSE bytes: the client sends the
/// Messages body with the version header, and the `event:`-framed stream
/// (which ends on `message_stop`, with no `[DONE]`) reads back as chunks.
#[test]
fn the_client_streams_a_messages_reply_over_the_wire() {
    let (url, seen) = serve_once(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_w\",\"model\":\"claude-sonnet-5-5\",\"usage\":{\"input_tokens\":3,\"cache_read_input_tokens\":50,\"output_tokens\":1}}}\n\n\
event: ping\ndata: {\"type\":\"ping\"}\n\n\
event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n\n\
event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    let client = messages_client(&url);
    let mut acc = ChunkAccumulator::new();
    for chunk in client.chat_streaming(&request(vec![Message::user("hi")])).unwrap() {
        acc.ingest(&chunk.unwrap());
    }
    let r = acc.into_response();
    assert_eq!(r.choices[0].message.content.as_deref(), Some("OK"));
    assert_eq!(r.choices[0].finish_reason, "stop");
    assert_eq!(r.usage.unwrap().cached, Some(50));
    let sent = seen.recv().unwrap().to_ascii_lowercase();
    assert!(sent.starts_with("post /v1/messages "), "{sent}");
    assert!(sent.contains("anthropic-version: 2023-06-01"), "{sent}");
    assert!(sent.contains(r#""cache_control":{"type":"ephemeral"}"#), "{sent}");
    assert!(sent.contains(r#""stream":true"#), "{sent}");
}

#[test]
fn the_client_reads_a_non_streamed_messages_reply() {
    let (url, seen) = serve_once(
        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n\
{\"id\":\"msg_n\",\"model\":\"claude-sonnet-5-5\",\"stop_reason\":\"end_turn\",\"content\":[{\"type\":\"text\",\"text\":\"done\"}],\"usage\":{\"input_tokens\":7,\"output_tokens\":1}}",
    );
    let r = messages_client(&url).chat(&request(vec![Message::user("hi")])).unwrap();
    assert_eq!(r.choices[0].message.content.as_deref(), Some("done"));
    let sent = seen.recv().unwrap().to_ascii_lowercase();
    assert!(sent.contains("anthropic-version: 2023-06-01"), "{sent}");
    assert!(!sent.contains(r#""stream""#), "{sent}");
}

/// The other dialects never send the Messages version header.
#[test]
fn a_chat_completions_client_sends_no_anthropic_version() {
    let (url, seen) = serve_once(
        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n\
{\"id\":\"c\",\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}",
    );
    let client = crate::lmstudio::LmStudioClient::with_base_url("http://unused.invalid")
        .with_chat_url(url.replace("/messages", "/chat/completions"))
        .with_dialect(crate::lmstudio::Dialect::ChatCompletions);
    client.chat(&request(vec![Message::user("hi")])).unwrap();
    assert!(!seen.recv().unwrap().to_ascii_lowercase().contains("anthropic-version"));
}


/// A cut turn handed back as a trailing assistant message (the checkpoint's
/// prefill) is followed by the continuation note, since Claude refuses a
/// conversation that ends on the assistant.
#[test]
fn a_trailing_assistant_prefill_is_followed_by_the_continuation_note() {
    let req = request(vec![Message::user("Write the file."), Message::assistant_prefill("fn main() {\n    let x = ")]);
    let body = request_body(&req, false).unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3, "{body}");
    assert_eq!(msgs[1], json!({"role": "assistant", "content": [{"type": "text", "text": "fn main() {\n    let x = "}]}));
    assert_eq!(msgs[2], json!({"role": "user", "content": [{"type": "text", "text": CONTINUE_CUT_TURN}]}));
    // A conversation that already ends on the user gets no note.
    let ends_on_user = request_body(&request(vec![Message::user("hi")]), false).unwrap();
    assert_eq!(ends_on_user["messages"].as_array().unwrap().len(), 1);
}

/// Two tool calls in one reply, their argument deltas interleaved, land in
/// their own slots.
#[test]
fn interleaved_tool_call_deltas_land_in_their_own_calls() {
    let events = [
        json!({"type": "message_start", "message": {"id": "m", "usage": {"input_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t_a", "name": "read"}}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "t_b", "name": "grep"}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"q\":"}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"path\":\"a\"}"}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "\"x\"}"}}),
    ];
    let mut t = StreamTranslator::default();
    let mut acc = ChunkAccumulator::new();
    for e in &events {
        if let Translated::Chunk(c) = t.translate(e).unwrap() {
            acc.ingest(&c);
        }
    }
    let calls = acc.into_response().choices[0].message.tool_calls.clone().unwrap();
    let got: Vec<(&str, &str, &str)> =
        calls.iter().map(|c| (c.id.as_str(), c.function.name.as_str(), c.function.arguments.as_str())).collect();
    assert_eq!(got, vec![("t_a", "read", r#"{"path":"a"}"#), ("t_b", "grep", r#"{"q":"x"}"#)]);
}

/// A stream cut before `message_delta` (a client-side checkpoint, an idle
/// cut) still reports the prompt it was served.
#[test]
fn a_stream_cut_before_its_final_delta_still_reports_the_prompt() {
    let mut t = StreamTranslator::default();
    let mut acc = ChunkAccumulator::new();
    let start = json!({"type": "message_start", "message": {"id": "m",
        "usage": {"input_tokens": 5, "cache_read_input_tokens": 50_000, "output_tokens": 1}}});
    if let Translated::Chunk(c) = t.translate(&start).unwrap() {
        acc.ingest(&c);
    }
    let u = acc.into_response().usage.unwrap();
    assert_eq!((u.prompt, u.cached, u.completion, u.total), (Some(50_005), Some(50_000), None, None));
}

/// The chat-completions stream still ends at the first unparseable chunk:
/// a valid chunk after it is never read.
#[test]
fn a_bad_chat_completions_chunk_ends_the_stream() {
    use crate::lmstudio::ChunkStream;
    let sse = "data: {not json}\n\ndata: {\"id\":\"c\",\"choices\":[]}\n\n";
    let mut s = ChunkStream::new(std::io::BufReader::new(sse.as_bytes()));
    assert!(s.next().unwrap().is_err());
    assert!(s.next().is_none(), "nothing is read after a parse failure");
}
