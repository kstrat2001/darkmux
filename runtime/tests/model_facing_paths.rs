//! Contract 6 as the endpoint receives it: the system prompt the host
//! passes as `--system` is the system prompt sent, byte for byte, under a
//! container's environment and a host sandbox's (`DARKMUX_RUNTIME_WORKSPACE`
//! set to the workspace's host path). The host hashes exactly the text it
//! passes into `dispatch.start`'s `system_sha256`, so equal bytes here are
//! the proof that the recorded hash is the hash of what was sent. Where a
//! host sandbox sees the workspace is written into that text by the host
//! (`darkmux-crew`'s `system_prompt_as_sent`), never rewritten here.
//!
//! Every test drives the real `darkmux-runtime` binary against a mock
//! endpoint that keeps each request body as the bytes it received, never as
//! parsed JSON: key order is part of what a model is sent.
//!
//! A container's first request is pinned in
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
                let reply = replies
                    .get(n)
                    .map(Value::to_string)
                    .unwrap_or_else(|| "{}".into());
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
            "run",
            "--model",
            "brain-model",
            "--system",
            SYSTEM,
            "--prompt",
            PROMPT,
            "--context-window",
            "32000",
            "--no-stream",
            "--execution-id",
            "exec-1",
            "--base-url",
        ])
        .arg(mock.base_url())
        .stdin(Stdio::null())
        .output()
        .expect("spawning darkmux-runtime");
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The `messages[i].content` of a request body.
fn content(body: &[u8], i: usize) -> String {
    let v: Value = serde_json::from_slice(body).unwrap();
    v["messages"][i]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// `content` exactly as it sits in the body: its JSON string literal.
fn wire(content: &str) -> Vec<u8> {
    serde_json::to_vec(&Value::String(content.to_string())).unwrap()
}

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/request-bytes/first_request.json")
}

/// A container's first request is the golden, byte for byte: the system
/// prompt, the user's message and every tool description.
#[test]
fn a_containers_first_request_is_the_golden_bytes() {
    let mock = Mock::start(vec![answer()]);
    let said = run(&mock, &[]);
    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 1, "{said}");
    if std::env::var_os("DARKMUX_BLESS_REQUEST_BYTES").is_some() {
        std::fs::create_dir_all(golden_path().parent().unwrap()).unwrap();
        std::fs::write(golden_path(), &bodies[0]).unwrap();
    }
    let golden = std::fs::read(golden_path())
        .expect("the golden; bless it with DARKMUX_BLESS_REQUEST_BYTES=1");
    assert!(
        bodies[0] == golden,
        "a container's first request is not the golden:\n{}",
        String::from_utf8_lossy(&bodies[0])
    );
}

/// Under a host sandbox's environment the system prompt is sent as the
/// host passed it, byte for byte, even where it names `/workspace`: the
/// recorded `system_sha256` (the hash of `--system`) is the hash of the
/// bytes sent. Under a container's, the same.
#[test]
fn the_system_prompt_is_sent_as_passed_under_a_host_sandbox_and_a_container() {
    let ws = tempfile::tempdir().unwrap();
    for env in [vec![("DARKMUX_RUNTIME_WORKSPACE", ws.path())], vec![]] {
        let mock = Mock::start(vec![answer()]);
        let said = run(&mock, &env);
        let bodies = mock.bodies();
        assert_eq!(bodies.len(), 1, "{said}");
        assert_eq!(content(&bodies[0], 0), SYSTEM, "{env:?}");
        assert!(
            contains(&bodies[0], std::str::from_utf8(&wire(SYSTEM)).unwrap()),
            "{env:?}: the literal bytes"
        );
    }
}
