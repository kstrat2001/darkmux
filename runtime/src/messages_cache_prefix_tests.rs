//! (#3193, #3202) Every engine the loop talks to caches the prompt prefix:
//! the `messages` dialect's automatic breakpoint on each request's last
//! block, and the KV cache LM Studio, lithos-metal and Splash keep behind
//! chat-completions. A later request reuses that cache only when its own body
//! starts with exactly the same messages. So in a real multi-turn loop, every
//! request's messages, through its last one, must be a prefix of the next
//! request's. These tests drive the real loop against a mock endpoint of each
//! dialect that records every request body, and check that.

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

/// (#3202) Every dialect the loop speaks. The engine behind each one caches
/// the prompt prefix (the Messages API's breakpoint; LM Studio, lithos-metal
/// and Splash reuse their KV cache), so the property below holds on all.
const DIALECTS: [Dialect; 3] = [Dialect::Messages, Dialect::ChatCompletions, Dialect::ChatCompletionsMaxTokens];

/// A client for `dialect` pointed at the recording server.
fn client_for(dialect: Dialect, url: &str) -> LmStudioClient {
    let client = LmStudioClient::with_base_url("http://unused.invalid").with_dialect(dialect);
    match dialect {
        Dialect::Messages => client.with_chat_url(url).with_auth_header("x-api-key", "test-key"),
        _ => client.with_chat_url(url.replace("/messages", "/chat/completions")),
    }
}

/// The Messages client is driven streaming (SSE replies); the
/// chat-completions clients take one JSON reply per request.
fn streams(dialect: Dialect) -> bool {
    dialect == Dialect::Messages
}

/// One scripted reply in `dialect`'s wire shape: optional text, then tool
/// calls `(id, name, input JSON)`.
fn reply(dialect: Dialect, text: Option<&str>, calls: &[(&str, &str, &str)]) -> String {
    if dialect == Dialect::Messages {
        return sse_reply(text, calls);
    }
    let tool_calls: Vec<Value> = calls
        .iter()
        .map(|(id, name, input)| json!({"id": id, "type": "function", "function": {"name": name, "arguments": input}}))
        .collect();
    let (tool_calls, finish) =
        if calls.is_empty() { (None, "stop") } else { (Some(Value::Array(tool_calls)), "tool_calls") };
    super::tests::chat_response_json(text, tool_calls, finish, 10, 2).to_string()
}

/// The first point where `next` stops extending `prev` through `prev`'s
/// last message, as `(message index, prev's message, next's message)`.
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
        // The last message. On the Messages dialect its blocks must open
        // `next`'s message at `i` (`next` may have merged more blocks into
        // the same turn); on chat-completions it must be unchanged.
        let extends = match (p["content"].as_array(), n["content"].as_array()) {
            (Some(pb), Some(nb)) => p["role"] == n["role"] && nb.len() >= pb.len() && nb[..pb.len()] == pb[..],
            _ => p == n,
        };
        if !extends {
            return Some((i, p.clone(), n.clone()));
        }
    }
    None
}

/// Every tool result in one request body, as `(call id, body)`, in either
/// dialect's shape.
fn tool_results(body: &Value) -> Vec<(String, String)> {
    let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let mut out = Vec::new();
    for m in body["messages"].as_array().into_iter().flatten() {
        if m["role"] == "tool" {
            out.push((text(&m["tool_call_id"]), text(&m["content"])));
        }
        for block in m["content"].as_array().into_iter().flatten() {
            if block["type"] == "tool_result" {
                out.push((text(&block["tool_use_id"]), text(&block["content"])));
            }
        }
    }
    out
}

/// Each consecutive pair of recorded requests: the tools and system prompt
/// are unchanged, and the later request's messages extend the earlier one's
/// through its last message.
fn assert_each_request_extends_the_previous(dialect: Dialect, bodies: &[Value]) {
    for (n, pair) in bodies.windows(2).enumerate() {
        // The cached prefix opens with the tools and the system prompt.
        assert_eq!(pair[0]["tools"], pair[1]["tools"], "{dialect:?}: request {} changes the tools", n + 2);
        assert_eq!(pair[0]["system"], pair[1]["system"], "{dialect:?}: request {} changes the system prompt", n + 2);
        let (prev, next) = (pair[0]["messages"].as_array().unwrap(), pair[1]["messages"].as_array().unwrap());
        if let Some((i, p, q)) = first_divergence(prev, next) {
            let (p, q) = (p.to_string(), q.to_string());
            panic!(
                "{dialect:?}: request {} stops extending request {} at message {i}:\n  request {}: …{}…\n  request {}: …{}…",
                n + 2,
                n + 1,
                n + 1,
                around_first_difference(&p, &q),
                n + 2,
                around_first_difference(&q, &p)
            );
        }
    }
}

/// Every large tool result the run appended (all but `seeded`) is already in
/// its final, trimmed form in every request that carries it, the first one
/// included.
fn assert_large_results_sent_in_final_form(dialect: Dialect, bodies: &[Value], seeded: &[&str]) {
    for (n, body) in bodies.iter().enumerate() {
        for (id, result) in tool_results(body).into_iter().filter(|(id, _)| !seeded.contains(&id.as_str())) {
            assert!(
                result.contains(crate::tool_result_prune::TOOL_RESULT_TRIM_MARKER_SENTINEL)
                    && crate::tool_result_prune::soft_trim_body(&result).is_none(),
                "{dialect:?}: request {} sends {id}'s {}-byte result before its final form",
                n + 1,
                result.len()
            );
        }
    }
}

/// Run `replies` from a two-message start on `dialect` and return every
/// request body the endpoint saw.
fn run_script(dialect: Dialect, replies: Vec<String>, max_turns: u32) -> Vec<Value> {
    let (url, bodies) = recording_server(replies);
    let client = client_for(dialect, &url);
    let tmp = tempfile::Builder::new().prefix("cache-prefix").tempdir().unwrap();
    let mut traj = Trajectory::open(tmp.path());
    run_with_sleeper(
        &client, &client, "claude-test", vec![Message::system("system prompt"), Message::user("do the task")],
        &[Tool::Echo], &mut traj, streams(dialect), &compaction::CompactionConfig::never_compact(), Some(max_turns),
        None, Some(4_096), None, Some(u32::MAX), None, std::collections::BTreeMap::new(), None, tmp.path(),
        "test-role", None, &RealSleeper,
    )
    .expect("the run completes");
    let seen = bodies.lock().unwrap().clone();
    seen
}

/// (#3193, #3202) On every dialect, each request's messages extend the
/// previous request's unchanged, and a large result goes out in its final
/// form the first time it is sent.
#[test]
#[serial_test::serial]
fn each_request_extends_the_previous_one_on_every_dialect() {
    // Echo results over the soft-trim threshold, several per turn, as a
    // review's file reads are.
    let big = |tag: &str| format!(r#"{{"text":"{tag} {}"}}"#, "x".repeat(5_000));
    let (one, two, three, four, five) = (big("one"), big("two"), big("three"), big("four"), big("five"));
    for dialect in DIALECTS {
        let replies = vec![
            reply(dialect, Some("Looking."), &[("toolu_1", "echo", &one), ("toolu_2", "echo", &two)]),
            reply(dialect, None, &[("toolu_3", "echo", &three), ("toolu_4", "echo", &four)]),
            reply(dialect, Some("Again."), &[("toolu_5", "echo", &five)]),
            reply(dialect, None, &[("toolu_6", "echo", &one)]),
            reply(dialect, None, &[("toolu_7", "echo", &two)]),
            reply(dialect, Some("All done."), &[]),
        ];
        let bodies = run_script(dialect, replies, 10);
        assert_eq!(bodies.len(), 6, "{dialect:?}: a six-turn run sends six requests: {}", bodies.len());
        assert_each_request_extends_the_previous(dialect, &bodies);
        assert_large_results_sent_in_final_form(dialect, &bodies, &[]);
    }
}

/// (#1391, #3202) The context stays bounded on a long run with many large
/// results: no tool result is ever sent over the trim threshold, so the
/// tool-result bytes in the last request grow by at most the threshold per
/// result instead of by each result's full size.
#[test]
#[serial_test::serial]
fn a_long_run_with_many_large_results_stays_bounded() {
    const TURNS: usize = 30;
    const RESULT_BYTES: usize = 20_000;
    let threshold = crate::tool_result_prune::TOOL_RESULT_TRIM_THRESHOLD_BYTES;
    let dialect = Dialect::ChatCompletions;
    let calls: Vec<(String, String)> = (0..TURNS)
        .map(|n| (format!("call_{n}"), format!(r#"{{"text":"{n} {}"}}"#, "x".repeat(RESULT_BYTES))))
        .collect();
    let mut replies: Vec<String> =
        calls.iter().map(|(id, input)| reply(dialect, None, &[(id.as_str(), "echo", input.as_str())])).collect();
    replies.push(reply(dialect, Some("All done."), &[]));
    let bodies = run_script(dialect, replies, TURNS as u32 + 5);
    assert_eq!(bodies.len(), TURNS + 1, "one request per turn");
    // (#3202) The cost of the run's requests on the chars/4 estimate: every
    // request's whole prompt, and the part past the previous request's
    // unchanged messages (what a prefix-caching engine prefills again).
    let chars = |v: &Value| v.to_string().len();
    let total: usize = bodies.iter().map(chars).sum();
    let uncached: usize = std::iter::once(chars(&bodies[0]))
        .chain(bodies.windows(2).map(|pair| {
            let (prev, next) = (pair[0]["messages"].as_array().unwrap(), pair[1]["messages"].as_array().unwrap());
            let shared = prev.iter().zip(next).take_while(|(p, q)| p == q).count();
            next[shared..].iter().map(chars).sum::<usize>()
        }))
        .sum();
    eprintln!(
        "#3202 cost: {} requests, ~{} prompt tokens in all, ~{} past the unchanged prefix",
        bodies.len(),
        total / 4,
        uncached / 4
    );
    for (n, body) in bodies.iter().enumerate() {
        for (id, result) in tool_results(body) {
            assert!(
                result.len() <= threshold,
                "request {} sends {id}'s result at {} bytes, over the trim threshold",
                n + 1,
                result.len()
            );
        }
    }
    let last_results: usize = tool_results(bodies.last().unwrap()).iter().map(|(_, r)| r.len()).sum();
    assert!(
        last_results <= TURNS * threshold,
        "the last request carries {last_results} bytes of tool results for {TURNS} results"
    );
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

/// Resume [`killed_mid_turn`] on `dialect`: one more turn that echoes
/// another large result, then a turn that ends the run. Returns the request
/// bodies the endpoint saw.
fn resume_twice(dialect: Dialect) -> Vec<Value> {
    let seven = format!(r#"{{"text":"seven {}"}}"#, "x".repeat(5_000));
    let replies = vec![reply(dialect, None, &[("toolu_7", "echo", &seven)]), reply(dialect, Some("All done."), &[])];
    let (url, bodies) = recording_server(replies);
    let client = client_for(dialect, &url);
    let tmp = tempfile::Builder::new().prefix("cache-prefix-resume").tempdir().unwrap();
    let mut traj = Trajectory::open(tmp.path());
    run_with_sleeper(
        &client, &client, "claude-test", vec![], &[Tool::Echo], &mut traj, streams(dialect),
        &compaction::CompactionConfig::never_compact(), Some(10), None, Some(4_096), None, Some(u32::MAX), None,
        std::collections::BTreeMap::new(), None, tmp.path(), "test-role", Some(killed_mid_turn()), &RealSleeper,
    )
    .expect("the resumed run completes");
    let seen = bodies.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "{dialect:?}: the resume makes two requests");
    seen
}

/// (#3193, #3202) After a resume, on every dialect: the result the request
/// before the kill already sent goes out unchanged, the catch-up's results
/// and every later one go out in final form, and each request extends the
/// previous one.
#[test]
#[serial_test::serial]
fn a_resume_extends_each_request_on_every_dialect() {
    for dialect in DIALECTS {
        let bodies = resume_twice(dialect);
        assert_each_request_extends_the_previous(dialect, &bodies);
        let first = tool_results(&bodies[0]);
        assert_eq!(first[0].0, "toolu_1", "{dialect:?}: {}", bodies[0]);
        assert_eq!(first[0].1, format!("one {}", "x".repeat(5_000)), "{dialect:?}: the sent result is not rewritten");
        assert_large_results_sent_in_final_form(dialect, &bodies, &["toolu_1"]);
    }
}
