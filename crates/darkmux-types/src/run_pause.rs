//! (#2902 step 5) Time this process spent deliberately paused by darkmux
//! itself, which a run's own time limits must not count.
//!
//! A budget `wait` holds a call to an endpoint until its rolling window has
//! room. That wait is darkmux obeying the operator's budget, not the run
//! stalling or overrunning, so the run's wall-clock bound
//! (`runtime.mission_wall_clock_timeout_seconds`, watched by
//! `launch_guard::spawn_wall_clock_watchdog`) is extended by every
//! millisecond recorded here. It is the host-side twin of the thermal
//! governor's pause, whose rests the runtime absorbs into its own
//! inactivity clock (`runtime/src/loop_runner.rs`,
//! `absorb_rest_into_soft_inactivity_clock`).
//!
//! Process-wide and monotonic: a waiter adds each slice as it sleeps, so a
//! watchdog reading [`total_ms`] mid-wait already sees the extension.

use std::sync::atomic::{AtomicU64, Ordering};

static PAUSED_MS: AtomicU64 = AtomicU64::new(0);

/// Record `ms` of deliberate pause.
pub fn add(ms: u64) {
    PAUSED_MS.fetch_add(ms, Ordering::SeqCst);
}

/// Every millisecond of deliberate pause this process has recorded.
pub fn total_ms() -> u64 {
    PAUSED_MS.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    #[test]
    fn add_accumulates() {
        let before = super::total_ms();
        super::add(250);
        super::add(750);
        assert!(super::total_ms() >= before + 1_000);
    }
}
