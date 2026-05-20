//! Internal runtime dispatch path.
//!
//! Routes a `darkmux crew dispatch --runtime internal <role>` invocation
//! to the `darkmux-runtime` docker container instead of openclaw.
//! Per-dispatch container, mounted workspace, structured output collected
//! from stdout.
//!
//! Opt-in via the explicit `--runtime internal` CLI flag while the
//! in-house runtime is being measured against openclaw. Promotion to
//! default is a separate decision tracked in `runtime/README.md`.
//!
//! Deliberately simpler than the openclaw path:
//!
//! - No openclaw pre-flight (it's not involved)
//! - No `--workdir` symlink injection (workspace is a fresh tempdir
//!   per dispatch; the gallery-incident class of bug is structurally
//!   impossible because there's nowhere persistent to leak into)
//! - No sprint-output persistence (later iteration)
//! - No watched-path post-dispatch echo (same)
//! - No model pin enforcement (probes whatever LMStudio currently has loaded)
//!
//! See `runtime/` for the container image this dispatches to.

use crate::crew::dispatch::DispatchResult;
use crate::crew::dispatch::DispatchOpts;
use crate::crew::loader::{load_role_prompt, load_roles};
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Docker image tag for the internal runtime. Built locally from
/// `runtime/Dockerfile`. Will become configurable when production
/// hardening lands.
const RUNTIME_IMAGE: &str = "darkmux-runtime:latest";

/// LMStudio /v1/models URL used to probe the currently-loaded model
/// when no explicit model is provided. Currently the internal runtime
/// uses "whatever's loaded"; future iteration will resolve via the
/// role pin table.
const LMSTUDIO_MODELS_URL: &str = "http://localhost:1234/v1/models";

pub fn dispatch(opts: DispatchOpts) -> Result<DispatchResult> {
    eprintln!(
        "darkmux crew dispatch: runtime=internal — image: {RUNTIME_IMAGE}"
    );

    // 1. Load the role manifest + .md prompt. The internal runtime uses
    //    the SAME on-disk role definition as the openclaw path so the
    //    prompts stay identical across runtimes — load-bearing for the
    //    runtime-vs-openclaw comparison.
    let roles = load_roles().context("loading crew roles for internal dispatch")?;
    let _role = roles
        .iter()
        .find(|r| r.id == opts.role_id)
        .ok_or_else(|| anyhow!("role not found: {}", opts.role_id))?;
    let system_prompt = load_role_prompt(&opts.role_id).ok_or_else(|| {
        anyhow!(
            "role '{}' has no .md system prompt — internal runtime requires one",
            opts.role_id
        )
    })?;

    // 2. Resolve the model. Currently probes LMStudio for whatever's
    //    loaded; future iteration will use the role pin + active profile.
    let model = probe_loaded_model().context(
        "no model loaded in LMStudio. Load one (darkmux swap <profile>) before dispatching."
    )?;
    eprintln!("darkmux crew dispatch: model={model}");

    // 3. Resolve session id — same shape as the openclaw path so
    //    callers that compare sessions across runtimes have a stable
    //    handle.
    let unix_micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    let session_id = opts.session_id.clone().unwrap_or_else(|| {
        format!(
            "crew-dispatch-{}-{unix_micros}-internal",
            opts.role_id
        )
    });

    // 4. Workspace resolution. Two paths (#206):
    //
    //    a) `--workdir <path>` → mount operator's chosen path at
    //       /workspace inside the container. The path must already
    //       exist; container writes persist there post-dispatch.
    //       This is the path real engagement work uses (refactor /
    //       audit / feature dispatches against an existing repo).
    //
    //    b) No `--workdir` → allocate a fresh tempdir. Useful for
    //       toy tests, sanity probes, and one-shot dispatches that
    //       don't need persistent operator workspace state.
    //
    //    NEITHER path auto-cleans the workspace dir — the operator
    //    can inspect trajectory.jsonl + any files the agent wrote
    //    after the container exits. That's half the point of replacing
    //    the openclaw workspace model (operator visibility into what
    //    the dispatch did).
    let workspace = match opts.workdir.as_deref() {
        Some(custom) => {
            // Symlink-escape guard (#227 + #232). Walk each operator-typed
            // component and bail if any is a symlink — except for known
            // macOS firmlinks (/tmp, /var, /etc) which operators traverse
            // routinely without realizing they're symlinks.
            //
            // Scope (#232 Issue 2): symlink-only. `..`-traversal paths
            // like /tmp/safe/../../../etc are operator-explicit (typed
            // intentionally) and out of scope — the original threat was
            // an operator surprised by indirection, not by their own
            // deliberate path arithmetic.
            //
            // Why the component-walk over canonicalize-vs-absolute (PR
            // #228 / the canonical-to-canonical pattern proposed in #232):
            // both .canonicalize() calls resolve all symlinks including
            // user-named leaf symlinks, so they always agree on simple
            // path/symlink cases — the comparison silently passes the
            // attack from #227 back through.
            if let Some(offending) = first_user_symlink_in(custom)
                .with_context(|| format!("checking --workdir for symlinks: {}", custom.display()))?
            {
                bail!(
                    "--workdir traverses an operator-named symlink at {} — refusing to follow.\n  \
                     Use the real directory path directly to prevent unintended container r/w.",
                    offending.display()
                );
            }
            let resolved = custom.canonicalize().with_context(|| {
                format!(
                    "--workdir path does not exist or cannot be resolved: {}",
                    custom.display()
                )
            })?;
            if !resolved.is_dir() {
                bail!(
                    "--workdir path is not a directory: {}",
                    resolved.display()
                );
            }
            resolved
        }
        None => {
            let auto = std::env::temp_dir().join(format!(
                "darkmux-dispatch-{}-{unix_micros}",
                opts.role_id
            ));
            fs::create_dir_all(&auto)
                .with_context(|| format!("creating dispatch workspace: {}", auto.display()))?;
            auto
        }
    };
    let workspace_source = if opts.workdir.is_some() {
        "operator-provided via --workdir"
    } else {
        "fresh tempdir (no --workdir given)"
    };
    eprintln!(
        "darkmux crew dispatch: workspace={} ({})",
        workspace.display(),
        workspace_source
    );

    // 5. Emit dispatch.start flow record with runtime metadata in payload
    //    (#204). Pairs with dispatch.complete below via session_id, same
    //    as the openclaw path does.
    let dispatch_start_payload = serde_json::json!({
        "runtime": "internal",
        "prompt_chars": opts.message.chars().count(),
        "system_chars": system_prompt.chars().count(),
        "workspace": workspace.display().to_string(),
    });
    let _ = crate::flow::record(crate::crew::dispatch::build_dispatch_record_with_payload(
        crate::flow::Level::Info,
        "dispatch start",
        &opts.role_id,
        &session_id,
        Some(&model),
        Some(dispatch_start_payload),
    ));
    let dispatch_start_instant = std::time::Instant::now();

    // 6. Spawn the docker container. Async via `spawn()` (vs the older
    //    `output()`) so the live trajectory tailer (step 7) can run in
    //    parallel and emit flow records mid-dispatch — without that,
    //    topology edges go stale during long streaming turns (#231).
    //    `--rm` cleans up the container on exit.
    let mut cmd = Command::new("docker");
    cmd.arg("run")
        .arg("--rm")
        .arg("-v")
        .arg(format!("{}:/workspace", workspace.display()))
        .arg(RUNTIME_IMAGE)
        .arg("run")
        .arg("--model")
        .arg(&model)
        .arg("--system")
        .arg(&system_prompt)
        .arg("--prompt")
        .arg(&opts.message)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd.spawn().context("spawning darkmux-runtime container")?;

    // 7. Live trajectory tailer (#231). Background thread polls
    //    `<workspace>/.darkmux-runtime/trajectory.jsonl` every 250ms while
    //    the container runs; emits flow records in real time:
    //
    //      - `model.completed`  → `dispatch.turn`
    //      - `tool.completed`   → `dispatch.tool`
    //      - `compaction`       → `dispatch.compaction`
    //      - `model.reasoning`  → `dispatch.reasoning` (with S6 size cap)
    //      - `model.partial`    → `dispatch.turn.heartbeat` (rate-limited)
    //
    //    Best-effort: read failures are non-fatal (flow records are
    //    observability, not correctness). After the container exits, the
    //    main thread signals stop; the tailer does one final flush pass
    //    so straggler lines written between the last poll and exit are
    //    not lost.
    let stop_flag = Arc::new(AtomicBool::new(false));
    let tailer_handle = {
        let stop = Arc::clone(&stop_flag);
        let workspace = workspace.clone();
        let session_id = session_id.clone();
        let role_id = opts.role_id.clone();
        let model = model.clone();
        thread::spawn(move || run_tailer(workspace, session_id, role_id, model, stop))
    };

    let output = child
        .wait_with_output()
        .context("waiting for darkmux-runtime container")?;

    let wall_ms = dispatch_start_instant.elapsed().as_millis() as u64;
    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // Signal the tailer to do its final flush + return. join() can
    // theoretically panic if the tailer thread panicked; degrade to a
    // default summary rather than failing the dispatch over a
    // best-effort observability path.
    stop_flag.store(true, Ordering::SeqCst);
    let trajectory_summary = tailer_handle
        .join()
        .unwrap_or_else(|_| TrajectorySummary::default());

    // 8. Emit dispatch.complete flow record with summary metadata.
    let dispatch_complete_payload = serde_json::json!({
        "runtime": "internal",
        "wall_ms": wall_ms,
        "stdout_chars": stdout.chars().count(),
        "stderr_chars": stderr.chars().count(),
        "exit_code": exit_code,
        "result_class": if exit_code == 0 { "ok" } else { "error" },
        "total_turns": trajectory_summary.turns,
        "total_tools": trajectory_summary.tool_calls,
        "total_compactions": trajectory_summary.compactions,
    });
    let (action, level) = if exit_code == 0 {
        ("dispatch complete", crate::flow::Level::Info)
    } else {
        ("dispatch error", crate::flow::Level::Error)
    };
    let _ = crate::flow::record(crate::crew::dispatch::build_dispatch_record_with_payload(
        level,
        action,
        &opts.role_id,
        &session_id,
        Some(&model),
        Some(dispatch_complete_payload),
    ));

    Ok(DispatchResult {
        exit_code,
        stdout,
        stderr,
        session_id,
        watched_state: Vec::new(),
    })
}

/// Summary of what the trajectory tailer surfaced. Used to enrich the
/// dispatch.complete payload with end-of-dispatch counts.
#[derive(Default, Debug, Clone)]
struct TrajectorySummary {
    turns: u32,
    tool_calls: u32,
    compactions: u32,
    heartbeats: u32,
}

/// How often the live tailer polls `trajectory.jsonl` while the container
/// is alive. 250ms matches the daemon's `tail_lines` poll cadence in
/// `serve.rs` — short enough for sub-second responsiveness, long enough
/// to keep CPU+IO cost negligible for an idle dispatch.
const TAILER_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Minimum interval between consecutive `dispatch.turn.heartbeat` flow
/// records. The runtime emits one `model.partial` trajectory event per
/// SSE chunk (potentially hundreds per second on a streaming turn);
/// the tailer coalesces them into a coarser heartbeat so the topology
/// viewer's edge-animation 5s decay window stays alive without
/// flooding the flow stream + audit chain. (#231)
const HEARTBEAT_MIN_INTERVAL: Duration = Duration::from_secs(2);

/// Hard cap on `reasoning_text` carried in a `dispatch.reasoning` flow
/// record payload. Thinking-mode models can emit 10MB+ of reasoning on
/// hard problems; without this cap the full text flows through audit +
/// Redis + browser. Truncated payloads carry a marker indicating the
/// original size so the operator knows it was capped. (#231 / S6)
const MAX_REASONING_TEXT_BYTES: usize = 256 * 1024;

/// Run the live trajectory tailer to completion. Polls until `stop_flag`
/// is set, then does one final flush pass to drain any straggler lines
/// the container wrote between the last poll tick and exit. Returns the
/// accumulated event-count summary; never errors (observability path).
fn run_tailer(
    workspace: PathBuf,
    session_id: String,
    role_id: String,
    model: String,
    stop_flag: Arc<AtomicBool>,
) -> TrajectorySummary {
    let trajectory_path = workspace
        .join(".darkmux-runtime")
        .join("trajectory.jsonl");
    let mut state = TailerState::new(trajectory_path, session_id, role_id, model);

    loop {
        state.poll_and_emit();
        if stop_flag.load(Ordering::SeqCst) {
            // Final flush — pick up anything written between the last
            // sleep tick and the container's exit signal.
            state.poll_and_emit();
            break;
        }
        thread::sleep(TAILER_POLL_INTERVAL);
    }

    state.summary
}

/// State machine for tailing `trajectory.jsonl`. Tracks the file offset,
/// any partial-line tail bytes carried across polls, and the last
/// heartbeat instant for rate limiting.
struct TailerState {
    trajectory_path: PathBuf,
    offset: u64,
    /// Trailing partial line carried from one poll to the next when the
    /// file ends mid-line (a write was in progress at our read).
    pending: String,
    session_id: String,
    role_id: String,
    model: String,
    last_heartbeat_at: Option<Instant>,
    summary: TrajectorySummary,
}

impl TailerState {
    fn new(
        trajectory_path: PathBuf,
        session_id: String,
        role_id: String,
        model: String,
    ) -> Self {
        Self {
            trajectory_path,
            offset: 0,
            pending: String::new(),
            session_id,
            role_id,
            model,
            last_heartbeat_at: None,
            summary: TrajectorySummary::default(),
        }
    }

    /// One poll round: open the trajectory file, read new bytes since
    /// the previous offset, drain complete lines, dispatch each event.
    /// Silent on errors — file may not exist yet (container hasn't
    /// written) and any IO hiccup is best-effort.
    fn poll_and_emit(&mut self) {
        use std::io::{Read, Seek, SeekFrom};

        let mut file = match std::fs::File::open(&self.trajectory_path) {
            Ok(f) => f,
            Err(_) => return,
        };
        let size = match file.metadata() {
            Ok(m) => m.len(),
            Err(_) => return,
        };
        // File truncated below our offset (shouldn't happen in practice
        // since the runtime writes append-only, but defensive): reset.
        if size < self.offset {
            self.offset = 0;
            self.pending.clear();
        }
        if size <= self.offset {
            return;
        }

        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut buf = Vec::with_capacity((size - self.offset) as usize);
        if file.read_to_end(&mut buf).is_err() {
            return;
        }
        self.offset = size;

        self.pending.push_str(&String::from_utf8_lossy(&buf));
        while let Some(nl) = self.pending.find('\n') {
            let line: String = self.pending.drain(..nl).collect();
            self.pending.drain(..1); // consume the \n
            if !line.is_empty() {
                self.handle_event(&line);
            }
        }
    }

    fn handle_event(&mut self, line: &str) {
        let event: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return,
        };
        let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match event_type {
            "model.completed" => {
                self.summary.turns += 1;
                let payload = serde_json::json!({
                    "turn_seq": event.get("seq"),
                    "finish_reason": event.get("finish_reason"),
                    "tool_calls_count": event.get("tool_calls").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0),
                    "usage": event.get("usage"),
                });
                self.emit("dispatch.turn", crate::flow::Level::Info, payload);
            }
            "tool.completed" => {
                self.summary.tool_calls += 1;
                let payload = serde_json::json!({
                    "tool_seq": event.get("tool_seq"),
                    "tool_name": event.get("tool_name"),
                    "args_chars": event.get("args_chars"),
                    "result_chars": event.get("result_chars"),
                });
                self.emit("dispatch.tool", crate::flow::Level::Info, payload);
            }
            "compaction" => {
                self.summary.compactions += 1;
                let payload = serde_json::json!({
                    "generation": event.get("generation"),
                    "before_messages": event.get("before_messages"),
                    "after_messages": event.get("after_messages"),
                    "summary_chars": event.get("summary_chars"),
                });
                self.emit("dispatch.compaction", crate::flow::Level::Info, payload);
            }
            "model.reasoning" => {
                // The runtime emits these when it parses <think>...</think>
                // blocks from the assistant content (#204). The full
                // reasoning text rides in payload so the flow viewer can
                // render a collapse/expand block. Capped at
                // MAX_REASONING_TEXT_BYTES so a single huge thinking
                // session can't blow up downstream storage. (#231 / S6)
                let reasoning_text = cap_reasoning_text(event.get("reasoning_text"));
                let payload = serde_json::json!({
                    "turn_seq": event.get("seq"),
                    "reasoning_chars": event.get("reasoning_chars"),
                    "reasoning_text": reasoning_text,
                    "reasoning_format": event.get("reasoning_format").unwrap_or(&serde_json::Value::String("inline-think-tags".into())),
                });
                self.emit("dispatch.reasoning", crate::flow::Level::Info, payload);
            }
            "model.partial" => {
                // Per-SSE-chunk events coalesced into a coarser heartbeat
                // (rate-limited via HEARTBEAT_MIN_INTERVAL). Keeps
                // topology edges animated during long streaming turns
                // without flooding the flow stream + audit chain. (#231)
                let now = Instant::now();
                let should_emit = match self.last_heartbeat_at {
                    None => true,
                    Some(prev) => now.duration_since(prev) >= HEARTBEAT_MIN_INTERVAL,
                };
                if should_emit {
                    self.last_heartbeat_at = Some(now);
                    self.summary.heartbeats += 1;
                    let payload = serde_json::json!({
                        "runtime": "internal",
                        "turn_seq": event.get("seq"),
                        "partial_index": event.get("partial_index"),
                        "cumulative_chars": event.get("cumulative_chars"),
                    });
                    self.emit("dispatch.turn.heartbeat", crate::flow::Level::Info, payload);
                }
            }
            _ => {
                // Other event types (dispatch.start/complete from the
                // runtime side; model.streaming.start/end) are ignored —
                // the CLI emits canonical dispatch bookends; streaming
                // start/end events are runtime-internal observability
                // with no flow-stream consumer yet.
            }
        }
    }

    fn emit(&self, action: &str, level: crate::flow::Level, payload: serde_json::Value) {
        let _ = crate::flow::record(crate::crew::dispatch::build_dispatch_record_with_payload(
            level,
            action,
            &self.role_id,
            &self.session_id,
            Some(&self.model),
            Some(payload),
        ));
    }
}

/// Cap a JSON string value at `MAX_REASONING_TEXT_BYTES`, appending a
/// human-readable marker so downstream consumers know it was truncated.
/// Truncates at a UTF-8 char boundary to avoid invalid encoding.
/// Non-string values pass through unchanged. (#231 / S6)
fn cap_reasoning_text(value: Option<&serde_json::Value>) -> serde_json::Value {
    let Some(v) = value else {
        return serde_json::Value::Null;
    };
    let Some(s) = v.as_str() else {
        return v.clone();
    };
    if s.len() <= MAX_REASONING_TEXT_BYTES {
        return v.clone();
    }
    // Truncate at a UTF-8 char boundary so the resulting string is valid.
    let mut cut = MAX_REASONING_TEXT_BYTES;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let original_bytes = s.len();
    let original_chars = s.chars().count();
    serde_json::Value::String(format!(
        "{}… [truncated; original {original_chars} chars / {original_bytes} bytes]",
        &s[..cut]
    ))
}

/// Shell out to curl to fetch `/v1/models` from the host's LMStudio and
/// return the first model id. Uses curl so we don't drag a Rust HTTP
/// client dep into darkmux's main crate for one probe call.
fn probe_loaded_model() -> Result<String> {
    let output = Command::new("curl")
        .args([
            "-sf",
            "-m",
            "5",
            LMSTUDIO_MODELS_URL,
        ])
        .output()
        .context("running curl to probe LMStudio")?;

    if !output.status.success() {
        bail!("LMStudio /v1/models probe failed (curl exit {})", output.status.code().unwrap_or(-1));
    }

    let body: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("parsing LMStudio /v1/models response as JSON")?;

    body["data"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|m| m["id"].as_str())
        .map(String::from)
        .ok_or_else(|| anyhow!("LMStudio /v1/models returned no models"))
}

/// Walk each component of an operator-typed `--workdir` path and return
/// the first symlink encountered that ISN'T a known macOS system
/// firmlink. Returns `Ok(None)` when no operator-named symlink is
/// present along the path. Returns the offending accumulated path so
/// the caller can name it in the error message. (#232)
///
/// The walk stops short (with `Ok(None)`) when a component doesn't
/// exist — the subsequent canonicalize() will surface that as the
/// canonical "does not exist" error.
pub(crate) fn first_user_symlink_in(path: &Path) -> std::io::Result<Option<PathBuf>> {
    let mut acc = PathBuf::new();
    for component in path.components() {
        acc.push(component);
        let meta = match std::fs::symlink_metadata(&acc) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if meta.file_type().is_symlink() && !is_macos_firmlink(&acc) {
            return Ok(Some(acc));
        }
    }
    Ok(None)
}

/// True for the macOS top-level firmlinks operators routinely traverse
/// without thinking. Deliberately narrow: only the three that real
/// `--workdir` paths cross (`/tmp`, `/var`, `/etc`). Other macOS
/// firmlinks (`/Applications`, `/Library`, `/Users`, `/Volumes`, ...)
/// aren't typical workdir destinations; if an operator hits one, the
/// bail is correct behavior. On Linux those paths are real
/// directories, so this never trips. (#232)
pub(crate) fn is_macos_firmlink(p: &Path) -> bool {
    matches!(p.to_str(), Some("/tmp" | "/var" | "/etc"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    // ─── cap_reasoning_text (S6) ──────────────────────────────────────

    #[test]
    fn cap_reasoning_text_passes_through_short_string() {
        let v = serde_json::Value::String("short".into());
        let out = cap_reasoning_text(Some(&v));
        assert_eq!(out, v);
    }

    #[test]
    fn cap_reasoning_text_passes_through_null() {
        assert_eq!(cap_reasoning_text(None), serde_json::Value::Null);
    }

    #[test]
    fn cap_reasoning_text_passes_through_non_string() {
        let v = serde_json::Value::Number(42.into());
        let out = cap_reasoning_text(Some(&v));
        assert_eq!(out, v);
    }

    #[test]
    fn cap_reasoning_text_truncates_oversize_and_marks() {
        let oversize = "x".repeat(MAX_REASONING_TEXT_BYTES + 100);
        let v = serde_json::Value::String(oversize.clone());
        let out = cap_reasoning_text(Some(&v));
        let s = out.as_str().expect("output is string");
        assert!(s.len() < oversize.len(), "must be shorter than input");
        assert!(s.contains("[truncated"), "must carry truncation marker");
        assert!(s.contains(&oversize.len().to_string()), "marker must include original byte count");
    }

    #[test]
    fn cap_reasoning_text_truncates_at_utf8_boundary() {
        // Build a string where the byte just past the cap is mid-codepoint
        // (4-byte emoji starting at a position near the cap). Result must
        // still be valid UTF-8.
        let pad_bytes = MAX_REASONING_TEXT_BYTES - 1;
        let mut s = "a".repeat(pad_bytes);
        s.push('🦀'); // 4 bytes, starts at pad_bytes
        s.push_str(&"b".repeat(50));
        let v = serde_json::Value::String(s);
        let out = cap_reasoning_text(Some(&v));
        let truncated = out.as_str().expect("output is string");
        // The marker is appended; the actual truncated content is valid UTF-8
        // because String::from_utf8_lossy isn't used — we sliced on a boundary.
        assert!(truncated.is_char_boundary(0));
        assert!(truncated.contains("[truncated"));
    }

    // ─── TailerState::poll_and_emit (live tailing) ────────────────────

    fn fixture_state(trajectory_path: PathBuf) -> TailerState {
        TailerState::new(
            trajectory_path,
            "test-session".into(),
            "test-role".into(),
            "test-model".into(),
        )
    }

    #[test]
    fn tailer_state_handles_missing_file() {
        // poll_and_emit must be a no-op when the trajectory file doesn't
        // exist yet (container hasn't written anything).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("never-written.jsonl");
        let mut state = fixture_state(path);
        state.poll_and_emit(); // no panic; no events
        assert_eq!(state.offset, 0);
        assert!(state.pending.is_empty());
    }

    #[test]
    fn tailer_state_carries_partial_line_across_polls() {
        // Write the first half of a line, poll, write the second half,
        // poll again — the state's pending buffer must stitch them together
        // and only dispatch the event once the newline arrives.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        // First write: incomplete (no newline)
        {
            let mut f = std::fs::File::create(&path).unwrap();
            write!(f, "{{\"type\":\"model.compl").unwrap();
        }
        state.poll_and_emit();
        assert_eq!(state.summary.turns, 0, "no complete line yet");
        assert!(!state.pending.is_empty(), "partial line carried");

        // Second write: appends the rest of the line with newline
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(f, "eted\",\"seq\":1,\"finish_reason\":\"stop\"}}").unwrap();
        }
        state.poll_and_emit();
        assert_eq!(state.summary.turns, 1, "complete line dispatched after second poll");
        assert!(state.pending.is_empty(), "pending drained after newline");
    }

    #[test]
    fn tailer_state_resets_on_truncation() {
        // Defensive path: if the file shrinks below our offset, the
        // tailer must reset its offset to 0 rather than trying to seek
        // past EOF.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        std::fs::write(&path, b"some-bytes\n").unwrap();
        state.poll_and_emit();
        let offset_before = state.offset;
        assert!(offset_before > 0);

        // Truncate to a smaller size.
        std::fs::write(&path, b"").unwrap();
        state.poll_and_emit();
        // After truncation poll, offset should reset to 0 (file is empty,
        // so 0 ≤ size = 0 and offset is 0).
        assert_eq!(state.offset, 0);
    }

    #[test]
    fn tailer_skips_malformed_lines() {
        // A non-JSON line in the trajectory must not crash the tailer or
        // stop later events from being processed.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        let lines = "not json\n\
            {\"type\":\"tool.completed\",\"tool_seq\":1,\"tool_name\":\"bash\"}\n";
        std::fs::write(&path, lines).unwrap();
        state.poll_and_emit();
        assert_eq!(state.summary.tool_calls, 1, "later valid event still processed");
    }

    // ─── Heartbeat rate limiting ──────────────────────────────────────

    #[test]
    fn heartbeat_first_partial_emits() {
        // The very first model.partial should produce a heartbeat (no
        // prior last_heartbeat_at).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        let line = r#"{"type":"model.partial","seq":1,"partial_index":0,"cumulative_chars":10}"#;
        std::fs::write(&path, format!("{line}\n")).unwrap();
        state.poll_and_emit();
        assert_eq!(state.summary.heartbeats, 1);
        assert!(state.last_heartbeat_at.is_some());
    }

    #[test]
    fn heartbeat_rate_limits_consecutive_partials() {
        // Two model.partial events back-to-back (under the 2s window)
        // should produce exactly one heartbeat.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        let lines = "\
            {\"type\":\"model.partial\",\"seq\":1,\"partial_index\":0,\"cumulative_chars\":10}\n\
            {\"type\":\"model.partial\",\"seq\":1,\"partial_index\":1,\"cumulative_chars\":20}\n";
        std::fs::write(&path, lines).unwrap();
        state.poll_and_emit();
        assert_eq!(state.summary.heartbeats, 1, "second partial within window must be coalesced");
    }


    // ─── first_user_symlink_in / is_macos_firmlink (#232) ─────────────

    #[test]
    fn first_user_symlink_in_returns_none_for_real_path() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let result = first_user_symlink_in(&real).unwrap();
        assert!(result.is_none(), "non-symlink path must pass");
    }

    #[test]
    fn first_user_symlink_in_detects_leaf_symlink() {
        // The original #227 attack vector: a user-named symlink as the
        // last component of the path. Must be caught.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let sym = tmp.path().join("evilsym");
        symlink(&target, &sym).unwrap();

        let result = first_user_symlink_in(&sym).unwrap();
        assert_eq!(result, Some(sym), "leaf symlink must be detected");
    }

    #[test]
    fn first_user_symlink_in_detects_middle_component_symlink() {
        // A middle-of-path symlink — also catchable.
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::create_dir(real.join("child")).unwrap();
        let sym = tmp.path().join("sym");
        symlink(&real, &sym).unwrap();
        let probe = sym.join("child");

        let result = first_user_symlink_in(&probe).unwrap();
        assert_eq!(result, Some(sym), "middle-component symlink must be detected");
    }

    #[test]
    fn first_user_symlink_in_returns_none_when_path_does_not_exist() {
        // canonicalize() will surface the not-exists error; the symlink
        // check should not pre-empt it with a spurious result.
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let result = first_user_symlink_in(&missing).unwrap();
        assert!(result.is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn first_user_symlink_in_tolerates_macos_tmp_firmlink() {
        // `/tmp/<real-dir>` traverses the macOS firmlink `/tmp` →
        // `/private/tmp`. The check must NOT bail on this. (#232 Issue 1)
        use std::time::{SystemTime, UNIX_EPOCH};
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros();
        let path = std::path::PathBuf::from(format!("/tmp/dm_firmlink_test_{unique}"));
        std::fs::create_dir(&path).unwrap();
        let result = first_user_symlink_in(&path);
        let _ = std::fs::remove_dir(&path);
        assert!(
            result.unwrap().is_none(),
            "/tmp/foo must not trip on macOS firmlink"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn first_user_symlink_in_still_catches_user_symlink_under_tmp() {
        // A user-named symlink one level under /tmp must still be caught
        // even though /tmp itself is a tolerated firmlink. (#232 Issue 1
        // can't be paid for by reopening #227 — both have to hold.)
        use std::time::{SystemTime, UNIX_EPOCH};
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros();
        let target = std::path::PathBuf::from(format!("/tmp/dm_target_{unique}"));
        let sym = std::path::PathBuf::from(format!("/tmp/dm_sym_{unique}"));
        std::fs::create_dir(&target).unwrap();
        symlink(&target, &sym).unwrap();

        let result = first_user_symlink_in(&sym);
        let _ = std::fs::remove_file(&sym);
        let _ = std::fs::remove_dir(&target);

        let offending = result.unwrap().expect("user symlink under /tmp must still be caught");
        // The matched component is the symlink itself (under either /tmp
        // or its resolved canonical /private/tmp).
        assert!(
            offending.to_string_lossy().contains(&format!("dm_sym_{unique}")),
            "expected sym in offending path; got {offending:?}"
        );
    }

    #[test]
    fn is_macos_firmlink_allowlist_is_narrow() {
        assert!(is_macos_firmlink(Path::new("/tmp")));
        assert!(is_macos_firmlink(Path::new("/var")));
        assert!(is_macos_firmlink(Path::new("/etc")));
        // Subpaths under firmlinks are NOT tolerated — only the anchors.
        assert!(!is_macos_firmlink(Path::new("/tmp/sub")));
        assert!(!is_macos_firmlink(Path::new("/var/log")));
        assert!(!is_macos_firmlink(Path::new("/")));
        assert!(!is_macos_firmlink(Path::new("/home")));
        assert!(!is_macos_firmlink(Path::new("/Users")));
    }
}
