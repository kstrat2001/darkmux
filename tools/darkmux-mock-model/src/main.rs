//! darkmux-mock-model — a standalone, deterministic chat-completions mock
//! server.
//!
//! ## What this is for
//!
//! darkmux's real dispatch machinery (`crates/darkmux-crew`'s
//! `dispatch_internal::dispatch`, used by `darkmux dispatch` /
//! `darkmux mission run` / `darkmux phase review`, and eventually the
//! review pipeline) talks to "the model" over one narrow seam: an
//! OpenAI/LMStudio-compatible `POST /v1/chat/completions` call from inside
//! a real Docker container (see `runtime/src/lmstudio.rs`). This binary
//! stands in for that seam with a SCRIPTED, DETERMINISTIC fake response —
//! not a real model, not a smaller local model, not a simulation of model
//! behavior. It exists to prove the rest of the system's wiring (container
//! spawn, the agent loop, flow records, scheduler/mission-state
//! transitions) end-to-end, fast and reproducibly, on any machine —
//! including ones with zero local-AI capability.
//!
//! ## Why a separate process, not an in-process mock
//!
//! A container reaches this over the network the exact same way it reaches
//! real LMStudio on the host (`http://host.docker.internal:<port>/v1`) —
//! genuine TCP, genuine HTTP, genuine cross-process concurrency, all the
//! things an in-process closure standing in for a function call cannot
//! exercise. This binary is intentionally NOT a subcommand of the main
//! `darkmux` CLI and NOT a library folded into `darkmux-crew` — it is its
//! own crate with its own `[[bin]]`, in its own workspace-root
//! `Cargo.toml` (mirrors `runtime/` and `plugins/darkmux-bundler-rust`;
//! see the root `Cargo.toml`'s `exclude` list), started as a genuinely
//! separate OS process by whatever is proving the dispatch path (a test
//! harness today; potentially an operator-facing `--mock` flag on a future
//! darkmux CLI verb — deliberately NOT built here, see darkmux's CLAUDE.md
//! "Engagement never enters CLI arg surface" doctrine's sibling principle:
//! don't build the CLI verb before the underlying mechanism is proven).
//!
//! ## How it's built
//!
//! `httpmock` (the same crate + version `runtime/`'s own compaction-loop
//! integration tests already depend on, there used in-process) ships a
//! genuine "standalone mode": a real server process
//! (`httpmock::standalone::start_standalone_server`, the same entry point
//! httpmock's own CLI binary calls) that a REMOTE client
//! (`httpmock::MockServer::connect_async`) registers mocks against over the
//! server's own HTTP admin API, after it's already listening. This binary
//! is a thin wrapper around exactly that flow:
//!
//!   1. Start the real standalone server on `--port` (binds `127.0.0.1`,
//!      the same bind LMStudio itself defaults to — and the one
//!      `host.docker.internal` on Docker Desktop for Mac is verified to
//!      reach; see the mock-dispatch proof test for the empirical check).
//!   2. Connect to it as a client (same crate, `remote` mode) and register
//!      one mock per scripted turn.
//!   3. Print a `MOCK-MODEL-READY` line once every turn is registered — the
//!      proof harness's signal that this process is actually up and
//!      listening (not a fixed sleep-and-hope).
//!   4. Block forever (killed by the harness when the dispatch under test
//!      is done).
//!
//! No YAML: httpmock's own static-mock-dir loader reads YAML files, but
//! darkmux's CLAUDE.md is explicit — "Manifests are JSON... The repo
//! briefly used YAML; that switch is done. Don't reintroduce YAML." — so
//! this binary reads its own JSON script format instead (see
//! [`ScriptedTurn`]) and registers each turn itself via the mock builder
//! API, rather than handing httpmock a YAML directory.
//!
//! ## Script format
//!
//! `--script <path>.json` — a JSON array of scripted turns:
//!
//! ```json
//! [
//!   { "trigger": "some substring", "response": { ...chat-completions response body... } },
//!   { "response": { ...fallback, matches any request... } }
//! ]
//! ```
//!
//! `trigger` (optional): when present, this turn's response is only
//! returned for a request whose JSON body contains the given substring
//! (httpmock's `body_contains`) — the "map a request-content substring
//! match to a canned response" scripting mode. Omit it (or pass
//! `--script`'s default, see below) to match every request unconditionally
//! — the "always return one fixed response" mode.
//!
//! `response`: the FULL, NON-streaming chat-completions response body — the
//! exact JSON shape `runtime/src/lmstudio.rs::ChatResponse` deserializes
//! (`choices[0].message.{content,tool_calls}`, `choices[0].finish_reason`,
//! `usage`). This binary does not know or care what a "tool call" is; the
//! script author (a test, or an operator hand-authoring a scenario) builds
//! the exact wire shape they want the runtime to see. On the wire, this
//! binary always answers as an SSE STREAM (`to_sse_body`) regardless of the
//! script's non-streaming shape — `runtime`'s agent loop streams by default
//! (`--no-stream` is the opt-out darkmux dispatch never passes), so a plain
//! `application/json` response is invalid framing the client's `ChunkStream`
//! parser silently ingests as zero chunks (see `to_sse_body`'s doc comment
//! for the empirical failure this was found from). The script format stays
//! the simpler non-streaming shape; only the wire encoding is streaming.
//!
//! With no `--script`, one fixed turn is registered: a plain-text "task
//! complete, no tool calls" `stop` response for every request — the
//! simplest possible proof that a dispatch round-trips end-to-end.
//!
//! Each turn is matched by its own predicate (`body_contains(trigger)`, or
//! unconditional when `trigger` is absent) — a no-trigger turn does NOT
//! preempt a trigger-scoped one just by being registered first, verified
//! empirically with a two-turn (tool-call → tool-role-triggered stop)
//! script. Keep triggers distinguishing (a substring genuinely unique to
//! the turn it should select) rather than relying on registration order.

use anyhow::{bail, Context, Result};
use clap::Parser;
use httpmock::standalone::start_standalone_server;
use httpmock::{Method, MockServer};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    name = "darkmux-mock-model",
    about = "Standalone deterministic chat-completions mock server for the darkmux mock-model dispatch harness (v1)."
)]
struct Args {
    /// TCP port to listen on. The caller (a test harness, an operator) picks
    /// this explicitly — httpmock's standalone entry point doesn't report
    /// back an OS-assigned ephemeral port, so `0` is not a valid `--port`
    /// here. A harness that wants an ephemeral port binds+drops a listener
    /// itself first to find a free one, then passes it here.
    #[arg(long, default_value_t = 5000)]
    port: u16,

    /// Path to a JSON script file (see the module doc for the shape).
    /// Absent ⇒ one fixed "task complete, no tool calls" `stop` response
    /// for every request.
    #[arg(long)]
    script: Option<PathBuf>,

    /// Print each request as it comes in (path + method) — off by default
    /// to keep a scripted-multi-turn dispatch's stderr readable; turn on
    /// when debugging a script that isn't matching.
    #[arg(long, default_value_t = false)]
    verbose: bool,
}

/// One scripted turn read from `--script <file>.json`. See the module doc
/// for the full shape + semantics.
#[derive(Debug, Deserialize)]
struct ScriptedTurn {
    #[serde(default)]
    trigger: Option<String>,
    response: Value,
}

/// The default (no `--script`) turn: unconditionally returns one fixed
/// `stop`-finished, tool-call-free assistant message. Wire shape matches
/// `runtime/src/lmstudio.rs::ChatResponse` — the same helper shape
/// `runtime/`'s own httpmock-based tests build by hand (see
/// `loop_runner.rs`'s `chat_response_json` test helper).
fn default_turns() -> Vec<ScriptedTurn> {
    vec![ScriptedTurn {
        trigger: None,
        response: json!({
            "id": "darkmux-mock-model-fixed",
            "object": "chat.completion",
            "created": 0,
            "model": "darkmux-mock-model",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "mock dispatch complete — no tool calls.",
                },
                "finish_reason": "stop",
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
        }),
    }]
}

fn load_turns(script: Option<&PathBuf>) -> Result<Vec<ScriptedTurn>> {
    let Some(path) = script else {
        return Ok(default_turns());
    };
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading mock-model script {}", path.display()))?;
    let turns: Vec<ScriptedTurn> = serde_json::from_str(&raw).with_context(|| {
        format!(
            "parsing {} as a JSON array of scripted turns (see darkmux-mock-model's module doc for the shape)",
            path.display()
        )
    })?;
    if turns.is_empty() {
        bail!(
            "{} parsed to an empty array — a mock server with zero scripted turns \
             answers every request with httpmock's default 404, which would make \
             every dispatch turn fail at the transport layer, not exercise \
             anything. Add at least one turn.",
            path.display()
        );
    }
    Ok(turns)
}

/// Convert a script turn's `response` — a full, NON-streaming
/// chat-completions body (`choices[0].message.{content,tool_calls}`,
/// `choices[0].finish_reason`) — into an SSE-framed streaming response body.
///
/// `runtime`'s agent loop streams by default (`runtime/src/main.rs`'s
/// `streaming` flag defaults to `true`; `--no-stream` is the opt-out darkmux
/// dispatch never passes), so it calls `LmStudioClient::chat_streaming`, not
/// `chat` — a plain `200 application/json` body (this module's first
/// implementation) is NOT valid SSE framing: `ChunkStream`'s parser only
/// recognizes `data: <json>\n\n` lines, sees none, and terminates having
/// ingested zero chunks. `ChunkAccumulator::into_response()` then defaults
/// silently to an EMPTY `stop` response (documented in
/// `runtime/src/lmstudio.rs` as the "malformed/incomplete stream" case) —
/// which is exactly what the mock-dispatch proof test observed empirically
/// before this fix: `result: "stop"`, `final_assistant: "<empty>"`, and a
/// suspiciously-fast `wall_ms` (the container never actually parsed a
/// response, it just hit the stream's default fallback instantly).
///
/// The fix: always answer in SSE — one `data:` chunk carrying the FULL
/// message content/tool_calls as a single streaming delta (no fragmentation
/// needed; `ChunkAccumulator` ingests a complete-in-one-chunk delta exactly
/// like a fragmented one), then `data: [DONE]`. Tool-call deltas are passed
/// through WITHOUT adding a synthetic `index` field — `ChunkAccumulator`
/// already falls back to routing by `id` when `index` is absent (the
/// Google-compat path added for #1197), so an `id`-bearing, `index`-less
/// tool_calls array round-trips correctly.
fn to_sse_body(response: &Value) -> String {
    let message = response
        .pointer("/choices/0/message")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let finish_reason = response
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop");
    let id = response.get("id").and_then(Value::as_str).unwrap_or("darkmux-mock-model");
    let model = response
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("darkmux-mock-model");

    let mut delta = serde_json::Map::new();
    delta.insert("role".to_string(), json!("assistant"));
    if let Some(content) = message.get("content") {
        if !content.is_null() {
            delta.insert("content".to_string(), content.clone());
        }
    }
    if let Some(tool_calls) = message.get("tool_calls") {
        if !tool_calls.is_null() {
            delta.insert("tool_calls".to_string(), tool_calls.clone());
        }
    }

    let mut chunk = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": 0,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": Value::Object(delta),
            "finish_reason": finish_reason,
        }],
    });
    if let Some(usage) = response.get("usage") {
        chunk["usage"] = usage.clone();
    }

    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}

/// Register one scripted turn against the (already-listening) standalone
/// server. `client` is the remote-mode `MockServer` handle from
/// `MockServer::connect_async` — registration happens over its own real
/// HTTP admin API, exactly as a cross-machine test harness would do it.
async fn register_turn(client: &MockServer, turn: &ScriptedTurn) {
    let body = to_sse_body(&turn.response);
    match &turn.trigger {
        Some(trigger) => {
            let trigger = trigger.clone();
            client
                .mock_async(move |when, then| {
                    when.method(Method::POST)
                        .path("/v1/chat/completions")
                        .body_contains(trigger);
                    then.status(200)
                        .header("content-type", "text/event-stream")
                        .body(body);
                })
                .await;
        }
        None => {
            client
                .mock_async(move |when, then| {
                    when.method(Method::POST).path("/v1/chat/completions");
                    then.status(200)
                        .header("content-type", "text/event-stream")
                        .body(body);
                })
                .await;
        }
    }
}

/// Connect to the just-started standalone server, retrying briefly — the
/// server task above needs a moment to bind its listener before the admin
/// API answers pings. Bounded (2s / 40 attempts) so a genuinely broken
/// server fails loud instead of hanging.
async fn connect_with_retry(addr: &str) -> Result<MockServer> {
    let mut last_err: Option<String> = None;
    for _ in 0..40 {
        // `connect_async` panics internally on failure (its `with_retry`
        // unwraps) rather than returning `Result` — so probe with a plain
        // TCP connect first, which is a clean `Result` we can retry on.
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return Ok(MockServer::connect_async(addr).await);
        }
        last_err = Some(format!("no listener yet at {addr}"));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!(
        "darkmux-mock-model: server never came up on {addr} within 2s ({})",
        last_err.unwrap_or_default()
    )
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let turns = load_turns(args.script.as_ref())?;
    let addr = format!("127.0.0.1:{}", args.port);

    // 1. Start the REAL standalone httpmock server as a background task on
    //    this process's own tokio runtime — genuinely binds a TCP socket
    //    and serves over real HTTP (warp/hyper under the hood), the same
    //    entry point httpmock's own CLI binary calls. `expose = false`:
    //    127.0.0.1 only, matching LMStudio's own default bind — verified
    //    (see the mock-dispatch proof test) that Docker Desktop for Mac's
    //    `host.docker.internal` reaches a host service bound this way, the
    //    same reachability real LMStudio dispatches already depend on.
    let port = args.port;
    let verbose = args.verbose;
    let server_task = tokio::spawn(async move {
        start_standalone_server(
            port,
            false, // expose
            None,  // no YAML static-mock-dir — see module doc on why
            verbose,
            1000,
            std::future::pending::<()>(),
        )
        .await
    });

    // 2. Connect as a client and register every scripted turn over the
    //    server's real HTTP admin API.
    let client = connect_with_retry(&addr).await?;
    for turn in &turns {
        register_turn(&client, turn).await;
    }

    // 3. Signal readiness. The proof harness watches stdout for this exact
    //    prefix rather than sleeping-and-hoping.
    println!("MOCK-MODEL-READY port={port} turns={}", turns.len());
    eprintln!(
        "darkmux-mock-model: listening on http://{addr} — chat-completions endpoint: http://{addr}/v1/chat/completions ({} scripted turn(s))",
        turns.len()
    );

    // 4. Run until killed (the harness tears this process down once the
    //    dispatch under test has completed).
    match server_task.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => bail!("darkmux-mock-model: server task failed: {e}"),
        Err(e) => bail!("darkmux-mock-model: server task panicked: {e}"),
    }
}
