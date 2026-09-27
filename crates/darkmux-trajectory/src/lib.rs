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

/// `<out_dir>/.darkmux-runtime/trajectory.jsonl`.
pub fn trajectory_path(out_dir: &std::path::Path) -> std::path::PathBuf {
    out_dir.join(TRAJECTORY_SUBDIR).join(TRAJECTORY_FILE)
}

#[cfg(test)]
mod tests;
