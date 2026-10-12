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
