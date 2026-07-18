//! Real end-to-end mock-dispatch proof (mock-model harness, v1, item 4).
//!
//! Runs `darkmux_crew::dispatch::dispatch(DispatchOpts { .. })`, the SAME
//! function `darkmux dispatch` and the `mission launch coder-phase` /
//! `review` pipelines call, for real:
//!
//!   - a REAL, genuinely separate OS process (`tools/darkmux-mock-model`)
//!     is spawned and reached over a real TCP socket, standing in for
//!     "the model" with a scripted/deterministic response
//!   - a REAL `docker run` executes (`darkmux-runtime:latest`, built from
//!     this worktree's `runtime/` — see the repo's release doctrine: "dogfood
//!     the dispatch critical path first")
//!   - the container's real agent loop makes a real HTTP call out to the
//!     mock server via `http://host.docker.internal:<port>/v1` (the exact
//!     seam real LMStudio dispatches use — proven reachable here for a
//!     127.0.0.1-bound service, the same bind LMStudio itself defaults to)
//!   - real flow records (`dispatch.start` / `dispatch.complete`) land on
//!     disk
//!
//! Zero LMStudio, zero GPU, zero real AI anywhere in this test — the ONLY
//! thing standing in for "the model" is the scripted mock-model process.
//!
//! **Requires Docker** (a running daemon + `darkmux-runtime:latest` built
//! locally — `docker build -t darkmux-runtime:latest runtime/` from the
//! repo root) and a build of `tools/darkmux-mock-model` (built on demand
//! below via `cargo build`, so the only manual precondition is Docker).
//! `#[ignore]`d by default so `cargo test --workspace` never requires
//! Docker; run explicitly with:
//!
//! ```sh
//! cargo test -p darkmux-crew --test mock_dispatch_proof -- --ignored --nocapture
//! ```

use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use darkmux_crew::dispatch::{dispatch, CompactionDispatchArgs, DispatchOpts};

/// Repo root, resolved from this crate's own manifest dir at compile time —
/// `crates/darkmux-crew` → repo root is two levels up.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/darkmux-crew has a repo root two levels up")
        .to_path_buf()
}

/// Bind an ephemeral port, read what the OS assigned, then drop the
/// listener immediately so `darkmux-mock-model` can bind it next. Standard
/// "find a free port" trick for spawning a child that needs an explicit
/// `--port` (httpmock's standalone entry point doesn't report back an
/// OS-assigned port itself — see `tools/darkmux-mock-model/src/main.rs`'s
/// module doc). Small TOCTOU window; acceptable for local dev-tool tests.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

/// Build `tools/darkmux-mock-model` (debug profile — this is a test-time
/// tool, not a release artifact) and return its binary path. Builds fresh
/// every run rather than caching a stale binary across source edits;
/// `cargo build` itself is a no-op when nothing changed.
fn build_mock_model_binary() -> PathBuf {
    let manifest = repo_root().join("tools/darkmux-mock-model/Cargo.toml");
    let status = Command::new("cargo")
        .args(["build", "--quiet", "--manifest-path"])
        .arg(&manifest)
        .status()
        .expect("spawning `cargo build` for darkmux-mock-model");
    assert!(status.success(), "darkmux-mock-model failed to build — see cargo output above");

    let bin = repo_root().join("tools/darkmux-mock-model/target/debug/darkmux-mock-model");
    assert!(bin.is_file(), "expected mock-model binary at {}", bin.display());
    bin
}

/// Spawn the mock-model binary as a REAL child process on `port`, wait
/// (bounded) for its `MOCK-MODEL-READY` stdout line, and hand back the
/// live `Child` handle. `script` is an optional path to a JSON script file
/// (`--script`); `None` uses the built-in fixed-stop-response default.
fn spawn_mock_model(bin: &Path, port: u16, script: Option<&Path>) -> Child {
    let mut cmd = Command::new(bin);
    cmd.args(["--port", &port.to_string()]);
    if let Some(script) = script {
        cmd.args(["--script"]).arg(script);
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning darkmux-mock-model as a real child process");

    // Read stdout on a background thread until MOCK-MODEL-READY appears (or
    // the pipe closes / a bounded deadline elapses) — a real "wait for the
    // process to be listening" gate, not a fixed sleep-and-hope.
    let stdout = child.stdout.take().expect("mock-model stdout piped");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            let ready = line.starts_with("MOCK-MODEL-READY");
            let _ = tx.send(line);
            if ready {
                break;
            }
        }
    });

    let mut saw_ready = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                eprintln!("[mock-model stdout] {line}");
                if line.starts_with("MOCK-MODEL-READY") {
                    saw_ready = true;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    assert!(
        saw_ready,
        "darkmux-mock-model never printed MOCK-MODEL-READY within 10s — it did not come up"
    );
    child
}

/// Write a minimal profiles registry naming exactly one LOCAL model (no
/// `endpoint` block) — resolves through the same
/// `resolve_dispatch_model_internal` path a real profile would, with no
/// gestalt/`lms` involvement (confirmed by reading
/// `dispatch_internal::dispatch`: it resolves a model NAME from the
/// registry and never calls a `ModelHost` — real model residency is a
/// separate, not-yet-wired concern; see the PR description this test ships
/// with for the full finding).
fn write_mock_profiles_registry(dir: &Path) -> PathBuf {
    let path = dir.join("profiles.json");
    let body = serde_json::json!({
        "schema_version": "1.5",
        "default_profile": "mock",
        "profiles": {
            "mock": {
                "models": [
                    { "id": "mock-model", "n_ctx": 8192 }
                ]
            }
        }
    });
    std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap())
        .expect("writing temp profiles.json");
    path
}

/// Shared harness for both proof tests below: spawn a real, separate
/// mock-model process (optionally with a `--script`), run one real
/// container dispatch pointed at it, tear the mock-model process down, and
/// hand back the dispatch result + the isolated flow-records dir for the
/// caller's own assertions. `message` is the operator-facing prompt text
/// (kept distinct per test so trigger-based scripts can key off it).
fn run_mock_dispatch(
    bin: &Path,
    script: Option<&Path>,
    message: &str,
) -> (darkmux_crew::dispatch::DispatchResult, tempfile::TempDir, String) {
    let port = free_port();
    let mut mock = spawn_mock_model(bin, port, script);

    // Isolate flow-record output + the profiles registry to tempdirs —
    // never touches the operator's real `~/.darkmux`.
    let flows_dir = tempfile::tempdir().expect("tempdir for flow records");
    let registry_dir = tempfile::tempdir().expect("tempdir for profiles registry");
    let profiles_path = write_mock_profiles_registry(registry_dir.path());

    // SAFETY (matches the pattern `darkmux-flow`'s own tests use for this
    // exact env var): these tests are `#[ignore]`d and run one at a time by
    // hand (`cargo test ... -- --ignored`), so there's no cross-test race
    // over DARKMUX_FLOWS_DIR here.
    let prev_flows_dir = std::env::var("DARKMUX_FLOWS_DIR").ok();
    unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", flows_dir.path()) };

    let session_id = format!("mock-model-proof-{}-{}", std::process::id(), port);
    let opts = DispatchOpts {
        role_id: "analyst".to_string(),
        message: message.to_string(),
        session_id: Some(session_id.clone()),
        timeout_seconds: 120,
        skip_preflight: true,
        json: true,
        workdir: None,
        phase_id: None,
        machine: None,
        wait: true,
        compaction: CompactionDispatchArgs::default(),
        profile_name: Some("mock".to_string()),
        config_path: Some(profiles_path.to_string_lossy().to_string()),
        force_container: false,
        max_completion_tokens: None,
        image: None,
        model_base_url_override: Some(format!("http://host.docker.internal:{port}/v1")),
        step_id: None, // (#1483) set on the graph-step path only
    };

    let result = dispatch(opts);

    // Tear the mock-model process down regardless of dispatch outcome — a
    // failed assertion in the caller must not leak the child.
    let _ = mock.kill();
    let _ = mock.wait();

    if let Some(prev) = prev_flows_dir {
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", prev) };
    } else {
        unsafe { std::env::remove_var("DARKMUX_FLOWS_DIR") };
    }

    let result = result.expect("dispatch() must return Ok — the container path ran to completion");
    eprintln!("--- dispatch stdout ---\n{}", result.stdout);
    eprintln!("--- dispatch stderr (tail) ---\n{}", tail(&result.stderr, 4000));
    (result, flows_dir, session_id)
}

#[test]
#[ignore = "requires Docker + a local darkmux-runtime:latest image"]
fn real_container_dispatch_round_trips_through_a_standalone_mock_model_process() {
    let bin = build_mock_model_binary();
    let (result, flows_dir, session_id) = run_mock_dispatch(
        &bin,
        None,
        "Say something short. This is the darkmux mock-model harness proof — \
         there is no real model on the other end of this dispatch.",
    );

    assert_eq!(result.exit_code, 0, "the runtime container must exit 0 on a clean stop");

    // The internal runtime's `--json` envelope: { result, final_assistant,
    // metrics, trajectory_path }. `result` must be "stop" (the mock's fixed
    // response has finish_reason "stop", no tool_calls) and
    // `final_assistant` must contain the mock's scripted content —
    // PROVING the completion the agent loop acted on genuinely came back
    // over the network from the separate mock-model process, not from any
    // fallback/default text baked into the runtime itself.
    let envelope: Value =
        serde_json::from_str(&result.stdout).expect("stdout is the runtime's JSON envelope");
    assert_eq!(
        envelope.get("result").and_then(Value::as_str),
        Some("stop"),
        "envelope: {envelope}"
    );
    let final_assistant = envelope
        .get("final_assistant")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        final_assistant.contains("mock dispatch complete"),
        "final_assistant must be the mock server's scripted content, not a fallback \
         string, got: {final_assistant:?}"
    );

    // Real flow records: dispatch.start + a terminal dispatch.complete for
    // THIS session_id, on disk under the isolated flows dir (contract #2,
    // "dispatch liveness" — CLAUDE.md's cross-system contracts section).
    let (saw_start, saw_complete) = scan_flow_records_for_session(flows_dir.path(), &session_id);
    assert!(saw_start, "no dispatch.start flow record found for session {session_id}");
    assert!(
        saw_complete,
        "no terminal dispatch.complete/dispatch.error flow record found for session {session_id}"
    );
}

/// The "small script" mode (item 1's second scripting shape): a two-turn
/// script — turn 1 calls the `read` tool, turn 2 (triggered by the
/// tool-role message the runtime appends after executing it) returns a
/// final `stop`. Proves the trigger-based multi-turn scripting mechanism
/// through the REAL container path, not just via the direct-curl smoke
/// check against the standalone server used to validate the routing logic
/// during development.
#[test]
#[ignore = "requires Docker + a local darkmux-runtime:latest image"]
fn real_container_dispatch_executes_a_scripted_multi_turn_tool_call_sequence() {
    let bin = build_mock_model_binary();

    let script_dir = tempfile::tempdir().expect("tempdir for the script file");
    let script_path = script_dir.path().join("read-then-stop.json");
    let script = serde_json::json!([
        {
            // Fires once the runtime has appended the tool's result as a
            // `role: "tool"` message — i.e. turn 2 onward.
            "trigger": "\"role\":\"tool\"",
            "response": {
                "id": "turn2", "object": "chat.completion", "created": 0, "model": "mock-model",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "all done after reading the file", "tool_calls": null },
                    "finish_reason": "stop",
                }],
                "usage": { "prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10 },
            },
        },
        {
            // No trigger ⇒ matches turn 1 (no tool-role message yet).
            "response": {
                "id": "turn1", "object": "chat.completion", "created": 0, "model": "mock-model",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "read",
                                "arguments": "{\"path\":\"nope.txt\",\"offset\":1,\"limit\":0}",
                            },
                        }],
                    },
                    "finish_reason": "tool_calls",
                }],
                "usage": { "prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10 },
            },
        },
    ]);
    std::fs::write(&script_path, serde_json::to_string_pretty(&script).unwrap())
        .expect("writing the scripted-turn JSON file");

    let (result, _flows_dir, _session_id) = run_mock_dispatch(
        &bin,
        Some(&script_path),
        "Read a file, then tell me you're done. This is the darkmux mock-model \
         harness's scripted multi-turn proof.",
    );

    assert_eq!(result.exit_code, 0, "the runtime container must exit 0 on a clean stop");

    let envelope: Value =
        serde_json::from_str(&result.stdout).expect("stdout is the runtime's JSON envelope");
    assert_eq!(envelope.get("result").and_then(Value::as_str), Some("stop"), "envelope: {envelope}");
    assert_eq!(
        envelope.get("metrics").and_then(|m| m.get("turns")).and_then(Value::as_u64),
        Some(2),
        "the script's two turns must have driven exactly two loop iterations — proves the \
         tool-call turn genuinely round-tripped before the trigger-matched stop turn fired, \
         not that the loop got lucky and matched the fallback stop response first"
    );
    let final_assistant = envelope
        .get("final_assistant")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        final_assistant.contains("all done after reading the file"),
        "final_assistant must be turn 2's scripted content, got: {final_assistant:?}"
    );
}

fn tail(s: &str, max: usize) -> &str {
    let n = s.len();
    if n <= max {
        s
    } else {
        &s[n - max..]
    }
}

/// Walk every `*.jsonl` file under `flows_dir` (LocalFileSink's per-day
/// layout) and report whether a `dispatch.start` and a terminal
/// `dispatch.complete`/`dispatch.error` record for `session_id` were seen.
fn scan_flow_records_for_session(flows_dir: &Path, session_id: &str) -> (bool, bool) {
    let mut saw_start = false;
    let mut saw_complete = false;
    let Ok(entries) = std::fs::read_dir(flows_dir) else {
        return (false, false);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        for line in content.lines() {
            let Ok(record) = serde_json::from_str::<Value>(line) else { continue };
            if record.get("session_id").and_then(Value::as_str) != Some(session_id) {
                continue;
            }
            // `FlowRecord::action` carries the literal string (with a space,
            // not a dot — "dispatch start" / "dispatch complete" / "dispatch
            // error"; see `darkmux-flow/src/schema.rs::FlowRecord` and
            // `dispatch_internal.rs`'s emission call sites).
            match record.get("action").and_then(Value::as_str) {
                Some("dispatch start") => saw_start = true,
                Some("dispatch complete") | Some("dispatch error") => saw_complete = true,
                _ => {}
            }
        }
    }
    (saw_start, saw_complete)
}
