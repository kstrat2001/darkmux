use super::*;

/// `body` reassembled, when it is a finished SSE stream.
fn ok(body: &str) -> Vec<u8> {
    assemble(body.as_bytes(), Shape::Chat).expect("an SSE body").expect("a finished stream")
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
    assert_eq!(assemble(br#"{"error":{"message":"bad key"}}"#, Shape::Chat), None);
    assert_eq!(assemble(br#"{"choices":[{"message":{"content":"x"}}]}"#, Shape::Chat), None);
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
    let body = stream_body(&serde_json::json!({ "model": "m", "stream": false }), Shape::Chat);
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
    assert!(matches!(assemble(cut.as_bytes(), Shape::Chat), Some(Err(_))));
    assert!(assemble(format!("{cut}data: [DONE]\n\n").as_bytes(), Shape::Chat).unwrap().is_ok());
    let finish = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"length\"}]}\n\n";
    assert!(assemble(finish.as_bytes(), Shape::Chat).unwrap().is_ok());
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

/// (#316 4.6) The Messages stream: a thinking block, a ping, two text
/// deltas, the stop reason and output count on `message_delta`, then
/// `message_stop`. The input and cache counts arrive on `message_start`.
const MSSE: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-5-5\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":30,\"output_tokens\":1}}}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: ping\n\
data: {\"type\":\"ping\"}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":5}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

/// A Messages SSE body, reassembled and read the way `finish_reply` reads it.
fn messages_reply(body: &str) -> Result<Value, crate::dispatch_internal::HostedCallError> {
    let bytes = assemble(body.as_bytes(), Shape::Messages).expect("an SSE body").expect("a finished stream");
    crate::dispatch_internal::parse_hosted_response(&crate::messages_wire::reply_bytes_as_chat(true, bytes))
}

#[test]
fn a_streamed_messages_reply_is_put_back_together_as_the_unstreamed_one() {
    let parsed = messages_reply(MSSE).expect("classifies as Ok");
    assert_eq!(parsed["choices"][0]["finish_reason"], "stop");
    assert_eq!(parsed["model"], "claude-sonnet-5-5");
    let out = crate::single_shot::extract_reply(&parsed);
    assert_eq!(out.content, "Hello", "the thinking block is not the answer");
    let c = out.counts;
    assert_eq!((c.prompt, c.completion, c.total, c.cached), (Some(42), Some(5), Some(47), Some(30)));
}

/// (#3287) A sparse Messages stream: a null id is no id, a null stop reason
/// never erases a real one, a data line that is not JSON is skipped, and a
/// stream that reported no usage puts no `usage` block on the reply.
#[test]
fn a_sparse_messages_stream_carries_only_the_values_it_sent() {
    let body = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":null,\"model\":\"m\"}}\n\n\
data: not json\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":null}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";
    let bytes = assemble(body.as_bytes(), Shape::Messages).expect("an SSE body").expect("a finished stream");
    let reply: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(reply["model"], "m");
    assert!(reply.get("id").is_none(), "a null id is not copied: {reply}");
    assert_eq!(reply["stop_reason"], "max_tokens", "a later null does not erase it: {reply}");
    assert!(reply.get("usage").is_none(), "no counts, no usage block: {reply}");
    assert_eq!(reply["content"][0]["text"], "Hi");
}

/// The first generated text is the time-to-first-token mark. A role chunk
/// is not one, and neither is a usage-only event. Reasoning counts: the
/// fold's first output is generated text, which includes it.
#[test]
fn the_tail_marks_the_first_generated_text_and_counts_those_chunks() {
    let mut role = SseTail::default();
    assert!(role.feed(b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n"));
    assert!(role.progress.first_output_at.is_none(), "a role chunk is not output");
    assert_eq!(role.progress.output_events, 0);

    let mut tail = SseTail::sent_at(std::time::Instant::now(), Shape::Chat);
    assert!(tail.feed(SSE.as_bytes()));
    assert!(tail.progress.first_output_at.is_some());
    // reasoning "think", then "Hel", then "lo". The role chunk and the usage event are not output.
    assert_eq!(tail.progress.output_events, 3);
}

#[test]
fn the_tail_counts_messages_text_and_thinking_deltas() {
    let mut tail = SseTail::new(Shape::Messages);
    assert!(tail.feed(MSSE.as_bytes()));
    assert_eq!((tail.progress.content_chars, tail.progress.generated_chars), (5, 8));
}

#[test]
fn a_messages_stream_cut_off_mid_way_is_not_a_reply() {
    let cut = &MSSE[..MSSE.find("event: message_delta").unwrap()];
    assert!(matches!(assemble(cut.as_bytes(), Shape::Messages), Some(Err(_))));
    // The stop reason alone says it finished, before `message_stop` arrives.
    let stopped = &MSSE[..MSSE.find("event: message_stop").unwrap()];
    assert!(assemble(stopped.as_bytes(), Shape::Messages).unwrap().is_ok());
}

#[test]
fn a_messages_error_event_reaches_the_error_classification() {
    let body = format!(
        "{}event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}}}\n\n",
        &MSSE[..MSSE.find("event: content_block_start").unwrap()]
    );
    assert!(messages_reply(&body).is_err());
}

#[test]
fn a_streamed_messages_body_asks_to_stream_with_no_chat_options() {
    let body = stream_body(&serde_json::json!({ "model": "m" }), Shape::Messages);
    assert_eq!(body["stream"], true);
    assert!(body.get("stream_options").is_none(), "Messages has no stream_options: {body}");
}

#[test]
fn a_messages_turn_streams() {
    let mut ev = TurnEvidence::open(darkmux_types::Dialect::Messages, 10, std::time::Duration::from_secs(2), |_| {});
    let (_, body) = ev.send(
        serde_json::json!({ "model": "m" }),
        |_, progress| {
            assert!(progress.is_some(), "a Messages call is sent with the progress callback");
            Ok(serde_json::json!({}))
        },
        |_| false,
    );
    assert_eq!(body["stream"], true);
    assert!(ev.streams);
}

/// (#3210) The issue's capture: `mlx_lm.server` writes one comment line per
/// prefill step before its first event.
const MLX_PREFILL: &str = ": keepalive 512/2516\n\n: keepalive 1024/2516\n\n";

/// (#3210) The tail reads the engine's prefill progress (the same shapes the
/// runtime's reader recognizes, through one parser), keeps the LATEST
/// report, and says it read something, so the caller's heartbeat fires.
#[test]
fn the_tail_reads_mlx_prefill_progress_and_keeps_the_latest_report() {
    let mut tail = SseTail::new(Shape::Chat);
    let bytes = MLX_PREFILL.as_bytes();
    let cut = MLX_PREFILL.find("1024").unwrap();
    assert!(tail.feed(&bytes[..cut]), "the first report is complete");
    let sample = |processed| Some(darkmux_trajectory::PromptProgressSample { processed, total: 2516 });
    assert_eq!(tail.progress.prompt, sample(512));
    assert!(tail.feed(&bytes[cut..]), "the second report completes across the split");
    assert_eq!(tail.progress.prompt, sample(1024));
    assert_eq!(tail.progress.chunks, 0, "a report is not an event");
    assert!(!tail.feed(b": OPENROUTER PROCESSING\n\n"), "another comment is not progress");
    assert_eq!(tail.progress.prompt, sample(1024));
}

/// (#3210) A heartbeat sent while the engine reads the prompt carries the
/// neutral fact with nothing generated and no chunk index; once chunks
/// arrive the turn's latest report still rides along.
#[test]
fn a_heartbeat_carries_the_prefill_progress_fact() {
    let mut tail = SseTail::new(Shape::Chat);
    tail.feed(MLX_PREFILL.as_bytes());
    let beat = serde_json::to_value(chunk_beat(&tail.progress)).unwrap();
    assert_eq!((beat["prompt_processed"].as_u64(), beat["prompt_total"].as_u64()), (Some(1024), Some(2516)), "{beat}");
    assert_eq!(beat["generated_chars"], 0);
    assert!(beat.get("partial_index").is_none(), "no chunk was read: {beat}");
    assert!(beat.get("prompt_chars").is_none(), "not the opener: {beat}");

    tail.feed(SSE.as_bytes());
    let beat = serde_json::to_value(chunk_beat(&tail.progress)).unwrap();
    assert_eq!(beat["prompt_processed"], 1024);
    assert_eq!(beat["partial_index"], 5);
    let plain = serde_json::to_value(chunk_beat(&SseProgress { chunks: 1, ..SseProgress::default() })).unwrap();
    assert!(plain.get("prompt_processed").is_none(), "absent when the engine reported none: {plain}");
}

/// (#3210) The progress lines do not change the reply.
#[test]
fn a_stream_with_prefill_progress_reassembles_to_the_same_reply() {
    assert_eq!(ok(&format!("{MLX_PREFILL}{SSE}")), ok(SSE));
}

/// (#3210, PR #3293 review) A tool-call-first reply on mlx: the deltas carry
/// only tool-call arguments, which `delta_chars` does not count, so every
/// beat after the prefill says `generated_chars: 0` and carries the turn's
/// count. None of them is a prompt-phase heartbeat: a beat that read a chunk
/// never rides the hub. The beat before any chunk, and the opener, still do.
#[test]
fn a_tool_call_first_reply_sends_no_prompt_phase_beat_after_the_prefill() {
    let mut tail = SseTail::new(Shape::Chat);
    tail.feed(MLX_PREFILL.as_bytes());
    assert!(darkmux_flow::is_prompt_phase_heartbeat(&chunk_beat(&tail.progress)), "the prefill report travels");
    assert!(darkmux_flow::is_prompt_phase_heartbeat(&opener(9000)), "the opener travels");
    let tool_args = "data: {\"model\":\"m\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"write\",\"arguments\":\"\"}}]}}]}\n\n\
data: {\"model\":\"m\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n";
    tail.feed(tool_args.as_bytes());
    let beat = chunk_beat(&tail.progress);
    assert_eq!((beat.generated_chars, beat.prompt_processed, beat.partial_index), (Some(0), Some(1024), Some(2)), "{beat:?}");
    assert!(!darkmux_flow::is_prompt_phase_heartbeat(&beat), "a beat that read a chunk stays local: {beat:?}");
}

/// (#3210, PR #3293 live check) The direct path marks a generation beat for
/// the hub at most once per 20 s: a burst of chunk beats (the 2 s gate set
/// to zero, all inside one second) marks only the first. The prefill beat
/// before it travels as a prompt-phase beat and is not marked.
#[test]
fn the_direct_path_marks_one_generation_beat_per_interval() {
    use std::cell::RefCell;
    let emitted = RefCell::new(Vec::new());
    let mut ev = TurnEvidence::open(darkmux_types::Dialect::ChatCompletions, 10, std::time::Duration::ZERO, |p| emitted.borrow_mut().push(p));
    let prompt = Some(darkmux_trajectory::PromptProgressSample { processed: 2516, total: 2516 });
    let _ = ev.send(
        serde_json::json!({ "model": "m" }),
        |_, progress| {
            let on = progress.expect("a streamed call");
            on(&SseProgress { prompt, ..SseProgress::default() });
            for n in 1..=10u64 {
                on(&SseProgress { prompt, chunks: n, generated_chars: n * 40, content_chars: n * 30, ..SseProgress::default() });
            }
            Ok(serde_json::json!({ "choices": [{ "finish_reason": "stop" }] }))
        },
        |_| false,
    );
    let beats: Vec<darkmux_flow::payload::DispatchHeartbeatPayload> = emitted
        .borrow()
        .iter()
        .filter_map(|p| match p {
            darkmux_flow::Payload::DispatchTurnHeartbeat(b) => Some(b.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(beats.len(), 12, "the opener, the prefill beat and ten chunk beats");
    let marked: Vec<Option<u64>> = beats.iter().filter(|b| b.hub_relay == Some(true)).map(|b| b.partial_index).collect();
    assert_eq!(marked, [Some(1)], "only the first generation beat inside 20 s");
    assert_eq!(beats.iter().filter(|b| darkmux_flow::heartbeat_rides_hub(b)).count(), 3);
}

/// (Matrix review) The direct path relays a stream's first beat past the
/// stream-open chunk as well as the chunk itself: a peer then has a pair to
/// read a rate from on a turn shorter than 20 s.
#[test]
fn the_direct_path_relays_the_first_output_past_the_stream_open_chunk() {
    use std::cell::RefCell;
    let emitted = RefCell::new(Vec::new());
    let mut ev = TurnEvidence::open(darkmux_types::Dialect::ChatCompletions, 10, std::time::Duration::ZERO, |p| emitted.borrow_mut().push(p));
    let _ = ev.send(
        serde_json::json!({ "model": "m" }),
        |_, progress| {
            let on = progress.expect("a streamed call");
            on(&SseProgress { chunks: 1, generated_chars: 3, ..SseProgress::default() });
            for n in 2..=6u64 {
                on(&SseProgress { chunks: n, generated_chars: n * 40, content_chars: n * 30, ..SseProgress::default() });
            }
            Ok(serde_json::json!({ "choices": [{ "finish_reason": "stop" }] }))
        },
        |_| false,
    );
    let marked: Vec<Option<u64>> = emitted
        .borrow()
        .iter()
        .filter_map(|p| match p {
            darkmux_flow::Payload::DispatchTurnHeartbeat(b) if b.hub_relay == Some(true) => Some(b.partial_index),
            _ => None,
        })
        .collect();
    assert_eq!(marked, [Some(1), Some(2)], "the 3-char chunk and the first beat past it, nothing more inside 20 s");
}

/// The wait for the first generated text is taken from the progress that
/// carried it, not from the coalesced heartbeat. A later progress does not
/// move it. A call that never streamed reports neither.
#[test]
fn the_turn_keeps_the_wait_for_the_first_generated_text() {
    let mut ev = TurnEvidence::open(darkmux_types::Dialect::ChatCompletions, 10, std::time::Duration::from_secs(60), |_| {});
    let sent = std::time::Instant::now() - std::time::Duration::from_millis(2_000);
    let at = sent + std::time::Duration::from_millis(400);
    let _ = ev.send(
        serde_json::json!({ "model": "m" }),
        |_, progress| {
            let on = progress.expect("a streamed call");
            on(&SseProgress { started: Some(sent), chunks: 1, ..SseProgress::default() });
            on(&SseProgress {
                started: Some(sent),
                first_output_at: Some(at),
                output_events: 1,
                generated_chars: 3,
                chunks: 2,
                ..SseProgress::default()
            });
            on(&SseProgress {
                started: Some(sent),
                first_output_at: Some(at + std::time::Duration::from_secs(5)),
                output_events: 4,
                generated_chars: 80,
                chunks: 5,
                ..SseProgress::default()
            });
            Ok(serde_json::json!({ "choices": [{ "finish_reason": "stop" }] }))
        },
        |_| false,
    );
    assert_eq!(ev.first_token_ms(), Some(400), "the first text, not a later one and not the heartbeat");
    assert_eq!(ev.output_events(), Some(4));

    let mut refused = TurnEvidence::open(darkmux_types::Dialect::ChatCompletions, 10, std::time::Duration::from_secs(60), |_| {});
    let _ = refused.send(
        serde_json::json!({ "model": "m" }),
        |body, progress| {
            if body.get("stream").is_some() {
                let on = progress.expect("the streamed attempt");
                on(&SseProgress { started: Some(sent), first_output_at: Some(at), output_events: 2, ..SseProgress::default() });
                Err(anyhow::anyhow!("refused"))
            } else {
                Ok(serde_json::json!({}))
            }
        },
        |_| true,
    );
    assert_eq!(refused.first_token_ms(), None, "an unstreamed retry has no first token");
    assert_eq!(refused.output_events(), None);
}
