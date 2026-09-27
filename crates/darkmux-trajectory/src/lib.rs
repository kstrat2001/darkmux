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

pub mod event;
pub mod fold;
pub mod legacy;
pub mod usage;

pub use event::*;
pub use fold::*;
pub use usage::{TokenSum, Usage, UsageCounts};

/// The runtime's bookkeeping directory, under its out-dir mount. The dot
/// marks it as runtime metadata rather than agent content.
pub const TRAJECTORY_SUBDIR: &str = ".darkmux-runtime";

/// The trajectory file inside [`TRAJECTORY_SUBDIR`].
pub const TRAJECTORY_FILE: &str = "trajectory.jsonl";

/// The counters of a runtime checkpoint (`<out_dir>/checkpoint.json`) the
/// host reads. The loop seeds its turn cap from them across a resume; the
/// host adds what the resumed run recorded to report the whole task's
/// count. The runtime's own `RunCheckpoint` carries the same two fields
/// (pinned by a test in the runtime crate).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct CheckpointCounts {
    pub turns: u32,
    pub compactions: u32,
}

impl CheckpointCounts {
    /// The counters of a checkpoint's JSON text; zero for one that does not
    /// parse (the host validated it before resuming from it).
    pub fn of(checkpoint_json: &str) -> Self {
        serde_json::from_str(checkpoint_json).unwrap_or_default()
    }
}

/// `<out_dir>/.darkmux-runtime/trajectory.jsonl`.
pub fn trajectory_path(out_dir: &std::path::Path) -> std::path::PathBuf {
    out_dir.join(TRAJECTORY_SUBDIR).join(TRAJECTORY_FILE)
}

#[cfg(test)]
mod tests;
