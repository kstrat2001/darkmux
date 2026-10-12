//! (#3207) The compactor's own unmanaged endpoint reaches the REAL
//! `darkmux-runtime` binary's argument loop and stdin read: the flags are
//! accepted, a bad value exits 2 naming the flag, and a compactor header the
//! stdin blob does not carry exits 2 before any model call. Same seam as
//! `compactor_disclosure.rs`; `--base-url http://127.0.0.1:1` is a dead port,
//! so nothing here reaches a model.

use std::io::Write;
use std::process::{Command, Stdio};

fn run(extra: &[&str], stdin: Option<&str>) -> (Option<i32>, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_darkmux-runtime"));
    cmd.args([
        "run",
        "--model",
        "darkmux:does-not-need-to-exist",
        "--system",
        "You are a test.",
        "--prompt",
        "hello",
        "--context-window",
        "32000",
        "--compactor-model",
        "mlx-util",
        "--base-url",
        "http://127.0.0.1:1",
        "--no-stream",
    ])
    .args(extra)
    .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawning darkmux-runtime");
    if let Some(blob) = stdin {
        let mut pipe = child.stdin.take().unwrap();
        pipe.write_all(blob.as_bytes()).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    (out.status.code(), String::from_utf8_lossy(&out.stderr).to_string())
}

#[test]
fn the_compactor_endpoint_flags_are_accepted() {
    let (_, stderr) = run(
        &[
            "--compactor-chat-url",
            "http://127.0.0.1:1/v1/chat/completions",
            "--compactor-dialect",
            "chat-completions-max-tokens",
            "--compactor-auth-header-stdin",
        ],
        Some(r#"{"compactor":{"header":"Authorization","value":"Bearer x"}}"#),
    );
    assert!(!stderr.contains("unknown flag"), "{stderr}");
    assert!(!stderr.contains("--compactor-"), "no compactor flag was refused: {stderr}");
}

#[test]
fn a_bad_compactor_dialect_exits_2_naming_the_flag() {
    let (code, stderr) = run(&["--compactor-dialect", "nope"], None);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("--compactor-dialect must be"), "{stderr}");
}

#[test]
fn a_compactor_header_missing_from_stdin_exits_2_before_any_call() {
    let (code, stderr) = run(&["--compactor-auth-header-stdin"], Some(r#"{"header":"h","value":"v"}"#));
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("--compactor-auth-header-stdin"), "{stderr}");
}

/// (#3207 review F1) The compactor client the binary builds from these
/// flags is the one compaction uses: with the brain on one mock (LM Studio's
/// stand-in, at `--base-url`) and the compactor's endpoint on another, a
/// forced compaction reaches the compactor's endpoint with the bare id, its
/// dialect (`max_tokens`) and the header piped on stdin, and never the brain.
// A bare `MockServer`: an integration test cannot reach the bin crate's
// `test_support::GuardedMockServer`. The #2599 shadowing it guards against
// cannot happen here: each server's mocks match disjoint `model` values, and
// the assertions read each mock's own hit count.
#[allow(clippy::disallowed_types)]
#[test]
fn a_forced_compaction_reaches_the_compactor_endpoint_with_its_stdin_header() {
    use httpmock::prelude::*;
    let brain = MockServer::start();
    let read_call = serde_json::json!([{
        "id": "call_1", "type": "function",
        "function": { "name": "read", "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":0}" },
    }]);
    // Every brain turn reports a prompt far past the threshold and asks for
    // a tool, so the loop keeps going and compaction fires once there is a
    // middle to replace.
    let _brain_turns = brain.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions").body_contains("\"brain-model\"");
        then.status(200).json_body(serde_json::json!({
            "id": "b", "object": "chat.completion", "created": 0, "model": "brain-model",
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": null, "tool_calls": read_call }, "finish_reason": "tool_calls" }],
            "usage": { "prompt_tokens": 5000, "completion_tokens": 20, "total_tokens": 5020 },
        }));
    });
    let brain_compactions = brain.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions").body_contains("\"mlx-util\"");
        then.status(500);
    });
    let endpoint = MockServer::start();
    let summary = "Summary: the assistant read the workspace file several times and inspected what it \
                   returned. Nothing was decided and nothing was modified yet. The next step is to keep \
                   reading the file and then act on what it contains.";
    let compactions = endpoint.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .header("authorization", "Bearer util-secret")
            .json_body_partial(r#"{"model":"mlx-util"}"#)
            .body_contains("\"max_tokens\"");
        then.status(200).json_body(serde_json::json!({
            "id": "c", "object": "chat.completion", "created": 0, "model": "mlx-util",
            "choices": [{ "index": 0, "message": { "role": "assistant", "content": summary }, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 900, "completion_tokens": 40, "total_tokens": 940 },
        }));
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_darkmux-runtime"))
        .args(["run", "--model", "brain-model", "--system", "You are a test.", "--prompt", "read the file"])
        .args(["--compact-threshold-tokens", "1000", "--compactor-model", "mlx-util", "--max-turns", "6", "--no-stream"])
        .args(["--base-url", &format!("{}/v1", brain.base_url())])
        .args(["--compactor-chat-url", &format!("{}/v1/chat/completions", endpoint.base_url())])
        .args(["--compactor-dialect", "chat-completions-max-tokens", "--compactor-auth-header-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning darkmux-runtime");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"compactor":{"header":"Authorization","value":"Bearer util-secret"}}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(compactions.hits() >= 1, "no compaction reached the compactor's endpoint; stderr: {stderr}");
    assert_eq!(brain_compactions.hits(), 0, "a compaction went to the brain's LM Studio; stderr: {stderr}");
}
