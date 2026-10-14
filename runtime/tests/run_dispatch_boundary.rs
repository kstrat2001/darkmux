//! (#316 stage 1, #3136) Boundary pins for `main.rs::run_dispatch`, before it
//! is split by responsibility. Every test here drives the REAL
//! `darkmux-runtime` binary and asserts only what an observer outside the
//! process sees: the exit code, the stdout/stderr lines that are contract,
//! the `--json` envelope, and the exact requests a mock model endpoint
//! receives. Nothing here names an internal a split would move.
//!
//! These pin behavior AS IT IS, oddities included (each marked "ODDITY");
//! a test that fails after a refactor means the refactor changed what an
//! observer sees, not that the old behavior was right.
//!
//! The out-dir (`/darkmux-out` in production) is read through the test seam
//! `DARKMUX_RUNTIME_OUT_DIR` (`trajectory::out_base`): the tests at the end
//! of this file point it at a tempdir and pin `dispatch.start`,
//! `dispatch.complete`, bound provenance, `role_id` in `checkpoint.json` and
//! the SIGTERM record. Every other test clears `DARKMUX_*`, so its recorder
//! degrades to a no-op ("trajectory recording disabled").
//!
//! The mock endpoint is a hand-rolled `TcpListener` server rather than
//! `httpmock`: the pins need the exact bodies and headers each request
//! carried, in order, which httpmock 0.7 does not hand back.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

// ─── harness ────────────────────────────────────────────────────────────

/// One request the mock endpoint received.
#[derive(Debug, Clone)]
struct Recorded {
    /// Request target, query string included.
    target: String,
    /// Header names lowercased.
    headers: Vec<(String, String)>,
    body: Value,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
    fn tool_names(&self) -> Vec<String> {
        self.body
            .get("tools")
            .and_then(Value::as_array)
            .map(|ts| {
                ts.iter()
                    .map(|t| {
                        t.pointer("/function/name")
                            .or_else(|| t.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    fn keys(&self) -> Vec<String> {
        let mut k: Vec<String> = self.body.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    }
}

/// What the mock answers with.
struct Reply {
    status: u16,
    content_type: &'static str,
    body: String,
}

impl Reply {
    fn json(v: Value) -> Self {
        Reply { status: 200, content_type: "application/json", body: v.to_string() }
    }
}

/// A mock model endpoint on an ephemeral port. `respond` is called with the
/// 0-based index of the request and the request itself.
struct Mock {
    port: u16,
    seen: Arc<Mutex<Vec<Recorded>>>,
}

impl Mock {
    fn start(respond: impl Fn(usize, &Recorded) -> Reply + Send + 'static) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<Recorded>>> = Arc::default();
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                    continue;
                }
                let target = request_line.split_whitespace().nth(1).unwrap_or_default().to_string();
                let mut headers = Vec::new();
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        let k = k.trim().to_ascii_lowercase();
                        if k == "content-length" {
                            len = v.trim().parse().unwrap();
                        }
                        headers.push((k, v.trim().to_string()));
                    }
                }
                let mut body = vec![0u8; len];
                reader.read_exact(&mut body).unwrap();
                let rec = Recorded {
                    target,
                    headers,
                    body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                };
                let n = {
                    let mut l = log.lock().unwrap();
                    l.push(rec.clone());
                    l.len() - 1
                };
                let reply = respond(n, &rec);
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
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
    fn requests(&self) -> Vec<Recorded> {
        self.seen.lock().unwrap().clone()
    }
}

/// What the process showed the outside world.
#[derive(Debug)]
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    /// The `--json` envelope: stdout's one line.
    fn envelope(&self) -> Value {
        let lines: Vec<&str> = self.stdout.lines().collect();
        assert_eq!(lines.len(), 1, "a --json stdout is exactly one line: {self:?}");
        serde_json::from_str(lines[0]).unwrap()
    }
}

/// Spawn the binary with `args`, every `DARKMUX_*` from the test's own
/// environment removed (so the runner's shell cannot leak a resume path or a
/// pacing knob in), plus `env`, and `stdin` piped when given.
fn runtime(args: &[&str], stdin: Option<&str>, env: &[(&str, &str)]) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_darkmux-runtime"));
    for (k, _) in std::env::vars() {
        if k.starts_with("DARKMUX_") {
            cmd.env_remove(k);
        }
    }
    cmd.envs(env.iter().copied())
        .args(args)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawning darkmux-runtime");
    if let Some(blob) = stdin {
        child.stdin.take().unwrap().write_all(blob.as_bytes()).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    Run {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

const SYSTEM: &str = "You are a test.";

/// The minimal well-formed dispatch, against `base_url`, non-streaming.
fn dispatch_args(base_url: &str) -> Vec<String> {
    [
        "run", "--model", "brain-model", "--system", SYSTEM, "--prompt", "hi", "--context-window", "32000",
        "--no-stream", "--execution-id", "exec-1", "--base-url", base_url,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn run_with(base: Vec<String>, extra: &[&str], stdin: Option<&str>, env: &[(&str, &str)]) -> Run {
    let mut args = base;
    args.extend(extra.iter().map(|s| s.to_string()));
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    runtime(&refs, stdin, env)
}

fn chat_reply(message: Value, finish: &str, completion_tokens: u64) -> Reply {
    Reply::json(json!({
        "id": "r", "object": "chat.completion", "created": 0, "model": "brain-model",
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
        "usage": { "prompt_tokens": 50, "completion_tokens": completion_tokens, "total_tokens": 50 + completion_tokens },
    }))
}

fn answer(text: &str) -> Reply {
    chat_reply(json!({ "role": "assistant", "content": text }), "stop", 3)
}

fn tool_call(content: Value) -> Reply {
    chat_reply(
        json!({
            "role": "assistant", "content": content,
            "tool_calls": [{ "id": "call_1", "type": "function",
                "function": { "name": "read", "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":0}" } }],
        }),
        "tool_calls",
        20,
    )
}

fn server_error() -> Reply {
    Reply { status: 500, content_type: "application/json", body: r#"{"error":"boom"}"#.into() }
}

/// Runs where the process exits before any model call: the base URL is a
/// dead port, so a test that wrongly reaches the network fails differently.
fn preflight(extra: &[&str]) -> Run {
    let mut args = vec!["run"];
    args.extend_from_slice(extra);
    runtime(&args, None, &[])
}

fn assert_exit2_with(run: &Run, needle: &str) {
    assert_eq!(run.code, Some(2), "expected exit 2 for {needle:?}: {run:?}");
    assert!(run.stderr.contains(needle), "stderr must contain {needle:?}: {run:?}");
}

// ─── B. argument parsing ────────────────────────────────────────────────

/// Every value-taking flag, given as the LAST argument with no value, exits 2
/// before any model call with this exact message.
#[test]
fn b_every_value_flag_without_a_value_exits_2_with_its_message() {
    let cases: &[(&str, &str)] = &[
        ("--model", "--model requires a value"),
        ("--prompt", "--prompt requires a value"),
        ("--prompt-file", "--prompt-file requires a value"),
        ("--system", "--system requires a value"),
        ("--response-schema", "--response-schema requires a value"),
        ("--base-url", "--base-url requires a value"),
        ("--chat-url", "--chat-url requires a value"),
        ("--dialect", "--dialect requires a value"),
        ("--resume", "--resume requires a value"),
        ("--role-id", "--role-id requires a value"),
        ("--execution-id", "--execution-id requires a value"),
        ("--allowed-tools", "--allowed-tools requires a comma-separated list"),
        ("--compact-threshold-tokens", "--compact-threshold-tokens requires a value"),
        ("--compactor-model", "--compactor-model requires a value"),
        ("--compact-threshold-ratio", "--compact-threshold-ratio requires a value"),
        ("--compactor-context-window", "--compactor-context-window requires a value"),
        ("--context-window", "--context-window requires a value"),
        ("--compact-strategy", "--compact-strategy requires a value (`narrative` or `structured-slot`)"),
        ("--bail-after-compactions", "--bail-after-compactions requires a value"),
        ("--compactor-custom-instructions", "--compactor-custom-instructions requires a value"),
        ("--max-turns", "--max-turns requires a value"),
        ("--reasoning-checkpoint-interval", "--reasoning-checkpoint-interval requires a value"),
        ("--reasoning-checkpoint-interval-source", "--reasoning-checkpoint-interval-source requires a value"),
        ("--generation-checkpoint-interval", "--generation-checkpoint-interval requires a value"),
        ("--generation-checkpoint-interval-source", "--generation-checkpoint-interval-source requires a value"),
        ("--max-stall-recoveries", "--max-stall-recoveries requires a value"),
        ("--max-tokens-per-call", "--max-tokens-per-call requires a value"),
        ("--max-tokens-per-call-source", "--max-tokens-per-call-source requires a value"),
        ("--max-tokens", "--max-tokens requires a value"),
        ("--feedback-templates-json", "--feedback-templates-json requires a JSON-string value"),
        ("--compactor-chat-url", "--compactor-chat-url requires a value"),
        ("--compactor-dialect", "--compactor-dialect requires a value"),
    ];
    for (flag, message) in cases {
        // `--prompt-file` after `--prompt` trips the mutual-exclusion check
        // first, so that one case goes without `--prompt`.
        let run = if *flag == "--prompt-file" {
            preflight(&["--model", "m", flag])
        } else {
            preflight(&["--model", "m", "--prompt", "p", flag])
        };
        assert_exit2_with(&run, message);
        assert!(run.stdout.is_empty(), "{flag}: nothing on stdout before a parse refusal: {run:?}");
    }
}

/// Every numeric flag refuses a non-number with its own message, exit 2.
#[test]
fn b_every_numeric_flag_refuses_a_non_number() {
    let cases: &[(&str, &str, &str)] = &[
        ("--compact-threshold-tokens", "x", "--compact-threshold-tokens requires a positive integer (got: x)"),
        ("--compact-threshold-ratio", "x", "--compact-threshold-ratio requires a fraction in 0.1-0.9 (got: x)"),
        ("--compactor-context-window", "x", "--compactor-context-window requires a positive integer (got: x)"),
        ("--context-window", "x", "--context-window requires a positive integer (got: x)"),
        ("--bail-after-compactions", "x", "--bail-after-compactions requires a positive integer (got: x)"),
        ("--max-turns", "x", "--max-turns requires a positive integer (got: x)"),
        ("--reasoning-checkpoint-interval", "x", "--reasoning-checkpoint-interval requires a positive integer (got: x)"),
        ("--generation-checkpoint-interval", "x", "--generation-checkpoint-interval requires a positive integer (got: x)"),
        ("--max-stall-recoveries", "x", "--max-stall-recoveries requires a non-negative integer (got: x)"),
        ("--max-tokens-per-call", "x", "--max-tokens-per-call requires a positive integer (got: x)"),
        ("--max-tokens", "x", "--max-tokens requires a positive integer (got: x)"),
        ("--compact-strategy", "x", "--compact-strategy: "),
        ("--dialect", "x", "--dialect must be `chat-completions`, `chat-completions-max-tokens` or `messages` (got \"x\")"),
        ("--compactor-dialect", "x", "--compactor-dialect must be `chat-completions`, `chat-completions-max-tokens` or `messages` (got \"x\")"),
    ];
    for (flag, value, message) in cases {
        let run = preflight(&["--model", "m", "--prompt", "p", flag, value]);
        assert_exit2_with(&run, message);
    }
}

/// The range and zero rules, which differ flag by flag.
/// ODDITY: the two check-in intervals refuse 0 ("positive"), while
/// `--max-turns 0`, `--max-tokens 0` and the other "positive integer" flags
/// accept it; only the parse error says "positive".
#[test]
fn b_range_and_zero_rules() {
    for v in ["0.05", "0.95"] {
        let run = preflight(&["--model", "m", "--prompt", "p", "--compact-threshold-ratio", v]);
        assert_exit2_with(&run, &format!("--compact-threshold-ratio must be in range 0.1-0.9 (got: {v})"));
    }
    for flag in ["--reasoning-checkpoint-interval", "--generation-checkpoint-interval"] {
        let run = preflight(&["--model", "m", "--prompt", "p", flag, "0"]);
        assert_exit2_with(&run, &format!("{flag} requires a positive integer (got: 0)"));
    }
    // Accepted: the run gets past parsing to the compaction-input check
    // (no --context-window / --compact-threshold-tokens given).
    for flag in ["--max-turns", "--max-tokens", "--max-tokens-per-call", "--bail-after-compactions", "--max-stall-recoveries"] {
        let run = preflight(&["--model", "m", "--prompt", "p", flag, "0"]);
        assert_eq!(run.code, Some(2), "{flag} 0: {run:?}");
        assert!(run.stderr.contains("error: "), "{flag} 0 is accepted by the parser: {run:?}");
        assert!(!run.stderr.contains(flag), "{flag} 0 is accepted by the parser: {run:?}");
    }
    for v in ["0.1", "0.9"] {
        let run = preflight(&["--model", "m", "--prompt", "p", "--compact-threshold-ratio", v]);
        assert!(!run.stderr.contains("--compact-threshold-ratio must be in range"), "{v} is in range: {run:?}");
        assert!(run.stderr.contains("error: --compact-threshold-ratio was supplied without --context-window"), "{run:?}");
    }
}

#[test]
fn b_an_unknown_flag_exits_2() {
    let run = preflight(&["--model", "m", "--prompt", "p", "--nope"]);
    assert_exit2_with(&run, "unknown flag: --nope");
    // A positional value is an unknown flag too.
    let run = preflight(&["--model", "m", "--prompt", "p", "stray"]);
    assert_exit2_with(&run, "unknown flag: stray");
}

/// `--prompt-file` reads the brief from a file; `--prompt` then
/// `--prompt-file` is refused. ODDITY: the reverse order is not refused,
/// and the later `--prompt` silently replaces the file's contents.
#[test]
fn b_prompt_file_rules() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("brief.txt");
    std::fs::write(&file, "brief from a file\n").unwrap();
    let file = file.to_str().unwrap();

    let run = preflight(&["--model", "m", "--prompt", "p", "--prompt-file", file]);
    assert_exit2_with(&run, "--prompt and --prompt-file are mutually exclusive");

    let missing = tmp.path().join("missing.txt");
    let run = preflight(&["--model", "m", "--prompt-file", missing.to_str().unwrap()]);
    assert_exit2_with(&run, &format!("--prompt-file {}: ", missing.display()));

    let mock = Mock::start(|_, _| answer("ok"));
    let base = |p: &[&str]| {
        let mut a: Vec<String> = dispatch_args(&mock.base_url()).into_iter().filter(|s| s != "--prompt" && s != "hi").collect();
        a.extend(p.iter().map(|s| s.to_string()));
        a
    };
    let run = run_with(base(&["--prompt-file", file]), &[], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let run = run_with(base(&["--prompt-file", file, "--prompt", "later prompt"]), &[], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let users: Vec<Value> = mock.requests().iter().map(|r| r.body["messages"][1].clone()).collect();
    assert_eq!(users[0], json!({ "role": "user", "content": "brief from a file\n" }));
    assert_eq!(users[1], json!({ "role": "user", "content": "later prompt" }));
}

/// A later value replaces an earlier one for every repeatable flag (last
/// wins); pinned on `--model`, which reaches the request body.
#[test]
fn b_a_repeated_flag_last_wins() {
    let mock = Mock::start(|_, _| answer("ok"));
    let run = run_with(dispatch_args(&mock.base_url()), &["--model", "second-model"], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    assert_eq!(mock.requests()[0].body["model"], "second-model");
}

/// ODDITY: an unparsable `--feedback-templates-json` is NOT refused: it warns
/// and the dispatch runs on the default templates.
#[test]
fn b_bad_feedback_templates_json_warns_and_continues() {
    let mock = Mock::start(|_, _| answer("ok"));
    let run = run_with(dispatch_args(&mock.base_url()), &["--feedback-templates-json", "{not json"], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    assert!(run.stderr.contains("--feedback-templates-json failed to parse: "), "{run:?}");
    assert!(run.stderr.contains("Ignoring; runtime will use defaults. (#457 Step 2)"), "{run:?}");
    let run = run_with(dispatch_args(&mock.base_url()), &["--feedback-templates-json", r#"{"k":"v"}"#], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    assert!(!run.stderr.contains("--feedback-templates-json"), "{run:?}");
}

/// The `-source` companions are accepted with any value (an unknown tier is
/// not refused here; it falls back to built-in in the trajectory).
#[test]
fn b_source_companions_accept_any_value() {
    let mock = Mock::start(|_, _| answer("ok"));
    let run = run_with(
        dispatch_args(&mock.base_url()),
        &[
            "--max-tokens-per-call-source", "nonsense",
            "--reasoning-checkpoint-interval-source", "env",
            "--generation-checkpoint-interval-source", "config",
            "--role-id", "coder",
        ],
        None,
        &[],
    );
    assert_eq!(run.code, Some(0), "{run:?}");
}

// ─── C. required arguments ──────────────────────────────────────────────

#[test]
fn c_model_and_prompt_are_required() {
    let run = preflight(&["--prompt", "p"]);
    assert_exit2_with(&run, "--model is required (e.g. darkmux:qwen3.6-35b-a3b-turboquant-mlx)");
    let run = preflight(&["--model", "m"]);
    assert_exit2_with(&run, "--prompt is required");
    // `--model` is checked first.
    let run = preflight(&[]);
    assert_exit2_with(&run, "--model is required");
    assert!(!run.stderr.contains("--prompt is required"), "{run:?}");
}

// ─── D. default system prompt (model-facing text, contract 6) ───────────

/// With no `--system`, the first message is this built-in text, byte for
/// byte. ODDITY: it says "six tools" and lists `echo`, but `echo` is never
/// advertised (the request's `tools` carries five).
#[test]
fn d_the_default_system_prompt_is_byte_locked() {
    const DEFAULT_SYSTEM: &str = "You are running inside the darkmux-runtime container. You have access to six tools:\n\n- `echo`   — echoes its `text` argument back (sanity check)\n- `bash`   — runs a bash command with cwd=/workspace; returns exit + stdout + stderr\n- `read`   — reads from a file inside /workspace; requires offset (1-indexed start line) and limit (max lines; 0 = read entire file from offset to end). Prefer specifying a small limit when you only need a region — pair with `search` to find the right offset. For multiple reads in one turn, emit multiple `read` tool_calls in one assistant response.\n- `write`  — writes a NEW file (or fully replaces one) inside /workspace\n- `edit`   — applies one or more targeted patches in a single call (edits[] array; each entry replaces old_string with new_string against the current file state); prefer this over `write` for modifications, and batch related changes into one call's edits[] array rather than emitting many edit calls\n- `search` — finds a literal substring pattern in a file or directory tree, returning `path:line:content` matches. Use this to LOCATE text (function names, imports, error strings) before reading or editing — much cheaper than reading whole files when you only need to find specific identifiers\n\nAll file paths must resolve inside /workspace. Paths that escape (via .. or symlinks or absolute paths outside /workspace) are rejected by the runtime. Use tools as needed; stop when the task is done.";
    let mock = Mock::start(|_, _| answer("ok"));
    let args: Vec<String> = dispatch_args(&mock.base_url()).into_iter().filter(|s| s != "--system" && s != SYSTEM).collect();
    let run = run_with(args, &[], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let req = &mock.requests()[0];
    assert_eq!(req.body["messages"][0], json!({ "role": "system", "content": DEFAULT_SYSTEM }));
    assert_eq!(req.tool_names(), ["search", "read", "edit", "write", "bash"]);
}

// ─── E. dispatch context + initial messages ─────────────────────────────

/// The first request carries exactly `[system, user]` from `--system` and
/// `--prompt`, and `--execution-id` silences the "no --execution-id" notice.
#[test]
fn e_initial_messages_and_the_execution_id_notice() {
    let mock = Mock::start(|_, _| answer("ok"));
    let run = run_with(dispatch_args(&mock.base_url()), &[], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    assert!(!run.stderr.contains("no --execution-id was passed"), "{run:?}");
    assert_eq!(
        mock.requests()[0].body["messages"],
        json!([{ "role": "system", "content": SYSTEM }, { "role": "user", "content": "hi" }])
    );
    let args: Vec<String> = dispatch_args(&mock.base_url()).into_iter().filter(|s| s != "--execution-id" && s != "exec-1").collect();
    let run = run_with(args, &[], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    assert!(
        run.stderr.contains("[darkmux-runtime] no --execution-id was passed: finding keys cannot be grounded on this dispatch"),
        "{run:?}"
    );
}

// ─── F + I. resume: load and validate ───────────────────────────────────

fn write_checkpoint(dir: &std::path::Path, system: &str, turns: u32, pending: Value) -> String {
    let msg = |role: &str, content: &str| {
        json!({ "role": role, "content": content, "tool_calls": null, "tool_call_id": null, "name": null, "reasoning_content": null })
    };
    let body = json!({
        "schema_version": 3, "role_id": "coder",
        "messages": [msg("system", system), msg("user", "the checkpointed brief"), msg("assistant", "earlier work")],
        "turns": turns, "total_prompt_tokens": 10, "total_completion_tokens": 5, "compactions": 0,
        "rest_ms": 0, "rests": 0, "pending_hand_back": null, "pending_tool_calls": pending,
        "pending_tool_calls_seq_base": 0, "written_at_unix_ms": 0,
    });
    let path = dir.join(format!("checkpoint-{turns}.json"));
    std::fs::write(&path, serde_json::to_vec(&body).unwrap()).unwrap();
    path.to_str().unwrap().to_string()
}

/// A resumed dispatch sends the checkpoint's history, not `--prompt`, and
/// its envelope names the path and the checkpoint's own turn index.
#[test]
fn f_a_resume_sends_the_checkpoint_history_and_names_it_in_the_envelope() {
    let tmp = tempfile::tempdir().unwrap();
    let path = write_checkpoint(tmp.path(), SYSTEM, 7, Value::Null);
    let mock = Mock::start(|_, _| answer("resumed answer"));
    let run = run_with(dispatch_args(&mock.base_url()), &["--json", "--resume", &path], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let env = run.envelope();
    assert_eq!(env["resumed_from"], json!({ "path": path, "turn_index": 7 }));
    let msgs = &mock.requests()[0].body["messages"];
    assert_eq!(msgs[1]["content"], "the checkpointed brief", "{msgs}");
    assert_eq!(msgs[2]["content"], "earlier work", "{msgs}");
}

/// `DARKMUX_RESUME_CHECKPOINT` resumes like `--resume`, and the flag wins
/// over the env var when both are set.
#[test]
fn f_the_resume_env_var_and_flag_precedence() {
    let tmp = tempfile::tempdir().unwrap();
    let from_env = write_checkpoint(tmp.path(), SYSTEM, 3, Value::Null);
    let from_flag = write_checkpoint(tmp.path(), SYSTEM, 9, Value::Null);
    let mock = Mock::start(|_, _| answer("ok"));
    let run = run_with(dispatch_args(&mock.base_url()), &["--json"], None, &[("DARKMUX_RESUME_CHECKPOINT", &from_env)]);
    assert_eq!(run.code, Some(0), "{run:?}");
    assert_eq!(run.envelope()["resumed_from"], json!({ "path": from_env, "turn_index": 3 }));
    let run = run_with(
        dispatch_args(&mock.base_url()),
        &["--json", "--resume", &from_flag],
        None,
        &[("DARKMUX_RESUME_CHECKPOINT", "/nonexistent/checkpoint.json")],
    );
    assert_eq!(run.code, Some(0), "{run:?}");
    assert_eq!(run.envelope()["resumed_from"], json!({ "path": from_flag, "turn_index": 9 }));
}

/// A missing checkpoint exits 2 naming the path, before any model call and
/// before the status line.
#[test]
fn f_a_missing_checkpoint_exits_2_before_any_call() {
    let mock = Mock::start(|_, _| answer("ok"));
    let run = run_with(dispatch_args(&mock.base_url()), &["--resume", "/nonexistent/cp.json"], None, &[]);
    assert_exit2_with(&run, "--resume /nonexistent/cp.json: ");
    assert!(run.stdout.is_empty(), "{run:?}");
    assert!(mock.requests().is_empty());
}

/// The resume guard checks pending tool calls against the ADVERTISED tools
/// (after `--allowed-tools`), and the system message against `--system`.
#[test]
fn i_a_resume_is_validated_against_the_advertised_tools_and_system() {
    let tmp = tempfile::tempdir().unwrap();
    let pending = json!([{ "id": "c1", "type": "function", "function": { "name": "bash", "arguments": "{\"command\":\"true\"}" } }]);
    let path = write_checkpoint(tmp.path(), SYSTEM, 2, pending);
    let mock = Mock::start(|_, _| answer("ok"));
    let run = run_with(dispatch_args(&mock.base_url()), &["--resume", &path, "--allowed-tools", "read"], None, &[]);
    assert_exit2_with(&run, &format!("--resume {path}: RESUME CHECKPOINT REFUSED — pending tool call `bash`"));
    assert!(run.stdout.is_empty(), "the refusal comes before the status line: {run:?}");
    assert!(mock.requests().is_empty());

    let other = write_checkpoint(tmp.path(), "a different system prompt", 4, Value::Null);
    let run = run_with(dispatch_args(&mock.base_url()), &["--resume", &other], None, &[]);
    assert_exit2_with(&run, "RESUME CHECKPOINT REFUSED — the checkpoint's system message does not byte-match");
    assert!(mock.requests().is_empty());
}

// ─── G. stdin auth + client construction, per dialect ───────────────────

/// Default (LM Studio, `--base-url`): POST `<base>/chat/completions`, the
/// cap as `max_tokens`, a `temperature`, no auth header.
#[test]
fn g_lm_studio_default_request_shape() {
    let mock = Mock::start(|_, _| answer("ok"));
    let run = run_with(dispatch_args(&mock.base_url()), &[], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let req = &mock.requests()[0];
    assert_eq!(req.target, "/v1/chat/completions");
    assert_eq!(req.keys(), ["max_tokens", "messages", "model", "temperature", "tool_choice", "tools"]);
    assert_eq!(req.body["model"], "brain-model");
    assert_eq!(req.body["tool_choice"], "auto");
    assert_eq!(req.body["max_tokens"], 4000);
    assert_eq!(req.header("authorization"), None);
    assert_eq!(req.header("anthropic-version"), None);
}

/// `--chat-url` with no `--dialect`: the URL is used verbatim (query string
/// kept), and the body takes the hosted shape (`max_completion_tokens`, no
/// `temperature`). The brain's stdin header rides every request.
#[test]
fn g_chat_url_without_a_dialect_uses_the_hosted_shape_and_the_stdin_header() {
    let mock = Mock::start(|_, _| answer("ok"));
    let run = run_with(
        dispatch_args("http://127.0.0.1:1/v1"),
        &["--chat-url", &mock.url("/deploy/chat?api-version=1"), "--auth-header-stdin"],
        Some(r#"{"header":"api-key","value":"brain-secret"}"#),
        &[],
    );
    assert_eq!(run.code, Some(0), "{run:?}");
    let req = &mock.requests()[0];
    assert_eq!(req.target, "/deploy/chat?api-version=1");
    assert_eq!(req.keys(), ["max_completion_tokens", "messages", "model", "tool_choice", "tools"]);
    assert_eq!(req.body["max_completion_tokens"], 4000);
    assert_eq!(req.header("api-key"), Some("brain-secret"));
}

/// `--dialect` decides the cap field over the `--chat-url` rule, both ways.
#[test]
fn g_a_declared_dialect_decides_the_body_shape() {
    let mock = Mock::start(|_, _| answer("ok"));
    // chat-completions-max-tokens on a --chat-url: keeps max_tokens + temperature.
    let run = run_with(
        dispatch_args("http://127.0.0.1:1/v1"),
        &["--chat-url", &mock.url("/x/chat/completions"), "--dialect", "chat-completions-max-tokens"],
        None,
        &[],
    );
    assert_eq!(run.code, Some(0), "{run:?}");
    // chat-completions with no --chat-url: LM Studio's URL, hosted shape.
    let run = run_with(dispatch_args(&mock.base_url()), &["--dialect", "chat-completions"], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let reqs = mock.requests();
    assert_eq!(reqs[0].target, "/x/chat/completions");
    assert_eq!(reqs[0].keys(), ["max_tokens", "messages", "model", "temperature", "tool_choice", "tools"]);
    assert_eq!(reqs[1].target, "/v1/chat/completions");
    assert_eq!(reqs[1].keys(), ["max_completion_tokens", "messages", "model", "tool_choice", "tools"]);
}

/// `--dialect messages`: the Messages body (system lifted out of the
/// message list, `input_schema` tools) and the `anthropic-version` header,
/// with the reply parsed in the Messages shape.
#[test]
fn g_the_messages_dialect_request_and_reply() {
    let mock = Mock::start(|_, _| {
        Reply::json(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "brain-model",
            "content": [{ "type": "text", "text": "messages answer" }],
            "stop_reason": "end_turn", "usage": { "input_tokens": 50, "output_tokens": 3 },
        }))
    });
    let run = run_with(
        dispatch_args("http://127.0.0.1:1/v1"),
        &["--chat-url", &mock.url("/v1/messages"), "--dialect", "messages", "--auth-header-stdin", "--json"],
        Some(r#"{"header":"x-api-key","value":"k"}"#),
        &[],
    );
    assert_eq!(run.code, Some(0), "{run:?}");
    assert_eq!(run.envelope()["final_assistant"], "messages answer");
    let req = &mock.requests()[0];
    assert_eq!(req.target, "/v1/messages");
    assert_eq!(req.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(req.header("x-api-key"), Some("k"));
    assert_eq!(req.keys(), ["cache_control", "max_tokens", "messages", "model", "system", "tool_choice", "tools"]);
    assert_eq!(req.tool_names(), ["search", "read", "edit", "write", "bash"]);
    assert_eq!(req.body["messages"][0]["role"], "user");
}

/// A broken auth pipe is fatal at startup (exit 2, no request).
#[test]
fn g_a_bad_stdin_auth_blob_exits_2_before_any_call() {
    let mock = Mock::start(|_, _| answer("ok"));
    let run = run_with(dispatch_args(&mock.base_url()), &["--auth-header-stdin"], Some("not json"), &[]);
    assert_exit2_with(&run, "--auth-header-stdin: invalid JSON: ");
    let run = run_with(dispatch_args(&mock.base_url()), &["--auth-header-stdin"], Some(r#"{"header":"h"}"#), &[]);
    assert_exit2_with(&run, r#"--auth-header-stdin: expected {"header": "...", "value": "..."}"#);
    assert!(run.stdout.is_empty(), "{run:?}");
    assert!(mock.requests().is_empty());
}

/// Without `--auth-header-stdin`, stdin is never read and no header is sent,
/// even when a blob is piped.
#[test]
fn g_stdin_is_ignored_without_the_flag() {
    let mock = Mock::start(|_, _| answer("ok"));
    let run = run_with(dispatch_args(&mock.base_url()), &[], Some(r#"{"header":"api-key","value":"v"}"#), &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    assert_eq!(mock.requests()[0].header("api-key"), None);
}

// ─── H. tool catalog and `--allowed-tools` ──────────────────────────────

/// Catalog ∩ allow in catalog order, then allowed names outside the
/// catalog (`create_finding`) in allow order; blanks dropped, unknown names
/// warned about and ignored.
#[test]
fn h_allowed_tools_filters_and_extends_the_catalog() {
    let mock = Mock::start(|_, _| answer("ok"));
    let run = run_with(dispatch_args(&mock.base_url()), &["--allowed-tools", "bash, read,,create_finding,nope"], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    assert_eq!(mock.requests()[0].tool_names(), ["read", "bash", "create_finding"]);
    assert!(run.stderr.contains("--allowed-tools names `nope`, which is not a tool this runtime knows; ignoring it"), "{run:?}");
    assert!(!run.stderr.contains("names ``"), "a blank entry is dropped, not warned about: {run:?}");
}

/// An empty list advertises no tools at all.
#[test]
fn h_an_empty_allow_list_advertises_nothing() {
    let mock = Mock::start(|_, _| answer("ok"));
    let run = run_with(dispatch_args(&mock.base_url()), &["--allowed-tools", ""], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let req = &mock.requests()[0];
    assert!(req.tool_names().is_empty(), "{:?}", req.body.get("tools"));
}

// ─── J. status line ─────────────────────────────────────────────────────

/// Human mode: the status line and a blank line open stdout. JSON mode: it
/// goes to stderr and stdout is the envelope alone.
#[test]
fn j_the_status_line_moves_to_stderr_in_json_mode() {
    let mock = Mock::start(|_, _| answer("ok"));
    let run = run_with(dispatch_args(&mock.base_url()), &[], None, &[]);
    assert!(run.stdout.starts_with("dispatching to model: brain-model\n\n--- final assistant message ---\n"), "{run:?}");
    assert!(!run.stderr.contains("dispatching to model"), "{run:?}");
    let run = run_with(dispatch_args(&mock.base_url()), &["--json"], None, &[]);
    assert!(run.stderr.contains("dispatching to model: brain-model\n"), "{run:?}");
    run.envelope();
}

// ─── K. compaction-input validation ─────────────────────────────────────

/// Neither `--context-window` nor `--compact-threshold-tokens`: exit 2 with
/// the #482 message, before the trajectory is opened and before any call.
/// ODDITY: the status line has already been printed ("dispatching to
/// model") when the refusal comes.
#[test]
fn k_no_window_and_no_threshold_is_refused_before_the_trajectory_opens() {
    let mock = Mock::start(|_, _| answer("ok"));
    let args: Vec<String> = dispatch_args(&mock.base_url()).into_iter().filter(|s| s != "--context-window" && s != "32000").collect();
    let run = run_with(args, &[], None, &[]);
    assert_exit2_with(&run, "error: ");
    assert!(!run.stderr.contains("trajectory"), "{run:?}");
    assert_eq!(run.stdout, "dispatching to model: brain-model\n\n");
    assert!(mock.requests().is_empty());
    // A threshold alone is enough.
    let args: Vec<String> = dispatch_args(&mock.base_url()).into_iter().filter(|s| s != "--context-window" && s != "32000").collect();
    let run = run_with(args, &["--compact-threshold-tokens", "30000"], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
}

// ─── L. trajectory open ─────────────────────────────────────────────────

/// The trajectory is opened after the preflight checks; on a host where the
/// fixed out-dir cannot be created the run continues without it.
#[test]
fn l_the_trajectory_opens_after_preflight_and_degrades_without_an_out_dir() {
    let mock = Mock::start(|_, _| answer("ok"));
    let run = run_with(dispatch_args(&mock.base_url()), &[], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    assert!(
        run.stderr.contains("darkmux-runtime: trajectory → /darkmux-out/.darkmux-runtime/trajectory.jsonl")
            || run.stderr.contains("darkmux-runtime: trajectory recording disabled ("),
        "{run:?}"
    );
}

// ─── N. response_format ─────────────────────────────────────────────────

/// A valid `--response-schema` becomes a strict `json_schema`
/// `response_format` on every brain request. ODDITY: an unparsable schema is
/// dropped silently (no warning, free-form output).
#[test]
fn n_response_schema_becomes_a_strict_response_format() {
    let mock = Mock::start(|_, _| answer("{}"));
    let schema = r#"{"type":"object","properties":{"a":{"type":"string"}},"required":["a"],"additionalProperties":false}"#;
    let run = run_with(dispatch_args(&mock.base_url()), &["--response-schema", schema], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let run2 = run_with(dispatch_args(&mock.base_url()), &["--response-schema", "{not json"], None, &[]);
    assert_eq!(run2.code, Some(0), "{run2:?}");
    assert!(!run2.stderr.contains("response-schema"), "{run2:?}");
    let reqs = mock.requests();
    assert_eq!(
        reqs[0].body["response_format"],
        json!({ "type": "json_schema", "json_schema": { "name": "role_output", "strict": true, "schema": serde_json::from_str::<Value>(schema).unwrap() } })
    );
    assert!(reqs[1].body.get("response_format").is_none(), "{}", reqs[1].body);
}

// ─── loop knobs: each flag reaches the loop ─────────────────────────────

/// `--max-tokens-per-call` and `--generation-checkpoint-interval` reach the
/// wire cap (`max_tokens`, built-in 4000 on a first call).
#[test]
fn p_the_per_call_caps_reach_the_wire() {
    let mock = Mock::start(|_, _| answer("ok"));
    let run = run_with(dispatch_args(&mock.base_url()), &["--max-tokens-per-call", "300"], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let run = run_with(dispatch_args(&mock.base_url()), &["--generation-checkpoint-interval", "700"], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let caps: Vec<Value> = mock.requests().iter().map(|r| r.body["max_tokens"].clone()).collect();
    assert_eq!(caps, [json!(300), json!(700)]);
}

/// Streaming is the default: the body carries `stream: true` and
/// `stream_options.include_usage`; `--no-stream` drops both.
#[test]
fn p_streaming_is_the_default() {
    let mock = Mock::start(|_, _| {
        let chunk = |delta: Value, finish: Value, usage: Value| {
            format!(
                "data: {}\n\n",
                json!({ "id": "s", "object": "chat.completion.chunk", "created": 0, "model": "brain-model",
                    "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }], "usage": usage })
            )
        };
        let body = chunk(json!({ "role": "assistant", "content": "streamed answer" }), Value::Null, Value::Null)
            + &chunk(json!({}), json!("stop"), json!({ "prompt_tokens": 50, "completion_tokens": 3, "total_tokens": 53 }))
            + "data: [DONE]\n\n";
        Reply { status: 200, content_type: "text/event-stream", body }
    });
    let args: Vec<String> = dispatch_args(&mock.base_url()).into_iter().filter(|s| s != "--no-stream").collect();
    let run = run_with(args, &["--json"], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    assert_eq!(run.envelope()["final_assistant"], "streamed answer");
    let req = &mock.requests()[0];
    assert_eq!(req.body["stream"], true);
    assert_eq!(req.body["stream_options"], json!({ "include_usage": true }));
}

// ─── P/Q/R/S. the loop's outcome → result, output, exit code ────────────

/// Stop: exit 0. Human stdout is the final message then the run summary;
/// JSON stdout is the envelope.
#[test]
fn q_stop_human_and_json() {
    let mock = Mock::start(|_, _| answer("all done"));
    let run = run_with(dispatch_args(&mock.base_url()), &[], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    assert!(
        run.stdout.contains("--- final assistant message ---\nall done\n\n--- run ---\nresult:            stop\nturns:             "),
        "{run:?}"
    );
    assert!(run.stdout.contains("\nwall:              "), "{run:?}");
    let run = run_with(dispatch_args(&mock.base_url()), &["--json"], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    assert_eq!(
        run.envelope(),
        json!({
            "result": "stop", "final_assistant": "all done",
            "trajectory_path": "/darkmux-out/.darkmux-runtime/trajectory.jsonl",
            "failed_tool_invocations": [],
        })
    );
}

/// Max turns: `result: max_turns` and exit 0 (ODDITY: a run that never
/// answered exits 0, unlike an escalation). With no answer, the final
/// assistant falls back to the last assistant message's content.
#[test]
fn q_max_turns_exits_0_with_the_last_assistant_content() {
    let mock = Mock::start(|_, _| tool_call(json!("looking at the file")));
    let run = run_with(dispatch_args(&mock.base_url()), &["--max-turns", "1", "--json"], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let env = run.envelope();
    assert_eq!(env["result"], "max_turns", "{env}");
    assert_eq!(env["final_assistant"], "looking at the file", "{env}");
    assert_eq!(mock.requests().len(), 1);
}

/// ODDITY: when the LAST assistant message has no content, the fallback is
/// `<empty>`; it does not look further back for one that had content.
#[test]
fn q_max_turns_with_no_content_reads_empty() {
    let mock = Mock::start(|n, _| if n == 0 { tool_call(json!("first look")) } else { tool_call(Value::Null) });
    let run = run_with(dispatch_args(&mock.base_url()), &["--max-turns", "2", "--json"], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let env = run.envelope();
    assert_eq!(env["result"], "max_turns", "{env}");
    assert_eq!(env["final_assistant"], "<empty>", "{env}");
}

/// ODDITY: an empty answer (`content: ""`, stop) reports `final_assistant:
/// ""`, not `<empty>`: the fallback finds the same empty message and takes
/// its content as found.
#[test]
fn q_an_empty_answer_reports_an_empty_string() {
    let mock = Mock::start(|_, _| answer(""));
    let run = run_with(dispatch_args(&mock.base_url()), &["--json"], None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let env = run.envelope();
    assert_eq!((env["result"].as_str(), env["final_assistant"].as_str()), (Some("stop"), Some("")), "{env}");
}

/// Loop error: `dispatch failed: …` on stderr, exit 1; JSON gets the error
/// envelope (null final_assistant, no failed_tool_invocations key), human
/// mode gets a failure block.
#[test]
fn r_a_loop_error_exits_1_with_the_error_envelope() {
    let mock = Mock::start(|_, _| server_error());
    let run = run_with(dispatch_args(&mock.base_url()), &["--json"], None, &[]);
    assert_eq!(run.code, Some(1), "{run:?}");
    assert!(run.stderr.contains("dispatch failed: "), "{run:?}");
    assert_eq!(
        run.envelope(),
        json!({ "result": "error", "final_assistant": null, "trajectory_path": "/darkmux-out/.darkmux-runtime/trajectory.jsonl" })
    );
    let run = run_with(dispatch_args(&mock.base_url()), &[], None, &[]);
    assert_eq!(run.code, Some(1), "{run:?}");
    assert!(
        run.stdout.contains("--- dispatch failed ---\nmodel:  brain-model\n--- run ---\nresult:            error\n"),
        "{run:?}"
    );
    assert!(!run.stdout.contains("--- final assistant message ---"), "{run:?}");
}

/// Escalation: the envelope's `result` is the escalation reason, the answer
/// so far is still reported, and the exit code is 1.
#[test]
fn s_an_escalation_exits_1_with_its_reason() {
    // A cumulative cap below one reply's completion tokens.
    let mock = Mock::start(|_, _| tool_call(json!("working")));
    let run = run_with(dispatch_args(&mock.base_url()), &["--max-tokens", "5", "--json"], None, &[]);
    assert_eq!(run.code, Some(1), "{run:?}");
    let env = run.envelope();
    assert_eq!(env["result"], "escalation_cumulative_tokens_exceeded", "{env}");
    assert!(env["final_assistant"].is_string(), "{env}");
    assert!(env["failed_tool_invocations"].is_array(), "{env}");
    // Human mode prints the answer block and summary, then exits 1.
    let run = run_with(dispatch_args(&mock.base_url()), &["--max-tokens", "5"], None, &[]);
    assert_eq!(run.code, Some(1), "{run:?}");
    assert!(run.stdout.contains("result:            escalation_cumulative_tokens_exceeded\n"), "{run:?}");
    assert!(run.stdout.contains("--- final assistant message ---"), "{run:?}");
}

/// `--max-stall-recoveries` reaches the loop: empty replies escalate after
/// exactly N+1 requests.
#[test]
fn p_max_stall_recoveries_reaches_the_loop() {
    let empty = || chat_reply(json!({ "role": "assistant", "content": "", "tool_calls": [] }), "tool_calls", 3);
    let mock = Mock::start(move |_, _| empty());
    let run = run_with(dispatch_args(&mock.base_url()), &["--max-stall-recoveries", "0", "--json"], None, &[]);
    let n0 = mock.requests().len();
    let env0 = run.envelope();
    let run = run_with(dispatch_args(&mock.base_url()), &["--max-stall-recoveries", "1", "--json"], None, &[]);
    let n1 = mock.requests().len() - n0;
    let env1 = run.envelope();
    assert_eq!((env0["result"].as_str(), env1["result"].as_str()), (Some("escalation_empty_tool_calls"), Some("escalation_empty_tool_calls")), "{env0} {env1}");
    assert_eq!((n0, n1), (1, 2));
    assert_eq!(run.code, Some(1));
}

// ─── M. compaction config: each flag reaches the compactor ─────────────

const SUMMARY: &str = "Summary: the assistant read the workspace file several times and inspected what it \
                       returned. Nothing was decided and nothing was modified yet. The next step is to keep \
                       reading the file and then act on what it contains.";

/// A mock that answers the brain (`brain-model`) with a tool call reporting
/// a 5000-token prompt, and the compactor (`util-model`) with a summary.
fn compaction_mock() -> Mock {
    Mock::start(|_, req| {
        if req.body["model"] == "util-model" {
            chat_reply(json!({ "role": "assistant", "content": SUMMARY }), "stop", 40)
        } else {
            Reply::json(json!({
                "id": "b", "object": "chat.completion", "created": 0, "model": "brain-model",
                "choices": [{ "index": 0, "message": { "role": "assistant", "content": null, "tool_calls": [{ "id": "call_1", "type": "function",
                    "function": { "name": "read", "arguments": "{\"path\":\"/workspace/x.txt\",\"offset\":1,\"limit\":0}" } }] }, "finish_reason": "tool_calls" }],
                "usage": { "prompt_tokens": 5000, "completion_tokens": 20, "total_tokens": 5020 },
            }))
        }
    })
}

/// The base dispatch for compaction pins: no `--context-window` unless
/// `extra` adds one, the compactor bound, six turns.
fn compaction_args(base_url: &str) -> Vec<String> {
    dispatch_args(base_url)
        .into_iter()
        .filter(|s| s != "--context-window" && s != "32000")
        .chain(["--compactor-model", "util-model", "--max-turns", "6"].iter().map(|s| s.to_string()))
        .collect()
}

fn compactor_requests(mock: &Mock) -> Vec<Recorded> {
    mock.requests().into_iter().filter(|r| r.body["model"] == "util-model").collect()
}

/// (#1187) On a `--chat-url` brain, the compactor still goes to LM Studio at
/// `--base-url`, without the brain's stdin header.
#[test]
fn m_the_compactor_uses_base_url_never_the_brains_chat_url_or_auth() {
    let lm_studio = compaction_mock();
    let hosted = compaction_mock();
    let run = run_with(
        compaction_args(&lm_studio.base_url()),
        &["--compact-threshold-tokens", "1000", "--chat-url", &hosted.url("/v1/chat/completions"), "--auth-header-stdin"],
        Some(r#"{"header":"api-key","value":"brain-secret"}"#),
        &[],
    );
    assert_eq!(run.code, Some(0), "{run:?}");
    let compactions = compactor_requests(&lm_studio);
    assert!(!compactions.is_empty(), "the compactor reached LM Studio: {run:?}");
    assert!(compactions.iter().all(|r| r.header("api-key").is_none() && r.target == "/v1/chat/completions"));
    assert!(compactor_requests(&hosted).is_empty(), "no compaction went to the brain's endpoint");
    assert!(hosted.requests().iter().all(|r| r.header("api-key") == Some("brain-secret")));
    assert!(lm_studio.requests().iter().all(|r| r.body["model"] == "util-model"), "the brain never went to LM Studio");
}

/// `--compact-strategy structured-slot` adds the slot response_format and
/// carries `--compactor-custom-instructions` to the compactor. ODDITY: under
/// the default narrative strategy the custom instructions are dropped
/// silently (no warning, not in the request).
#[test]
fn m_custom_instructions_and_strategy_reach_the_compactor() {
    let custom = ["--compact-threshold-tokens", "1000", "--compactor-custom-instructions", "KEEP-THE-FILE-NAMES-7731"];
    let mock = compaction_mock();
    let run = run_with(compaction_args(&mock.base_url()), &custom, None, &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let narrative = compactor_requests(&mock);
    assert!(!narrative.is_empty(), "{run:?}");
    assert!(!narrative[0].body.to_string().contains("KEEP-THE-FILE-NAMES-7731"), "{}", narrative[0].body);
    assert!(narrative[0].body.get("response_format").is_none(), "narrative is free-form: {}", narrative[0].body);
    assert!(!run.stderr.contains("custom"), "{run:?}");

    let mock = compaction_mock();
    let mut args = custom.to_vec();
    args.extend(["--compact-strategy", "structured-slot"]);
    let run = run_with(compaction_args(&mock.base_url()), &args, None, &[]);
    let structured = compactor_requests(&mock);
    assert!(!structured.is_empty(), "{run:?}");
    assert!(structured[0].body.to_string().contains("KEEP-THE-FILE-NAMES-7731"), "{}", structured[0].body);
    assert_eq!(
        structured[0].body.pointer("/response_format/json_schema/name"),
        Some(&json!("StructuredCompactionOutput")),
        "{}",
        structured[0].body
    );
}

/// The trigger: an absolute threshold below the reported prompt fires; the
/// ratio × window formula fires at 0.1 × 10000 and not at 0.9 × 10000.
#[test]
fn m_the_threshold_flags_decide_whether_compaction_fires() {
    let fired = |extra: &[&str]| {
        let mock = compaction_mock();
        let run = run_with(compaction_args(&mock.base_url()), extra, None, &[]);
        assert!(run.code == Some(0) || run.code == Some(1), "{run:?}");
        !compactor_requests(&mock).is_empty()
    };
    assert!(fired(&["--compact-threshold-tokens", "1000"]));
    assert!(!fired(&["--compact-threshold-tokens", "90000"]));
    assert!(fired(&["--context-window", "10000", "--compact-threshold-ratio", "0.1"]));
    assert!(!fired(&["--context-window", "10000", "--compact-threshold-ratio", "0.9"]));
}

/// `--bail-after-compactions 1`: the first compaction escalates, exit 1.
#[test]
fn m_bail_after_compactions_escalates() {
    let mock = compaction_mock();
    let run = run_with(
        compaction_args(&mock.base_url()),
        &["--compact-threshold-tokens", "1000", "--bail-after-compactions", "1", "--json"],
        None,
        &[],
    );
    assert_eq!(run.code, Some(1), "{run:?}");
    assert_eq!(run.envelope()["result"], "escalation_compaction_limit_reached");
}

/// `--compactor-context-window` reaches compaction: a window too small for
/// the compactor's own framing refuses every compaction (#2808), naming the
/// window, and no compactor request is made.
#[test]
fn m_the_compactor_window_reaches_compaction() {
    let mock = compaction_mock();
    let run = run_with(
        compaction_args(&mock.base_url()),
        &["--compact-threshold-tokens", "1000", "--compactor-context-window", "600"],
        None,
        &[],
    );
    assert_eq!(run.code, Some(0), "{run:?}");
    assert!(run.stderr.contains("compaction #1 was refused and SKIPPED"), "{run:?}");
    assert!(run.stderr.contains("the compactor `util-model` loads at 600 tokens"), "{run:?}");
    assert!(compactor_requests(&mock).is_empty());
}

/// Without `--compactor-model`, a configured trigger discloses that
/// compaction is off and no compactor request is ever made.
#[test]
fn m_no_compactor_model_means_no_compaction_and_a_disclosure() {
    let mock = compaction_mock();
    let args: Vec<String> = compaction_args(&mock.base_url()).into_iter().filter(|s| s != "--compactor-model" && s != "util-model").collect();
    let run = run_with(args, &["--compact-threshold-tokens", "1000"], None, &[]);
    assert!(run.stderr.contains("no compactor is configured for this dispatch"), "{run:?}");
    assert!(compactor_requests(&mock).is_empty());
}

/// `--reasoning-checkpoint-interval` reaches the wire: once the model has
/// shown a reasoning region, the next call's cap is the interval (built-in
/// 1000).
#[test]
fn p_the_reasoning_interval_reaches_the_wire() {
    let caps = |extra: &[&str]| {
        let mock = Mock::start(|n, _| {
            if n == 0 {
                chat_reply(
                    json!({ "role": "assistant", "content": null, "reasoning_content": "thinking",
                        "tool_calls": [{ "id": "c", "type": "function", "function": { "name": "read", "arguments": "{\"path\":\"/workspace/x\",\"offset\":1,\"limit\":0}" } }] }),
                    "tool_calls",
                    3,
                )
            } else {
                chat_reply(json!({ "role": "assistant", "content": "done", "reasoning_content": "more" }), "stop", 3)
            }
        });
        let run = run_with(dispatch_args(&mock.base_url()), extra, None, &[]);
        assert_eq!(run.code, Some(0), "{run:?}");
        mock.requests().iter().map(|r| r.body["max_tokens"].as_u64().unwrap()).collect::<Vec<_>>()
    };
    assert_eq!(caps(&[]), [4000, 1000]);
    assert_eq!(caps(&["--reasoning-checkpoint-interval", "50"]), [4000, 50]);
}

/// `--feedback-templates-json` reaches the loop: a repeatedly failing tool
/// triggers the tool-failure-cascade feedback, and the role's template
/// replaces the built-in text in a later request.
#[test]
fn p_feedback_templates_reach_the_model() {
    let injected = |extra: &[&str]| {
        let mock = Mock::start(|_, _| tool_call(Value::Null));
        let run = run_with(dispatch_args(&mock.base_url()), extra, None, &[]);
        assert_eq!(run.code, Some(0), "{run:?}");
        mock.requests().iter().any(|r| r.body["messages"].to_string().contains("CUSTOM-CASCADE-NOTE-4411"))
    };
    let templates = r#"{"tool_failure_cascade":"CUSTOM-CASCADE-NOTE-4411"}"#;
    assert!(injected(&["--max-turns", "6", "--feedback-templates-json", templates]));
    assert!(!injected(&["--max-turns", "6"]));
}

// ─── out-dir records (DARKMUX_RUNTIME_OUT_DIR) ──────────────────────────

/// A dispatch whose out-dir is `dir` (the test seam for `/darkmux-out`).
fn run_in(dir: &std::path::Path, base: Vec<String>, extra: &[&str]) -> Run {
    run_with(base, extra, None, &[("DARKMUX_RUNTIME_OUT_DIR", dir.to_str().unwrap())])
}

fn trajectory_of(dir: &std::path::Path) -> Vec<Value> {
    let path = dir.join(".darkmux-runtime/trajectory.jsonl");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("no trajectory at {}: {e}", path.display()))
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn of_type<'a>(records: &'a [Value], ty: &str) -> Vec<&'a Value> {
    records.iter().filter(|r| r["type"] == ty).collect()
}

/// A turn the model ends with an empty `tool_calls`: the loop drops it and
/// records the recovery with the bound that applied.
fn empty_tool_calls() -> Reply {
    chat_reply(json!({ "role": "assistant", "content": null, "tool_calls": [] }), "tool_calls", 0)
}

/// L: with an out-dir, the run's own bookkeeping lands there: the header,
/// `dispatch.start` (model, the two prompt lengths, the ADVERTISED tools),
/// and a terminal `dispatch.complete` naming the result. The stderr line
/// names the overridden path.
#[test]
fn l_the_out_dir_records_dispatch_start_and_complete() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::start(|_, _| answer("ok"));
    let run = run_in(dir.path(), dispatch_args(&mock.base_url()), &["--allowed-tools", "bash,read"]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let path = dir.path().join(".darkmux-runtime/trajectory.jsonl");
    assert!(run.stderr.contains(&format!("darkmux-runtime: trajectory → {}\n", path.display())), "{run:?}");

    let t = trajectory_of(dir.path());
    assert_eq!(t[0], json!({ "schema_version": "1.0", "type": "trajectory.header" }));
    let start = &t[1];
    assert_eq!(start["type"], "dispatch.start");
    assert_eq!(start["model"], "brain-model");
    assert_eq!(start["system_chars"], SYSTEM.len());
    assert_eq!(start["prompt_chars"], 2);
    assert_eq!(start["tools"], json!(["read", "bash"]));
    let last = t.last().unwrap();
    assert_eq!(last["type"], "dispatch.complete");
    assert_eq!(last["result"], "stop");
    assert_eq!(of_type(&t, "dispatch.complete").len(), 1);
}

/// Q: `dispatch.complete` carries the loop's own result, not a fixed one.
#[test]
fn q_dispatch_complete_names_max_turns() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::start(|_, _| tool_call(Value::Null));
    let run = run_in(dir.path(), dispatch_args(&mock.base_url()), &["--max-turns", "1"]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let t = trajectory_of(dir.path());
    assert_eq!(t.last().unwrap()["type"], "dispatch.complete");
    assert_eq!(t.last().unwrap()["result"], "max_turns");
}

/// R: the `--json` envelope's `trajectory_path` is the out-dir's file, and
/// the human summary reads its counts back from that same file.
#[test]
fn r_envelope_and_summary_read_the_out_dir() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::start(|_, _| answer("ok"));
    let run = run_in(dir.path(), dispatch_args(&mock.base_url()), &["--json"]);
    assert_eq!(run.code, Some(0), "{run:?}");
    let expected = dir.path().join(".darkmux-runtime/trajectory.jsonl");
    assert_eq!(run.envelope()["trajectory_path"], expected.display().to_string());

    let dir = tempfile::tempdir().unwrap();
    let run = run_in(dir.path(), dispatch_args(&mock.base_url()), &[]);
    assert_eq!(run.code, Some(0), "{run:?}");
    assert!(run.stdout.contains("turns:             1\n"), "{run:?}");
    assert!(run.stdout.contains("prompt tokens:     50\n"), "{run:?}");
    assert!(run.stdout.contains("completion tokens: 3\n"), "{run:?}");
}

/// O: each `-source` flag reaches the record of the bound that acted, and an
/// unknown tier reads `built-in`. Generation applies on a non-reasoning
/// model's call; reasoning once the model has reasoned; the per-call cap
/// when it is at or below the generation interval (the check-in is off).
#[test]
fn o_bound_sources_reach_the_recovery_record() {
    let bound = |reasoning: bool, extra: &[&str]| {
        let dir = tempfile::tempdir().unwrap();
        let mock = Mock::start(move |n, _| match n {
            0 if reasoning => chat_reply(
                json!({ "role": "assistant", "content": null, "reasoning_content": "thinking",
                    "tool_calls": [{ "id": "c", "type": "function", "function": { "name": "read", "arguments": "{\"path\":\"/workspace/x\",\"offset\":1,\"limit\":0}" } }] }),
                "tool_calls",
                3,
            ),
            0 => empty_tool_calls(),
            1 if reasoning => empty_tool_calls(),
            _ => answer("ok"),
        });
        let run = run_in(dir.path(), dispatch_args(&mock.base_url()), extra);
        assert_eq!(run.code, Some(0), "{run:?}");
        let t = trajectory_of(dir.path());
        let rec = of_type(&t, "dispatch.empty_tool_calls.recovered");
        assert_eq!(rec.len(), 1, "{t:#?}");
        let b = &rec[0]["bound"];
        (b["kind"].as_str().unwrap().to_string(), b["source"].as_str().unwrap().to_string())
    };
    let sources = [
        "--generation-checkpoint-interval-source", "config",
        "--reasoning-checkpoint-interval-source", "cli",
        "--max-tokens-per-call-source", "env",
    ];
    assert_eq!(bound(false, &sources), ("generation_checkpoint_interval".into(), "config".into()));
    assert_eq!(bound(false, &[]), ("generation_checkpoint_interval".into(), "built-in".into()));
    assert_eq!(bound(true, &sources), ("reasoning_checkpoint_interval".into(), "cli".into()));
    let capped: Vec<&str> = sources.iter().copied().chain(["--max-tokens-per-call", "3000"]).collect();
    assert_eq!(bound(false, &capped), ("max_tokens_per_call".into(), "env".into()));
    assert_eq!(
        bound(false, &["--max-tokens-per-call", "3000", "--max-tokens-per-call-source", "nonsense"]),
        ("max_tokens_per_call".into(), "built-in".into())
    );
}

/// P: `--role-id` is stamped into `checkpoint.json`, written in the out-dir
/// before a workspace-changing tool runs (the resume guard reads it).
#[test]
fn p_role_id_is_stamped_into_the_checkpoint() {
    let role_in_checkpoint = |extra: &[&str]| {
        let dir = tempfile::tempdir().unwrap();
        let mock = Mock::start(|n, _| {
            if n == 0 {
                chat_reply(
                    json!({ "role": "assistant", "content": null,
                        "tool_calls": [{ "id": "c", "type": "function", "function": { "name": "write", "arguments": "{\"path\":\"/workspace/y.txt\",\"content\":\"z\"}" } }] }),
                    "tool_calls",
                    3,
                )
            } else {
                answer("ok")
            }
        });
        let run = run_in(dir.path(), dispatch_args(&mock.base_url()), extra);
        assert_eq!(run.code, Some(0), "{run:?}");
        let ckpt: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("checkpoint.json")).unwrap()).unwrap();
        ckpt["role_id"].as_str().unwrap().to_string()
    };
    assert_eq!(role_in_checkpoint(&["--role-id", "coder"]), "coder");
    assert_eq!(role_in_checkpoint(&[]), "");
}

/// L: a SIGTERM mid-call ends the run with 128 + 15 and closes the
/// trajectory with a terminal `interrupted` record (the handler is
/// installed only on a live recorder).
#[test]
fn l_sigterm_closes_the_trajectory_as_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let mock = Mock::start(|_, _| {
        std::thread::sleep(std::time::Duration::from_secs(30));
        answer("late")
    });
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_darkmux-runtime"));
    for (k, _) in std::env::vars() {
        if k.starts_with("DARKMUX_") {
            cmd.env_remove(k);
        }
    }
    let args = dispatch_args(&mock.base_url());
    let mut child = cmd
        .args(&args)
        .env("DARKMUX_RUNTIME_OUT_DIR", dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Wait for the model call to be in flight: the handler is installed
    // before `dispatch.start`, and the call follows it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while mock.requests().is_empty() {
        assert!(std::time::Instant::now() < deadline, "the model call never started");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // SAFETY: kill(2) on our own child's pid.
    assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) }, 0);
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(143), "{status:?}");
    let t = trajectory_of(dir.path());
    let last = t.last().unwrap();
    assert_eq!(last["type"], "dispatch.complete");
    assert_eq!(last["result"], "interrupted");
}

/// An empty `DARKMUX_RUNTIME_OUT_DIR` is unset, never the current directory:
/// the run falls back to `/darkmux-out` and writes nothing beside itself.
#[test]
fn l_an_empty_out_dir_override_is_ignored() {
    let cwd = tempfile::tempdir().unwrap();
    let mock = Mock::start(|_, _| answer("ok"));
    let args = dispatch_args(&mock.base_url());
    let out = Command::new(env!("CARGO_BIN_EXE_darkmux-runtime"))
        .args(&args)
        .env("DARKMUX_RUNTIME_OUT_DIR", "")
        .current_dir(cwd.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("darkmux-runtime: trajectory → /darkmux-out/.darkmux-runtime/trajectory.jsonl")
            || stderr.contains("darkmux-runtime: trajectory recording disabled ("),
        "{stderr}"
    );
    assert_eq!(std::fs::read_dir(cwd.path()).unwrap().count(), 0);
}
