//! Contract 6 and the workspace path a model is shown, as the endpoint
//! receives them. A container sees its workspace at `/workspace`; a sandbox
//! on the host (Seatbelt, Landlock) sees it at its host path, which the host
//! passes as `DARKMUX_RUNTIME_WORKSPACE`. The model is shown `/workspace` on
//! every backend: the runtime maps a path the model names onto the host path
//! and a host path in a tool's output back, so no request body carries the
//! host path (the operator's home folder, and so the user name), and the
//! system prompt reaches the endpoint as the bytes the host hashed into
//! `dispatch.start`'s `system_sha256`.
//!
//! Every test drives the real `darkmux-runtime` binary against a mock
//! endpoint that keeps each request body as the bytes it received, never as
//! parsed JSON: key order is part of what a model is sent.
//!
//! The first request's bytes are pinned in
//! `runtime/testdata/request-bytes/first_request.json`. To accept a
//! deliberate change to model-facing text, rerun with
//! `DARKMUX_BLESS_REQUEST_BYTES=1` and review the golden's diff.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

const SYSTEM: &str = "You are a test. Work in /workspace: run commands with cwd=/workspace and read /workspace/hello.txt.";
const PROMPT: &str = "Read `/workspace/hello.txt` and say what it holds.";

/// A mock endpoint answering request `n` with `replies[n]`, keeping every
/// request body as received.
struct Mock {
    port: u16,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Mock {
    fn start(replies: Vec<Value>) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let bodies: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let log = Arc::clone(&bodies);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let body = read_body(&mut BufReader::new(stream.try_clone().unwrap()));
                let n = {
                    let mut l = log.lock().unwrap();
                    l.push(body);
                    l.len() - 1
                };
                let reply = replies.get(n).map(Value::to_string).unwrap_or_else(|| "{}".into());
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    reply.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.flush();
            }
        });
        Mock { port, bodies }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    fn bodies(&self) -> Vec<Vec<u8>> {
        self.bodies.lock().unwrap().clone()
    }
}

/// One request's body, after its head.
fn read_body(reader: &mut BufReader<std::net::TcpStream>) -> Vec<u8> {
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; len];
    let _ = reader.read_exact(&mut body);
    body
}

fn chat(message: Value, finish: &str) -> Value {
    json!({
        "id": "r", "object": "chat.completion", "created": 0, "model": "brain-model",
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
        "usage": { "prompt_tokens": 50, "completion_tokens": 5, "total_tokens": 55 },
    })
}

fn answer() -> Value {
    chat(json!({ "role": "assistant", "content": "done" }), "stop")
}

fn call(id: &str, name: &str, args: Value) -> Value {
    json!({ "id": id, "type": "function", "function": { "name": name, "arguments": args.to_string() } })
}

/// Run the binary against `mock` with `env` (every `DARKMUX_*` of the
/// runner's removed first). Returns stdout and stderr, for a failure's message.
fn run(mock: &Mock, env: &[(&str, &Path)]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_darkmux-runtime"));
    for (k, _) in std::env::vars() {
        if k.starts_with("DARKMUX_") {
            cmd.env_remove(k);
        }
    }
    let out = cmd
        .envs(env.iter().map(|(k, v)| (*k, *v)))
        .env("DARKMUX_TURN_DELAY_MS", "0")
        .args([
            "run", "--model", "brain-model", "--system", SYSTEM, "--prompt", PROMPT, "--context-window", "32000",
            "--no-stream", "--execution-id", "exec-1", "--base-url",
        ])
        .arg(mock.base_url())
        .stdin(Stdio::null())
        .output()
        .expect("spawning darkmux-runtime");
    format!("stdout:\n{}\nstderr:\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// A folder under the build's own target dir: inside the repository, so
/// under the user's home folder on a developer's machine and on CI, the
/// class of path a sandbox on the host would otherwise show the model.
fn home_like_dir() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("model-facing-paths").tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

/// Every form of `dir` a body could carry: as given, and as resolved.
fn forms(dir: &Path) -> Vec<String> {
    let mut out = vec![dir.display().to_string(), dir.canonicalize().unwrap().display().to_string()];
    out.dedup();
    out
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// The `messages[i].content` of a request body.
fn content(body: &[u8], i: usize) -> String {
    let v: Value = serde_json::from_slice(body).unwrap();
    v["messages"][i]["content"].as_str().unwrap_or_default().to_string()
}

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/request-bytes/first_request.json")
}

/// The first request, under a container's environment (no workspace
/// override) and a host sandbox's (the workspace at its host path), is the
/// same bytes, and those bytes are the golden: the system prompt, the
/// user's message and every tool description, byte for byte.
#[test]
fn the_first_request_is_the_same_bytes_under_a_container_and_a_host_sandbox() {
    let docker = Mock::start(vec![answer()]);
    let said = run(&docker, &[]);
    let ws = home_like_dir();
    let seatbelt = Mock::start(vec![answer()]);
    let said_seatbelt = run(&seatbelt, &[("DARKMUX_RUNTIME_WORKSPACE", ws.path())]);
    let (docker, seatbelt) = (docker.bodies(), seatbelt.bodies());
    assert_eq!(docker.len(), 1, "{said}");
    assert_eq!(seatbelt.len(), 1, "{said_seatbelt}");
    if std::env::var_os("DARKMUX_BLESS_REQUEST_BYTES").is_some() {
        std::fs::create_dir_all(golden_path().parent().unwrap()).unwrap();
        std::fs::write(golden_path(), &docker[0]).unwrap();
    }
    let golden = std::fs::read(golden_path()).expect("the golden; bless it with DARKMUX_BLESS_REQUEST_BYTES=1");
    assert!(docker[0] == golden, "a container's first request is not the golden:\n{}", String::from_utf8_lossy(&docker[0]));
    assert!(
        seatbelt[0] == golden,
        "a host sandbox's first request is not the golden:\n{}",
        String::from_utf8_lossy(&seatbelt[0])
    );
}

/// Under a host sandbox's environment, no request carries the workspace's
/// host path or the home folder, through a tool call that names
/// `/workspace` (bash and read), a command that prints the working
/// directory, and a read that escapes the workspace; the system prompt and
/// the user's message are the bytes the host passed, so the SHA-256 the
/// host recorded is the SHA-256 of what was sent.
#[test]
fn a_host_sandbox_request_carries_no_host_path_and_the_system_prompt_as_hashed() {
    let outer = home_like_dir();
    let ws = outer.path().join("ws");
    let out = outer.path().join("out");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(ws.join("hello.txt"), "hello from the workspace\n").unwrap();
    std::fs::write(outer.path().join("outside.txt"), "not yours\n").unwrap();
    let tools = chat(
        json!({ "role": "assistant", "content": "", "tool_calls": [
            call("c1", "bash", json!({ "command": "pwd && cat /workspace/hello.txt" })),
            call("c2", "read", json!({ "path": "/workspace/hello.txt", "offset": 1, "limit": 0 })),
            call("c3", "read", json!({ "path": "../outside.txt", "offset": 1, "limit": 0 })),
        ]}),
        "tool_calls",
    );
    let mock = Mock::start(vec![tools, answer()]);
    let said = run(&mock, &[("DARKMUX_RUNTIME_WORKSPACE", &ws), ("DARKMUX_RUNTIME_OUT_DIR", &out)]);
    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 2, "{said}");

    let mut host = forms(outer.path());
    host.extend(std::env::var("HOME").ok().filter(|h| h.len() > 1));
    for (n, body) in bodies.iter().enumerate() {
        for path in &host {
            assert!(!contains(body, path), "request {n} carries the host path {path}:\n{}", String::from_utf8_lossy(body));
        }
    }
    assert_eq!(content(&bodies[0], 0).as_bytes(), SYSTEM.as_bytes(), "the system prompt is sent as the host hashed it");
    assert_eq!(content(&bodies[0], 1), PROMPT, "the user's message is sent as given");

    let bash = content(&bodies[1], 3);
    let read = content(&bodies[1], 4);
    let escape = content(&bodies[1], 5);
    assert!(bash.contains("/workspace\n") && bash.contains("hello from the workspace"), "{bash}");
    assert!(read.contains("hello from the workspace"), "{read}");
    assert!(escape.contains("escapes workspace"), "{escape}");
}

/// A `DARKMUX_RUNTIME_PATHS` that does not parse refuses the dispatch,
/// naming it, before any model call: read as no pairs, every host path it
/// names would reach the model.
#[test]
fn a_malformed_paths_map_refuses_the_dispatch() {
    let mock = Mock::start(vec![answer()]);
    let ws = home_like_dir();
    let bad = ws.path().join("not-json");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_darkmux-runtime"));
    for (k, _) in std::env::vars() {
        if k.starts_with("DARKMUX_") {
            cmd.env_remove(k);
        }
    }
    let out = cmd
        .env("DARKMUX_RUNTIME_PATHS", bad.as_os_str())
        .args(["run", "--model", "m", "--system", SYSTEM, "--prompt", PROMPT, "--no-stream", "--base-url"])
        .arg(mock.base_url())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("DARKMUX_RUNTIME_PATHS"), "{stderr}");
    assert!(mock.bodies().is_empty(), "no model call");
}
