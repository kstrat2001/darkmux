//! (#3193) The `messages` dialect's automatic cache breakpoint sits on each
//! request's last block, and a later request reads that cache entry back
//! only when its own body starts with the exact same blocks. So in a real
//! multi-turn loop, every request's messages, through its last block, must be
//! a prefix of the next request's. These tests drive the real loop against a
//! mock Messages server that records every request body, and check that.

use super::*;
use crate::lmstudio::{Dialect, LmStudioClient, Message};
use crate::tools::Tool;
use crate::trajectory::Trajectory;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// One scripted Messages reply as SSE: optional text, then tool calls
/// `(id, name, input JSON)`, ending on `tool_use` (or `end_turn` with none).
fn sse_reply(text: Option<&str>, calls: &[(&str, &str, &str)]) -> String {
    let mut events = vec![json!({"type": "message_start", "message": {
        "id": "msg_t", "model": "claude-test",
        "usage": {"input_tokens": 10, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0, "output_tokens": 1}}})];
    let mut index = 0;
    if let Some(t) = text {
        events.push(json!({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}}));
        events.push(json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": t}}));
        events.push(json!({"type": "content_block_stop", "index": index}));
        index += 1;
    }
    for (id, name, input) in calls {
        events.push(json!({"type": "content_block_start", "index": index,
            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}));
        events.push(json!({"type": "content_block_delta", "index": index,
            "delta": {"type": "input_json_delta", "partial_json": input}}));
        events.push(json!({"type": "content_block_stop", "index": index}));
        index += 1;
    }
    let stop = if calls.is_empty() { "end_turn" } else { "tool_use" };
    events.push(json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 5}}));
    events.push(json!({"type": "message_stop"}));
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// An endpoint on loopback that answers the n-th request with `replies[n]`
/// (the last one repeats; SSE when it opens with `event:`, else JSON) and
/// records every request body.
fn recording_server(replies: Vec<String>) -> (String, Arc<Mutex<Vec<Value>>>) {
    use std::io::{BufRead, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&bodies);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut sock) = conn else { return };
            let mut reader = std::io::BufReader::new(sock.try_clone().unwrap());
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            let _ = reader.read_exact(&mut body);
            let n = {
                let mut all = seen.lock().unwrap();
                all.push(serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null));
                all.len() - 1
            };
            let reply = &replies[n.min(replies.len() - 1)];
            let kind = if reply.starts_with("event:") { "text/event-stream" } else { "application/json" };
            let _ = sock.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .as_bytes(),
            );
        }
    });
    (format!("http://{addr}/v1/messages"), bodies)
}

/// The first point where `next` stops extending `prev` through `prev`'s
/// last block, as `(message index, prev's message, next's message)`.
fn first_divergence(prev: &[Value], next: &[Value]) -> Option<(usize, Value, Value)> {
    for (i, p) in prev.iter().enumerate() {
        let Some(n) = next.get(i) else {
            return Some((i, p.clone(), Value::Null));
        };
        if i + 1 < prev.len() {
            if p != n {
                return Some((i, p.clone(), n.clone()));
            }
            continue;
        }
        // The last message: its blocks must open `next`'s message at `i`
        // (`next` may have merged more blocks into the same turn).
        let (pb, nb) = (p["content"].as_array().unwrap(), n["content"].as_array().unwrap());
        if p["role"] != n["role"] || nb.len() < pb.len() || nb[..pb.len()] != pb[..] {
            return Some((i, p.clone(), n.clone()));
        }
    }
    None
}

#[test]
#[serial_test::serial]
fn each_messages_request_extends_the_previous_one_through_its_last_block() {
    // Echo results over the soft-trim threshold, several per turn, as a
    // review's file reads are: old ones age out of the protected window.
    let big = |tag: &str| format!(r#"{{"text":"{tag} {}"}}"#, "x".repeat(5_000));
    let (one, two, three, four, five) = (big("one"), big("two"), big("three"), big("four"), big("five"));
    let replies = vec![
        sse_reply(Some("Looking."), &[("toolu_1", "echo", &one), ("toolu_2", "echo", &two)]),
        sse_reply(None, &[("toolu_3", "echo", &three), ("toolu_4", "echo", &four)]),
        sse_reply(Some("Again."), &[("toolu_5", "echo", &five)]),
        sse_reply(None, &[("toolu_6", "echo", &one)]),
        sse_reply(None, &[("toolu_7", "echo", &two)]),
        sse_reply(Some("All done."), &[]),
    ];
    let (url, bodies) = recording_server(replies);
    let client = LmStudioClient::with_base_url("http://unused.invalid")
        .with_chat_url(url)
        .with_dialect(Dialect::Messages)
        .with_auth_header("x-api-key", "test-key");
    let tmp = tempfile::Builder::new().prefix("cache-prefix").tempdir().unwrap();
    let mut traj = Trajectory::open(tmp.path());
    run_with_sleeper(
        &client, &client, "claude-test", vec![Message::system("system prompt"), Message::user("do the task")],
        &[Tool::Echo], &mut traj, true, &compaction::CompactionConfig::never_compact(), Some(10), None,
        Some(4_096), None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None, tmp.path(),
        "test-role", None, &RealSleeper,
    )
    .expect("the run completes");
    let bodies = bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 6, "a six-turn run sends six requests: {}", bodies.len());
    for (n, pair) in bodies.windows(2).enumerate() {
        // The cached prefix opens with the tools and the system prompt.
        assert_eq!(pair[0]["tools"], pair[1]["tools"], "request {} changes the tools", n + 2);
        assert_eq!(pair[0]["system"], pair[1]["system"], "request {} changes the system prompt", n + 2);
        let (prev, next) = (pair[0]["messages"].as_array().unwrap(), pair[1]["messages"].as_array().unwrap());
        if let Some((i, p, q)) = first_divergence(prev, next) {
            let (p, q) = (p.to_string(), q.to_string());
            panic!(
                "request {} stops extending request {} at message {i}:\n  request {}: …{}…\n  request {}: …{}…",
                n + 2, n + 1, n + 1, around_first_difference(&p, &q), n + 2, around_first_difference(&q, &p)
            );
        }
    }
}

/// `a` around the first byte where it differs from `b`, so a failure shows
/// the divergence rather than two whole messages.
fn around_first_difference(a: &str, b: &str) -> String {
    let at = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    let (start, end) = (at.saturating_sub(40), (at + 100).min(a.len()));
    a.get(start..end).unwrap_or(a).to_string()
}

/// A checkpoint killed mid-turn: one large echo result already recorded,
/// then an assistant turn whose five large echo calls are all pending. The
/// resume's catch-up appends their five results, which pushes the first
/// result out of the soft trim's protected window.
fn killed_mid_turn() -> checkpoint::RunCheckpoint {
    let call = |id: &str, tag: &str| crate::lmstudio::ToolCall {
        id: id.into(),
        kind: "function".into(),
        function: crate::lmstudio::FunctionCall {
            name: "echo".into(),
            arguments: format!(r#"{{"text":"{tag} {}"}}"#, "x".repeat(5_000)),
        },
        extra_content: None,
    };
    let assistant = |calls: Vec<crate::lmstudio::ToolCall>| Message {
        role: "assistant".into(),
        content: None,
        tool_calls: Some(calls),
        tool_call_id: None,
        name: None,
        reasoning_content: None,
    };
    let pending: Vec<_> = ["two", "three", "four", "five", "six"]
        .iter()
        .enumerate()
        .map(|(i, tag)| call(&format!("toolu_{}", i + 2), tag))
        .collect();
    checkpoint::RunCheckpoint {
        schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
        role_id: "test-role".into(),
        messages: vec![
            Message::system("system prompt"),
            Message::user("do the task"),
            assistant(vec![call("toolu_1", "one")]),
            Message::tool_result("toolu_1", "echo", format!("one {}", "x".repeat(5_000))),
            assistant(pending.clone()),
        ],
        turns: 2,
        total_completion_tokens: 10,
        compactions: 0,
        pending_hand_back: None,
        pending_tool_calls: Some(pending),
        pending_tool_calls_seq_base: 0,
        pending_head_started: false,
        written_at_unix_ms: checkpoint::unix_ms(),
    }
}

/// Resume [`killed_mid_turn`] against `client` (one more turn, which ends
/// the run) and return the request bodies the endpoint saw.
fn resume_once(client: &LmStudioClient, streaming: bool, bodies: &Arc<Mutex<Vec<Value>>>) -> Vec<Value> {
    let tmp = tempfile::Builder::new().prefix("cache-prefix-resume").tempdir().unwrap();
    let mut traj = Trajectory::open(tmp.path());
    run_with_sleeper(
        client, client, "claude-test", vec![], &[Tool::Echo], &mut traj, streaming,
        &compaction::CompactionConfig::never_compact(), Some(10), None, Some(4_096), None, Some(u32::MAX), None,
        std::collections::BTreeMap::new(), None, tmp.path(), "test-role", Some(killed_mid_turn()), &RealSleeper,
    )
    .expect("the resumed run completes");
    let seen = bodies.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "the resume makes one request");
    seen
}

/// (#3193) The post-resume check (`compact_after_resume`) does not elide a
/// result the request before the kill already sent: the first request after
/// a resume still extends the cache entry that request wrote.
#[test]
#[serial_test::serial]
fn a_resume_on_the_messages_dialect_does_not_elide_an_earlier_result() {
    let (url, bodies) = recording_server(vec![sse_reply(Some("All done."), &[])]);
    let client = LmStudioClient::with_base_url("http://unused.invalid")
        .with_chat_url(url)
        .with_dialect(Dialect::Messages)
        .with_auth_header("x-api-key", "test-key");
    let first = &resume_once(&client, true, &bodies)[0];
    let result = &first["messages"][2]["content"][0];
    assert_eq!(result["tool_use_id"], "toolu_1", "{first}");
    assert_eq!(result["content"], format!("one {}", "x".repeat(5_000)), "the first result is sent whole");
}

/// (#1391) The other side of the guard: an endpoint that does not cache the
/// prompt prefix still has old results soft-trimmed after a resume.
#[test]
#[serial_test::serial]
fn a_resume_on_a_chat_completions_dialect_still_soft_trims_an_old_result() {
    let reply = super::tests::chat_response_json(Some("All done."), None, "stop", 10, 2).to_string();
    let (url, bodies) = recording_server(vec![reply]);
    let client = LmStudioClient::with_base_url("http://unused.invalid")
        .with_chat_url(url.replace("/messages", "/chat/completions"))
        .with_dialect(Dialect::ChatCompletions);
    let first = &resume_once(&client, false, &bodies)[0];
    let result = &first["messages"][3];
    assert_eq!(result["tool_call_id"], "toolu_1", "{first}");
    let body = result["content"].as_str().unwrap();
    assert!(body.contains(crate::tool_result_prune::TOOL_RESULT_TRIM_MARKER_SENTINEL), "the old result is trimmed");
}
