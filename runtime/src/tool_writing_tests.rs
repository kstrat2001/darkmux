//! (#2889) The model writing a tool call must stay visible while the
//! endpoint is silent.
//!
//! The shape is measured, not invented. A raw SSE probe asking LM Studio for
//! one `write` call: reasoning chunks for 15.8s, then a `tool_calls` delta
//! carrying only the function NAME at 15.9s, then NOTHING for 7s while the
//! model generated the arguments, then every one of the 3,455 argument
//! characters in one chunk and `finish_reason: tool_calls`. The runtime only
//! wrote a trajectory event when a chunk arrived, so the host forwarded no
//! heartbeat during that silence and the viewer read STALL (or PROMPT).
//!
//! The fixture is that stream in miniature, with the tick scaled down: the
//! silence is several ticks long, so a runtime that only writes on chunk
//! arrival writes nothing inside it.
#![allow(clippy::too_many_arguments)]

use super::*;
use crate::lmstudio::{ChatRequest, LmStudioClient, Message};
use crate::test_support::sse_server_scripted;
use crate::trajectory::Trajectory;
use std::time::Duration;

const TICK: Duration = Duration::from_millis(100);

fn chunk(delta: serde_json::Value, finish: Option<&str>) -> String {
    serde_json::json!({
        "id": "c",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
    })
    .to_string()
}

fn events(ws: &std::path::Path) -> Vec<serde_json::Value> {
    let path = ws.join(".darkmux-runtime").join("trajectory.jsonl");
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn run(url: String, ws: &std::path::Path) {
    let client = LmStudioClient::with_base_url_and_read_timeout(url, Duration::from_secs(10));
    let request = ChatRequest {
        model: "m".into(),
        messages: vec![Message::user("write the file")],
        tools: Vec::new(),
        tool_choice: None,
        temperature: 0.0,
        max_tokens: None,
        response_format: None,
    };
    let mut trajectory = Trajectory::open(ws);
    let mut proof = std::time::Instant::now();
    let mut warned = false;
    run_streaming_turn(
        &client,
        &request,
        1,
        &mut trajectory,
        &mut proof,
        &mut warned,
        Watch { interval: 1000, carried: "", tick: TICK },
    )
    .expect("the stream completes");
}

/// Name chunk, then a silence of ~6 ticks, then the arguments in one chunk.
fn probe_shaped_stream() -> Vec<(Duration, String)> {
    let args = r#"{"path":"a.txt","content":"hello"}"#;
    vec![
        (Duration::ZERO, chunk(serde_json::json!({"reasoning_content": "plan"}), None)),
        (
            Duration::from_millis(20),
            chunk(
                serde_json::json!({"tool_calls": [{
                    "index": 0, "id": "call_1", "type": "function",
                    "function": {"name": "write", "arguments": ""}
                }]}),
                None,
            ),
        ),
        (
            TICK * 6,
            chunk(
                serde_json::json!({"tool_calls": [{"index": 0, "function": {"arguments": args}}]}),
                None,
            ),
        ),
        (Duration::ZERO, chunk(serde_json::json!({}), Some("tool_calls"))),
    ]
}

/// The silence between the name chunk and the arguments chunk must carry
/// "still writing" events naming the tool, with `generated_chars` unchanged
/// (nothing new arrived, so nothing new is counted).
#[test]
#[serial_test::serial]
fn a_silent_tool_call_keeps_emitting_writing_events_through_the_silence() {
    let ws = tempfile::Builder::new().prefix("tool-writing").tempdir().unwrap();
    run(sse_server_scripted(probe_shaped_stream()), ws.path());
    let evs = events(ws.path());

    let partials: Vec<&serde_json::Value> =
        evs.iter().filter(|e| e["type"] == "model.partial").collect();
    let name_partial = partials
        .iter()
        .find(|e| e["tool_calls_present"] == true)
        .expect("the name chunk produced a model.partial");
    let name_idx = evs.iter().position(|e| e == *name_partial).unwrap();
    let args_idx = evs
        .iter()
        .rposition(|e| e["type"] == "model.partial" && e["tool_calls_present"] == true
            && e["generated_chars"].as_u64() > name_partial["generated_chars"].as_u64())
        .expect("the arguments chunk produced a model.partial");

    let writing: Vec<&serde_json::Value> = evs[name_idx + 1..args_idx]
        .iter()
        .filter(|e| e["type"] == "model.tool_call.writing")
        .collect();
    assert!(
        writing.len() >= 2,
        "a {}ms silence against a {}ms tick must carry at least two writing events, \
         got {} — the runtime is still only writing on chunk arrival (#2889)",
        (TICK * 6).as_millis(),
        TICK.as_millis(),
        writing.len()
    );
    for w in &writing {
        assert_eq!(w["phase"], "writing_tool_call");
        assert_eq!(w["tool_name"], "write");
        assert_eq!(
            w["generated_chars"], name_partial["generated_chars"],
            "no new text arrived, so generated_chars must not move"
        );
        assert_eq!(w["seq"], 1);
    }

    // The chunk that NAMED the tool already says so — the viewer switches at
    // the name, not one tick later.
    assert_eq!(name_partial["phase"], "writing_tool_call");
    assert_eq!(name_partial["tool_name"], "write");
}

/// The inverse: a silence with NO tool call named (plain reasoning pauses,
/// prompt processing) writes no writing events and names no phase. Ticking
/// there would claim a state the model is not in.
#[test]
#[serial_test::serial]
fn a_silence_without_a_named_tool_call_writes_no_writing_events() {
    let ws = tempfile::Builder::new().prefix("tool-writing-none").tempdir().unwrap();
    let script = vec![
        (Duration::ZERO, chunk(serde_json::json!({"reasoning_content": "plan"}), None)),
        (TICK * 4, chunk(serde_json::json!({"content": "done"}), None)),
        (Duration::ZERO, chunk(serde_json::json!({}), Some("stop"))),
    ];
    run(sse_server_scripted(script), ws.path());
    let evs = events(ws.path());
    assert!(
        !evs.iter().any(|e| e["type"] == "model.tool_call.writing"),
        "no tool call was named, so no writing event may be written"
    );
    assert!(
        evs.iter()
            .filter(|e| e["type"] == "model.partial")
            .all(|e| e.get("phase").is_none()),
        "a partial with no tool call in flight names no phase"
    );
}

/// (#2889 review, C3) Once a named call's arguments have arrived, the call is
/// written. A silence after that (a delayed finish chunk, or a pause before a
/// second call) must not tick "writing" for the finished call, and the chunks
/// that follow must not be stamped with the writing phase.
#[test]
#[serial_test::serial]
fn a_silence_after_the_arguments_arrived_writes_no_writing_events() {
    let ws = tempfile::Builder::new().prefix("tool-writing-done").tempdir().unwrap();
    let args = r#"{"path":"a.txt","content":"hello"}"#;
    let script = vec![
        (
            Duration::ZERO,
            chunk(
                serde_json::json!({"tool_calls": [{
                    "index": 0, "id": "call_1", "type": "function",
                    "function": {"name": "write", "arguments": ""}
                }]}),
                None,
            ),
        ),
        (
            Duration::from_millis(20),
            chunk(serde_json::json!({"tool_calls": [{"index": 0, "function": {"arguments": args}}]}), None),
        ),
        // The finish chunk is late: several ticks of silence with the call done.
        (TICK * 5, chunk(serde_json::json!({}), Some("tool_calls"))),
    ];
    run(sse_server_scripted(script), ws.path());
    let evs = events(ws.path());
    let args_idx = evs
        .iter()
        .position(|e| {
            e["type"] == "model.partial"
                && e["generated_chars"].as_u64().unwrap_or(0) >= (args.len() + "write".len()) as u64
        })
        .expect("the arguments chunk produced a model.partial");
    let after = &evs[args_idx + 1..];
    let ticks = after.iter().filter(|e| e["type"] == "model.tool_call.writing").count();
    assert_eq!(ticks, 0, "the call's arguments arrived; no writing tick may follow for it");
    assert!(
        after
            .iter()
            .filter(|e| e["type"] == "model.partial")
            .all(|e| e.get("phase").is_none()),
        "a partial after the arguments landed names no writing phase"
    );
}

/// (#2928) The silence tick follows the host-forwarded live cadence, clamped
/// like the host clamps it, and keeps the pre-#2928 1 s when the channel is
/// off, the var is absent or it does not parse.
#[test]
fn the_silence_tick_follows_the_live_cadence() {
    assert_eq!(silence_tick(Some("250")), Duration::from_millis(250));
    assert_eq!(silence_tick(Some(" 500 ")), Duration::from_millis(500));
    assert_eq!(silence_tick(Some("0")), STREAM_TICK, "off keeps the 1 s tick");
    assert_eq!(silence_tick(None), STREAM_TICK, "an older host forwards nothing");
    assert_eq!(silence_tick(Some("fast")), STREAM_TICK);
    assert_eq!(silence_tick(Some("5")), Duration::from_millis(100), "never faster than 100 ms");
    assert_eq!(silence_tick(Some("60000")), Duration::from_secs(1), "never slower than 1 s");
}

/// (#2928) The runtime stamps its own sampling cost: the stream's end event
/// counts the silence ticks it woke for and the time spent on them.
#[test]
#[serial_test::serial]
fn the_stream_end_stamps_its_silence_ticks_and_their_cost() {
    let ws = tempfile::Builder::new().prefix("tool-writing-cost").tempdir().unwrap();
    run(sse_server_scripted(probe_shaped_stream()), ws.path());
    let evs = events(ws.path());
    let end = evs.iter().find(|e| e["type"] == "model.streaming.end").expect("a stream end");
    let writing = evs.iter().filter(|e| e["type"] == "model.tool_call.writing").count() as u64;
    let ticks = end["idle_ticks"].as_u64().expect("idle_ticks stamped");
    assert!(ticks >= writing && ticks >= 2, "every writing event is a tick: ticks={ticks} writing={writing}");
    assert!(end["idle_tick_us"].as_u64().is_some(), "the cost is stamped: {end}");
}

/// (#2928) The full loop reads the host-forwarded cadence: with
/// `DARKMUX_LIVE_SAMPLE_MS=100`, a 600 ms silence is ticked through at
/// 100 ms, where the fixed 1 s tick would not wake once.
#[test]
#[serial_test::serial]
fn the_loop_ticks_at_the_forwarded_live_cadence() {
    let ws = tempfile::Builder::new().prefix("live-cadence-loop").tempdir().unwrap();
    let script = vec![
        (Duration::ZERO, chunk(serde_json::json!({"content": "one"}), None)),
        (Duration::from_millis(600), chunk(serde_json::json!({"content": " two"}), None)),
        (Duration::ZERO, chunk(serde_json::json!({}), Some("stop"))),
    ];
    let url = sse_server_scripted(script);
    let prev = std::env::var("DARKMUX_LIVE_SAMPLE_MS").ok();
    unsafe { std::env::set_var("DARKMUX_LIVE_SAMPLE_MS", "100") };
    let client = LmStudioClient::with_base_url_and_read_timeout(url, Duration::from_secs(10));
    let mut trajectory = Trajectory::open(ws.path());
    let cfg = crate::compaction::CompactionConfig::never_compact();
    let result = super::run(
        &client,
        &client,
        "m",
        vec![Message::user("say two words")],
        &[],
        &mut trajectory,
        true,
        &cfg,
        Some(1),
        None,
        None,
        None,
        std::collections::BTreeMap::new(),
        None,
    );
    unsafe {
        match prev {
            Some(v) => std::env::set_var("DARKMUX_LIVE_SAMPLE_MS", v),
            None => std::env::remove_var("DARKMUX_LIVE_SAMPLE_MS"),
        }
    }
    result.expect("the loop completes");
    let evs = events(ws.path());
    let end = evs.iter().find(|e| e["type"] == "model.streaming.end").expect("a stream end");
    let ticks = end["idle_ticks"].as_u64().unwrap();
    assert!(ticks >= 3, "600 ms of silence at a 100 ms tick wakes several times, got {ticks}");
}
