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
pub use usage::{estimate_tokens, TokenSum, Usage, UsageCounts, CHARS_PER_TOKEN};

/// The runtime's bookkeeping directory, under its out-dir mount. The dot
/// marks it as runtime metadata rather than agent content.
pub const TRAJECTORY_SUBDIR: &str = ".darkmux-runtime";

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

    /// THE whole task's turn count after a run resumed from this checkpoint
    /// recorded `fold`. The runtime numbers every call's `seq` with the
    /// task's own turn counter, seeded from the checkpoint, so the count is
    /// the later of the checkpoint's and the last seq the run recorded. A
    /// hand-back resume continues turn N (`seq: N`) and a clean one starts
    /// N+1, so adding the run's own turn count to the seed would count the
    /// continued turn twice.
    pub fn cumulative_turns(&self, fold: &TrajectoryFold) -> u32 {
        let last = fold.last_turn_seq().map_or(0, |s| u32::try_from(s).unwrap_or(u32::MAX));
        self.turns.max(last)
    }

    /// The whole task's compactions: each is its own event, never
    /// continued across a resume, so the checkpoint's plus the run's.
    pub fn cumulative_compactions(&self, fold: &TrajectoryFold) -> u32 {
        self.compactions.saturating_add(fold.compactions())
    }
}

/// `<out_dir>/.darkmux-runtime/trajectory.jsonl`.
pub fn trajectory_path(out_dir: &std::path::Path) -> std::path::PathBuf {
    out_dir.join(TRAJECTORY_SUBDIR).join(TRAJECTORY_FILE)
}

#[cfg(test)]
mod tests;
