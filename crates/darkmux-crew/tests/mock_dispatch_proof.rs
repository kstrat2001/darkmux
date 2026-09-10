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

/// Restores a process-wide env var to its prior value on drop — the
/// clean-by-construction sibling of `brief_refs_graph_proof.rs`'s
/// `EnvGuard` in this same directory, duplicated here rather than shared
/// (each `tests/*.rs` file compiles as its own binary; no `tests/common/`
/// module exists yet, and one ten-line struct isn't worth inventing one
/// for). Panic-safe where a manual save/restore pair is not: a `dispatch()`
/// panic (not just an `Err`) still runs `Drop` and restores the var.
struct EnvVarGuard {
    key: &'static str,
    prev: Option<String>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let prev = std::env::var(key).ok();
        // SAFETY: `set_var`/`remove_var` are unsafe because of the
        // possibility of a concurrent `getenv` from another thread in
        // this process. `#[serial_test::serial]` on every caller of this
        // guard only bounds concurrency BETWEEN TESTS in this binary —
        // it says nothing about threads a single test's own call to
        // `dispatch()` spawns. The actual guarantee: this guard sets the
        // var BEFORE `dispatch()` is called and restores it only AFTER
        // `dispatch()` returns, and `dispatch_internal::dispatch` joins
        // its watchdog/sampler/tailer threads on every one of its exit
        // paths before returning (see `dispatch_internal.rs`) — so by
        // construction no thread `dispatch()` spawned is still alive,
        // and therefore no thread still reading env, at either mutation
        // point. This is a property of the call sequence in this file,
        // not something `serial_test` itself proves.
        unsafe { std::env::set_var(key, value) };
        EnvVarGuard { key, prev }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

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

    // Both callers of this helper carry `#[serial_test::serial]` (below), so
    // only one of this file's tests ever holds DARKMUX_FLOWS_DIR at a time —
    // `#[ignore]` alone does NOT guarantee that: the documented invocation
    // (`cargo test ... -- --ignored`, no `--test-threads=1`) runs both
    // `#[ignore]`d tests in this file concurrently by default, and two
    // threads racing `set_var`/`remove_var` on the same process-wide
    // DARKMUX_FLOWS_DIR is exactly what made this session's `dispatch.start`
    // (or `dispatch.complete`) land in the OTHER test's isolated flows dir —
    // see `real_container_dispatch_round_trips_through_a_standalone_mock_model_process`'s
    // own history (#2486): the container path and the flow-record emitter
    // were never at fault. This is a wide-window race, not a deterministic
    // failure — measured at 5 red out of 6 runs under the default parallel
    // invocation, and 0 red out of several once serialized. A future green
    // run at default parallelism is the expected occasional outcome of a
    // race, not evidence the diagnosis was wrong.
    let flows_dir_guard = EnvVarGuard::set("DARKMUX_FLOWS_DIR", flows_dir.path());

    let session_id = format!("mock-model-proof-{}-{}", std::process::id(), port);
    let opts = DispatchOpts {
        brief_refs: Vec::new(),
        workspace_read_only: false,
        record_context: None,
        resume_from: None,
        host_out: None,
        max_turns_override: None,
        timeout_override_seconds: None, // (#2480)
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
        system_prompt_override: None,
    };

    let result = dispatch(opts);

    // Restore DARKMUX_FLOWS_DIR as soon as the dispatch itself is done —
    // same point the old manual restore ran, before mock teardown / the
    // caller's own assertions.
    drop(flows_dir_guard);

    // Tear the mock-model process down regardless of dispatch outcome — a
    // failed assertion in the caller must not leak the child.
    let _ = mock.kill();
    let _ = mock.wait();

    let result = result.expect("dispatch() must return Ok — the container path ran to completion");
    eprintln!("--- dispatch stdout ---\n{}", result.stdout);
    eprintln!("--- dispatch stderr (tail) ---\n{}", tail(&result.stderr, 4000));
    (result, flows_dir, session_id)
}

#[test]
#[ignore = "requires Docker + a local darkmux-runtime:latest image"]
#[serial_test::serial] // env-scoped DARKMUX_FLOWS_DIR, like its neighbor below (#2486)
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
#[serial_test::serial] // env-scoped DARKMUX_FLOWS_DIR, like its neighbor above (#2486)
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

// ─── #2596: pin the host→container inactivity-budget hop ──────────────
//
// #2480 disclosed (and left unpinned) that the single line joining
// `DispatchOpts::timeout_override_seconds` to the container's ACTUAL
// inactivity budget — `effective_inactivity_timeout_seconds(opts.
// timeout_override_seconds)` at the `argv_config` call site in
// `dispatch_internal::dispatch` — has no test that fails when that
// argument is swapped for `None`: mutating it built clean and left all
// 1585 tests green. #2596 made it worse: `resolved_runtime_bounds_json`
// (the `dispatch.start` record's `bounds` block) reads the SAME
// `opts.timeout_override_seconds` on a SEPARATE call site and hand-rolls
// its own copy of the same precedence logic — so the two can
// independently drift, and a reader trusting the record would never know
// the container ran on a different number than the one it reports.
//
// Both tests below run the REAL container path (`dispatch()` + a real
// `docker run` + the mock-model harness above) and intercept the actual
// `docker run` invocation through a transparent argv-capturing wrapper
// placed ahead of the real `docker` on `PATH` — the wrapper writes every
// argv token to a file, one per line, then `exec`s straight into the real
// `docker` binary, so the container that runs is bit-for-bit the one a
// real dispatch would spawn. This is the direct way to observe what the
// container was ACTUALLY given: `--rm` removes the container the instant
// it exits (so `docker inspect` after the fact sees nothing), and the
// runtime never echoes its own resolved inactivity budget back into the
// JSON envelope on a clean run. It is not the ONLY way — a dispatch that
// stalls past 75% of its budget makes the runtime print its resolved
// bound to stderr (`runtime/src/loop_runner.rs`'s soft-threshold nudge)
// — but that requires provoking a stall and is far slower than reading
// the argv directly.
//
// A hop still sits below both tests here, unpinned by either: the
// runtime reads the forwarded value with
// `std::env::var("DARKMUX_INACTIVITY_TIMEOUT_SECONDS").ok().and_then(|s|
// s.parse().ok()).unwrap_or(600)` (`runtime/src/loop_runner.rs`) — these
// tests confirm the HOST forwards the right value to the container's
// environment, not that the RUNTIME inside the container parses that
// env var correctly. A typo'd literal or a flipped default there would
// leave every container silently running on 600 while both tests here
// stay green, since neither reads runtime-side behavior back out.

/// Locate the real `docker` binary via `command -v docker`, resolved
/// BEFORE this test prepends a capturing wrapper to `PATH` — the wrapper
/// script needs an absolute path to `exec` into, not another PATH lookup
/// that would just find itself.
fn real_docker_path() -> PathBuf {
    let output = Command::new("sh")
        .arg("-c")
        .arg("command -v docker")
        .output()
        .expect("locating the real docker binary via `command -v docker`");
    assert!(
        output.status.success(),
        "`command -v docker` failed — is Docker installed on PATH?"
    );
    let path = String::from_utf8(output.stdout)
        .expect("docker path is valid UTF-8")
        .trim()
        .to_string();
    assert!(!path.is_empty(), "`command -v docker` returned nothing");
    PathBuf::from(path)
}

/// Single-quote a path for embedding literally in the generated shell
/// script below — these are tempdir-derived paths on this machine, never
/// operator input, but quoting defensively costs nothing.
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// Write a `docker` shell wrapper into `dir` that appends every argv
/// token it receives (one per line, `---END---`-delimited per
/// invocation) to `capture_file`, then `exec`s the real docker binary
/// with the SAME argv — a transparent tee, not a mock: the container
/// that spawns is the real one, byte-for-byte the same invocation a real
/// dispatch would make. Prepending `dir` to `PATH` (see
/// `PathPrependGuard`) is what makes `Command::new("docker")` inside
/// `dispatch()` resolve to this wrapper instead of the real binary.
fn write_docker_argv_capture_wrapper(dir: &Path, real_docker: &Path, capture_file: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let wrapper_path = dir.join("docker");
    let script = format!(
        "#!/bin/sh\n\
         {{\n\
         for a in \"$@\"; do printf '%s\\n' \"$a\"; done\n\
         printf -- '---END---\\n'\n\
         }} >> {capture}\n\
         exec {real} \"$@\"\n",
        capture = shell_quote(capture_file),
        real = shell_quote(real_docker),
    );
    std::fs::write(&wrapper_path, script).expect("writing the docker argv-capture wrapper");
    let mut perms = std::fs::metadata(&wrapper_path)
        .expect("stat the wrapper we just wrote")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&wrapper_path, perms).expect("chmod +x the docker argv-capture wrapper");
}

/// Prepends `dir` to `PATH` for the process's lifetime, restoring the
/// original value on drop — same shape as `EnvVarGuard` above, but a
/// prepend instead of a replace (PATH must keep resolving every other
/// tool this process needs, and once the wrapper `exec`s into the real
/// `docker` binary the rest of that process's own PATH lookups are none
/// of this guard's business).
struct PathPrependGuard {
    prev: Option<String>,
}

impl PathPrependGuard {
    fn new(dir: &Path) -> Self {
        let prev = std::env::var("PATH").ok();
        let joined = match &prev {
            Some(existing) => format!("{}:{}", dir.display(), existing),
            None => dir.display().to_string(),
        };
        // SAFETY: matches `EnvVarGuard`'s own safety note above — the
        // guarantee is the call sequence (mutate before `dispatch()`,
        // restore only after it returns, and `dispatch()` joins every
        // thread it spawned on every exit path before returning), not
        // `serial_test` alone. `serial_test` only bounds concurrency
        // BETWEEN tests in this binary.
        unsafe { std::env::set_var("PATH", &joined) };
        PathPrependGuard { prev }
    }
}

impl Drop for PathPrependGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
    }
}

/// The forwarded `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` value + companion
/// `_SOURCE` string from the LAST `docker run` invocation recorded in
/// `capture_file` — the ground truth of what the container was actually
/// given, read back from the real `docker run` argv rather than inferred
/// from anything darkmux itself reports.
fn captured_container_inactivity_budget(capture_file: &Path) -> (String, String) {
    let content = std::fs::read_to_string(capture_file).unwrap_or_else(|e| {
        panic!("reading docker argv capture file {}: {e}", capture_file.display())
    });
    // Each invocation is a run of lines terminated by `---END---`; take
    // the LAST one whose argv contains a bare `run` token (there is
    // exactly one per dispatch — `--rm` means no separate teardown
    // `docker rm` call, and the happy path here never calls `docker
    // kill`).
    let mut value = None;
    let mut source = None;
    for invocation in content.split("---END---\n") {
        let lines: Vec<&str> = invocation.lines().collect();
        if !lines.contains(&"run") {
            continue;
        }
        // Reset per matching invocation — without this, an EARLIER `run`
        // invocation's values would survive into a LATER one that lacks
        // the forwarded vars (e.g. a future code path that stops
        // forwarding them), reporting a stale value as if it were the
        // last invocation's ground truth instead of failing loudly.
        value = None;
        source = None;
        for line in &lines {
            if let Some(v) = line.strip_prefix("DARKMUX_INACTIVITY_TIMEOUT_SECONDS=") {
                value = Some(v.to_string());
            } else if let Some(s) = line.strip_prefix("DARKMUX_INACTIVITY_TIMEOUT_SECONDS_SOURCE=") {
                source = Some(s.to_string());
            }
        }
    }
    (
        value.unwrap_or_else(|| {
            panic!(
                "no `docker run` invocation with a forwarded \
                 DARKMUX_INACTIVITY_TIMEOUT_SECONDS env var found in {}:\n{content}",
                capture_file.display()
            )
        }),
        source.unwrap_or_else(|| {
            panic!(
                "no `docker run` invocation with a forwarded \
                 DARKMUX_INACTIVITY_TIMEOUT_SECONDS_SOURCE env var found in {}:\n{content}",
                capture_file.display()
            )
        }),
    )
}

/// The `dispatch.start` flow record's `payload.bounds.inactivity_timeout_seconds`
/// block (`{"value": ..., "source": ...}`) for `session_id` — the
/// operator-facing record of what THIS dispatch's budget was resolved to.
fn dispatch_start_inactivity_bounds(flows_dir: &Path, session_id: &str) -> Value {
    let entries = std::fs::read_dir(flows_dir).expect("reading the isolated flows dir");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        for line in content.lines() {
            let Ok(record) = serde_json::from_str::<Value>(line) else { continue };
            if record.get("session_id").and_then(Value::as_str) != Some(session_id) {
                continue;
            }
            if record.get("action").and_then(Value::as_str) != Some("dispatch start") {
                continue;
            }
            return record
                .pointer("/payload/bounds/inactivity_timeout_seconds")
                .cloned()
                .unwrap_or_else(|| {
                    panic!(
                        "dispatch.start record for {session_id} has no \
                         payload.bounds.inactivity_timeout_seconds: {record}"
                    )
                });
        }
    }
    panic!(
        "no dispatch.start flow record found for session {session_id} under {}",
        flows_dir.display()
    );
}

/// Shared harness for the two tests below: identical in shape to
/// `run_mock_dispatch` above except it (a) sets `timeout_override_seconds`
/// on the dispatch (the field that harness hardcodes to `None`) and (b)
/// prepends a `docker` argv-capturing wrapper to `PATH` so the caller can
/// read back what the container was ACTUALLY given, not just what the
/// flow record claims.
fn run_mock_dispatch_with_timeout_override(
    bin: &Path,
    message: &str,
    timeout_override_seconds: u32,
) -> (
    darkmux_crew::dispatch::DispatchResult,
    tempfile::TempDir,
    String,
    PathBuf,
    tempfile::TempDir,
) {
    let port = free_port();
    let mut mock = spawn_mock_model(bin, port, None);

    // Isolate flow-record output + the profiles registry to tempdirs —
    // never touches the operator's real `~/.darkmux`.
    let flows_dir = tempfile::tempdir().expect("tempdir for flow records");
    let registry_dir = tempfile::tempdir().expect("tempdir for profiles registry");
    let profiles_path = write_mock_profiles_registry(registry_dir.path());

    let wrapper_dir = tempfile::tempdir().expect("tempdir for the docker argv-capture wrapper");
    let capture_file = wrapper_dir.path().join("docker-argv.log");
    std::fs::write(&capture_file, "").expect("creating the empty capture file");
    let real_docker = real_docker_path();
    write_docker_argv_capture_wrapper(wrapper_dir.path(), &real_docker, &capture_file);

    // Both guards restore on drop regardless of how this function returns
    // (including via the `.expect()` panic on `dispatch()` below) — same
    // panic-safety property `EnvVarGuard` documents.
    let flows_dir_guard = EnvVarGuard::set("DARKMUX_FLOWS_DIR", flows_dir.path());
    let path_guard = PathPrependGuard::new(wrapper_dir.path());

    let session_id = format!("mock-model-proof-i2596-{}-{}", std::process::id(), port);
    let opts = DispatchOpts {
        brief_refs: Vec::new(),
        workspace_read_only: false,
        record_context: None,
        resume_from: None,
        host_out: None,
        max_turns_override: None,
        timeout_override_seconds: Some(timeout_override_seconds), // (#2596) the hop under test
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
        system_prompt_override: None,
    };

    let result = dispatch(opts);

    // Restore both guards as soon as the dispatch itself is done — same
    // point the shared harness above restores `DARKMUX_FLOWS_DIR`, before
    // mock teardown / the caller's own assertions.
    drop(path_guard);
    drop(flows_dir_guard);

    // Tear the mock-model process down regardless of dispatch outcome — a
    // failed assertion in the caller must not leak the child.
    let _ = mock.kill();
    let _ = mock.wait();

    let result = result.expect("dispatch() must return Ok — the container path ran to completion");
    eprintln!("--- dispatch stdout ---\n{}", result.stdout);
    eprintln!("--- dispatch stderr (tail) ---\n{}", tail(&result.stderr, 4000));
    // `wrapper_dir` is returned alongside `capture_file` (a bare path into
    // it) and must stay alive in the CALLER's scope until it has finished
    // reading `capture_file` — `TempDir::drop` recursively removes the
    // directory, and returning only the `PathBuf` here would drop
    // `wrapper_dir` (and delete the capture file) the instant this
    // function returns, before the caller ever gets to read it back.
    (result, flows_dir, session_id, capture_file, wrapper_dir)
}

/// Pins the hop #2480 disclosed and left unpinned: the container's
/// ACTUAL `DARKMUX_INACTIVITY_TIMEOUT_SECONDS` must be the `--timeout`
/// override, not the built-in default. Mutating
/// `effective_inactivity_timeout_seconds(opts.timeout_override_seconds)`'s
/// call site in `dispatch_internal::dispatch` to pass `None` instead
/// compiled clean and left all 1585 tests green before this test
/// existed (#2596) — this test fails on that exact mutation because it
/// reads the value the real container was actually started with, off
/// the real `docker run` argv, rather than trusting anything darkmux
/// itself reports about its own behavior.
#[test]
#[ignore = "requires Docker + a local darkmux-runtime:latest image"]
#[serial_test::serial]
fn dispatch_i2596_forwards_the_cli_timeout_override_into_the_container() {
    let bin = build_mock_model_binary();
    let (result, _flows_dir, _session_id, capture_file, _wrapper_dir) =
        run_mock_dispatch_with_timeout_override(
            &bin,
            "Say something short. #2596 proof: the container's ACTUAL inactivity \
             budget must be the --timeout override, not the built-in default.",
            45,
        );
    assert_eq!(result.exit_code, 0, "the runtime container must exit 0 on a clean stop");

    let (value, source) = captured_container_inactivity_budget(&capture_file);
    assert_eq!(
        value, "45",
        "the container's forwarded DARKMUX_INACTIVITY_TIMEOUT_SECONDS must be the \
         --timeout override (45), not the built-in default (600) — #2596: this is \
         `effective_inactivity_timeout_seconds(opts.timeout_override_seconds)`'s call \
         site in dispatch_internal::dispatch, mutable to `None` with the whole crate \
         suite staying green before this test existed"
    );
    assert_eq!(
        source, "cli",
        "the companion DARKMUX_INACTIVITY_TIMEOUT_SECONDS_SOURCE must name the cli \
         tier for an explicit --timeout override"
    );
}

/// The more valuable half (#2596): pins that `dispatch.start`'s `bounds`
/// record and the container's actual budget cannot independently drift.
/// `resolved_runtime_bounds_json` (feeding the record) and
/// `effective_inactivity_timeout_seconds` (feeding the container) are TWO
/// SEPARATE functions, each re-reading `opts.timeout_override_seconds` at
/// its own call site in `dispatch_internal::dispatch` and hand-rolling
/// the same CLI-wins-outright precedence — nothing stops them from
/// disagreeing. This test catches divergence regardless of WHICH side
/// broke: it fails equally whether the container's call site drops the
/// override (the same mutation the sibling test above catches) or the
/// record's own call site drops it instead (a mutation the sibling test
/// above cannot see, because it never looks at the record at all).
#[test]
#[ignore = "requires Docker + a local darkmux-runtime:latest image"]
#[serial_test::serial]
fn dispatch_i2596_start_record_and_container_budget_must_agree() {
    let bin = build_mock_model_binary();
    let (result, flows_dir, session_id, capture_file, _wrapper_dir) =
        run_mock_dispatch_with_timeout_override(
            &bin,
            "Say something short. #2596 proof: whatever dispatch.start's bounds record \
             claims the inactivity budget is, that must be the budget the container \
             actually ran on.",
            77,
        );
    assert_eq!(result.exit_code, 0, "the runtime container must exit 0 on a clean stop");

    let (container_value, container_source) = captured_container_inactivity_budget(&capture_file);
    let record_bounds = dispatch_start_inactivity_bounds(flows_dir.path(), &session_id);
    let record_value = record_bounds
        .get("value")
        .and_then(Value::as_u64)
        .expect("record bounds.inactivity_timeout_seconds.value is a number");
    let record_source = record_bounds
        .get("source")
        .and_then(Value::as_str)
        .expect("record bounds.inactivity_timeout_seconds.source is a string");

    // Anchor to the actual override (77) before comparing the two sides
    // to each other. Without this, the test below only proves AGREEMENT
    // — a future refactor that routes both call sites through one shared
    // broken helper (e.g. the collapse the #2480-era comment at the
    // `dispatch_internal.rs` call site invites) would leave both sides
    // at the built-in default (600) and this test would stay green,
    // since 600 == 600 agrees just as well as 77 == 77 does.
    assert_eq!(
        container_value, "77",
        "the container's forwarded DARKMUX_INACTIVITY_TIMEOUT_SECONDS must be the \
         --timeout override (77) actually passed to this dispatch, not any other \
         value both call sites might agree on"
    );

    assert_eq!(
        record_value.to_string(),
        container_value,
        "dispatch.start's bounds.inactivity_timeout_seconds.value ({record_value}) must \
         equal what the container was ACTUALLY given \
         (DARKMUX_INACTIVITY_TIMEOUT_SECONDS={container_value}) — #2596: these are \
         resolved on two SEPARATE call sites (`resolved_runtime_bounds_json` for the \
         record, `effective_inactivity_timeout_seconds` for the container) that can \
         independently drift; a mismatch here means the record is lying about what \
         governed this run"
    );
    assert_eq!(
        record_source, container_source,
        "the record's bounds.inactivity_timeout_seconds.source ({record_source}) must \
         agree with the container's DARKMUX_INACTIVITY_TIMEOUT_SECONDS_SOURCE \
         ({container_source})"
    );
}
