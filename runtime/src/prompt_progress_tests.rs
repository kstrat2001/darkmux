//! (#3210) Prefill progress an engine reports in its stream reaches the
//! trajectory, so the host can carry it on the heartbeat and the scope can
//! read PROMPT as a percentage instead of a silence.
//!
//! The fixture is the issue's capture: `mlx_lm.server` writes one SSE comment
//! line per prefill step (`: keepalive <processed>/<total>`) before its first
//! `data:` event.

use super::*;
use crate::lmstudio::{ChatRequest, LmStudioClient, Message};
use crate::test_support::sse_server_scripted;
use crate::trajectory::Trajectory;
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(100);

fn chunk(content: &str, finish: Option<&str>) -> String {
    serde_json::json!({
        "id": "c",
        "choices": [{ "index": 0, "delta": { "content": content }, "finish_reason": finish }],
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

/// One streamed turn against `script`, with the proof-of-work clock starting
/// `aged` in the past. Returns the clock after the turn.
fn run(script: Vec<(Duration, String)>, ws: &std::path::Path, aged: Duration) -> Instant {
    let client = LmStudioClient::with_base_url_and_read_timeout(
        sse_server_scripted(script),
        Duration::from_secs(10),
    );
    let request = ChatRequest {
        model: "m".into(),
        messages: vec![Message::user("hi")],
        tools: Vec::new(),
        tool_choice: None,
        temperature: 0.0,
        max_tokens: None,
        response_format: None,
    };
    let mut trajectory = Trajectory::open(ws);
    let mut proof = Instant::now() - aged;
    let mut warned = true;
    run_streaming_turn(
        &client,
        &request,
        1,
        &mut trajectory,
        &mut proof,
        &mut warned,
        Watch {
            interval: 1000,
            carried: "",
            tick: TICK,
        },
    )
    .expect("the stream completes");
    proof
}

fn mlx_capture() -> Vec<(Duration, String)> {
    let mut script: Vec<(Duration, String)> = [512, 1024, 1536, 2048, 2512, 2516]
        .iter()
        .map(|n| (Duration::from_millis(10), format!(": keepalive {n}/2516")))
        .collect();
    script.push((Duration::ZERO, chunk("Hi", None)));
    script.push((Duration::ZERO, chunk("", Some("stop"))));
    script
}

/// Every keepalive line becomes one `model.prompt.progress` event, in order,
/// under the turn, before the first chunk.
#[test]
#[serial_test::serial]
fn each_mlx_keepalive_line_is_written_as_prompt_progress_before_the_first_chunk() {
    let ws = tempfile::Builder::new()
        .prefix("prompt-progress")
        .tempdir()
        .unwrap();
    run(mlx_capture(), ws.path(), Duration::ZERO);
    let evs = events(ws.path());
    let progress: Vec<(u64, u64)> = evs
        .iter()
        .filter(|e| e["type"] == "model.prompt.progress")
        .map(|e| {
            (
                e["processed"].as_u64().unwrap(),
                e["total"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        progress,
        vec![
            (512, 2516),
            (1024, 2516),
            (1536, 2516),
            (2048, 2516),
            (2512, 2516),
            (2516, 2516)
        ]
    );
    assert!(evs
        .iter()
        .filter(|e| e["type"] == "model.prompt.progress")
        .all(|e| e["seq"] == 1));
    let last_progress = evs
        .iter()
        .rposition(|e| e["type"] == "model.prompt.progress")
        .unwrap();
    let first_partial = evs
        .iter()
        .position(|e| e["type"] == "model.partial")
        .expect("the chunk was read");
    assert!(
        last_progress < first_partial,
        "progress comes before the first chunk: {evs:?}"
    );
}

/// An advancing prefill is proof of work: the runtime's own inactivity clock
/// resets, as it does for a chunk. The stream carries no chunk at all, so
/// only the progress line can have reset it.
#[test]
#[serial_test::serial]
fn an_advancing_prefill_resets_the_runtimes_inactivity_clock() {
    let ws = tempfile::Builder::new()
        .prefix("prompt-progress")
        .tempdir()
        .unwrap();
    let hour = Duration::from_secs(3600);
    let proof = run(
        vec![(Duration::ZERO, ": keepalive 5/10".to_string())],
        ws.path(),
        hour,
    );
    assert!(
        proof.elapsed() < hour / 2,
        "the progress line did not count as proof of work"
    );
}

/// A report that did not advance proves nothing: a wedged server repeating
/// itself must still read as silent.
#[test]
#[serial_test::serial]
fn a_progress_report_that_did_not_advance_is_not_proof_of_work() {
    let ws = tempfile::Builder::new()
        .prefix("prompt-progress")
        .tempdir()
        .unwrap();
    let hour = Duration::from_secs(3600);
    let proof = run(
        vec![(Duration::ZERO, ": keepalive 0/10".to_string())],
        ws.path(),
        hour,
    );
    assert!(
        proof.elapsed() >= hour,
        "a zero report reset the inactivity clock"
    );
}

/// An unknown comment line is ignored, as before: no progress event.
#[test]
#[serial_test::serial]
fn an_unknown_comment_line_writes_no_progress() {
    let ws = tempfile::Builder::new()
        .prefix("prompt-progress")
        .tempdir()
        .unwrap();
    run(
        vec![
            (Duration::ZERO, ": OPENROUTER PROCESSING".to_string()),
            (Duration::ZERO, chunk("ok", Some("stop"))),
        ],
        ws.path(),
        Duration::ZERO,
    );
    assert!(!events(ws.path())
        .iter()
        .any(|e| e["type"] == "model.prompt.progress"));
}
