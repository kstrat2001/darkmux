use super::*;

/// `body` reassembled, when it is a finished SSE stream.
fn ok(body: &str) -> Vec<u8> {
    assemble(body.as_bytes()).expect("an SSE body").expect("a finished stream")
}

/// The shape `mlx_lm.server` and LM Studio stream: content deltas, a finish
/// event, then a usage-only event (`stream_options.include_usage`).
const SSE: &str = "data: {\"model\":\"m\",\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
data: {\"model\":\"m\",\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n\n\
data: {\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
data: {\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n\
data: {\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"total_tokens\":10}}\n\n\
data: [DONE]\n\n";

#[test]
fn a_streamed_reply_is_put_back_together_as_the_unstreamed_one() {
    let reply: Value = serde_json::from_slice(&ok(SSE)).unwrap();
    assert_eq!(reply["choices"][0]["message"]["content"], "Hello");
    assert_eq!(reply["choices"][0]["message"]["reasoning_content"], "think");
    assert_eq!(reply["choices"][0]["finish_reason"], "stop");
    assert_eq!(reply["usage"]["total_tokens"], 10);
    assert_eq!(reply["model"], "m");
    // The readers of every reply read it as they read an unstreamed one.
    let parsed = crate::dispatch_internal::parse_hosted_response(&ok(SSE)).expect("classifies as Ok");
    let out = crate::single_shot::extract_reply(&parsed);
    assert_eq!((out.content.as_str(), out.counts.total, out.counts.prompt), ("Hello", Some(10), Some(7)));
}

#[test]
fn a_body_that_is_not_sse_is_left_for_the_ordinary_reading() {
    assert_eq!(assemble(br#"{"error":{"message":"bad key"}}"#), None);
    assert_eq!(assemble(br#"{"choices":[{"message":{"content":"x"}}]}"#), None);
}

#[test]
fn an_error_event_in_the_stream_reaches_the_error_classification() {
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\ndata: {\"error\":{\"code\":429,\"message\":\"quota\"}}\n\n";
    let bytes = ok(body);
    match crate::dispatch_internal::parse_hosted_response(&bytes) {
        Err(crate::dispatch_internal::HostedCallError::RateLimited(msg)) => assert_eq!(msg, "quota"),
        other => panic!("expected the stream's 429 to classify as rate-limited, got {other:?}"),
    }
}

#[test]
fn the_tail_counts_whole_events_across_split_reads() {
    let mut tail = SseTail::default();
    let bytes = SSE.as_bytes();
    // Split mid-line: nothing completes until its newline arrives.
    let cut = SSE.find("Hel").unwrap();
    assert!(tail.feed(&bytes[..cut]), "the first two events are complete");
    let counts = |p: &SseProgress| (p.content_chars, p.generated_chars, p.chunks);
    assert_eq!(counts(&tail.progress), (0, 5, 2));
    assert!(tail.feed(&bytes[cut..]));
    assert_eq!(counts(&tail.progress), (5, 10, 5));
    assert!(!tail.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}"), "no newline yet");
}

#[test]
fn a_streamed_body_asks_for_the_usage_block() {
    let body = stream_body(&serde_json::json!({ "model": "m", "stream": false }));
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["model"], "m");
}

#[test]
fn the_gate_lets_the_first_event_through_then_one_per_interval() {
    let t0 = std::time::Instant::now();
    let mut gate = HeartbeatGate::new(std::time::Duration::from_secs(2));
    assert!(gate.due(t0));
    assert!(!gate.due(t0 + std::time::Duration::from_millis(1999)));
    assert!(gate.due(t0 + std::time::Duration::from_secs(2)));
}

/// (#3211 review F1) OpenRouter sends a comment before its events, and a
/// stream may name its events: both are SSE, read as such.
#[test]
fn a_stream_opening_with_a_comment_or_an_event_line_is_read_as_sse() {
    let reply: Value = serde_json::from_slice(&ok(&format!(": OPENROUTER PROCESSING\n\n{SSE}"))).unwrap();
    assert_eq!(reply["choices"][0]["message"]["content"], "Hello");
    let err = ok("event: error\ndata: {\"error\":{\"code\":429,\"message\":\"quota\"}}\n\n");
    assert!(matches!(
        crate::dispatch_internal::parse_hosted_response(&err),
        Err(crate::dispatch_internal::HostedCallError::RateLimited(m)) if m == "quota"
    ));
}

/// (#3211 review F2) A stream cut off before `[DONE]` or a finish reason is
/// never read as a whole reply; either marker alone is enough.
#[test]
fn a_stream_cut_off_mid_way_is_not_a_reply() {
    let cut = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n";
    assert!(matches!(assemble(cut.as_bytes()), Some(Err(_))));
    assert!(assemble(format!("{cut}data: [DONE]\n\n").as_bytes()).unwrap().is_ok());
    let finish = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"length\"}]}\n\n";
    assert!(assemble(finish.as_bytes()).unwrap().is_ok());
}

/// (#3211 review F5) A server sending the reasoning under both names sends
/// one text twice: it is read once.
#[test]
fn reasoning_under_both_names_is_counted_once() {
    let both = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"abc\",\"reasoning\":\"abc\"},\"finish_reason\":\"stop\"}]}\n\n";
    let reply: Value = serde_json::from_slice(&ok(both)).unwrap();
    assert_eq!(reply["choices"][0]["message"]["reasoning_content"], "abc");
    let mut tail = SseTail::default();
    tail.feed(both.as_bytes());
    assert_eq!(tail.progress.generated_chars, 3);
    // An empty placeholder under one name never hides the other's text.
    let placeholder = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"\",\"reasoning\":\"xy\"},\"finish_reason\":\"stop\"}]}\n\n";
    let reply: Value = serde_json::from_slice(&ok(placeholder)).unwrap();
    assert_eq!(reply["choices"][0]["message"]["reasoning_content"], "xy");
    let mut tail = SseTail::default();
    tail.feed(placeholder.as_bytes());
    assert_eq!(tail.progress.generated_chars, 2);
    // Sent under `reasoning` alone, it is still read.
    let only = "data: {\"choices\":[{\"delta\":{\"reasoning\":\"xy\"},\"finish_reason\":\"stop\"}]}\n\n";
    let reply: Value = serde_json::from_slice(&ok(only)).unwrap();
    assert_eq!(reply["choices"][0]["message"]["reasoning_content"], "xy");
}

/// (#3211 review F4) The model time runs from the streaming attempt's send,
/// so a retried call's backoff is not in it.
#[test]
fn the_model_time_starts_at_the_streaming_attempts_send() {
    use std::cell::RefCell;
    let emitted = RefCell::new(Vec::new());
    let mut ev = TurnEvidence::open(darkmux_types::Dialect::ChatCompletions, 10, std::time::Duration::from_secs(2), |p| emitted.borrow_mut().push(p));
    let sent = std::time::Instant::now() - std::time::Duration::from_secs(5);
    let earlier = sent - std::time::Duration::from_secs(60);
    let (reply, body) = ev.send(
        serde_json::json!({ "model": "m" }),
        |_, progress| {
            let on = progress.expect("a streamed call");
            // An earlier attempt streamed too (then was retried): the last
            // attempt's send is the model time's start.
            on(&SseProgress { started: Some(earlier), chunks: 1, ..SseProgress::default() });
            on(&SseProgress { started: Some(sent), chunks: 1, ..SseProgress::default() });
            Ok(serde_json::json!({ "choices": [{ "finish_reason": "stop" }] }))
        },
        |_| false,
    );
    assert_eq!(body["stream"], true);
    ev.ended(&reply.unwrap(), &darkmux_trajectory::UsageCounts::default());
    let ms = emitted.borrow().iter().find_map(|p| match p {
        darkmux_flow::Payload::DispatchTurn(t) => t.generation_ms,
        _ => None,
    });
    assert!(ms.is_some_and(|ms| (5000..6000).contains(&ms)), "{ms:?}");
}

/// (#3211 review F3) An endpoint refusing the streamed request is asked
/// once more, unstreamed, and the turn is then not a streamed one.
#[test]
fn a_refused_stream_request_is_sent_again_unstreamed() {
    let sent = std::cell::RefCell::new(Vec::new());
    let mut ev = TurnEvidence::open(darkmux_types::Dialect::ChatCompletions, 10, std::time::Duration::from_secs(2), |_| {});
    let (reply, body) = ev.send(
        serde_json::json!({ "model": "m" }),
        |body, _| {
            sent.borrow_mut().push(body.clone());
            if body.get("stream").is_some() { Err(anyhow::anyhow!("refused")) } else { Ok(serde_json::json!({})) }
        },
        |_| true,
    );
    assert!(reply.is_ok());
    assert_eq!(sent.borrow().len(), 2);
    assert!(body.get("stream").is_none(), "the body sent last: {body}");
    assert!(!ev.streams);
}
