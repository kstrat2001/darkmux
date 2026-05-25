//! Internal runtime dispatch path.
//!
//! Routes a `darkmux crew dispatch <role>` invocation to the
//! `darkmux-runtime` docker container. Per-dispatch container, mounted
//! workspace, structured output collected from stdout.
//!
//! Default runtime as of the runtime-default flip. Openclaw remains
//! available via the explicit `--runtime openclaw` flag for operators
//! who already have it installed and configured.
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
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Docker image tag for the internal runtime. Built locally from
/// `runtime/Dockerfile`. Will become configurable when production
/// hardening lands.
const RUNTIME_IMAGE: &str = "darkmux-runtime:latest";

/// (#368) Translate the host-side `CompactionDispatchArgs` into
/// runtime CLI flags. Each `Some(v)` becomes a `--flag v` pair on
/// the docker run command; `None` is omitted so the runtime falls
/// back to its hardcoded default for that knob. Extracted from the
/// docker-spawn site so the translation rule is unit-testable
/// without spawning a container.
fn apply_compaction_flags(
    cmd: &mut Command,
    compaction: &crate::crew::dispatch::CompactionDispatchArgs,
) {
    if let Some(n) = compaction.threshold_tokens {
        cmd.arg("--compact-threshold-tokens").arg(n.to_string());
    }
    if let Some(model) = &compaction.compactor_model {
        cmd.arg("--compactor-model").arg(model);
    }
    if let Some(share) = compaction.threshold_ratio {
        cmd.arg("--compact-threshold-ratio").arg(share.to_string());
    }
    if let Some(window) = compaction.context_window {
        cmd.arg("--context-window").arg(window.to_string());
    }
}

/// Default per-dispatch wall-clock deadline. 10 minutes covers
/// realistic long-agentic dispatches (Article-2 era `long-agentic`
/// historical slow_cluster_seconds maxed ~950s, so 600s is conservative
/// for medium workloads and most operators). Override with the env
/// var. Phase B dogfood (Beat 39) showed thinking-mode hangs can
/// burn unbounded wall-clock without this cap.
const DEFAULT_DISPATCH_DEADLINE_SECS: u64 = 600;

fn dispatch_deadline_seconds() -> u64 {
    std::env::var("DARKMUX_RUNTIME_DEADLINE_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_DISPATCH_DEADLINE_SECS)
}

/// LMStudio /v1/models URL used to probe the currently-loaded model
/// when no explicit model is provided. Currently the internal runtime
/// uses "whatever's loaded"; future iteration will resolve via the
/// role pin table.
const LMSTUDIO_MODELS_URL: &str = "http://localhost:1234/v1/models";

pub fn dispatch(opts: DispatchOpts) -> Result<DispatchResult> {
    eprintln!(
        "darkmux crew dispatch: runtime=internal — image: {RUNTIME_IMAGE}"
    );

    // Pre-flight: Docker reachable + runtime image present. The
    // internal runtime is the default as of the runtime-default flip;
    // these are the prereqs a new user might not have set up yet. Bail
    // loud + operator-actionable BEFORE we run the role-load / model-
    // probe / workspace-setup work below.
    if !opts.skip_preflight {
        check_docker_preflight()?;
    }

    // 1. Load the role manifest + .md prompt. The internal runtime uses
    //    the SAME on-disk role definition as the openclaw path so the
    //    prompts stay identical across runtimes — load-bearing for the
    //    runtime-vs-openclaw comparison.
    let roles = load_roles().context("loading crew roles for internal dispatch")?;
    let role = roles
        .iter()
        .find(|r| r.id == opts.role_id)
        .ok_or_else(|| anyhow!("role not found: {}", opts.role_id))?;
    let system_prompt = load_role_prompt(&opts.role_id).ok_or_else(|| {
        anyhow!(
            "role '{}' has no .md system prompt — internal runtime requires one",
            opts.role_id
        )
    })?;
    // #340 — surface unknown role-vocab tokens loudly. Unknown tokens
    // (typos like "exce" for "exec", future tokens not yet wired)
    // get silently dropped by `role_to_runtime`; without this warning
    // the operator sees `tool_palette filtered to []` and has no
    // signal about WHY their role got zero tools.
    let unknown_tokens = unknown_role_vocab_tokens(&role.tool_palette);
    if !unknown_tokens.is_empty() {
        eprintln!(
            "darkmux crew dispatch: role `{}` declares unknown tool-vocab tokens: [{}] \
             — these will be silently dropped from the runtime catalog (likely typos). \
             Known tokens: {}",
            opts.role_id,
            unknown_tokens.join(", "),
            known_role_vocab_csv()
        );
    }
    // Compute the runtime tool catalog from the role's tool_palette
    // (allow minus deny). When the palette is empty, returns None and
    // the runtime falls back to its full catalog (back-compat). When
    // restrictive, denied tools never reach the model — they're not in
    // the chat-completions `tools[]` field, so the model structurally
    // cannot call them. This is the runtime-side gate that prevents
    // a model from ignoring its .md doctrine and calling a denied tool
    // (the gap that let D call `edit` despite code-reviewer denying it).
    let allowed_tools = compute_runtime_allowed_tools(&role.tool_palette);

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
            // Symlink-escape guard via the shared validator (#255 Wave-E.2).
            // Same scope as #227 + #232 — symlink-only; `..`-traversal
            // is operator-explicit and intentionally out of scope.
            crate::workdir::validate_workdir(custom)?
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
    // (#363) Generate a unique container name so the watchdog thread
    // (below) can `docker kill` it after the wall-clock deadline.
    // Without --name, we'd have no stable handle to kill. session_id
    // is already unique-per-dispatch and reads well in `docker ps`
    // when debugging. Sanitize since docker container names allow only
    // [a-zA-Z0-9][a-zA-Z0-9_.-]*.
    let container_name = format!(
        "darkmux-dispatch-{}",
        session_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '-'
            })
            .collect::<String>()
    );

    let mut cmd = Command::new("docker");
    cmd.arg("run")
        .arg("--rm")
        .arg("--name")
        .arg(&container_name)
        .arg("-v")
        .arg(format!("{}:/workspace", workspace.display()))
        .arg(RUNTIME_IMAGE)
        .arg("run")
        .arg("--model")
        .arg(&model)
        .arg("--system")
        .arg(&system_prompt)
        .arg("--prompt")
        .arg(&opts.message);
    if opts.json {
        // Plumb the operator's `--json` request through to the
        // container CLI so downstream parsers (qa-review skill, lab
        // adapter, ad-hoc `jq` users) get a structured envelope on
        // stdout instead of the human-readable separator format. The
        // runtime emits status lines to stderr in JSON mode so stdout
        // stays clean. See `runtime/src/main.rs::build_json_envelope`
        // for the schema contract.
        cmd.arg("--json");
    }
    if let Some(allowed) = allowed_tools.as_ref() {
        let csv = allowed.join(",");
        eprintln!(
            "darkmux crew dispatch: tool_palette filtered to [{}] (role={})",
            csv, opts.role_id
        );
        cmd.arg("--allowed-tools").arg(csv);
    }
    // (#368) Compaction config passthrough to the runtime CLI. Each
    // flag is optional: when the operator's profile didn't set a
    // value, no flag is passed and the runtime uses its hardcoded
    // default. End-state: operator tunes via profile JSON → these
    // four flags flow into the container → no env vars touch the
    // surface anywhere.
    apply_compaction_flags(&mut cmd, &opts.compaction);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

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

    // (#363) Wall-clock deadline watchdog. Phase B dogfood (Beat 39,
    // 2026-05-25) surfaced: thinking-mode models can hang intra-turn
    // for arbitrary wall-clock with the agent loop's MAX_TURNS cap
    // never firing. The watchdog runs `docker kill <name>` after the
    // deadline; the container dies with SIGKILL (exit 137); the main
    // thread's `wait_with_output()` returns; the harness detects the
    // timeout via the watchdog's atomic flag + the 137 exit code and
    // surfaces a structured `dispatch.timeout` failure rather than
    // letting the dispatch hang forever.
    let deadline_secs = dispatch_deadline_seconds();
    let timeout_fired = Arc::new(AtomicBool::new(false));
    let watchdog_done = Arc::new(AtomicBool::new(false));
    let watchdog_handle = {
        let timeout_fired = Arc::clone(&timeout_fired);
        let watchdog_done = Arc::clone(&watchdog_done);
        let container_name = container_name.clone();
        thread::spawn(move || {
            // Poll every 500ms so we don't oversleep the natural exit by
            // a full deadline. When the main thread signals
            // `watchdog_done`, exit promptly without firing the kill.
            let deadline = Instant::now() + Duration::from_secs(deadline_secs);
            while Instant::now() < deadline {
                if watchdog_done.load(Ordering::SeqCst) {
                    return;
                }
                thread::sleep(Duration::from_millis(500));
            }
            // Race window: a natural exit can land in the final 500ms
            // sleep above — the loop condition `Instant::now() < deadline`
            // goes false, but `watchdog_done` may have been set during
            // the sleep. Re-check before firing to avoid stamping a
            // spurious timeout on a clean exit (QA finding 2026-05-25).
            if watchdog_done.load(Ordering::SeqCst) {
                return;
            }
            // Deadline genuinely hit before the dispatch completed.
            // Mark timeout BEFORE the kill so the post-wait detection
            // sees the flag, then SIGKILL the container.
            timeout_fired.store(true, Ordering::SeqCst);
            let _ = Command::new("docker")
                .args(["kill", &container_name])
                .output();
        })
    };

    let output = child
        .wait_with_output()
        .context("waiting for darkmux-runtime container")?;

    // Tell the watchdog we're done so it doesn't fire spuriously after
    // a natural exit. Best-effort join (it's a kill-only thread —
    // panic-resilience isn't load-bearing).
    watchdog_done.store(true, Ordering::SeqCst);
    let _ = watchdog_handle.join();

    let wall_ms = dispatch_start_instant.elapsed().as_millis() as u64;
    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // (#363) If the watchdog fired, prepend a structured timeout
    // marker to stderr so the lab harness can detect + surface the
    // timeout. The container died via SIGKILL (exit ~137); without
    // this marker the harness would just see "non-zero exit" with no
    // diagnostic detail about why.
    if timeout_fired.load(Ordering::SeqCst) {
        stderr = format!(
            "darkmux dispatch: TIMEOUT after {deadline_secs}s — container `{container_name}` \
             was killed by the watchdog. Override the default deadline with \
             DARKMUX_RUNTIME_DEADLINE_SECONDS=<N>. Thinking-mode hangs are the most common \
             cause (see #363).\n{stderr}"
        );
    }

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
/// Drain complete (newline-terminated) lines from a byte buffer,
/// returning each as a decoded `String`. The buffer is mutated in
/// place: drained line bytes are removed; any partial tail without a
/// trailing `\n` stays in the buffer for the next call.
///
/// Decoding via `from_utf8_lossy` happens ONCE per complete line —
/// never on a partial read. This is the load-bearing invariant for
/// #329: multi-byte UTF-8 sequences (emoji, CJK) that straddle a
/// read boundary stay intact across calls. Pre-fix, the buffer was a
/// `String` with per-poll `from_utf8_lossy(&buf)`, which replaced
/// any partial multi-byte tail with U+FFFD and silently corrupted
/// the subsequent JSON payload.
///
/// Empty lines are skipped (matches the pre-fix loop's semantics).
fn drain_complete_lines_from_bytes(pending: &mut Vec<u8>) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
        // Drain through and including the newline; line content is
        // everything before the newline byte.
        let line_bytes: Vec<u8> = pending.drain(..=nl).collect();
        let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]).into_owned();
        if !line.is_empty() {
            out.push(line);
        }
    }
    out
}

/// any partial-line tail bytes carried across polls, and the last
/// heartbeat instant for rate limiting.
struct TailerState {
    trajectory_path: PathBuf,
    offset: u64,
    /// Trailing partial line carried from one poll to the next when the
    /// file ends mid-line (a write was in progress at our read).
    ///
    /// Raw bytes — NOT a string. Multi-byte UTF-8 characters (emoji,
    /// CJK) can split across `poll_and_emit` rounds; decoding via
    /// `from_utf8_lossy` on partial bytes would emit U+FFFD
    /// replacement chars and silently corrupt the JSON payload (#329).
    /// We accumulate bytes here and decode per-line, after the
    /// trailing newline arrives.
    pending: Vec<u8>,
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
            pending: Vec::new(),
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

        // Append raw bytes; decode happens per-line (after the
        // trailing newline arrives) so multi-byte UTF-8 chars that
        // straddle a poll boundary don't corrupt to U+FFFD (#329).
        self.pending.extend_from_slice(&buf);
        for line in drain_complete_lines_from_bytes(&mut self.pending) {
            self.handle_event(&line);
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
/// Verify Docker is reachable + the runtime image exists. Called by
/// `dispatch()` BEFORE the role-load / model-probe / workspace setup
/// so a new user without Docker (or with Docker but no runtime image)
/// gets a clean, operator-actionable bail message instead of an
/// opaque `Command::new("docker")` "No such file or directory" or
/// a runtime-time "Unable to find image" failure mid-dispatch.
fn check_docker_preflight() -> Result<()> {
    // Step 1: docker binary exists + daemon is reachable.
    let docker_check = Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output();
    match docker_check {
        Ok(out) if out.status.success() => {} // Docker daemon up
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!(
                "darkmux's default runtime (`--runtime internal`) requires Docker, \
                 but `docker version` failed:\n  {}\n\
                 Options:\n  \
                 - Start Docker Desktop, OR\n  \
                 - Re-run with `--runtime openclaw` if you have openclaw installed",
                stderr.trim()
            );
        }
        Err(_) => {
            bail!(
                "darkmux's default runtime (`--runtime internal`) requires Docker, \
                 but the `docker` binary isn't on PATH.\n\
                 Options:\n  \
                 - Install Docker Desktop (https://www.docker.com/products/docker-desktop), OR\n  \
                 - Re-run with `--runtime openclaw` if you have openclaw installed"
            );
        }
    }

    // Step 2: runtime image exists locally. `docker images -q <tag>`
    // exits 0 even when the image is missing; the load-bearing signal
    // is empty stdout (no image id printed). Daemon-unreachable cases
    // were already caught in Step 1.
    let image_check = Command::new("docker")
        .args(["images", "-q", RUNTIME_IMAGE])
        .output()
        .context("running `docker images` to check for runtime image")?;
    if image_check.stdout.is_empty() {
        bail!(
            "darkmux runtime image `{RUNTIME_IMAGE}` not found locally.\n\
             Build it once from the darkmux repo root:\n  \
             docker build -t {RUNTIME_IMAGE} runtime/\n\
             (Or use `--runtime openclaw` if you have openclaw installed.)"
        );
    }
    Ok(())
}

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

// `first_user_symlink_in` and `is_macos_firmlink` moved to
// `crate::workdir` as part of Wave-E.2 (#255). Workers + both runtime
// paths now share one implementation via `workdir::validate_workdir`.

/// Map a role's tool_palette (allow/deny in role-vocab) to the list of
/// runtime-vocab tool names that should be exposed to the model via
/// `--allowed-tools`.
///
/// Role-vocab and runtime-vocab don't align 1-to-1:
///   - role "read" → runtime ["read", "search"] (search is a specialized
///     read; "you may read files" implies it)
///   - role "edit" → runtime ["edit"]
///   - role "write" → runtime ["write"]
///   - role "exec" → runtime ["bash"]
///   - role "process", "update_plan" → no runtime equivalent (silently
///     dropped; no runtime tool implements these concepts today)
///
/// Allow first, then deny removes. Deny wins on conflict. Unknown
/// role-vocab tokens are silently dropped — keeps forward-compatibility
/// for roles that name tools the runtime doesn't yet implement.
///
/// Returns `None` when the palette is empty (no allow, no deny) so the
/// caller can decide between "fail loud (no tools)" and "back-compat
/// default (full catalog)." Today the caller passes `None` →
/// runtime's `--allowed-tools` flag is omitted → runtime exposes full
/// catalog. The empty-palette case usually means "role definition is
/// incomplete," not "no tools allowed."
/// All role-vocab tokens darkmux currently knows how to map to
/// runtime tools. Source of truth for `role_to_runtime` (mapping)
/// AND `unknown_role_vocab_tokens` (typo detection). When the
/// runtime gains a new tool or roles gain a new capability token,
/// add it here AND to the match in `role_to_runtime`.
const KNOWN_ROLE_VOCAB: &[&str] = &["read", "edit", "write", "exec", "process", "update_plan"];

/// Single source of truth for role-vocab → runtime-vocab. Add new
/// mappings here when the runtime gains a new tool or roles gain
/// new capability tokens. Unknown tokens return an empty slice;
/// detection of unknowns lives in `unknown_role_vocab_tokens` so
/// the caller can warn loudly (#340).
fn role_to_runtime(role_name: &str) -> &'static [&'static str] {
    match role_name {
        "read" => &["read", "search"],
        "edit" => &["edit"],
        "write" => &["write"],
        "exec" => &["bash"],
        // Known role-vocab tokens with no runtime equivalent today.
        // NOT typos — silently dropped is correct behavior.
        "process" | "update_plan" => &[],
        // Truly unknown. Empty result here; the caller should run
        // `unknown_role_vocab_tokens` separately to surface this as
        // an operator-visible warning (#340 — silent drops are
        // typo-prone). Don't bail here; doing so would break valid
        // dispatches when a future role manifest references a token
        // we haven't wired yet.
        _ => &[],
    }
}

/// Tokens in a role's tool_palette that aren't in [`KNOWN_ROLE_VOCAB`]
/// — typos, future tokens, vendor-specific names. Returned sorted +
/// deduplicated. Empty when the palette only contains known tokens.
///
/// Caller pattern: call before dispatch; if non-empty, warn the
/// operator with the unknown tokens + the list of known ones so
/// they can correct the manifest. Doctor uses the same helper to
/// surface unknowns across all role manifests proactively (#340).
pub(crate) fn unknown_role_vocab_tokens(palette: &crate::crew::types::ToolPalette) -> Vec<String> {
    let mut unknowns: Vec<String> = palette
        .allow
        .iter()
        .chain(palette.deny.iter())
        .filter(|name| !KNOWN_ROLE_VOCAB.contains(&name.as_str()))
        .cloned()
        .collect();
    unknowns.sort();
    unknowns.dedup();
    unknowns
}

fn compute_runtime_allowed_tools(palette: &crate::crew::types::ToolPalette) -> Option<Vec<String>> {
    // Empty palette: caller decides; today we return None so the
    // runtime exposes its full catalog (back-compat).
    if palette.allow.is_empty() && palette.deny.is_empty() {
        return None;
    }

    // Allow first.
    let mut allowed: Vec<String> = palette
        .allow
        .iter()
        .flat_map(|name| role_to_runtime(name).iter().map(|s| s.to_string()))
        .collect();

    // Deny removes. Deny wins on conflict.
    let denied: Vec<String> = palette
        .deny
        .iter()
        .flat_map(|name| role_to_runtime(name).iter().map(|s| s.to_string()))
        .collect();
    allowed.retain(|t| !denied.contains(t));

    // Dedupe while preserving order (a role manifest could allow both
    // "read" and "edit" — `read` brings ["read","search"] and we don't
    // want duplicates of either if some future mapping overlaps).
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    allowed.retain(|t| seen.insert(t.clone()));

    Some(allowed)
}

/// Comma-separated list of all known role-vocab tokens, for use in
/// operator-facing warning messages (#340). Wrapped in a helper so
/// the formatting stays consistent across dispatch + doctor surfaces.
pub(crate) fn known_role_vocab_csv() -> String {
    KNOWN_ROLE_VOCAB.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::io::Write;
    use tempfile::TempDir;

    // ─── #368: compaction-flag passthrough to runtime CLI ────────────

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn apply_compaction_flags_omits_when_all_none() {
        let mut cmd = Command::new("docker");
        let compaction = crate::crew::dispatch::CompactionDispatchArgs::default();
        apply_compaction_flags(&mut cmd, &compaction);
        let args = args_of(&cmd);
        assert!(
            !args.iter().any(|a| a.starts_with("--compact") || a == "--context-window"),
            "default config should emit no compaction flags; got {args:?}"
        );
    }

    #[test]
    fn apply_compaction_flags_emits_threshold_when_set() {
        let mut cmd = Command::new("docker");
        let compaction = crate::crew::dispatch::CompactionDispatchArgs {
            threshold_tokens: Some(35_000),
            ..Default::default()
        };
        apply_compaction_flags(&mut cmd, &compaction);
        let args = args_of(&cmd);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--compact-threshold-tokens" && w[1] == "35000"),
            "expected --compact-threshold-tokens 35000; got {args:?}"
        );
    }

    #[test]
    fn apply_compaction_flags_emits_all_when_set() {
        let mut cmd = Command::new("docker");
        let compaction = crate::crew::dispatch::CompactionDispatchArgs {
            threshold_tokens: Some(45_000),
            compactor_model: Some("custom-compactor".to_string()),
            threshold_ratio: Some(0.35),
            context_window: Some(101_000),
        };
        apply_compaction_flags(&mut cmd, &compaction);
        let args = args_of(&cmd);
        assert!(args.iter().any(|a| a == "--compact-threshold-tokens"));
        assert!(args.iter().any(|a| a == "45000"));
        assert!(args.iter().any(|a| a == "--compactor-model"));
        assert!(args.iter().any(|a| a == "custom-compactor"));
        assert!(args.iter().any(|a| a == "--compact-threshold-ratio"));
        assert!(args.iter().any(|a| a == "--context-window"));
        assert!(args.iter().any(|a| a == "101000"));
    }

    #[test]
    fn from_profile_derives_typed_threshold() {
        use crate::types::{
            ModelRole, Profile, ProfileModel, ProfileRuntime, RuntimeCompactionConfig,
        };
        let profile = Profile {
            description: None,
            models: vec![ProfileModel {
                id: "primary-x".into(),
                n_ctx: 100_000,
                role: ModelRole::Primary,
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                config_path: None,
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: None,
                    threshold_tokens: Some(40_000),
                    threshold_ratio: None,
                    tier1: None,
                    tier2: None,
                    reserve: None,
                    extras: Default::default(),
                }),
            }),
            use_when: None,
        };
        let args = crate::crew::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert_eq!(args.threshold_tokens, Some(40_000));
        assert_eq!(args.context_window, Some(100_000), "primary n_ctx");
    }

    #[test]
    fn from_profile_derives_typed_threshold_ratio() {
        // (#368 clean break) `threshold_ratio` reads ONLY from the
        // typed schema field. `compactor_model` does NOT read from
        // extras at all (Beat-39 smoke caught HTTP 400 when openclaw's
        // `lmstudio/<id>` format was passed to LMStudio's direct API
        // which only knows the bare/namespaced form). Until a typed
        // `compaction.compactor_model` lands, runtime uses default.
        use crate::types::{
            ModelRole, Profile, ProfileModel, ProfileRuntime, RuntimeCompactionConfig,
        };
        let mut extras = serde_json::Map::new();
        // This openclaw-flavored value must NOT influence the dispatch.
        extras.insert(
            "model".into(),
            serde_json::json!("lmstudio/qwen3-4b-instruct-2507"),
        );
        let profile = Profile {
            description: None,
            models: vec![ProfileModel {
                id: "primary-x".into(),
                n_ctx: 101_000,
                role: ModelRole::Primary,
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                config_path: None,
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: None,
                    threshold_tokens: None,
                    threshold_ratio: Some(0.35),
                    tier1: None,
                    tier2: None,
                    reserve: None,
                    extras,
                }),
            }),
            use_when: None,
        };
        let args = crate::crew::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert_eq!(args.threshold_ratio, Some(0.35));
        assert!(
            args.compactor_model.is_none(),
            "clean break: openclaw extras `model` must NOT auto-populate compactor_model \
             (would pass `lmstudio/<id>` prefix to LMStudio's direct API → HTTP 400)"
        );
        assert_eq!(args.context_window, Some(101_000));
    }

    /// (#368 clean break invariant) When ONLY `extras["maxHistoryShare"]`
    /// is set — no typed `threshold_ratio` — the host MUST NOT silently
    /// translate openclaw's history-cap to darkmux's pre-trigger ratio.
    /// They're different concepts; mapping across would surface in
    /// methodology citations as "this run tuned to X" when the operator
    /// never actually expressed X in the darkmux-side surface.
    #[test]
    fn from_profile_ignores_openclaw_maxhistoryshare_extras() {
        use crate::types::{
            ModelRole, Profile, ProfileModel, ProfileRuntime, RuntimeCompactionConfig,
        };
        let mut extras = serde_json::Map::new();
        // Operator carries openclaw's historical config — this should
        // pass through untouched in extras (for any downstream
        // openclaw-aware consumer) but NOT influence darkmux's trigger.
        extras.insert("maxHistoryShare".into(), serde_json::json!(0.35));
        let profile = Profile {
            description: None,
            models: vec![ProfileModel {
                id: "primary-x".into(),
                n_ctx: 100_000,
                role: ModelRole::Primary,
                identifier: None,
            }],
            runtime: Some(ProfileRuntime {
                config_path: None,
                context_tokens: None,
                compaction: Some(RuntimeCompactionConfig {
                    strategy: None,
                    threshold_tokens: None,
                    threshold_ratio: None,
                    tier1: None,
                    tier2: None,
                    reserve: None,
                    extras,
                }),
            }),
            use_when: None,
        };
        let args = crate::crew::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert!(
            args.threshold_ratio.is_none(),
            "clean break: openclaw extras must NOT auto-populate threshold_ratio"
        );
    }

    #[test]
    fn from_profile_handles_missing_compaction_block() {
        use crate::types::{ModelRole, Profile, ProfileModel};
        let profile = Profile {
            description: None,
            models: vec![ProfileModel {
                id: "primary-x".into(),
                n_ctx: 50_000,
                role: ModelRole::Primary,
                identifier: None,
            }],
            runtime: None,
            use_when: None,
        };
        let args = crate::crew::dispatch::CompactionDispatchArgs::from_profile(&profile);
        assert!(args.threshold_tokens.is_none());
        assert!(args.compactor_model.is_none());
        assert!(args.threshold_ratio.is_none());
        // Primary n_ctx still captured even without compaction block.
        assert_eq!(args.context_window, Some(50_000));
    }

    // ─── #363: dispatch wall-clock deadline ────────────────────────────

    #[test]
    #[serial]
    fn deadline_defaults_when_env_unset() {
        // Saved + restored — tests share process env, so be polite.
        let prev = std::env::var("DARKMUX_RUNTIME_DEADLINE_SECONDS").ok();
        unsafe { std::env::remove_var("DARKMUX_RUNTIME_DEADLINE_SECONDS") };
        assert_eq!(dispatch_deadline_seconds(), DEFAULT_DISPATCH_DEADLINE_SECS);
        if let Some(v) = prev {
            unsafe { std::env::set_var("DARKMUX_RUNTIME_DEADLINE_SECONDS", v) };
        }
    }

    #[test]
    #[serial]
    fn deadline_reads_env_override() {
        let prev = std::env::var("DARKMUX_RUNTIME_DEADLINE_SECONDS").ok();
        unsafe { std::env::set_var("DARKMUX_RUNTIME_DEADLINE_SECONDS", "30") };
        assert_eq!(dispatch_deadline_seconds(), 30);
        unsafe { std::env::remove_var("DARKMUX_RUNTIME_DEADLINE_SECONDS") };
        if let Some(v) = prev {
            unsafe { std::env::set_var("DARKMUX_RUNTIME_DEADLINE_SECONDS", v) };
        }
    }

    #[test]
    #[serial]
    fn deadline_falls_back_on_garbage_env() {
        let prev = std::env::var("DARKMUX_RUNTIME_DEADLINE_SECONDS").ok();
        unsafe { std::env::set_var("DARKMUX_RUNTIME_DEADLINE_SECONDS", "not-a-number") };
        assert_eq!(dispatch_deadline_seconds(), DEFAULT_DISPATCH_DEADLINE_SECS);
        unsafe { std::env::remove_var("DARKMUX_RUNTIME_DEADLINE_SECONDS") };
        if let Some(v) = prev {
            unsafe { std::env::set_var("DARKMUX_RUNTIME_DEADLINE_SECONDS", v) };
        }
    }

    // ─── role tool_palette → runtime allowed-tools mapping ────────────

    fn palette(allow: &[&str], deny: &[&str]) -> crate::crew::types::ToolPalette {
        crate::crew::types::ToolPalette {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn allowed_tools_empty_palette_returns_none_so_runtime_uses_full_catalog() {
        let p = palette(&[], &[]);
        assert_eq!(compute_runtime_allowed_tools(&p), None);
    }

    #[test]
    fn allowed_tools_coder_palette_exposes_all_runtime_tools() {
        // coder role: allow [read, edit, write, exec, process], deny []
        let p = palette(&["read", "edit", "write", "exec", "process"], &[]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        // Expected: read + search (from "read"), edit, write, bash (from "exec").
        // "process" has no runtime equivalent; silently dropped.
        let mut sorted = result.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["bash", "edit", "read", "search", "write"]);
    }

    #[test]
    fn allowed_tools_code_reviewer_palette_excludes_edit_and_write() {
        // code-reviewer: allow [read, exec, update_plan], deny [edit, write, process]
        let p = palette(&["read", "exec", "update_plan"], &["edit", "write", "process"]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        let mut sorted = result.clone();
        sorted.sort();
        // Expected: read + search (from "read"), bash (from "exec").
        // "update_plan" has no runtime equivalent.
        assert_eq!(sorted, vec!["bash", "read", "search"]);
        // Hard regression guard: code-reviewer must NEVER see edit/write.
        assert!(!result.contains(&"edit".to_string()));
        assert!(!result.contains(&"write".to_string()));
    }

    #[test]
    fn allowed_tools_deny_overrides_allow() {
        // Pathological: same tool in both lists. Deny wins.
        let p = palette(&["edit"], &["edit"]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        assert!(result.is_empty(), "deny must win over allow; got {result:?}");
    }

    #[test]
    fn allowed_tools_unknown_role_vocab_silently_dropped() {
        let p = palette(&["fake-tool", "not-a-thing"], &[]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        assert!(result.is_empty(), "unknown role-vocab → empty; got {result:?}");
    }

    #[test]
    fn allowed_tools_role_read_expands_to_runtime_read_and_search() {
        // Conceptual contract: role "read" means "the model may read";
        // runtime "search" is a specialized read (find pattern in tree)
        // that's implied by the broader "read" allowance.
        let p = palette(&["read"], &[]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        let mut sorted = result.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["read", "search"]);
    }

    // ─── #340: unknown role-vocab token detection ───────────────────

    #[test]
    fn unknown_role_vocab_empty_palette_returns_empty() {
        let p = palette(&[], &[]);
        assert!(unknown_role_vocab_tokens(&p).is_empty());
    }

    #[test]
    fn unknown_role_vocab_all_known_tokens_returns_empty() {
        let p = palette(&["read", "edit", "write"], &["exec", "process", "update_plan"]);
        assert!(unknown_role_vocab_tokens(&p).is_empty());
    }

    /// Typo in allow list (the canonical failure shape from #340 spec —
    /// "exce" instead of "exec" silently drops the agent's only exec
    /// capability).
    #[test]
    fn unknown_role_vocab_typo_in_allow_is_surfaced() {
        let p = palette(&["read", "exce"], &[]);
        assert_eq!(unknown_role_vocab_tokens(&p), vec!["exce".to_string()]);
    }

    /// Typos in both lists are deduplicated + sorted.
    #[test]
    fn unknown_role_vocab_dedupes_and_sorts_across_allow_and_deny() {
        let p = palette(&["fake-tool", "exce"], &["fake-tool", "another-typo"]);
        let unknowns = unknown_role_vocab_tokens(&p);
        assert_eq!(
            unknowns,
            vec![
                "another-typo".to_string(),
                "exce".to_string(),
                "fake-tool".to_string(),
            ]
        );
    }

    /// Future tokens (vendor-specific, not yet wired) are flagged as
    /// unknown — operator-facing signal that the manifest references
    /// a token darkmux doesn't know how to map yet. NOT a hard error;
    /// the dispatch can still proceed (the unknown just drops from
    /// the runtime catalog) but the operator gets warned.
    #[test]
    fn unknown_role_vocab_future_token_is_flagged() {
        let p = palette(&["read", "vendor-specific-tool"], &[]);
        assert_eq!(
            unknown_role_vocab_tokens(&p),
            vec!["vendor-specific-tool".to_string()]
        );
    }

    /// Known tokens (including those with no runtime equivalent like
    /// `process` and `update_plan`) MUST NOT be flagged. They're
    /// intentionally dropped, not accidentally.
    #[test]
    fn unknown_role_vocab_does_not_flag_known_no_runtime_tokens() {
        let p = palette(&["process", "update_plan"], &[]);
        assert!(
            unknown_role_vocab_tokens(&p).is_empty(),
            "process and update_plan are known role-vocab; should NOT be flagged"
        );
    }

    #[test]
    fn known_role_vocab_csv_contains_all_known_tokens() {
        let csv = known_role_vocab_csv();
        for token in ["read", "edit", "write", "exec", "process", "update_plan"] {
            assert!(
                csv.contains(token),
                "known_role_vocab_csv missing `{token}`; got: {csv}"
            );
        }
    }

    #[test]
    fn allowed_tools_role_exec_maps_to_runtime_bash() {
        let p = palette(&["exec"], &[]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        assert_eq!(result, vec!["bash".to_string()]);
    }

    /// QA NIT 1 — deny strips ALL of a role-vocab token's runtime
    /// expansions, not just the literal name. Pins the contract: if a
    /// future refactor switched deny to "literal-string only," role
    /// "read" denied would still leak `search` (which expands from
    /// "read"). Regression guard for the expansion-stripping invariant.
    #[test]
    fn allowed_tools_deny_role_read_strips_both_read_and_search() {
        let p = palette(&["read"], &["read"]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        assert!(
            result.is_empty(),
            "denying role-vocab `read` must strip BOTH runtime `read` and `search`; got {result:?}"
        );
    }

    /// Sibling: partial overlap. `allow:["read","exec"], deny:["read"]`
    /// must result in `["bash"]` only — both `read` and `search` removed
    /// by the deny.
    #[test]
    fn allowed_tools_deny_role_read_alongside_allowed_exec_leaves_only_bash() {
        let p = palette(&["read", "exec"], &["read"]);
        let result = compute_runtime_allowed_tools(&p).expect("non-empty palette → Some");
        assert_eq!(result, vec!["bash".to_string()]);
    }

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

    /// Regression guard for #329 — multi-byte UTF-8 characters split
    /// across reads must not corrupt to U+FFFD.
    ///
    /// The `drain_complete_lines_from_bytes` helper is the pure-
    /// function extract that makes the bug directly testable. Before
    /// the fix: pending was String; each poll did from_utf8_lossy on
    /// partial bytes; emoji split across the boundary became U+FFFD.
    /// After the fix: pending is Vec<u8>; decode happens once per
    /// complete line.
    #[test]
    fn drain_complete_lines_preserves_multibyte_across_extends() {
        let mut pending: Vec<u8> = Vec::new();

        // First chunk: prefix + first 2 bytes of 🦀.
        pending.extend_from_slice(b"{\"reasoning_text\":\"");
        pending.extend_from_slice(b"\xF0\x9F");
        let lines = drain_complete_lines_from_bytes(&mut pending);
        assert!(lines.is_empty(), "no newline yet — nothing drained");

        // Second chunk: last 2 bytes of 🦀, close out, newline.
        pending.extend_from_slice(b"\xA6\x80 reactor\"}\n");
        let lines = drain_complete_lines_from_bytes(&mut pending);
        assert_eq!(lines.len(), 1, "complete line drained");
        assert!(
            lines[0].contains("🦀 reactor"),
            "multi-byte char must round-trip intact; got: {}",
            lines[0]
        );
        assert!(
            !lines[0].contains('\u{FFFD}'),
            "no replacement chars should appear; got: {}",
            lines[0]
        );
        assert!(pending.is_empty(), "pending drained after newline");
    }

    /// Two complete lines in one buffer, plus a partial third line.
    /// The helper must drain both complete lines and leave the
    /// partial third in pending.
    #[test]
    fn drain_complete_lines_handles_multiple_lines_per_call() {
        let mut pending: Vec<u8> = b"line one\nline two\npartial".to_vec();
        let lines = drain_complete_lines_from_bytes(&mut pending);
        assert_eq!(lines, vec!["line one".to_string(), "line two".to_string()]);
        assert_eq!(pending, b"partial");
    }

    /// Empty lines (consecutive newlines) are skipped — matches the
    /// pre-fix behavior of the line-emit loop.
    #[test]
    fn drain_complete_lines_skips_empty_lines() {
        let mut pending: Vec<u8> = b"alpha\n\nbeta\n".to_vec();
        let lines = drain_complete_lines_from_bytes(&mut pending);
        assert_eq!(lines, vec!["alpha".to_string(), "beta".to_string()]);
        assert!(pending.is_empty());
    }

    /// End-to-end through TailerState: write a line with an emoji
    /// split across two polls; the tailer's handle_event sees the
    /// intact line. Verified by writing a model.completed event
    /// (which the summary DOES track) interleaved with the emoji
    /// line — the turn count proves the second line was parsed.
    #[test]
    fn tailer_state_dispatches_event_after_multibyte_split() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trajectory.jsonl");
        let mut state = fixture_state(path.clone());

        // First write: model.completed line intact + start of a
        // second line containing 🦀 (4-byte UTF-8 seq), broken
        // mid-codepoint.
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(b"{\"type\":\"model.completed\",\"seq\":1,\"finish_reason\":\"stop\"}\n")
                .unwrap();
            f.write_all(b"{\"type\":\"model.reasoning\",\"reasoning_text\":\"")
                .unwrap();
            f.write_all(b"\xF0\x9F").unwrap();
        }
        state.poll_and_emit();
        assert_eq!(state.summary.turns, 1, "first line dispatched");

        // Second write: completes the 🦀 + closes the JSON.
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"\xA6\x80 reactor\"}\n").unwrap();
        }
        state.poll_and_emit();
        // pending should be empty — both lines now drained.
        assert!(
            state.pending.is_empty(),
            "all lines drained after second poll; got pending={:?}",
            state.pending
        );
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


    // first_user_symlink_in / is_macos_firmlink tests moved to
    // `crate::workdir::tests` as part of Wave-E.2 (#255).
}
