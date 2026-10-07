//! darkmux's trajectory: the event log one role execution writes, read
//! through one fold.
//!
//! The runtime (`runtime/`, built into the darkmux-runtime image) writes
//! [`TrajectoryEvent`]s to `<out>/.darkmux-runtime/trajectory.jsonl` as they
//! happen. The host tails that file live and the lab reads it after the
//! fact; both parse the same type and count through [`TrajectoryFold`]. The
//! trajectory is the only per-execution record of turns, rests and
//! per-call usage; the host turns each call's usage into its usage record
//! (`telemetry.tokens`), and every token sum is a sum of those.

pub mod envelope;
pub mod event;
pub mod fold;
pub mod usage;

pub use envelope::{FailedExec, ResumedFrom, RuntimeEnvelope};
pub use event::*;
pub use fold::*;
pub use usage::{estimate_tokens, TokenSum, Usage, UsageCounts, CHARS_PER_TOKEN};

/// The runtime's bookkeeping directory, under its out-dir mount. The dot
/// marks it as runtime metadata rather than agent content.
pub const TRAJECTORY_SUBDIR: &str = ".darkmux-runtime";

/// (#3035) The data-shape version of the trajectory, written in the
/// `trajectory.header` event that opens every file. Independent of the
/// release number; a minor bump adds a field or an event type, a major bump
/// renames or retypes one. The host (which can compare versions through
/// `darkmux_types::data_version`) refuses to inspect a trajectory whose
/// header is newer than this; a file with no header predates the marker.
pub const TRAJECTORY_SCHEMA_VERSION: &str = "1.0";

/// The trajectory file inside [`TRAJECTORY_SUBDIR`].
pub const TRAJECTORY_FILE: &str = "trajectory.jsonl";

/// Uniqueness ratio below which a reasoning slice is degenerate: the
/// threshold the runtime's detector judges with, and the one a reader
/// re-derives a run's degenerate set from.
///
/// Deliberately far from BOTH measured clusters. Real productive reasoning
/// (the 40,608-char pepper-grinder turn, its body alone, a resumed
/// continuation, a checkpoint accumulation) all scored **1.000**; synthetic
/// loops scored **0.013-0.015**. Anything in 0.1-0.8 separates them, so 0.25
/// sits with margin on both sides. It is kept low because the costs are
/// asymmetric: a false CLEAN costs one more checkpoint, while a false
/// DEGENERATE destroys an analysis pass.
pub const DEGENERATE_TAIL_RATIO: f64 = 0.25;

/// `<out_dir>/.darkmux-runtime/trajectory.jsonl`.
pub fn trajectory_path(out_dir: &std::path::Path) -> std::path::PathBuf {
    out_dir.join(TRAJECTORY_SUBDIR).join(TRAJECTORY_FILE)
}

/// Close a trajectory whose execution ended without recording its own end:
/// append a `dispatch.complete` with `result: "interrupted"`
/// ([`RESULT_INTERRUPTED`]), clocked at the last event and carrying the wall
/// time the events cover. For a copy the host preserved after the execution
/// was killed (SIGKILL cannot be caught, so the runtime wrote no terminal
/// record). Returns `Ok(true)` when it appended, `Ok(false)` when there is
/// nothing to close (no events, or a terminal record already present). A
/// final line cut short by the kill — including one torn mid-character,
/// which is not valid UTF-8 — is left in place, ended, and ignored by
/// every reader.
pub fn close_if_unterminated(path: &std::path::Path) -> std::io::Result<bool> {
    use std::io::Write;
    let bytes = std::fs::read(path)?;
    let raw = String::from_utf8_lossy(&bytes);
    let fold = TrajectoryFold::from_lines(&raw);
    if fold.events == 0 || fold.complete.is_some() {
        return Ok(false);
    }
    let event = TrajectoryEvent::DispatchComplete(DispatchComplete {
        ts: fold.last_ts.unwrap_or(0),
        result: RESULT_INTERRUPTED.to_string(),
        wall_ms: fold.wall_ms().unwrap_or(0),
        turn_delay_effective_ms: None,
    });
    let mut line = String::new();
    if !raw.ends_with('\n') {
        line.push('\n');
    }
    line.push_str(&serde_json::to_string(&event).map_err(std::io::Error::other)?);
    line.push('\n');
    std::fs::OpenOptions::new().append(true).open(path)?.write_all(line.as_bytes())?;
    Ok(true)
}

#[cfg(test)]
mod tests;
