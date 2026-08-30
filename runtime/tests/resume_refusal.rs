//! (MUST FIX 2, merge-gate review of e6c99319) Pins `main.rs`'s
//! `checkpoint::validate_for_resume` call site — a fresh reviewer proved
//! that deleting the whole `if let Some(checkpoint) = &resume_from { ... }`
//! block in `main.rs` leaves every OTHER runtime test green, because
//! nothing spawns the actual binary and exercises `--resume` end to end.
//!
//! This does: builds a checkpoint whose system message differs from the
//! `--system` this invocation supplies by exactly ONE trailing space,
//! invokes the real `darkmux-runtime` binary with `--resume <path>`, and
//! asserts it exits 2 with `RESUME CHECKPOINT REFUSED` on stderr — BEFORE
//! any network call (no LMStudio, no mock server; if the call-site guard
//! were missing, the process would instead try to dial the default
//! `http://host.docker.internal:1234/v1` and fail differently — a
//! connection-refused error, not this exit code + message).
//!
//! Plain `std::process::Command` (via `CARGO_BIN_EXE_<name>`), no
//! `assert_cmd` dependency — matches this crate's small-dep-set doctrine
//! (see the workspace CLAUDE.md's "Don't add dependencies casually"); the
//! cargo-provided binary path env var is all a spawn-and-assert needs.

use std::process::Command;

const SYSTEM_PROMPT: &str = "You are the coder role. Do the task.";

fn write_checkpoint(dir: &std::path::Path, system_message: &str) -> std::path::PathBuf {
    let body = serde_json::json!({
        "schema_version": 3,
        "role_id": "coder",
        "messages": [
            {
                "role": "system",
                "content": system_message,
                "tool_calls": null,
                "tool_call_id": null,
                "name": null,
                "reasoning_content": null,
            },
            {
                "role": "user",
                "content": "do the thing",
                "tool_calls": null,
                "tool_call_id": null,
                "name": null,
                "reasoning_content": null,
            }
        ],
        "turns": 1,
        "total_prompt_tokens": 10,
        "total_completion_tokens": 5,
        "compactions": 0,
        "rest_ms": 0,
        "rests": 0,
        "pending_hand_back": null,
        "pending_tool_calls": null,
        "pending_tool_calls_seq_base": 0,
        "written_at_unix_ms": 0,
    });
    let path = dir.join("checkpoint.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
    path
}

#[test]
fn resume_is_refused_when_the_system_message_differs_by_one_trailing_space() {
    let tmp = tempfile::tempdir().unwrap();
    // The checkpoint's system message is SYSTEM_PROMPT with one trailing
    // space appended — everything else about this invocation matches
    // (same --model, same --role-id-equivalent context isn't even needed
    // runtime-side; only --system is compared).
    let checkpoint_path = write_checkpoint(tmp.path(), &format!("{SYSTEM_PROMPT} "));

    let bin = env!("CARGO_BIN_EXE_darkmux-runtime");
    let output = Command::new(bin)
        .args([
            "run",
            "--model",
            "darkmux:does-not-need-to-exist",
            "--system",
            SYSTEM_PROMPT, // no trailing space — one byte different
            "--prompt",
            "do the thing",
            "--resume",
        ])
        .arg(&checkpoint_path)
        .output()
        .expect("spawning darkmux-runtime");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit code 2 on a refused resume; stderr was: {stderr}"
    );
    assert!(
        stderr.contains("RESUME CHECKPOINT REFUSED"),
        "expected stderr to contain the named refusal, got: {stderr}"
    );
    assert!(
        !stderr.contains("connection") && !stderr.contains("host.docker.internal"),
        "the refusal must happen BEFORE any network attempt — stderr suggests a network call \
         was made instead: {stderr}"
    );
}

#[test]
fn resume_succeeds_past_validation_when_the_system_message_matches_exactly() {
    // Sibling of the refusal test above — proves the guard isn't refusing
    // EVERY resume, only a genuine mismatch. This checkpoint's system
    // message is byte-identical to --system, so `validate_for_resume`
    // passes and the process proceeds into the loop, where it WILL then
    // fail (no LMStudio reachable in this test environment) — but that
    // failure must be a network/connection error, never the
    // "RESUME CHECKPOINT REFUSED" this test is distinguishing itself from.
    let tmp = tempfile::tempdir().unwrap();
    let checkpoint_path = write_checkpoint(tmp.path(), SYSTEM_PROMPT);

    let bin = env!("CARGO_BIN_EXE_darkmux-runtime");
    let output = Command::new(bin)
        .args([
            "run",
            "--model",
            "darkmux:does-not-need-to-exist",
            "--system",
            SYSTEM_PROMPT,
            "--prompt",
            "do the thing",
            "--resume",
        ])
        .arg(&checkpoint_path)
        .output()
        .expect("spawning darkmux-runtime");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("RESUME CHECKPOINT REFUSED"),
        "a byte-identical system message must pass validate_for_resume; stderr was: {stderr}"
    );
}
