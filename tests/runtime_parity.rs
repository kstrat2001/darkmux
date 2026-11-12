//! (#3112) `darkmux runtime …` is the agent runtime compiled into the
//! darkmux binary, which a Seatbelt sandbox runs with no separate build. It
//! must behave exactly like the standalone `darkmux-runtime` the Docker image
//! ships: the same exit code, the same stdout and stderr, the same bytes on
//! the wire to the model, and the same files in the out-dir and workspace.
//!
//! Each case here runs BOTH programs on one fixture, offline, against a mock
//! model endpoint of its own, and compares everything an observer outside the
//! process can see. Bodies are compared as the bytes the endpoint received,
//! never as parsed JSON: key order is part of what a model is sent.
//!
//! Inside darkmux the runtime is linked into a larger dependency graph, and
//! cargo turns a dependency's feature on for every crate in one build as soon
//! as any crate asks for it (`serde_json`'s `preserve_order`, which
//! `agent-client-protocol` asks for, is the one that changes bytes). So a
//! case that passes here is the proof that no such feature reaches what the
//! runtime emits.
//!
//! The standalone program is built from `runtime/` with its own lockfile
//! (`cargo build --locked --manifest-path runtime/Cargo.toml`), exactly as
//! the Docker image builds it, minus musl.
//!
//! What differs between two runs and is masked before comparing: the
//! tempdirs each run is given, the mock's port, and the clock (timestamps
//! and durations, by key; `mask`). Nothing else.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{json, Value};

// ─── the two programs ──────────────────────────────────────────────────

/// The standalone runtime, built once per test process with its own
/// lockfile. A test lane's `CARGO_TARGET_DIR` is honored (the build is a
/// separate workspace, so it shares nothing with the outer build).
fn standalone() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let manifest = root.join("runtime/Cargo.toml");
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let out = Command::new(cargo)
            .args(["build", "--quiet", "--locked", "--bin", "darkmux-runtime", "--message-format", "json", "--manifest-path"])
            .arg(&manifest)
            .stderr(Stdio::inherit())
            .output()
            .expect("running cargo to build the standalone runtime");
        assert!(out.status.success(), "building the standalone runtime failed: {:?}", out.status);
        // The executable cargo reports, wherever the target dir is.
        let exe = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == "darkmux-runtime")
            .filter_map(|m| m["executable"].as_str().map(PathBuf::from))
            .next_back()
            .expect("cargo named the darkmux-runtime executable");
        assert!(exe.is_file(), "{}", exe.display());
        exe
    })
}

/// The program and its leading arguments: standalone, or darkmux's hidden
/// `runtime` subcommand.
#[derive(Clone, Copy, Debug)]
enum Program {
    Standalone,
    Darkmux,
}

// ===== DARKMUX-SPAWN-HELPERS: BEGIN (#3112) ==========================
//
// Every darkmux this file starts or names. Each program gets the same
// environment: the state variables neutralized, every other `DARKMUX_*` of
// the runner's removed, `HOME` pinned by the caller. A sandbox's program is
// named here and started by the backend with only its policy's environment.

impl Program {
    fn command(self) -> Command {
        let mut cmd = match self {
            Program::Standalone => Command::new(standalone()),
            Program::Darkmux => {
                let mut c = Command::new(darkmux_exe());
                c.arg("runtime");
                c
            }
        };
        darkmux_types::test_isolation::neutralize_state_vars(&mut cmd);
        for (k, _) in std::env::vars() {
            if k.starts_with("DARKMUX_") {
                cmd.env_remove(k);
            }
        }
        cmd
    }
}

/// This build's darkmux, by its real path.
fn darkmux_exe() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_darkmux")).canonicalize().unwrap()
}

/// A darkmux `cargo install` put in `~/.cargo/bin`, when there is one.
fn installed_darkmux() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo/bin/darkmux")).filter(|p| p.is_file())
}

/// `darkmux --help`, isolated like every other spawn here.
fn darkmux_help() -> String {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(darkmux_exe());
    darkmux_types::test_isolation::neutralize_state_vars(&mut cmd);
    let out = cmd.env("HOME", home.path()).env("DARKMUX_HOME", home.path()).arg("--help").output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// ===== DARKMUX-SPAWN-HELPERS: END (#3112) ============================

// ─── the mock model endpoint ───────────────────────────────────────────

/// One request, as received.
#[derive(Debug, Clone, PartialEq)]
struct Received {
    request_line: String,
    /// In the order sent, names lowercased.
    headers: Vec<(String, String)>,
    body: String,
}

/// What the mock answers with.
#[derive(Clone)]
struct Reply {
    status: u16,
    content_type: &'static str,
    body: String,
    /// Hold the reply this long first (a call in flight for a signal).
    delay_ms: u64,
}

fn json_reply(v: Value) -> Reply {
    Reply { status: 200, content_type: "application/json", body: v.to_string(), delay_ms: 0 }
}

struct Mock {
    port: u16,
    seen: Arc<Mutex<Vec<Received>>>,
}

impl Mock {
    /// `script(n, body)` answers the `n`th request (0-based).
    fn start(script: fn(usize, &str) -> Reply) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<Received>>> = Arc::default();
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                    continue;
                }
                let mut headers = Vec::new();
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        let k = k.trim().to_ascii_lowercase();
                        if k == "content-length" {
                            len = v.trim().parse().unwrap_or(0);
                        }
                        headers.push((k, v.trim().to_string()));
                    }
                }
                let mut body = vec![0u8; len];
                if reader.read_exact(&mut body).is_err() {
                    continue;
                }
                let body = String::from_utf8_lossy(&body).into_owned();
                let n = {
                    let mut l = log.lock().unwrap();
                    l.push(Received { request_line: request_line.trim_end().to_string(), headers, body: body.clone() });
                    l.len() - 1
                };
                let reply = script(n, &body);
                std::thread::sleep(std::time::Duration::from_millis(reply.delay_ms));
                let head = format!(
                    "HTTP/1.1 {} X\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    reply.status,
                    reply.content_type,
                    reply.body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(reply.body.as_bytes());
                let _ = stream.flush();
            }
        });
        Mock { port, seen }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    fn requests(&self) -> Vec<Received> {
        self.seen.lock().unwrap().clone()
    }
}

// ─── replies ───────────────────────────────────────────────────────────

fn chat(message: Value, finish: &str, prompt_tokens: u64) -> Reply {
    json_reply(json!({
        "id": "r", "object": "chat.completion", "created": 0, "model": "brain-model",
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
        "usage": { "prompt_tokens": prompt_tokens, "completion_tokens": 7, "total_tokens": prompt_tokens + 7 },
    }))
}

fn answer(text: &str) -> Reply {
    chat(json!({ "role": "assistant", "content": text }), "stop", 50)
}

fn calls(calls: Value) -> Reply {
    chat(json!({ "role": "assistant", "content": "working", "tool_calls": calls }), "tool_calls", 50)
}

fn call(id: &str, name: &str, args: Value) -> Value {
    json!({ "id": id, "type": "function", "function": { "name": name, "arguments": args.to_string() } })
}

fn with_extra_content(mut call: Value) -> Value {
    call["extra_content"] = json!({ "google": { "thought_signature": "SIG", "b": 1, "a": 2 }, "z": true });
    call
}

/// Every tool, a malformed edit (whose refusal teaches the schema back to the
/// model), an accepted finding and mod (records with JSON values in them), an
/// empty turn (a recovery record naming its bound), then an answer. Arguments
/// arrive in an order that is not sorted, as a model writes them.
fn tool_turns(n: usize, _: &str) -> Reply {
    match n {
        0 => calls(json!([
            // A provider's opaque per-call blob, echoed back on every later
            // request and kept in the checkpoint.
            with_extra_content(call("c1", "search", json!({ "pattern": "hello", "path": "." }))),
            call("c2", "read", json!({ "limit": 5, "path": "x.txt", "offset": 1 })),
        ])),
        1 => calls(json!([
            call("c3", "edit", json!({ "path": "x.txt", "old_string": "hello", "new_string": "bye" })),
            call("c4", "write", json!({ "content": "made\n", "path": "new.txt" })),
        ])),
        2 => calls(json!([
            call("c5", "edit", json!({ "edits": [{ "new_string": "bye", "old_string": "hello" }], "path": "x.txt" })),
            call("c6", "bash", json!({ "command": "cat new.txt x.txt" })),
            call("c7", "create_finding", json!({ "why": "w", "line": 2, "file": "x.txt", "pattern": "world", "evidence": "world" })),
            call("c8", "create_mod", json!({ "kit": "replace world", "for": ["exec-1/1"] })),
        ])),
        3 => chat(json!({ "role": "assistant", "content": null, "tool_calls": [] }), "tool_calls", 50),
        _ => answer("all done"),
    }
}

/// The same turns, streamed.
fn streamed(n: usize, body: &str) -> Reply {
    let whole: Value = serde_json::from_str(&tool_turns(n, body).body).unwrap();
    let message = &whole["choices"][0]["message"];
    let mut delta = message.clone();
    if let Some(calls) = delta.get_mut("tool_calls").and_then(Value::as_array_mut) {
        for (i, c) in calls.iter_mut().enumerate() {
            c["index"] = json!(i);
        }
    }
    let chunk = |delta: Value, finish: Value, usage: Value| {
        format!(
            "data: {}\n\n",
            json!({ "id": "s", "object": "chat.completion.chunk", "created": 0, "model": "brain-model",
                "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }], "usage": usage })
        )
    };
    let body = chunk(delta, Value::Null, Value::Null)
        + &chunk(json!({}), whole["choices"][0]["finish_reason"].clone(), whole["usage"].clone())
        + "data: [DONE]\n\n";
    Reply { status: 200, content_type: "text/event-stream", body, delay_ms: 0 }
}

/// The Messages dialect: a tool call, then an answer.
fn messages_turns(n: usize, _: &str) -> Reply {
    let content = if n == 0 {
        json!([{ "type": "text", "text": "looking" },
               { "type": "tool_use", "id": "tu1", "name": "read", "input": { "path": "x.txt", "offset": 1, "limit": 3 } }])
    } else {
        json!([{ "type": "text", "text": "messages answer" }])
    };
    json_reply(json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "brain-model", "content": content,
        "stop_reason": if n == 0 { "tool_use" } else { "end_turn" },
        "usage": { "input_tokens": 50, "output_tokens": 3 },
    }))
}

/// The brain reports a prompt over the threshold; the compactor (`util-model`)
/// answers with a summary (structured-slot JSON when it was asked for it).
fn compaction_turns(_: usize, body: &str) -> Reply {
    if body.contains("\"util-model\"") {
        let summary = if body.contains("response_format") {
            json!({ "goal": "g", "decisions": [], "files": ["x.txt"], "next_step": "keep reading" }).to_string()
        } else {
            "Summary: the assistant read the file several times. Nothing was modified. Next: act on it.".to_string()
        };
        return chat(json!({ "role": "assistant", "content": summary }), "stop", 40);
    }
    chat(
        json!({ "role": "assistant", "content": null,
                "tool_calls": [call("c1", "read", json!({ "path": "x.txt", "offset": 1, "limit": 0 }))] }),
        "tool_calls",
        5000,
    )
}

/// A Messages reply with no content array: an error that prints the reply.
fn messages_broken(_: usize, _: &str) -> Reply {
    json_reply(json!({ "type": "message", "zeta": 1, "role": "assistant", "alpha": { "y": 1, "x": 2 } }))
}

/// A Messages stream that ends in an error event: an error that prints it.
fn messages_stream_error(_: usize, _: &str) -> Reply {
    let event = json!({ "type": "error", "zeta": 1, "error": { "type": "overloaded_error", "message": "busy" } });
    Reply { status: 200, content_type: "text/event-stream", body: format!("event: error\ndata: {event}\n\n"), delay_ms: 0 }
}

/// A chat-completions stream whose payload is the engine's error with no
/// string `message`: an error that prints the error object.
fn stream_error_object(_: usize, _: &str) -> Reply {
    let event = json!({ "error": { "type": "server_error", "code": "overloaded" } });
    Reply { status: 200, content_type: "text/event-stream", body: format!("data: {event}\n\n"), delay_ms: 0 }
}

/// The structured-slot compactor answering with slots of the wrong type,
/// unsorted: the parse error names the first bad field it meets.
fn compaction_bad_slots(n: usize, body: &str) -> Reply {
    if body.contains("\"util-model\"") {
        let slots = json!({ "verify_criteria": 5, "objective": 7 }).to_string();
        return chat(json!({ "role": "assistant", "content": slots }), "stop", 40);
    }
    compaction_turns(n, body)
}

fn server_error(_: usize, _: &str) -> Reply {
    Reply { status: 500, content_type: "application/json", body: r#"{"error":"boom"}"#.into(), delay_ms: 0 }
}

fn hang(_: usize, _: &str) -> Reply {
    Reply { delay_ms: 30_000, ..answer("late") }
}

// ─── one run, observed ─────────────────────────────────────────────────

/// A fixture both programs run.
struct Case {
    /// The runtime's arguments; `{base}` is this run's mock base URL.
    args: Vec<String>,
    script: fn(usize, &str) -> Reply,
    stdin: Option<&'static str>,
    /// SIGTERM once the first model call is in flight.
    sigterm: bool,
}

fn case(args: &[&str], script: fn(usize, &str) -> Reply) -> Case {
    Case { args: args.iter().map(|s| s.to_string()).collect(), script, stdin: None, sigterm: false }
}

/// The minimal dispatch, plus `extra`.
fn dispatch(extra: &[&str]) -> Vec<String> {
    let base = [
        "run", "--model", "brain-model", "--system", "You are a test. Work in /workspace.", "--prompt", "do the task",
        "--context-window", "32000", "--execution-id", "exec-1", "--base-url", "{base}",
    ];
    base.iter().chain(extra).map(|s| s.to_string()).collect()
}

/// Everything one run showed, masked.
#[derive(Debug, PartialEq)]
struct Observed {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    requests: Vec<Received>,
    /// Every file left in the out-dir and the workspace, by relative path.
    files: Vec<(String, String)>,
}

fn run(program: Program, c: &Case) -> Observed {
    let out = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("x.txt"), "hello\nworld\n").unwrap();
    let mock = Mock::start(c.script);
    let args: Vec<String> = c.args.iter().map(|a| a.replace("{base}", &mock.base_url())).collect();
    let mut cmd = program.command();
    cmd.args(&args)
        .env("DARKMUX_RUNTIME_OUT_DIR", out.path())
        .env("DARKMUX_RUNTIME_WORKSPACE", ws.path())
        .env("DARKMUX_TURN_DELAY_MS", "0")
        .env("HOME", ws.path())
        .current_dir(ws.path())
        .stdin(if c.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawning the runtime");
    if let Some(blob) = c.stdin {
        child.stdin.take().unwrap().write_all(blob.as_bytes()).unwrap();
    }
    if c.sigterm {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while mock.requests().is_empty() {
            assert!(std::time::Instant::now() < deadline, "{program:?}: the model call never started");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // SAFETY: kill(2) on our own child's pid.
        assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) }, 0);
    }
    let output = child.wait_with_output().unwrap();
    let dirs = [(out.path(), "<OUT>"), (ws.path(), "<WS>")];
    let m = |s: &str| mask(s, &dirs, mock.port);
    let mut files = Vec::new();
    for (dir, tag) in dirs {
        collect_files(dir, dir, tag, &mut files);
    }
    Observed {
        code: output.status.code(),
        stdout: m(&String::from_utf8_lossy(&output.stdout)),
        stderr: m(&String::from_utf8_lossy(&output.stderr)),
        requests: mock
            .requests()
            .into_iter()
            .map(|r| Received {
                request_line: m(&r.request_line),
                headers: r.headers.iter().map(|(k, v)| (k.clone(), m(v))).collect(),
                body: m(&r.body),
            })
            .collect(),
        files: files.into_iter().map(|(p, body)| (p, m(&body))).collect(),
    }
}

fn collect_files(root: &Path, dir: &Path, tag: &str, into: &mut Vec<(String, String)>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_files(root, &path, tag, into);
        } else {
            let rel = format!("{tag}/{}", path.strip_prefix(root).unwrap().display());
            into.push((rel, String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned()));
        }
    }
}

/// Keys whose numbers are the clock (a timestamp or a duration).
const CLOCK_KEYS: &[&str] = &["ts", "wall_ms", "written_at_unix_ms"];

/// `text` with each run's own dirs, the mock's port and the clock masked.
fn mask(text: &str, dirs: &[(&Path, &str)], port: u16) -> String {
    let mut s = text.to_string();
    for (dir, tag) in dirs {
        // The canonical form first (macOS's `/var` is `/private/var`).
        if let Ok(real) = dir.canonicalize() {
            s = s.replace(&real.display().to_string(), tag);
        }
        s = s.replace(&dir.display().to_string(), tag);
    }
    s = s.replace(&format!("127.0.0.1:{port}"), "127.0.0.1:<PORT>");
    for key in CLOCK_KEYS {
        s = mask_numbers_after(&s, &format!("\"{key}\":"));
        s = mask_numbers_after(&s, &format!("\"{key}\": "));
    }
    // The human summary's wall-clock line.
    s.lines()
        .map(|l| if l.starts_with("wall:") { "wall: <MASKED>".to_string() } else { l.to_string() })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every run of digits right after `prefix`, as `0`.
fn mask_numbers_after(s: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find(prefix) {
        out.push_str(&rest[..at + prefix.len()]);
        rest = &rest[at + prefix.len()..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 {
            out.push('0');
            rest = &rest[digits..];
        }
    }
    out.push_str(rest);
    out
}

/// `a == b`, or a panic naming `what` and showing both around the first byte
/// that differs.
fn same(what: &str, a: &str, b: &str) {
    if a == b {
        return;
    }
    let at = a.bytes().zip(b.bytes()).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
    let window = |s: &str| {
        let mut start = at.saturating_sub(120).min(s.len());
        while !s.is_char_boundary(start) {
            start -= 1;
        }
        let mut end = (at + 240).min(s.len());
        while !s.is_char_boundary(end) {
            end += 1;
        }
        s[start..end].to_string()
    };
    panic!(
        "{what} differs at byte {at} (lengths {} and {}):\n  standalone: {:?}\n  darkmux:    {:?}",
        a.len(),
        b.len(),
        window(a),
        window(b)
    );
}

/// Run `c` under both programs and require the same observation, field by
/// field, so a failure names what differed.
fn assert_parity(name: &str, c: Case) -> Observed {
    let a = run(Program::Standalone, &c);
    let b = run(Program::Darkmux, &c);
    assert_eq!(a.code, b.code, "{name}: exit code\nstandalone stderr:\n{}\ndarkmux stderr:\n{}", a.stderr, b.stderr);
    same(&format!("{name}: stdout"), &a.stdout, &b.stdout);
    same(&format!("{name}: stderr"), &a.stderr, &b.stderr);
    assert_eq!(a.requests.len(), b.requests.len(), "{name}: number of model requests");
    for (i, (x, y)) in a.requests.iter().zip(&b.requests).enumerate() {
        assert_eq!(x.request_line, y.request_line, "{name}: request {i} line");
        assert_eq!(x.headers, y.headers, "{name}: request {i} headers");
        same(&format!("{name}: request {i} body"), &x.body, &y.body);
    }
    let paths = |o: &Observed| o.files.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>();
    assert_eq!(paths(&a), paths(&b), "{name}: files left behind");
    for ((path, x), (_, y)) in a.files.iter().zip(&b.files) {
        same(&format!("{name}: {path}"), x, y);
    }
    a
}

// ─── the cases ─────────────────────────────────────────────────────────

/// The subcommands that make no model call: version, usage, the environment
/// probe, an unknown subcommand.
#[test]
fn the_subcommands_without_a_model_match() {
    for (name, args) in [
        ("version", vec!["--version"]),
        ("version word", vec!["version"]),
        ("usage", vec![]),
        ("check", vec!["--check"]),
        ("unknown", vec!["frobnicate"]),
    ] {
        let o = assert_parity(name, case(&args, answer_any));
        assert!(o.requests.is_empty(), "{name}");
    }
}

fn answer_any(_: usize, _: &str) -> Reply {
    answer("ok")
}

/// Refusals before any model call exit 2 the same way, both the returned
/// kind and the `process::exit` kind (the check-in interval flags).
#[test]
fn startup_refusals_match() {
    for (name, extra) in [
        ("unknown flag", vec!["--frobnicate"]),
        ("bad number", vec!["--max-turns", "x"]),
        ("process-exit refusal", vec!["--reasoning-checkpoint-interval", "x"]),
        ("missing value", vec!["--model"]),
    ] {
        let o = assert_parity(name, case(&dispatch(&extra).iter().map(String::as_str).collect::<Vec<_>>(), answer_any));
        assert_eq!(o.code, Some(2), "{name}: {o:?}");
    }
    let mut c = case(&dispatch(&["--auth-header-stdin"]).iter().map(String::as_str).collect::<Vec<_>>(), answer_any);
    c.stdin = Some("not json");
    assert_eq!(assert_parity("bad auth blob", c).code, Some(2));
}

/// A full tool-using dispatch, `--json`: every tool's schema in every
/// request, a refusal that teaches the edit schema back, a finding, files
/// written, the envelope, the trajectory and the checkpoint.
#[test]
fn a_tool_using_dispatch_matches_in_json_mode() {
    let tools = "search,read,edit,write,bash,create_finding,create_mod";
    let o = assert_parity(
        "tools --json",
        case(&dispatch(&["--no-stream", "--json", "--allowed-tools", tools, "--role-id", "coder"]).iter().map(String::as_str).collect::<Vec<_>>(), tool_turns),
    );
    assert_eq!(o.code, Some(0), "{o:?}");
    assert_eq!(o.requests.len(), 5, "{o:?}");
    assert!(o.requests[2].body.contains("EXPECTED argument schema"), "the refusal reached the model: {}", o.requests[2].body);
    // What the fixture is for: the records that carry JSON values were
    // written, so their bytes were compared.
    let file = |suffix: &str| o.files.iter().find(|(p, _)| p.ends_with(suffix)).map(|(_, b)| b.clone()).unwrap_or_default();
    let trajectory = file("trajectory.jsonl");
    assert!(trajectory.contains("\"emitted\":{"), "an accepted finding or mod: {trajectory}");
    assert!(trajectory.contains("\"bound\":{"), "a recovery record naming its bound: {trajectory}");
    assert!(o.files.iter().any(|(p, _)| p.contains("finding")), "a findings file: {:?}", o.files.iter().map(|f| &f.0).collect::<Vec<_>>());
    assert!(o.files.iter().any(|(p, _)| p.contains("mod")), "a mods file: {:?}", o.files.iter().map(|f| &f.0).collect::<Vec<_>>());
}

/// The same dispatch in human mode, streamed.
#[test]
fn a_streamed_dispatch_matches_in_human_mode() {
    let o = assert_parity(
        "streamed human",
        case(&dispatch(&["--allowed-tools", "search,read,edit,write,bash,create_finding,create_mod"]).iter().map(String::as_str).collect::<Vec<_>>(), streamed),
    );
    assert_eq!(o.code, Some(0), "{o:?}");
}

/// A role's output schema, given in an order that is not sorted, becomes the
/// strict `response_format` on every request.
#[test]
fn a_response_schema_dispatch_matches() {
    let schema = r#"{"type":"object","properties":{"zeta":{"type":"string"},"alpha":{"type":"integer"}},"required":["zeta","alpha"],"additionalProperties":false}"#;
    let o = assert_parity("response schema", case(&dispatch(&["--no-stream", "--json", "--response-schema", schema]).iter().map(String::as_str).collect::<Vec<_>>(), answer_any));
    assert_eq!(o.code, Some(0), "{o:?}");
}

/// The Messages dialect's body and reply parsing, with a hosted auth header.
#[test]
fn a_messages_dialect_dispatch_matches() {
    let mut c = case(
        &dispatch(&["--no-stream", "--json", "--chat-url", "{base}/messages", "--dialect", "messages", "--auth-header-stdin"])
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        messages_turns,
    );
    c.stdin = Some(r#"{"header":"x-api-key","value":"k"}"#);
    let o = assert_parity("messages dialect", c);
    assert_eq!(o.code, Some(0), "{o:?}");

    let args = dispatch(&["--no-stream", "--json", "--chat-url", "{base}/messages", "--dialect", "messages"]);
    let o = assert_parity("messages reply without content", case(&args.iter().map(String::as_str).collect::<Vec<_>>(), messages_broken));
    assert_eq!(o.code, Some(1), "{o:?}");
    assert!(o.stderr.contains("messages reply has no content array"), "{}", o.stderr);

    let args = dispatch(&["--json", "--chat-url", "{base}/messages", "--dialect", "messages"]);
    let o = assert_parity("messages stream error", case(&args.iter().map(String::as_str).collect::<Vec<_>>(), messages_stream_error));
    assert_eq!(o.code, Some(1), "{o:?}");
    assert!(o.stderr.contains("error event mid-stream"), "{}", o.stderr);
}

/// Compaction, both strategies: the compactor's request (its system prompt,
/// the structured-slot schema) and the installed summary.
#[test]
fn compaction_matches_under_both_strategies() {
    for strategy in ["narrative", "structured-slot"] {
        let extra = [
            "--no-stream", "--json", "--compactor-model", "util-model", "--compact-threshold-tokens", "1000",
            "--max-turns", "4", "--compact-strategy", strategy, "--compactor-custom-instructions", "keep file names",
        ];
        let args: Vec<String> = dispatch(&extra).into_iter().filter(|a| a != "--context-window" && a != "32000").collect();
        let o = assert_parity(strategy, case(&args.iter().map(String::as_str).collect::<Vec<_>>(), compaction_turns));
        assert!(o.requests.iter().any(|r| r.body.contains("\"util-model\"")), "{strategy}: the compactor ran: {o:?}");
    }
}

/// A streamed engine error whose `error` has no string `message`: the
/// message prints the error object.
#[test]
fn a_streamed_error_object_matches() {
    let o = assert_parity("stream error object", case(&dispatch(&["--json"]).iter().map(String::as_str).collect::<Vec<_>>(), stream_error_object));
    assert_eq!(o.code, Some(1), "{o:?}");
    assert!(o.stderr.contains("error event mid-stream"), "{}", o.stderr);
}

/// A structured-slot compaction whose slots do not parse: the reason the
/// runtime gives names the same field in both programs.
#[test]
fn a_compaction_parse_failure_matches() {
    let extra = [
        "--no-stream", "--json", "--compactor-model", "util-model", "--compact-threshold-tokens", "1000",
        "--max-turns", "3", "--compact-strategy", "structured-slot",
    ];
    let args: Vec<String> = dispatch(&extra).into_iter().filter(|a| a != "--context-window" && a != "32000").collect();
    let o = assert_parity("compaction parse failure", case(&args.iter().map(String::as_str).collect::<Vec<_>>(), compaction_bad_slots));
    let said = format!("{}{}", o.stderr, o.files.iter().map(|f| f.1.as_str()).collect::<String>());
    assert!(said.contains("parsing compactor JSON failed"), "the parse failed and was reported: {said}");
}

/// A loop error: exit 1, the error envelope, the human failure block.
#[test]
fn a_loop_error_matches() {
    for mode in [vec!["--no-stream", "--json"], vec!["--no-stream"]] {
        let o = assert_parity("loop error", case(&dispatch(&mode).iter().map(String::as_str).collect::<Vec<_>>(), server_error));
        assert_eq!(o.code, Some(1), "{o:?}");
    }
}

/// A SIGTERM mid-call: exit 143 and a terminal `interrupted` record, from the
/// runtime's own handler in both programs.
#[test]
fn a_sigterm_mid_call_matches() {
    let mut c = case(&dispatch(&["--no-stream", "--json"]).iter().map(String::as_str).collect::<Vec<_>>(), hang);
    c.sigterm = true;
    let o = assert_parity("sigterm", c);
    assert_eq!(o.code, Some(143), "{o:?}");
    let trajectory = &o.files.iter().find(|(p, _)| p.ends_with("trajectory.jsonl")).expect("a trajectory").1;
    assert!(trajectory.trim_end().lines().next_back().unwrap().contains("\"interrupted\""), "{trajectory}");
}

// ─── inside a Seatbelt sandbox ─────────────────────────────────────────

/// (#3112) What a Seatbelt dispatch runs, run for real: this darkmux
/// executable, by its real path, with `runtime run …`, under `sandbox-exec`
/// and a dispatch's policy shape (the workspace and out-dir read-write and
/// seen at their own host paths, the default network, nothing else of the
/// host's). The tool-using fixture runs to its answer, its edits land in the
/// workspace and its records in the out-dir.
///
/// The executable sits under the user's home here (the repo's `target/`), the
/// same class of path as `~/.cargo/bin`: outside every system path the
/// profile allows, so only the profile's rule for its entrypoint lets it run.
/// A Homebrew install runs from its Cellar, which the profile allows as a
/// whole; one Cellar program and, when present, an installed
/// `~/.cargo/bin/darkmux` are run as entrypoints too.
#[test]
#[ignore = "needs macOS sandbox-exec: cargo nextest run -p darkmux --test runtime_parity --run-ignored only"]
fn the_runtime_subcommand_runs_inside_a_seatbelt_sandbox() {
    use darkmux_sandbox::{Access, Limits, Mount, Network, SandboxBackend, SandboxPolicy, SeatbeltBackend};
    assert!(SeatbeltBackend::available(), "no /usr/bin/sandbox-exec on this host");
    let state = tempfile::tempdir().unwrap();
    // SAFETY: set before any thread of this test reads it; nextest runs each
    // test in its own process.
    std::env::set_var(darkmux_sandbox::seatbelt::STATE_DIR_ENV, state.path());
    let backend = SeatbeltBackend;

    let run_in_sandbox = |entrypoint: &Path, command: Vec<String>, mounts: Vec<Mount>, env: Vec<(String, String)>| {
        let policy = SandboxPolicy {
            name: format!("darkmux-runtime-parity-{}-{}", std::process::id(), command.len()),
            image: "unused".to_string(),
            network: Network::Default,
            limits: Limits::default(),
            labels: Vec::new(),
            mounts,
            ports: Vec::new(),
            stdin: false,
            host_alias: None,
            user: None,
            env,
            workdir: None,
            entrypoint: Some(entrypoint.display().to_string()),
            command,
        };
        let (mut cmd, _handle) = backend.run(&policy);
        cmd.stdin(Stdio::null()).output().expect("running the sandbox")
    };

    // The dispatch.
    let exe = darkmux_exe();
    let ws = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("x.txt"), "hello\nworld\n").unwrap();
    let mock = Mock::start(tool_turns);
    let mounts = vec![
        Mount { host: ws.path().to_path_buf(), at: "/workspace".to_string(), access: Access::ReadWrite },
        Mount { host: out.path().to_path_buf(), at: "/darkmux-out".to_string(), access: Access::ReadWrite },
    ];
    let env = vec![
        ("DARKMUX_RUNTIME_WORKSPACE".to_string(), backend.place(&mounts[0])),
        ("DARKMUX_RUNTIME_OUT_DIR".to_string(), backend.place(&mounts[1])),
        ("DARKMUX_TURN_DELAY_MS".to_string(), "0".to_string()),
    ];
    let mut command = vec!["runtime".to_string()];
    command.extend(
        dispatch(&["--no-stream", "--json", "--allowed-tools", "search,read,edit,write,bash,create_finding,create_mod"])
            .into_iter()
            .map(|a| a.replace("{base}", &mock.base_url())),
    );
    let output = run_in_sandbox(&exe, command, mounts, env);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let envelope: Value = serde_json::from_str(stdout.trim()).expect("the --json envelope");
    assert_eq!(envelope["result"], "stop", "{envelope}");
    assert_eq!(envelope["final_assistant"], "all done", "{envelope}");
    assert_eq!(mock.requests().len(), 5, "every model call left the sandbox");
    assert_eq!(std::fs::read_to_string(ws.path().join("x.txt")).unwrap(), "bye\nworld\n", "the edit landed");
    assert_eq!(std::fs::read_to_string(ws.path().join("new.txt")).unwrap(), "made\n", "the write landed");
    let trajectory = std::fs::read_to_string(out.path().join(".darkmux-runtime/trajectory.jsonl")).unwrap();
    assert!(trajectory.trim_end().lines().next_back().unwrap().contains("\"dispatch.complete\""), "{trajectory}");

    // Entrypoints where installs put darkmux.
    let mut entrypoints: Vec<PathBuf> = Vec::new();
    entrypoints.extend(installed_darkmux());
    let cellar_program = ["fd", "gh", "git", "rg", "jq"].iter().find_map(|name| {
        let versions = std::fs::read_dir(Path::new("/opt/homebrew/Cellar").join(name)).ok()?;
        versions.filter_map(Result::ok).map(|v| v.path().join("bin").join(name)).find(|p| p.is_file())
    });
    entrypoints.extend(cellar_program);
    assert!(!entrypoints.is_empty(), "no installed program to run from ~/.cargo/bin or the Homebrew Cellar");
    for program in entrypoints {
        let output = run_in_sandbox(&program, vec!["--version".to_string()], Vec::new(), Vec::new());
        assert_eq!(
            output.status.code(),
            Some(0),
            "{} under Seatbelt: {}",
            program.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

// ─── the linked package stays the standalone one ───────────────────────

/// The tables of `manifest` that decide how its library is built, by
/// header (`[dependencies]`, `[features]`, `[target.….dependencies]`), each
/// as its `key = value` lines: comments and blank lines dropped, a `path`
/// resolved to the directory it names (the two manifests sit at different
/// depths).
fn build_tables(manifest: &Path) -> Vec<(String, Vec<(String, String)>)> {
    let text = std::fs::read_to_string(manifest).unwrap();
    let dir = manifest.parent().unwrap();
    let wanted = |h: &str| h == "[dependencies]" || h == "[features]" || (h.starts_with("[target.") && h.ends_with(".dependencies]"));
    let mut tables: Vec<(String, Vec<(String, String)>)> = Vec::new();
    let mut current: Option<usize> = None;
    for line in text.lines().map(|l| l.split('#').next().unwrap().trim()) {
        if line.starts_with('[') {
            current = wanted(line).then(|| {
                tables.push((line.to_string(), Vec::new()));
                tables.len() - 1
            });
            continue;
        }
        let (Some(at), Some((name, value))) = (current, line.split_once('=')) else { continue };
        let mut value = value.trim().to_string();
        if let Some(i) = value.find("path = \"") {
            let rest = &value[i + 8..];
            let rel = &rest[..rest.find('"').unwrap()];
            let real = dir.join(rel).canonicalize().unwrap().display().to_string();
            value = value.replace(rel, &real);
        }
        tables[at].1.push((name.trim().to_string(), value));
    }
    for (_, entries) in &mut tables {
        entries.sort();
    }
    tables.sort();
    tables
}

/// (#3112) `crates/darkmux-runtime-linked` compiles `runtime/src` against what
/// its manifest declares; its dependencies, features and per-target
/// dependencies must be the standalone runtime's own, names and features, or
/// the copy in darkmux is built differently from the one in the Docker image.
#[test]
fn the_linked_runtime_declares_the_standalone_runtimes_dependencies() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let standalone = build_tables(&root.join("runtime/Cargo.toml"));
    let linked = build_tables(&root.join("crates/darkmux-runtime-linked/Cargo.toml"));
    let deps = standalone.iter().find(|(h, _)| h == "[dependencies]").expect("the reader found the table");
    assert!(deps.1.iter().any(|(n, _)| n == "ureq"), "{standalone:?}");
    assert_eq!(linked, standalone);
}

/// `name -> versions` of every package `package` builds with (normal and
/// build dependencies, transitively), as `manifest`'s lockfile resolves them.
fn resolved_closure(manifest: &Path, package: &str) -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--locked", "--manifest-path"])
        .arg(manifest)
        .output()
        .unwrap();
    assert!(out.status.success(), "cargo metadata {}: {}", manifest.display(), String::from_utf8_lossy(&out.stderr));
    let m: Value = serde_json::from_slice(&out.stdout).unwrap();
    let name_version = |id: &str| {
        let p = m["packages"].as_array().unwrap().iter().find(|p| p["id"] == id).unwrap();
        (p["name"].as_str().unwrap().to_string(), p["version"].as_str().unwrap().to_string())
    };
    let nodes = m["resolve"]["nodes"].as_array().unwrap();
    let start = m["packages"].as_array().unwrap().iter().find(|p| p["name"] == package).unwrap()["id"].as_str().unwrap().to_string();
    let (mut seen, mut todo) = (std::collections::BTreeSet::new(), vec![start]);
    while let Some(id) = todo.pop() {
        let node = nodes.iter().find(|n| n["id"] == id.as_str()).unwrap();
        for dep in node["deps"].as_array().unwrap() {
            let built = dep["dep_kinds"].as_array().unwrap().iter().any(|k| k["kind"].is_null() || k["kind"] == "build");
            let pkg = dep["pkg"].as_str().unwrap().to_string();
            if built && seen.insert(pkg.clone()) {
                todo.push(pkg);
            }
        }
    }
    let mut closure: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> = Default::default();
    for id in &seen {
        let (name, version) = name_version(id);
        closure.entry(name).or_default().insert(version);
    }
    closure
}

/// (#3112) The two lockfiles resolve the runtime's dependencies to the same
/// versions: a package the standalone runtime builds with is the same version
/// in darkmux's build (which may add more, for features other crates turn
/// on). Otherwise the two programs differ in code no parity fixture reaches,
/// such as the CA roots `webpki-roots` trusts.
#[test]
fn both_lockfiles_resolve_the_runtimes_dependencies_to_the_same_versions() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let standalone = resolved_closure(&root.join("runtime/Cargo.toml"), "darkmux-runtime");
    let linked = resolved_closure(&root.join("Cargo.toml"), "darkmux-runtime-linked");
    assert!(standalone.contains_key("webpki-roots"), "the closure was read: {standalone:?}");
    let differing: Vec<String> = standalone
        .iter()
        .filter(|(name, versions)| linked.get(*name).is_none_or(|l| !versions.is_subset(l)))
        .map(|(name, versions)| format!("{name}: standalone {versions:?}, linked {:?}", linked.get(name)))
        .collect();
    assert!(differing.is_empty(), "align these (`cargo update -p <name> --precise <version>`):\n{}", differing.join("\n"));
}

/// (#3112) The subcommand is for sandboxes, not people: `--help` does not
/// list it.
#[test]
fn the_runtime_subcommand_is_hidden_from_help() {
    let help = darkmux_help();
    assert!(help.contains("dispatch"), "the command list was read: {help}");
    assert!(!help.lines().any(|l| l.trim_start().starts_with("runtime ")), "{help}");
}
