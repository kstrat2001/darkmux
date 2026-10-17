use super::*;

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
    let reply: Value = serde_json::from_slice(&assemble(SSE.as_bytes()).expect("an SSE body")).unwrap();
    assert_eq!(reply["choices"][0]["message"]["content"], "Hello");
    assert_eq!(reply["choices"][0]["message"]["reasoning_content"], "think");
    assert_eq!(reply["choices"][0]["finish_reason"], "stop");
    assert_eq!(reply["usage"]["total_tokens"], 10);
    assert_eq!(reply["model"], "m");
    // The readers of every reply read it as they read an unstreamed one.
    let parsed = crate::dispatch_internal::parse_hosted_response(&assemble(SSE.as_bytes()).unwrap()).expect("classifies as Ok");
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
    let bytes = assemble(body.as_bytes()).unwrap();
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
    assert_eq!(tail.progress, SseProgress { content_chars: 0, generated_chars: 5, chunks: 2 });
    assert!(tail.feed(&bytes[cut..]));
    assert_eq!(tail.progress, SseProgress { content_chars: 5, generated_chars: 10, chunks: 5 });
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
