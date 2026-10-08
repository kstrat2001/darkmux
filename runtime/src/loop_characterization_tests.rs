//! (#3136) Characterization tests for the agent loop: what `run_with_sleeper`
//! DOES, end to end, on each path that matters, pinned as a golden trace.
//!
//! These were written against the loop BEFORE it was split into deciders and
//! committed alone, so the split is checked against the behavior it started
//! from rather than against a reading of it. A trace is everything the loop
//! makes observable: the terminal outcome, the final message thread, every
//! rest it took, every request it sent (how many tokens it allowed and which
//! roles it carried), the checkpoint it left on disk, and every trajectory
//! record it wrote. Only timestamps are dropped, because they are the one
//! thing that differs between two runs of the same script.
//!
//! The goldens live in `runtime/testdata/loop-characterization/`. To accept a
//! deliberate behavior change, rerun with `DARKMUX_BLESS_LOOP_TRACES=1` and
//! review the diff of the golden files: a changed trace is a changed behavior.
//!
//! Rules that are only about one decision (when to rest, which bound a call
//! carries, when a budget escalates) are tested in `loop_deciders_tests.rs`;
//! what stays here is the loop wiring that connects them. Six traces written
//! with this file were about one rule each and moved to that rule's table
//! once the split landed: `max_turns` and the cumulative cap
//! (`budget_stop_table`), the context-overflow error (`length_effect_table`),
//! malformed names (`tally_dispatch_table`), the generation budget
//! (`draw_generation_budget_table`, `checkpoint_remedy_table`) and the
//! compaction bound (`compaction_bounds_table`). Their wiring is still
//! exercised end to end by the older loop tests in `loop_runner.rs`, each
//! red-proven by a mutation of its effect when they moved.
#![allow(clippy::too_many_arguments)]

use super::tests::chat_response_json;
use super::*;
use crate::lmstudio::{LmStudioClient, Message};
use crate::tools::Tool;
use crate::trajectory::Trajectory;

const JSON: &str = "application/json";

/// `(delay before answering, content type, body)`.
type Reply = (std::time::Duration, &'static str, String);
const NOW: std::time::Duration = std::time::Duration::ZERO;
const SSE: &str = "text/event-stream";

/// One scripted run of the loop. Every field defaults to the plainest run
/// (`Scenario::new`), so each test names only what its path needs.
struct Scenario {
    name: &'static str,
    replies: Vec<Reply>,
    initial: Vec<Message>,
    tools: Vec<Tool>,
    cfg: compaction::CompactionConfig,
    streaming: bool,
    max_turns: Option<u32>,
    max_cumulative_tokens: Option<u32>,
    max_tokens_per_call: Option<u32>,
    reasoning_interval: Option<u32>,
    generation_interval: Option<u32>,
    max_stall_recoveries: Option<u32>,
    resume_from: Option<checkpoint::RunCheckpoint>,
    turn_delay_ms: Option<&'static str>,
    inactivity_secs: Option<&'static str>,
    pace: Option<&'static str>,
}

impl Scenario {
    fn new(name: &'static str, replies: Vec<Reply>) -> Self {
        Self {
            name,
            replies,
            initial: vec![Message::system("test system"), Message::user("do the task")],
            tools: vec![Tool::Read, Tool::Edit],
            cfg: compaction::CompactionConfig::never_compact(),
            streaming: false,
            max_turns: Some(20),
            max_cumulative_tokens: None,
            max_tokens_per_call: None,
            reasoning_interval: None,
            generation_interval: Some(u32::MAX),
            max_stall_recoveries: None,
            resume_from: None,
            turn_delay_ms: None,
            inactivity_secs: None,
            pace: None,
        }
    }
}

/// Records each rest, and lifts any pace pause on every rest so a paused
/// scenario resumes on the next poll instead of waiting out the ceiling.
struct ScriptSleeper {
    calls: std::cell::RefCell<Vec<u64>>,
    out_dir: std::path::PathBuf,
}

impl TurnSleeper for ScriptSleeper {
    fn sleep(&self, ms: u64) {
        self.calls.borrow_mut().push(ms);
        let pace = pace::pace_file_path(&self.out_dir);
        if pace.exists() {
            std::fs::write(pace, r#"{"pause": false}"#).unwrap();
        }
    }
}

fn reply(
    content: Option<&str>,
    tool_calls: Option<serde_json::Value>,
    finish: &str,
    prompt: u32,
    completion: u32,
) -> Reply {
    (NOW, JSON, chat_response_json(content, tool_calls, finish, prompt, completion).to_string())
}

fn calls(names_and_args: &[(&str, &str)]) -> serde_json::Value {
    serde_json::Value::Array(
        names_and_args
            .iter()
            .enumerate()
            .map(|(i, (name, args))| {
                serde_json::json!({
                    "id": format!("call_{i}"),
                    "type": "function",
                    "function": { "name": name, "arguments": args },
                })
            })
            .collect(),
    )
}

const READ_X: &str = r#"{"path":"/workspace/x.txt","offset":1,"limit":1}"#;

fn read_x() -> serde_json::Value {
    calls(&[("read", READ_X)])
}

/// An SSE body whose text arrives on `field` (`content` or
/// `reasoning_content`), then a terminal chunk with `finish` and usage.
fn sse(field: &str, pieces: &[&str], finish: &str, completion: u32) -> Reply {
    let mut out = String::new();
    for p in pieces {
        out.push_str(&format!(
            "data: {}\n\n",
            serde_json::json!({"id":"c","choices":[{"index":0,"delta":{ field: p }}]})
        ));
    }
    out.push_str(&format!(
        "data: {}\n\n",
        serde_json::json!({
            "id":"c",
            "choices":[{"index":0,"delta":{},"finish_reason":finish}],
            "usage":{"prompt_tokens":100,"completion_tokens":completion,
                     "total_tokens":100+completion}
        })
    ));
    out.push_str("data: [DONE]\n\n");
    (NOW, SSE, out)
}

fn clear_env() {
    for var in [
        "DARKMUX_INACTIVITY_TIMEOUT_SECONDS",
        "DARKMUX_INACTIVITY_TIMEOUT_SECONDS_SOURCE",
        "DARKMUX_TURN_DELAY_MS",
        "DARKMUX_FEEDBACK_INJECTION",
        "DARKMUX_RUNTIME_DETECTION_DEGENERACY_POLICY",
        "DARKMUX_MAX_PAUSE_MS",
    ] {
        std::env::remove_var(var);
    }
}

fn short(s: &str) -> String {
    let flat: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    let head: String = flat.chars().take(48).collect();
    if flat.chars().count() > 48 {
        format!("{head}…")
    } else {
        head
    }
}

fn render_message(m: &Message) -> String {
    let content = m.content.as_deref().unwrap_or("");
    let calls = m.tool_calls.as_ref().map(|t| t.len()).unwrap_or(0);
    format!("{} chars={} calls={} {:?}", m.role, content.chars().count(), calls, short(content))
}

fn render_request(i: usize, body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let roles: Vec<&str> = v["messages"]
        .as_array()
        .map(|a| a.iter().map(|m| m["role"].as_str().unwrap_or("?")).collect())
        .unwrap_or_default();
    format!(
        "#{i} model={} max_tokens={} stream={} roles=[{}]",
        v["model"], v["max_tokens"], v["stream"], roles.join(",")
    )
}

fn render_events(out_dir: &std::path::Path) -> Vec<String> {
    let path = out_dir.join(".darkmux-runtime").join("trajectory.jsonl");
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| {
            let mut v: serde_json::Value = serde_json::from_str(line).unwrap();
            if let Some(obj) = v.as_object_mut() {
                obj.remove("ts");
            }
            v.to_string()
        })
        .collect()
}

/// The last checkpoint the run left on disk, without its timestamp: what a
/// resume after a kill at the end of the run would start from.
fn render_checkpoint(out_dir: &std::path::Path) -> String {
    let Ok(body) = std::fs::read_to_string(checkpoint::checkpoint_file_path(out_dir)) else {
        return "none".into();
    };
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let roles: Vec<&str> = v["messages"]
        .as_array()
        .map(|a| a.iter().map(|m| m["role"].as_str().unwrap_or("?")).collect())
        .unwrap_or_default();
    format!(
        "turns={} tokens={} compactions={} hand_back={} pending={} seq_base={} head_started={} roles=[{}]",
        v["turns"],
        v["total_completion_tokens"],
        v["compactions"],
        !v["pending_hand_back"].is_null(),
        v["pending_tool_calls"].as_array().map(|a| a.len()).unwrap_or(0),
        v["pending_tool_calls_seq_base"],
        v["pending_head_started"],
        roles.join(","),
    )
}

/// Run `s` and render everything it made observable.
fn trace(s: Scenario) -> String {
    clear_env();
    if let Some(ms) = s.turn_delay_ms {
        std::env::set_var("DARKMUX_TURN_DELAY_MS", ms);
    }
    if let Some(secs) = s.inactivity_secs {
        std::env::set_var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS", secs);
    }
    let (base, requests) = crate::test_support::json_server_sequenced(s.replies);
    let client = LmStudioClient::with_base_url(base);
    let tmp = tempfile::Builder::new().prefix("loop-char").tempdir().unwrap();
    if let Some(body) = s.pace {
        std::fs::write(pace::pace_file_path(tmp.path()), body).unwrap();
    }
    let sleeper = ScriptSleeper { calls: Default::default(), out_dir: tmp.path().to_path_buf() };
    let mut traj = Trajectory::open(tmp.path());
    let result = run_with_sleeper(
        &client,
        &client,
        "test-model",
        s.initial,
        &s.tools,
        &mut traj,
        s.streaming,
        &s.cfg,
        s.max_turns,
        s.max_cumulative_tokens,
        s.max_tokens_per_call,
        s.reasoning_interval,
        s.generation_interval,
        s.max_stall_recoveries,
        std::collections::BTreeMap::new(),
        None,
        tmp.path(),
        "test-role",
        s.resume_from,
        &sleeper,
    );
    drop(traj);
    clear_env();

    let mut out = vec![format!("scenario: {}", s.name)];
    match &result {
        Ok(o) => {
            out.push(format!("outcome: {:?}", o.terminal_reason));
            out.push(format!("final_answer: {:?}", o.final_answer.as_deref().map(short)));
            out.push(format!("turn_delay_effective_ms: {}", o.turn_delay_effective_ms));
            out.push(format!("failed_to_run: {}", o.failed_to_run.len()));
            out.push("messages:".into());
            out.extend(o.messages.iter().map(|m| format!("  {}", render_message(m))));
        }
        Err(e) => out.push(format!("outcome: Err({})", short(&e.to_string()))),
    }
    out.push(format!("sleeps: {:?}", sleeper.calls.borrow()));
    out.push("requests:".into());
    let sent = requests.lock().unwrap().clone();
    out.extend(sent.iter().enumerate().map(|(i, b)| format!("  {}", render_request(i + 1, b))));
    out.push(format!("checkpoint: {}", render_checkpoint(tmp.path())));
    out.push("events:".into());
    out.extend(render_events(tmp.path()).into_iter().map(|e| format!("  {e}")));
    mask_generated_call_ids(&(out.join("\n") + "\n"))
}

/// A promoted plain-text call gets a generated id (`call_<16 hex>_<n>`) that
/// differs on every run; it is masked so the trace keeps the shape of the id.
fn mask_generated_call_ids(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("call_") {
        out.push_str(&rest[..at + 5]);
        rest = &rest[at + 5..];
        let hex = rest.as_bytes().iter().take(16).filter(|b| b.is_ascii_hexdigit()).count();
        if hex == 16 && rest.as_bytes().get(16) == Some(&b'_') {
            out.push_str("<generated>");
            rest = &rest[16..];
        }
    }
    out.push_str(rest);
    out
}

/// Compare against the golden, or write it when blessing.
fn assert_golden(s: Scenario) {
    let name = s.name;
    let actual = trace(s);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata/loop-characterization")
        .join(format!("{name}.trace"));
    if std::env::var("DARKMUX_BLESS_LOOP_TRACES").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("no golden at {}; bless it first", path.display()));
    if actual != expected {
        let first = actual
            .lines()
            .zip(expected.lines())
            .position(|(a, e)| a != e)
            .unwrap_or_else(|| actual.lines().count().min(expected.lines().count()));
        panic!(
            "trace of `{name}` changed at line {}:\n  expected: {}\n  actual:   {}\n\nfull actual trace:\n{actual}",
            first + 1,
            expected.lines().nth(first).unwrap_or("<end>"),
            actual.lines().nth(first).unwrap_or("<end>"),
        );
    }
}

fn compaction_cfg(bail: Option<u32>) -> compaction::CompactionConfig {
    compaction::CompactionConfig {
        compactor_context_window: None,
        threshold_tokens: 1000,
        compactor_model: Some("test-compactor".to_string()),
        threshold_ratio: None,
        context_window: None,
        strategy: compaction::CompactionStrategy::Narrative,
        bail_after_compactions: bail,
        custom_instructions: None,
    }
}

fn padded_thread() -> Vec<Message> {
    let mut initial = vec![Message::system("test system"), Message::user("seed")];
    let pad = "context detail that occupies transcript space ".repeat(6);
    for i in 0..3 {
        initial.push(Message::user(format!("padding user {i}: {pad}")));
        initial.push(Message::assistant(format!("padding assistant {i}: {pad}")));
    }
    initial
}

const SUMMARY: &str = "Summary: the assistant repeatedly issued a read tool call against the \
     workspace file and inspected the returned contents. No decisions were \
     finalized and no files were modified. The next concrete action is to \
     continue reading and then act on what the file contains.";

// ─── terminal paths ──────────────────────────────────────────────────────

#[test]
#[serial_test::serial]
fn stop_on_the_first_turn() {
    assert_golden(Scenario::new("stop_first_turn", vec![reply(Some("done"), None, "stop", 100, 5)]));
}

#[test]
#[serial_test::serial]
fn tool_turns_then_stop_rest_between_turns() {
    let mut s = Scenario::new(
        "tools_then_stop_with_rest",
        vec![
            reply(None, Some(read_x()), "tool_calls", 100, 20),
            reply(None, Some(read_x()), "tool_calls", 120, 20),
            reply(Some("done"), None, "stop", 140, 5),
        ],
    );
    s.turn_delay_ms = Some("500");
    assert_golden(s);
}

#[test]
#[serial_test::serial]
fn an_unexpected_finish_reason_is_an_error() {
    assert_golden(Scenario::new(
        "unexpected_finish_reason",
        vec![reply(Some("filtered"), None, "content_filter", 100, 5)],
    ));
}

// ─── pacing ──────────────────────────────────────────────────────────────

#[test]
#[serial_test::serial]
fn a_pace_pause_parks_then_resumes() {
    let mut s = Scenario::new(
        "pace_pause",
        vec![
            reply(None, Some(read_x()), "tool_calls", 100, 20),
            reply(Some("done"), None, "stop", 120, 5),
        ],
    );
    s.pace = Some(r#"{"pause": true, "reason": "thermal", "state": "serious"}"#);
    assert_golden(s);
}

/// The one scenario that spends real time: the soft inactivity warning is
/// read off a real clock, at a one-second budget, so the first model call
/// takes just over a second and its failed tool call proves no work.
#[test]
#[serial_test::serial]
fn a_call_past_the_soft_inactivity_threshold_queues_the_warning() {
    let mut slow = reply(None, Some(read_x()), "tool_calls", 100, 20);
    slow.0 = std::time::Duration::from_millis(1_100);
    let mut s = Scenario::new("inactivity_soft_warning", vec![slow, reply(Some("done"), None, "stop", 120, 5)]);
    s.inactivity_secs = Some("1");
    assert_golden(s);
}

// ─── tool calls and their detectors ──────────────────────────────────────

#[test]
#[serial_test::serial]
fn repeated_failing_calls_fire_the_cycle_and_cascade_nudges() {
    let mut replies: Vec<_> = (0..4).map(|i| reply(None, Some(read_x()), "tool_calls", 100 + i, 20)).collect();
    replies.push(reply(Some("done"), None, "stop", 200, 5));
    assert_golden(Scenario::new("cycle_and_cascade", replies));
}

#[test]
#[serial_test::serial]
fn three_edits_to_one_file_fire_the_cadence_nudge() {
    let edit = r#"{"path":"/workspace/a.rs","old_string":"a","new_string":"b"}"#;
    let mut replies: Vec<_> =
        (0..3).map(|i| reply(None, Some(calls(&[("edit", edit)])), "tool_calls", 100 + i, 20)).collect();
    replies.push(reply(Some("done"), None, "stop", 200, 5));
    assert_golden(Scenario::new("cadence_drift", replies));
}

#[test]
#[serial_test::serial]
fn empty_tool_calls_recover_then_escalate() {
    let replies = (0..3).map(|i| reply(None, Some(serde_json::json!([])), "tool_calls", 100 + i, 20)).collect();
    assert_golden(Scenario::new("empty_tool_calls", replies));
}

#[test]
#[serial_test::serial]
fn a_well_formed_call_cut_at_the_cap_is_salvaged() {
    let mut s = Scenario::new(
        "per_turn_cap_salvage",
        vec![
            reply(Some("thinking about it"), Some(read_x()), "length", 100, 500),
            reply(Some("done"), None, "stop", 120, 5),
        ],
    );
    s.max_tokens_per_call = Some(500);
    assert_golden(s);
}

#[test]
#[serial_test::serial]
fn a_plain_text_tool_call_is_promoted_and_dispatched() {
    let markup = "Let me read the file:\n<tool_call><function=read>\
                  <parameter=path>/workspace/x.txt</parameter>\
                  <parameter=offset>1</parameter><parameter=limit>1</parameter>\
                  </function></tool_call>";
    assert_golden(Scenario::new(
        "plain_text_promotion",
        vec![
            reply(Some(markup), None, "stop", 100, 20),
            reply(Some("done"), None, "stop", 120, 5),
        ],
    ));
}

// ─── the length arm: stalls and checkpoints ──────────────────────────────

#[test]
#[serial_test::serial]
fn an_empty_completion_at_the_cap_recovers_then_escalates() {
    let mut s = Scenario::new(
        "intra_turn_stall",
        (0..3).map(|i| reply(None, None, "length", 100 + i, 500)).collect(),
    );
    s.max_tokens_per_call = Some(500);
    assert_golden(s);
}

#[test]
#[serial_test::serial]
fn content_at_the_cap_checkpoints_and_the_turn_concludes() {
    let mut s = Scenario::new(
        "checkpoint_then_stop",
        vec![
            reply(Some("the first part of a long answer that "), None, "length", 100, 500),
            reply(Some("ends here."), None, "stop", 120, 5),
        ],
    );
    s.max_tokens_per_call = Some(500);
    assert_golden(s);
}

#[test]
#[serial_test::serial]
fn a_reasoning_turn_checkpoints_on_the_reasoning_interval() {
    let mut s = Scenario::new(
        "reasoning_checkpoint",
        vec![
            reply(Some("<think>I should read the file first.</think>"), Some(read_x()), "tool_calls", 100, 40),
            reply(Some("<think>The file is missing, so I"), None, "length", 120, 300),
            reply(Some(" will say so.</think>The file is missing."), None, "stop", 140, 30),
        ],
    );
    s.reasoning_interval = Some(300);
    s.max_tokens_per_call = Some(5_000);
    assert_golden(s);
}

#[test]
#[serial_test::serial]
fn a_repeating_stream_is_concluded_then_handed_off() {
    let looped: String = "the same thing over and over ".repeat(400);
    let pieces: Vec<&str> = looped.split_inclusive(' ').collect();
    let mut s = Scenario::new(
        "degenerate_stream_handoff",
        vec![
            sse("reasoning_content", &pieces, "stop", 2_000),
            sse("reasoning_content", &pieces, "stop", 2_000),
        ],
    );
    s.streaming = true;
    s.max_tokens_per_call = Some(9_000);
    s.reasoning_interval = Some(1_000);
    s.generation_interval = Some(1_000);
    assert_golden(s);
}

#[test]
#[serial_test::serial]
fn a_streamed_stop() {
    let mut s = Scenario::new(
        "streamed_stop",
        vec![
            sse("content", &["Let me ", "look."], "stop", 10),
        ],
    );
    s.streaming = true;
    assert_golden(s);
}

// ─── context management ──────────────────────────────────────────────────

#[test]
#[serial_test::serial]
fn a_compaction_installs_and_the_dispatch_continues() {
    let mut s = Scenario::new(
        "compaction_installed",
        vec![
            reply(None, Some(read_x()), "tool_calls", 5000, 50),
            reply(Some(SUMMARY), None, "stop", 500, 30),
            reply(Some("done"), None, "stop", 300, 5),
        ],
    );
    s.initial = padded_thread();
    s.cfg = compaction_cfg(None);
    s.tools = vec![Tool::Read];
    assert_golden(s);
}

#[test]
#[serial_test::serial]
fn a_refused_compaction_is_skipped_not_fatal() {
    let mut s = Scenario::new(
        "compaction_refused",
        vec![
            reply(None, Some(read_x()), "tool_calls", 5000, 50),
            reply(Some("too short"), None, "stop", 500, 30),
            reply(Some("too short"), None, "stop", 500, 30),
            reply(Some("done"), None, "stop", 300, 5),
        ],
    );
    s.initial = padded_thread();
    s.cfg = compaction_cfg(None);
    s.tools = vec![Tool::Read];
    assert_golden(s);
}

#[test]
#[serial_test::serial]
fn an_oversized_request_is_hard_trimmed_before_it_is_sent() {
    let big = "x".repeat(40_000);
    let mut s = Scenario::new("pre_send_bound", vec![reply(Some("done"), None, "stop", 100, 5)]);
    let tc = serde_json::from_value::<Vec<ToolCall>>(read_x()).unwrap();
    let mut assistant = Message::assistant("");
    assistant.content = None;
    assistant.tool_calls = Some(tc);
    s.initial = vec![
        Message::system("test system"),
        Message::user("do the task"),
        assistant,
        Message::tool_result("call_0", "read", big),
        Message::user("continue"),
    ];
    s.cfg.context_window = Some(3_000);
    s.tools = vec![Tool::Read];
    assert_golden(s);
}

// ─── resume ──────────────────────────────────────────────────────────────

#[test]
#[serial_test::serial]
fn a_resume_dispatches_only_the_pending_calls_then_continues() {
    let two = serde_json::from_value::<Vec<ToolCall>>(calls(&[("read", READ_X), ("read", READ_X)])).unwrap();
    let mut assistant = Message::assistant("");
    assistant.content = None;
    assistant.tool_calls = Some(two.clone());
    let messages = vec![
        Message::system("test system"),
        Message::user("do the task"),
        assistant,
        Message::tool_result("call_0", "read", "first result"),
    ];
    let mut s = Scenario::new("resume_pending_calls", vec![reply(Some("done"), None, "stop", 100, 5)]);
    s.resume_from = Some(checkpoint::RunCheckpoint {
        schema_version: checkpoint::CHECKPOINT_SCHEMA_VERSION,
        role_id: "test-role".into(),
        messages,
        turns: 1,
        total_completion_tokens: 20,
        compactions: 0,
        pending_hand_back: None,
        pending_tool_calls: Some(two[1..].to_vec()),
        pending_tool_calls_seq_base: 1,
        pending_head_started: false,
        written_at_unix_ms: 0,
    });
    assert_golden(s);
}
