//! (#2779) The per-tick governor seam — the two `on_sample` calls the live
//! dispatch sampler makes, in one place both it and the scenario driver
//! call.
//!
//! # Why this is a module and not four lines inline
//!
//! `run_telemetry_sampler` runs on its own thread inside a live dispatch,
//! and no test reaches it. So the ORDER of the two governor calls, and the
//! predicate that gates the second on the first, were pinned by nothing —
//! which is exactly where #2774's F1 inversion lived: the battery
//! governor's stand-down was gated on `is_pacing` (which covers tier 2's
//! `pause: false` duty cycle) instead of `is_pausing` (a genuine pause).
//! A live battery-critical pause was therefore rewritten to `pause: false`
//! for the whole duration of a duty-cycle episode, and the machine drained
//! toward 0%. That defect was proven once, by a throwaway probe, and then
//! deleted.
//!
//! Three facts live here now, and all three are testable:
//!
//! 1. **Thermal ticks FIRST**, so the battery governor reads a
//!    post-decision thermal state rather than the previous tick's.
//! 2. **The gate is `is_pausing`, not `is_pacing`.** A duty cycle is not a
//!    pause, and must not stand the battery governor down.
//! 3. **Both are fed the SAME `elapsed_ms`** — one clock, so two
//!    governors' heartbeat accounting can never disagree about how much
//!    time passed.
//!
//! The EMISSION of the resulting events stays at the sampler: this seam
//! decides, the sampler reports. That split is what keeps the seam
//! callable from a test with no flow sink.

use crate::host_source::HostReading;
use crate::power_policy::{BatteryEvent, BatteryGovernor};
use crate::thermal_governor::{ThermalEvent, ThermalGovernor};
use std::path::Path;

/// What one tick decided. Either half is independently `None` — the
/// overwhelming common case on a healthy machine is both.
#[derive(Debug, Default, PartialEq)]
pub struct TickEvents {
    pub thermal: Option<ThermalEvent>,
    pub battery: Option<BatteryEvent>,
}

impl TickEvents {
    /// `true` when this tick decided nothing at all.
    pub fn is_quiet(&self) -> bool {
        self.thermal.is_none() && self.battery.is_none()
    }
}

/// The two governors that share a tick, a clock, and a pace file.
pub struct GovernorPair {
    pub thermal: ThermalGovernor,
    pub battery: BatteryGovernor,
}

impl GovernorPair {
    pub fn new(thermal: ThermalGovernor, battery: BatteryGovernor) -> Self {
        Self { thermal, battery }
    }

    /// Feed one reading to both governors, in the live sampler's own order.
    ///
    /// `elapsed_ms` is the REAL wall gap since the previous tick (the
    /// sampler measures it; a scenario driver supplies its simulated
    /// interval). `host_out` is the dispatch's out-dir, `stop_file` the
    /// crawl `STOP` path when this dispatch is a crawl unit.
    pub fn tick(
        &mut self,
        reading: &HostReading,
        elapsed_ms: u64,
        host_out: &Path,
        stop_file: Option<&Path>,
    ) -> TickEvents {
        let thermal = self.thermal.on_sample(reading.thermal.as_ref(), elapsed_ms, host_out, stop_file);
        // (#2774 review F1) `is_pausing`, NOT `is_pacing` — read AFTER the
        // thermal tick above, so this is the post-decision state. See this
        // module's doc for what gating on `is_pacing` cost.
        let thermal_pausing = self.thermal.is_pausing();
        let battery = self.battery.on_sample(reading.battery.as_ref(), elapsed_ms, host_out, thermal_pausing);
        TickEvents { thermal, battery }
    }
}
