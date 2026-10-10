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

/// A Messages endpoint on loopback that answers the n-th request with
/// `replies[n]` (the last one repeats) and records every request body.
fn recording_messages_server(replies: Vec<String>) -> (String, Arc<Mutex<Vec<Value>>>) {
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
            let _ = sock.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
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
    let (url, bodies) = recording_messages_server(replies);
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
