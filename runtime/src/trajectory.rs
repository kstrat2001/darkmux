//! Trajectory + metrics recording.
//!
//! Gives post-dispatch visibility. Writes a line-per-event JSONL trace
//! to `/workspace/.darkmux-runtime/trajectory.jsonl` plus a top-line
//! `metrics.json` at exit. Operators inspect these after the container
//! is gone (the `--rm` mode otherwise loses everything except stderr).
//!
//! The shape of each event mirrors openclaw's trajectory format
//! closely enough that a side-by-side diff between the two runtimes
//! is feasible — same `type` field, same `seq`, same `usage` shape
//! on model.completed events.
//!
//! Failure mode: if the trajectory directory can't be created or the
//! file can't be opened, the recorder degrades to a silent no-op so
//! the dispatch itself isn't blocked by an instrumentation problem.
//! Operator-visible stderr line announces success/failure.

use anyhow::Result;
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::lmstudio::{ToolCall, Usage};

/// Path inside `/workspace` where trajectory + metrics land. The
/// dot-prefix is a soft signal that this is runtime metadata rather
/// than agent content; agents that respect "don't muck with dotfiles"
/// conventions will leave it alone.
const TRAJECTORY_SUBDIR: &str = ".darkmux-runtime";
const TRAJECTORY_FILE: &str = "trajectory.jsonl";
const METRICS_FILE: &str = "metrics.json";

/// Trajectory + metrics recorder. Open at dispatch start; methods
/// append events as they occur; `save_metrics()` writes the final
/// summary at exit.
pub struct Trajectory {
    /// `None` when recording is disabled (open() failed; degraded
    /// silently). All append methods become no-ops in that case.
    file: Option<File>,
    metrics_path: Option<PathBuf>,
    started: Instant,
}

/// Top-line summary written to metrics.json at dispatch exit.
#[derive(Debug, Serialize)]
pub struct Metrics {
    pub runtime: &'static str,
    pub version: &'static str,
    pub model: String,
    pub started_at_unix_ms: u64,
    pub wall_ms: u128,
    pub result: String,
    pub turns: u32,
    pub compactions: u32,
    pub total_prompt_tokens: u32,
    pub total_completion_tokens: u32,
    pub total_messages: usize,
    pub max_turns_reached: bool,
    /// First 400 chars of the final assistant message (for at-a-glance
    /// "what did the agent end up saying"). Truncated to keep
    /// metrics.json human-readable in a terminal.
    pub final_assistant_preview: String,
}

impl Trajectory {
    /// Open a trajectory file at `<workspace>/.darkmux-runtime/`. If
    /// the directory can't be created (permission, missing workspace,
    /// etc.) returns a degraded no-op recorder rather than failing.
    pub fn open(workspace: &Path) -> Self {
        let dir = workspace.join(TRAJECTORY_SUBDIR);
        let trajectory_path = dir.join(TRAJECTORY_FILE);
        let metrics_path = dir.join(METRICS_FILE);

        match try_open(&dir, &trajectory_path) {
            Ok(file) => {
                eprintln!(
                    "darkmux-runtime: trajectory → {}",
                    trajectory_path.display()
                );
                Self {
                    file: Some(file),
                    metrics_path: Some(metrics_path),
                    started: Instant::now(),
                }
            }
            Err(e) => {
                eprintln!(
                    "darkmux-runtime: trajectory recording disabled ({e}); \
                     dispatch will continue without it"
                );
                Self {
                    file: None,
                    metrics_path: None,
                    started: Instant::now(),
                }
            }
        }
    }

    /// dispatch.start — first event in the trajectory.
    pub fn append_dispatch_start(
        &mut self,
        model: &str,
        system_chars: usize,
        prompt_chars: usize,
    ) {
        self.write_event(&serde_json::json!({
            "type": "dispatch.start",
            "ts": unix_ms(),
            "model": model,
            "system_chars": system_chars,
            "prompt_chars": prompt_chars,
        }));
    }

    /// model.completed — one per chat-completion response. Mirrors
    /// openclaw's `model.completed` event shape (seq + finish_reason +
    /// usage) so trajectories can be diffed cross-runtime.
    pub fn append_model_completed(
        &mut self,
        seq: u32,
        finish_reason: &str,
        usage: Option<&Usage>,
        tool_calls: Option<&[ToolCall]>,
    ) {
        let usage_json = usage.map(|u| {
            serde_json::json!({
                "prompt_tokens": u.prompt_tokens,
                "completion_tokens": u.completion_tokens,
                "total_tokens": u.total_tokens,
            })
        });
        let tool_calls_json = tool_calls.map(|calls| {
            calls
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "id": c.id,
                        "name": c.function.name,
                        "arguments_chars": c.function.arguments.len(),
                    })
                })
                .collect::<Vec<_>>()
        });
        self.write_event(&serde_json::json!({
            "type": "model.completed",
            "seq": seq,
            "ts": unix_ms(),
            "finish_reason": finish_reason,
            "usage": usage_json,
            "tool_calls": tool_calls_json,
        }));
    }

    /// tool.completed — one per executed tool call. Records the
    /// tool name + arg/result sizes (not the content — that can be
    /// large; the trajectory is for shape, not for full payload).
    pub fn append_tool_completed(
        &mut self,
        seq: u32,
        tool_seq: u32,
        tool_name: &str,
        args_chars: usize,
        result_chars: usize,
    ) {
        self.write_event(&serde_json::json!({
            "type": "tool.completed",
            "seq": seq,
            "tool_seq": tool_seq,
            "tool_name": tool_name,
            "args_chars": args_chars,
            "result_chars": result_chars,
            "ts": unix_ms(),
        }));
    }

    /// compaction — fires when middle-replace compaction runs. Records
    /// the size delta so the operator can verify compaction is actually
    /// shrinking the conversation.
    pub fn append_compaction(
        &mut self,
        generation: u32,
        before_message_count: usize,
        after_message_count: usize,
        summary_chars: usize,
    ) {
        self.write_event(&serde_json::json!({
            "type": "compaction",
            "generation": generation,
            "ts": unix_ms(),
            "before_messages": before_message_count,
            "after_messages": after_message_count,
            "summary_chars": summary_chars,
        }));
    }

    /// dispatch.complete — last event in the trajectory. Records the
    /// terminal outcome + wall time.
    pub fn append_dispatch_complete(&mut self, result: &str, wall_ms: u128) {
        self.write_event(&serde_json::json!({
            "type": "dispatch.complete",
            "ts": unix_ms(),
            "result": result,
            "wall_ms": wall_ms,
        }));
    }

    /// Save the metrics.json summary. Called once at dispatch exit.
    pub fn save_metrics(&mut self, metrics: &Metrics) -> Result<()> {
        let Some(path) = self.metrics_path.as_ref() else {
            return Ok(());
        };
        let json = serde_json::to_string_pretty(metrics)?;
        fs::write(path, json)?;
        eprintln!("darkmux-runtime: metrics → {}", path.display());
        Ok(())
    }

    /// Wall time since the trajectory was opened. Useful for the
    /// dispatch.complete event.
    pub fn elapsed_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }

    /// Internal: append one event to the JSONL file. Silently drops
    /// the write if the file isn't open (recorder degraded to no-op).
    /// Append errors are emitted to stderr but don't propagate up —
    /// the dispatch shouldn't fail because of an instrumentation
    /// problem.
    fn write_event(&mut self, event: &serde_json::Value) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let mut line = serde_json::to_string(event).unwrap_or_default();
        line.push('\n');
        if let Err(e) = file.write_all(line.as_bytes()) {
            eprintln!("darkmux-runtime: trajectory write failed: {e}");
        }
    }
}

fn try_open(dir: &Path, path: &Path) -> std::io::Result<File> {
    fs::create_dir_all(dir)?;
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempdir::TempDir;

    #[test]
    fn open_creates_dot_dir_and_file() {
        let ws = TempDir::new("traj-test").unwrap();
        let _t = Trajectory::open(ws.path());
        let traj_file = ws
            .path()
            .join(TRAJECTORY_SUBDIR)
            .join(TRAJECTORY_FILE);
        assert!(traj_file.exists(), "trajectory file should be created");
    }

    #[test]
    fn append_events_writes_jsonl() {
        let ws = TempDir::new("traj-test-2").unwrap();
        let mut t = Trajectory::open(ws.path());
        t.append_dispatch_start("test-model", 100, 50);
        t.append_model_completed(1, "stop", None, None);
        drop(t);

        let traj_file = ws
            .path()
            .join(TRAJECTORY_SUBDIR)
            .join(TRAJECTORY_FILE);
        let body = fs::read_to_string(&traj_file).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        // Each line should parse as JSON
        for line in &lines {
            let parsed: serde_json::Value =
                serde_json::from_str(line).expect("each line is valid JSON");
            assert!(parsed["type"].is_string());
            assert!(parsed["ts"].is_number());
        }
    }

    #[test]
    fn open_no_op_when_dir_unwritable() {
        // Point at a path under root that we can't create (assuming
        // tests don't run as root). Recorder should degrade silently.
        let bad = Path::new("/proc/cannot-create-this/please");
        let mut t = Trajectory::open(bad);
        // This shouldn't panic or fail:
        t.append_dispatch_start("model", 0, 0);
        // metrics save should also be a no-op:
        let m = Metrics {
            runtime: "darkmux-runtime",
            version: "0.1.0",
            model: "test".into(),
            started_at_unix_ms: 0,
            wall_ms: 0,
            result: "stop".into(),
            turns: 0,
            compactions: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_messages: 0,
            max_turns_reached: false,
            final_assistant_preview: "".into(),
        };
        t.save_metrics(&m).unwrap();
    }
}
