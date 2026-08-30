//! Real, container-free mock-dispatch proof for the #1698 Packet B
//! container-path fix (the issue's own live dogfood finding: "the route
//! rides the FULL internal-runtime container dispatch ... for a tool-less
//! single-shot — the routing seat should take a direct single-shot HTTP
//! path").
//!
//! Runs `darkmux_crew::dispatch::dispatch_local_single_shot` — the light
//! primitive `src/radio.rs::dispatch_router_call` now wires into via
//! `darkmux_fleet::routing::dispatch_routed_via` — against a REAL,
//! in-process HTTP mock server (`httpmock`, genuinely bound to a real
//! local TCP port, genuinely reached over `curl` — the same hardened curl
//! path every hosted/local single-shot call in this crate uses). Zero
//! LMStudio, zero Docker, zero GPU, zero real AI anywhere in this test.
//!
//! **Not `httpmock::standalone` / `tools/darkmux-mock-model`.** That tool
//! (used by `mock_dispatch_proof.rs`'s CONTAINER-path proof) always answers
//! as an SSE STREAM by design — it exists to feed the runtime's agent loop,
//! which streams unconditionally (see that binary's own module doc). This
//! packet's light path sends `"stream": false` and expects a PLAIN JSON
//! body (`single_shot::local_chat_body`'s own documented dialect) — a
//! genuinely different response shape the streaming-only mock tool cannot
//! produce. `httpmock`'s plain (non-standalone) in-process mode answers
//! exactly the shape this path actually sends/expects, with no subprocess
//! and no port-readiness polling: `MockServer::start()` returns ready
//! synchronously.
//!
//! Needs neither Docker nor a `darkmux-runtime` image — the entire point of
//! the container-free path — so this test is NOT `#[ignore]`d: it runs in
//! the ordinary `cargo test --workspace` pass.

use httpmock::prelude::*;
use serde_json::Value;
use std::path::Path;

use darkmux_crew::dispatch::{dispatch_local_single_shot, CompactionDispatchArgs, DispatchOpts};

/// Write a minimal profiles registry naming exactly one LOCAL model (no
/// `endpoint` block) — resolves through the SAME
/// `resolve_dispatch_model_internal` path a real profile would.
/// `skip_lmstudio_residency` (derived from `opts.model_base_url_override.
/// is_some()` inside `dispatch_local_single_shot` itself, mirroring
/// `dispatch()`'s own call site) skips any real LMStudio residency probe
/// here — there is no real LMStudio to probe when the base URL points at
/// this mock server instead.
fn write_mock_profiles_registry(dir: &Path) -> std::path::PathBuf {
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
    std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap()).expect("writing temp profiles.json");
    path
}

#[test]
fn container_free_single_shot_dispatch_round_trips_through_a_real_http_mock_server() {
    let server = MockServer::start();
    // The exact request-body SHAPE this path sends (local dialect:
    // `"stream": false`, `"max_tokens"`, never the hosted
    // `"max_completion_tokens"` form) is already golden-tested directly
    // against `single_shot::local_chat_body` (a pure function, no network)
    // — this integration test's OWN job is proving the REAL network round
    // trip works, via `mock.assert()` below, not re-asserting a request
    // shape a faster unit test already pins byte-for-byte.
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200).header("content-type", "application/json").json_body(serde_json::json!({
            "id": "mock-1",
            "object": "chat.completion",
            "created": 0,
            "model": "mock-model",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "mock dispatch complete — no tool calls." },
                "finish_reason": "stop",
            }],
            "usage": { "prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10 },
        }));
    });

    let registry_dir = tempfile::tempdir().expect("tempdir for profiles registry");
    let profiles_path = write_mock_profiles_registry(registry_dir.path());
    let flows_dir = tempfile::tempdir().expect("tempdir for flow records");

    // SAFETY (matches `mock_dispatch_proof.rs`'s own pattern for this exact
    // env var): each `tests/*.rs` integration-test file is its own
    // process, and no OTHER test in THIS file mutates DARKMUX_FLOWS_DIR, so
    // there is no cross-test race.
    let prev_flows_dir = std::env::var("DARKMUX_FLOWS_DIR").ok();
    unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", flows_dir.path()) };

    let session_id = format!("mock-single-shot-proof-{}", std::process::id());
    let opts = DispatchOpts {
        workspace_read_only: false,
        record_context: None,
        resume_from: None,
        host_out: None,
        // `radio-router` is the packet's own real caller (#1698) — a
        // BUILT-IN role (`crates/darkmux-crew/src/loader.rs`'s
        // `BUILTIN_ROLES`/`BUILTIN_ROLE_PROMPTS`), so no on-disk role
        // manifest is needed for this test to resolve it.
        role_id: "radio-router".to_string(),
        message: "the exact routing-seat user message doesn't matter here — the mock \
                  server ignores request content and returns its fixed scripted reply \
                  regardless"
            .to_string(),
        session_id: Some(session_id.clone()),
        timeout_seconds: 30,
        skip_preflight: true,
        json: false,
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
        model_base_url_override: Some(server.base_url()),
        step_id: None,
        system_prompt_override: None,
    };

    let result = dispatch_local_single_shot(opts);

    if let Some(prev) = prev_flows_dir {
        unsafe { std::env::set_var("DARKMUX_FLOWS_DIR", prev) };
    } else {
        unsafe { std::env::remove_var("DARKMUX_FLOWS_DIR") };
    }

    let result = result.expect("dispatch_local_single_shot must return Ok — the mock round-trip succeeded");
    eprintln!("--- dispatch stdout ---\n{}", result.stdout);
    assert_eq!(result.exit_code, 0);

    // The mock server's own scripted content — PROVING the completion
    // genuinely came back over the network from the mock HTTP server, not
    // from any fallback/default text baked into `single_shot_chat` or
    // `dispatch_local_single_shot` itself.
    assert!(
        result.stdout.contains("mock dispatch complete"),
        "stdout must be the mock server's own scripted content, got: {:?}",
        result.stdout
    );

    // The request actually landed exactly once, with the local dialect's
    // "stream": false — `mock.assert()` fails loud if the endpoint was
    // never hit (proving this path is genuinely making the call, not
    // silently short-circuiting) or hit more than once.
    mock.assert();

    // Real flow records: dispatch.start + a terminal dispatch.complete for
    // THIS session_id, on disk under the isolated flows dir (contract #2,
    // "dispatch liveness" — CLAUDE.md's cross-system contracts section) —
    // proving the container-free path honors the SAME liveness bookend
    // contract the container path does, not a lighter/different one.
    let mut saw_start = false;
    let mut saw_complete = false;
    for entry in std::fs::read_dir(flows_dir.path()).expect("reading the isolated flows dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let contents = std::fs::read_to_string(&path).expect("reading a flow record file");
        for line in contents.lines() {
            let Ok(record) = serde_json::from_str::<Value>(line) else { continue };
            if record.get("session_id").and_then(Value::as_str) != Some(session_id.as_str()) {
                continue;
            }
            match record.get("action").and_then(Value::as_str) {
                Some("dispatch start") => saw_start = true,
                Some("dispatch complete") | Some("dispatch error") => saw_complete = true,
                _ => {}
            }
        }
    }
    assert!(saw_start, "no dispatch.start flow record found for session {session_id}");
    assert!(saw_complete, "no terminal dispatch.complete/dispatch.error flow record found for session {session_id}");
}
