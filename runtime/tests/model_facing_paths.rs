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
//! `runtime/testdata/request-bytes/first_request.json`, and a host sandbox's
//! in `first_request_host_sandbox.json` (#3654: it names no host path, so it
//! is the same on every machine). To accept a deliberate change to
//! model-facing text, rerun with `DARKMUX_BLESS_REQUEST_BYTES=1` and review
//! the goldens' diff.

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
        Mock::scripted(move |n, _| replies.get(n).cloned().unwrap_or_else(|| json!({})))
    }

    /// A mock answering request `n`, whose body is `body`, with `reply(n, body)`.
    fn scripted(reply: impl Fn(usize, &[u8]) -> Value + Send + 'static) -> Mock {
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
                    l.push(body.clone());
                    l.len() - 1
                };
                let reply = reply(n, &body).to_string();
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
    run_with(mock, env, &[])
}

/// [`run`] with `extra` arguments after the minimal dispatch's.
fn run_with(mock: &Mock, env: &[(&str, &Path)], extra: &[&str]) -> String {
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
        .args(extra)
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

// ─── (#3654) no host path reaches a model under a host sandbox ─────────

/// A workspace the way a host sandbox sees it: at its own host path, under a
/// folder that stands in for the user's home (inside this build's target
/// folder, itself under the real home), with a file outside it.
struct HostWorkspace {
    home: tempfile::TempDir,
    out: tempfile::TempDir,
}

impl HostWorkspace {
    fn new() -> HostWorkspace {
        let home = tempfile::Builder::new()
            .prefix("home-of-op3654-")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        std::fs::create_dir_all(home.path().join("ws/sub")).unwrap();
        std::fs::write(home.path().join("ws/x.txt"), "hello\nworld\n").unwrap();
        std::fs::write(home.path().join("ws/hello.txt"), "hi\n").unwrap();
        std::fs::write(home.path().join("outside.txt"), "secret\n").unwrap();
        HostWorkspace { home, out: tempfile::tempdir().unwrap() }
    }

    fn ws(&self) -> PathBuf {
        self.home.path().join("ws")
    }

    fn env(&self) -> Vec<(&'static str, PathBuf)> {
        vec![("DARKMUX_RUNTIME_WORKSPACE", self.ws()), ("DARKMUX_RUNTIME_OUT_DIR", self.out.path().to_path_buf())]
    }

    /// Every spelling of a path that would name the user: the workspace as
    /// given and as resolved, the folder standing in for the home, and the
    /// real home this test runs under.
    fn private_paths(&self) -> Vec<String> {
        let mut paths = vec![
            self.ws().display().to_string(),
            self.ws().canonicalize().unwrap().display().to_string(),
            self.home.path().display().to_string(),
            "home-of-op3654".to_string(),
        ];
        paths.extend(std::env::var("HOME").ok().filter(|h| h.len() > 1));
        paths
    }

    /// Run the binary under this workspace's host-sandbox environment.
    fn run(&self, mock: &Mock, extra: &[&str]) -> String {
        let env = self.env();
        let env: Vec<(&str, &Path)> = env.iter().map(|(k, v)| (*k, v.as_path())).collect();
        run_with(mock, &env, extra)
    }

    /// Fail naming the first request body that carries a private path.
    fn assert_no_private_path(&self, bodies: &[Vec<u8>], said: &str) {
        for (i, body) in bodies.iter().enumerate() {
            for private in self.private_paths() {
                assert!(
                    !contains(body, &private),
                    "request {i} names {private:?}:\n{}\n{said}",
                    String::from_utf8_lossy(body)
                );
            }
        }
    }
}

fn call(id: &str, name: &str, args: Value) -> Value {
    json!({ "id": id, "type": "function", "function": { "name": name, "arguments": args.to_string() } })
}

fn calls(list: Value) -> Value {
    chat(json!({ "role": "assistant", "content": null, "tool_calls": list }), "tool_calls")
}

/// Every tool, used the way a model working in its current directory uses
/// them, and every refusal a file tool can return: a path that escapes, one
/// that does not exist, a directory, a write whose folder is missing, an
/// edit that matches nothing, a finding and an attachment outside.
fn every_tool_and_refusal(n: usize, _: &[u8]) -> Value {
    match n {
        0 => calls(json!([
            call("c1", "search", json!({ "pattern": "hello", "path": "." })),
            call("c2", "read", json!({ "path": "x.txt", "offset": 1, "limit": 5 })),
            call("c3", "read", json!({ "path": "sub", "offset": 1, "limit": 0 })),
            call("c4", "read", json!({ "path": "../outside.txt", "offset": 1, "limit": 0 })),
            call("c5", "read", json!({ "path": "missing.txt", "offset": 1, "limit": 0 })),
            call("c6", "search", json!({ "pattern": "x", "path": "nope" })),
            call("c7", "search", json!({ "pattern": "x", "path": "../outside.txt" })),
        ])),
        1 => calls(json!([
            call("c8", "edit", json!({ "path": "x.txt", "edits": [{ "old_string": "hello", "new_string": "bye" }] })),
            call("c9", "edit", json!({ "path": "x.txt", "edits": [{ "old_string": "absent", "new_string": "y" }] })),
            call("c10", "edit", json!({ "path": "./sub", "edits": [{ "old_string": "a", "new_string": "b" }] })),
            call("c11", "write", json!({ "path": "new.txt", "content": "made\n" })),
            call("c12", "write", json!({ "path": "./nodir/x.txt", "content": "x" })),
            call("c13", "write", json!({ "path": "../escape.txt", "content": "x" })),
            call("c14", "write", json!({ "path": "sub", "content": "x" })),
        ])),
        2 => calls(json!([
            call("c15", "bash", json!({ "command": "cat new.txt x.txt" })),
            call("c16", "create_finding", json!({ "file": "x.txt", "line": 2, "pattern": "p", "evidence": "world", "why": "w" })),
            call("c17", "create_finding", json!({ "file": "../outside.txt", "line": 1, "pattern": "p", "evidence": "secret", "why": "w" })),
            call("c18", "create_mod", json!({ "kit": "k", "attach": ["new.txt"] })),
            call("c19", "create_mod", json!({ "kit": "k", "attach": ["../outside.txt"] })),
            call("c20", "create_mod", json!({ "kit": "k", "attach": ["sub"] })),
            // Arguments of the wrong shape: the refusal teaches the schema back.
            call("c21", "write", json!({ "file": "x.txt" })),
        ])),
        _ => answer(),
    }
}

const ALL_TOOLS: [&str; 2] = ["--allowed-tools", "search,read,edit,write,bash,create_finding,create_mod"];

/// (#3654) Under a host sandbox no request carries the workspace's host
/// path, nor the home it sits in: not the tool definitions, not the user's
/// message, and not a single tool result or refusal the runtime writes.
/// The model is told the workspace is its current directory, works in it
/// with relative paths, and its work lands.
#[test]
fn no_request_under_a_host_sandbox_names_the_workspaces_host_path() {
    let hw = HostWorkspace::new();
    let mock = Mock::scripted(every_tool_and_refusal);
    let said = hw.run(&mock, &ALL_TOOLS);
    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 4, "{said}");
    hw.assert_no_private_path(&bodies, &said);
    // Nor does any tool result name a `/workspace` that does not exist here.
    let last: Value = serde_json::from_slice(&bodies[3]).unwrap();
    for m in last["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool") {
        assert!(!m["content"].as_str().unwrap_or_default().contains("/workspace"), "{m}");
    }
    // It worked in its current directory.
    assert_eq!(std::fs::read_to_string(hw.ws().join("x.txt")).unwrap(), "bye\nworld\n");
    assert_eq!(std::fs::read_to_string(hw.ws().join("new.txt")).unwrap(), "made\n");
    assert!(!hw.home.path().join("escape.txt").exists(), "the escape guard held");
    let last = String::from_utf8_lossy(&bodies[3]).to_string();
    for shown in ["Wrote 5 bytes to new.txt", "Edited x.txt", "x.txt:1:hello", "made\\nbye\\nworld"] {
        assert!(last.contains(shown), "{shown:?} not in:\n{last}");
    }
    // The tool definitions and the user's message name it as the current directory.
    let tools = serde_json::from_slice::<Value>(&bodies[0]).unwrap()["tools"].to_string();
    assert!(tools.contains("current directory") && !tools.contains("/workspace"), "{tools}");
    assert!(content(&bodies[0], 1).contains("`./hello.txt`"), "{}", content(&bodies[0], 1));
}

/// (#3654) The compactor's requests under a host sandbox carry no host path
/// either: the conversation it summarizes is the one above.
#[test]
fn no_compaction_request_under_a_host_sandbox_names_the_workspaces_host_path() {
    let hw = HostWorkspace::new();
    let mock = Mock::scripted(|_, body| {
        if contains(body, "\"util-model\"") {
            let summary = "Summary: the assistant read x.txt in the current directory several times and was refused a file \
                           outside the workspace each time. Nothing was modified, no decision was made, and the next step \
                           is to act on what x.txt holds.";
            return chat(json!({ "role": "assistant", "content": summary }), "stop");
        }
        let mut c = calls(json!([call("c1", "read", json!({ "path": "x.txt", "offset": 1, "limit": 0 })),
                                 call("c2", "read", json!({ "path": "../outside.txt", "offset": 1, "limit": 0 }))]));
        c["usage"]["prompt_tokens"] = json!(5000);
        c
    });
    let said = hw.run(
        &mock,
        &["--compactor-model", "util-model", "--compact-threshold-tokens", "1000", "--max-turns", "4"],
    );
    let bodies = mock.bodies();
    assert!(bodies.iter().any(|b| contains(b, "\"util-model\"")), "the compactor ran: {said}");
    hw.assert_no_private_path(&bodies, &said);
}

fn host_golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/request-bytes/first_request_host_sandbox.json")
}

/// (#3654, contract 6) A host sandbox's first request is pinned byte for
/// byte too: the tool definitions and the user's message as the runtime
/// names the workspace there, and the system prompt as passed (the host
/// names it before it hashes it). It names no path of the host's, so it is
/// the same on every machine. Bless it with `DARKMUX_BLESS_REQUEST_BYTES=1`,
/// as the container's.
#[test]
fn a_host_sandboxs_first_request_is_the_golden_bytes() {
    let hw = HostWorkspace::new();
    let mock = Mock::start(vec![answer()]);
    let said = hw.run(&mock, &ALL_TOOLS);
    let bodies = mock.bodies();
    assert_eq!(bodies.len(), 1, "{said}");
    if std::env::var_os("DARKMUX_BLESS_REQUEST_BYTES").is_some() {
        std::fs::write(host_golden_path(), &bodies[0]).unwrap();
    }
    let golden = std::fs::read(host_golden_path())
        .expect("the golden; bless it with DARKMUX_BLESS_REQUEST_BYTES=1");
    assert!(
        bodies[0] == golden,
        "a host sandbox's first request is not the golden:\n{}",
        String::from_utf8_lossy(&bodies[0])
    );
}
