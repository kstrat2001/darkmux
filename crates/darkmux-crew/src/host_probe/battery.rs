//! (#2705) Battery: fast CHARGE telemetry, and slow HEALTH inventory.
//!
//! Modeled on [`super::thermal`], whose contract binds here:
//!
//! - **In-process kernel reads, not shell-outs.** `pmset -g batt` is a
//!   process spawn that prints a human sentence over the very API this
//!   module calls directly: `IOPSCopyPowerSourcesInfo`.
//!
//!   **The two halves read from two different sources, and which one goes
//!   where was decided by measurement, not by taste.** The first
//!   implementation read BOTH halves from the `AppleSmartBattery`
//!   IORegistry node, because that node carries everything. It also costs
//!   ~4 ms per read — `IORegistryEntryCreateCFProperties` materializes that
//!   node's ENTIRE property tree (`BatteryData`, `LifetimeData`,
//!   `PortControllerInfo`, `PowerTelemetryData`, `AdapterDetails`, the raw
//!   resistance tables…) to hand back five scalars. Measured on the
//!   reference machine, that took the whole host probe from **6.10 ms mean
//!   / 7 ms max to 10.25 ms / 12 ms** — a 68% increase on a path that runs
//!   every sampler tick, for a reading that is five numbers wide.
//!
//!   So the FAST half reads `IOPSCopyPowerSourcesInfo` instead — the small,
//!   purpose-built power-source dictionary, measured at **~0.1 ms** — and
//!   the expensive registry walk moved to the HOURLY health path, where it
//!   is genuinely needed (cycle count, the raw capacity counters and the
//!   lifetime blob exist nowhere else) and where 4 ms once an hour is
//!   nothing. That is the split the observer-cost contract asks for: the
//!   observer's price is paid where the data actually justifies it.
//! - **Degrades to `None` independently.** No battery source is worth a
//!   panic, and a machine with no battery at all — every Mac Studio, mini
//!   and Pro — reports `None` cleanly for both halves, exactly as a machine
//!   with no thermal source does. `None` here is a first-class answer, not
//!   an error: #2706's gate is INERT on it by construction.
//! - **Zero model dispatches**, and the caller stamps the probe's own cost
//!   into the artifact so "the observer was negligible" stays a checkable
//!   fact (`HostSampleFull::cost_ms`).
//!
//! # The two populations move at completely different rates
//!
//! **Charge** — percentage, on-AC, charging, time-to-empty — is fast
//! telemetry and rides the existing host sample beside CPU and thermal.
//!
//! **Health** — cycle count, capacity against design, reported condition,
//! and the battery's own cumulative time-at-state-of-charge counters —
//! moves over WEEKS. Sampling it on the telemetry cadence is a kernel read
//! per tick for a value unchanged since the last thousand ticks, and a ring
//! full of identical rows. So health is polled on a long cadence
//! ([`HEALTH_POLL_INTERVAL_MS`], plus once at daemon start) and RECORDED
//! ONLY ON CHANGE — the same edge-triggered shape `machine.thermal` already
//! uses. Hour-over-hour capacity differences are below the noise floor;
//! emitting every poll would add 24 identical rows a day. Per the
//! observability contract the cadence is a RECORDED KNOB: the poll interval
//! rides in the emitted payload, so an artifact says what cadence produced
//! it and a tightened debug cadence is visible in the data.
//!
//! # Two capacity readings, both recorded, each labeled by its source
//!
//! macOS's own "Maximum Capacity" figure and the raw counter ratio differ
//! materially, so this module records what it READ and names the keys,
//! rather than picking one and calling it "capacity". Measured on the
//! reference machine (2026-09-15, 26 cycles):
//!
//! | reading | keys | value |
//! |---|---|---|
//! | raw | `AppleRawMaxCapacity` / `DesignCapacity` | 5648 / 6249 = **90.4%** |
//! | nominal | `NominalChargeCapacity` / `DesignCapacity` | 5800 / 6249 = **92.8%** |
//! | what macOS Settings displayed | — | **96%** |
//!
//! The third reproduces from NEITHER pair of counters — macOS derives it by
//! a formula Apple does not document. So this module records the two it can
//! stand behind, each labeled with the keys it came from, and does not
//! synthesize the third from a guess. That is the same describe-the-
//! mechanism posture the rest of the project takes: a number whose
//! derivation can be stated survives a macOS release; one reverse-
//! engineered from a single machine does not.
//!
//! # No advice
//!
//! darkmux reports the numbers. Nothing here warns that a battery is
//! unhealthy, recommends a charge policy, or editorializes about what a
//! cycle count means. #2706's gate is the one action taken on any of it,
//! and it enforces a threshold the OPERATOR wrote.

/// The IORegistry class every Mac laptop's battery publishes, and no
/// desktop does. Absence of this node is exactly "this machine has no
/// battery" — not an error, not a zero.
pub const BATTERY_SERVICE_CLASS: &str = "AppleSmartBattery";

/// `AppleSmartBattery`'s sentinel for "the OS has no estimate right now" on
/// `TimeRemaining`/`AvgTimeToEmpty`/`AvgTimeToFull` — `0xFFFF`, the
/// unsigned-16-bit max the underlying gas-gauge register reports when the
/// estimator has not settled. Observed live on the reference machine while
/// on AC (`"TimeRemaining" = 65535`).
///
/// This is the analogue of [`super::thermal::ThermalSample`]'s
/// `cpu_speed_limit_pct` documenting that `100` means "no cap recorded":
/// the OS declining to give a value must read as ABSENT, never as the
/// number zero, because "no estimate" and "zero minutes left" are opposite
/// claims and a consumer renders them differently.
pub const TIME_ESTIMATE_UNAVAILABLE: i64 = 65535;

/// Health poll cadence — **once an hour**, plus one read at daemon start so
/// a short-lived daemon still contributes a reading.
///
/// Set by how fast the values actually move, not by what is affordable.
/// Cycle count increments at most once or twice a day on a laptop, so
/// hourly oversamples it by an order of magnitude and can never miss one;
/// maximum capacity is recalculated by the OS around charge cycles and
/// moves visibly at day-to-day resolution at best. The cost is one
/// sub-millisecond IOKit read, so 24 reads a day is negligible either way.
///
/// **Polling is not emitting.** See the module doc: a poll whose values
/// match the last one emits nothing.
pub const HEALTH_POLL_INTERVAL_MS: u64 = 60 * 60 * 1000;

/// One CHARGE reading — the fast half, sampled on the host-telemetry
/// cadence beside CPU and thermal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatterySample {
    /// 0-100. Derived as `CurrentCapacity * 100 / MaxCapacity` rather than
    /// read from one key: on Apple Silicon `CurrentCapacity` is ALREADY a
    /// percent and `MaxCapacity` is the constant 100, while on Intel both
    /// are mAh. The ratio is correct on both, and needs no per-architecture
    /// branch that would be untestable on whichever machine ran the tests.
    pub charge_pct: u8,
    /// `ExternalConnected` — a charger is attached. Note this is NOT the
    /// negation of `charging`: a laptop sitting on AC at 100% reports
    /// `on_ac: true, charging: false`, which is the reference machine's own
    /// steady state.
    pub on_ac: bool,
    /// `IsCharging` — current is actually flowing into the pack.
    pub charging: bool,
    /// Minutes until empty, when the OS supplies an estimate.
    ///
    /// **`None` is the common case, and is not a failure**: absent on AC
    /// (there is no discharge to estimate), absent while charging, and
    /// absent during the estimator's settling period after a power
    /// transition, where the gas gauge reports
    /// [`TIME_ESTIMATE_UNAVAILABLE`]. It reads absent rather than zero —
    /// see that constant's own doc.
    pub minutes_to_empty: Option<u32>,
}

/// One HEALTH reading — the slow half, polled on
/// [`HEALTH_POLL_INTERVAL_MS`] and recorded only when it CHANGES.
///
/// Every field is independently optional: a key IOKit stops publishing, or
/// publishes at a different CF type, yields an absent field rather than a
/// fabricated one. `PartialEq` is what the edge detector compares, so
/// adding a field here automatically widens what counts as a change.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BatteryHealth {
    /// `CycleCount`.
    pub cycle_count: Option<u64>,
    /// `DesignCapacity`, in mAh — the pack's rating when new.
    pub design_capacity_mah: Option<u64>,
    /// `AppleRawMaxCapacity`, in mAh — the RAW full-charge capacity. See
    /// the module doc's capacity table for why this is recorded alongside
    /// the nominal figure instead of either being called "capacity".
    pub raw_max_capacity_mah: Option<u64>,
    /// `NominalChargeCapacity`, in mAh.
    pub nominal_charge_capacity_mah: Option<u64>,
    /// The OS's own reported condition word, verbatim from
    /// `IOPSCopyPowerSourcesInfo`'s `BatteryHealth` key (`"Good"`,
    /// `"Fair"`, `"Poor"`, `"Check Battery"` — whatever this macOS
    /// publishes).
    ///
    /// **This key is demonstrably unreliable on Apple Silicon and must
    /// never be presented as THE condition.** Measured on the reference
    /// machine (2026-09-23): this key reads `"Check Battery"` while both
    /// `system_profiler SPPowerDataType` (`Condition: Normal`) and `pmset
    /// -g rawbatt` agree the battery is healthy, and `PermanentFailureStatus`
    /// — the field a genuine permanent-failure/service condition sets — is
    /// `0`. The legacy IOKit enum is widely reported (other battery-health
    /// tools work around it the same way) to lag/misreport on Apple Silicon
    /// packs; System Settings does not appear to derive its displayed
    /// condition from it. Recorded here as read, for completeness and
    /// debugging, but [`Self::condition_word`] — derived from
    /// `permanent_failure_status`, the signal `pmset`/`system_profiler`
    /// agree with — is the field a UI should show as "condition". darkmux
    /// does not map this raw word to a verdict of its own, and an
    /// unrecognized future value is passed through rather than clamped into
    /// a known one, the same posture
    /// [`super::thermal::thermal_state_name`] takes.
    pub condition: Option<String>,
    /// `PermanentFailureStatus`, verbatim (`0` = no permanent failure
    /// detected). This is the signal `pmset -g batt`/`system_profiler`'s
    /// "Condition: Normal" agrees with on the reference machine, unlike
    /// `condition` above. See [`Self::condition_word`].
    pub permanent_failure_status: Option<i64>,
    /// `Temperature`, converted from the node's hundredths-of-a-degree
    /// units to degrees Celsius (`3094` -> `30.94`).
    pub temperature_c: Option<f64>,
    /// The battery's OWN cumulative `TimeAtHighSoc` counters, in hours,
    /// verbatim as a flat little-endian `u32` array.
    ///
    /// **Read from the pack's lifetime counters, not accumulated by
    /// darkmux** — so it survives restarts and does not depend on the
    /// daemon having been running, which is what makes it answer "how long
    /// has this battery sat at high charge" rather than "how long was
    /// darkmux watching".
    ///
    /// The buckets are recorded whole and UNLABELED: the array partitions
    /// operating time by state-of-charge band (28 values, 4 groups of 7, on
    /// the reference machine), but Apple documents neither the band edges
    /// nor the grouping, so naming any subset "high" would be a guess
    /// presented as a reading. The UNIT is not a guess: the buckets summed
    /// to 5181 against a `TotalOperatingTime` of 5178 on the reference
    /// machine (2026-09-15), which is what establishes hours.
    pub time_at_soc_hours: Option<Vec<u32>>,
    /// `LifetimeData.TotalOperatingTime`, in hours — recorded beside the
    /// buckets because it is the cross-check that gives them their unit
    /// (see [`Self::time_at_soc_hours`]).
    pub total_operating_time_hours: Option<u64>,
}

impl BatteryHealth {
    /// True when no field resolved at all. A reading this empty is treated
    /// as "no battery health source" rather than recorded as a machine fact
    /// with every field null — an all-absent row is noise, not an
    /// observation.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// `AppleRawMaxCapacity / DesignCapacity`, as a percent rounded to one
    /// decimal. See the module doc: labeled RAW because it is not the
    /// figure macOS Settings displays.
    pub fn raw_capacity_pct(&self) -> Option<f64> {
        capacity_pct(self.raw_max_capacity_mah, self.design_capacity_mah)
    }

    /// `NominalChargeCapacity / DesignCapacity`, as a percent rounded to
    /// one decimal. Labeled NOMINAL for the same reason.
    pub fn nominal_capacity_pct(&self) -> Option<f64> {
        capacity_pct(self.nominal_charge_capacity_mah, self.design_capacity_mah)
    }

    /// The condition word a UI should show as THE condition — `"Normal"` /
    /// `"Service Battery"`, derived from `permanent_failure_status` rather
    /// than the unreliable raw `condition` string (see that field's own
    /// doc for the measurement backing this). `None` when
    /// `permanent_failure_status` itself was not read (an older macOS
    /// node, or the key genuinely absent) — a caller with no computed word
    /// falls back to showing `condition` labeled precisely as the power
    /// source's own report, never silently as "Normal".
    pub fn condition_word(&self) -> Option<&'static str> {
        match self.permanent_failure_status? {
            0 => Some("Normal"),
            _ => Some("Service Battery"),
        }
    }
}

/// `numerator / design` as a percent to one decimal. `None` when either
/// side is absent or `design` is zero — never a division that reports
/// infinity or a silent zero.
fn capacity_pct(numerator: Option<u64>, design: Option<u64>) -> Option<f64> {
    let (n, d) = (numerator?, design?);
    if d == 0 {
        return None;
    }
    Some(((n as f64 / d as f64) * 1000.0).round() / 10.0)
}

/// Charge percent from the two capacity counters. `None` when either is
/// absent or `max` is zero — a machine that cannot report a denominator
/// reports no charge, rather than a percentage of nothing.
///
/// Clamped to 0-100: the gas gauge can briefly read slightly over its own
/// max right after a full charge, and a `charge_pct` of 103 would be a
/// worse answer than 100 for every consumer, #2706's floor comparison
/// included.
pub fn charge_pct_from(current: Option<i64>, max: Option<i64>) -> Option<u8> {
    let (c, m) = (current?, max?);
    if m <= 0 || c < 0 {
        return None;
    }
    Some(((c.saturating_mul(100) / m).clamp(0, 100)) as u8)
}

/// The time-to-empty rule, extracted so every absent case is pinned without
/// a battery: absent on AC, absent while charging, absent at the OS's
/// [`TIME_ESTIMATE_UNAVAILABLE`] sentinel (or anything at/past it), and
/// absent at zero or below.
///
/// Zero is treated as absent deliberately. The gas gauge reports `0` during
/// the estimator's settling window after a power transition, and a consumer
/// reading "0 minutes remaining" on a laptop at 80% charge is worse than
/// reading "no estimate yet" — the issue's own requirement that this "must
/// read absent rather than zero". The cost is that a genuinely-empty
/// machine's final minute reads absent too; it is about to sleep either
/// way, and no consumer acts on that last sample.
pub fn minutes_to_empty_from(raw: Option<i64>, on_ac: bool, charging: bool) -> Option<u32> {
    if on_ac || charging {
        return None;
    }
    let v = raw?;
    if v <= 0 || v >= TIME_ESTIMATE_UNAVAILABLE {
        return None;
    }
    Some(v as u32)
}

/// Parse a `TimeAtHighSoc` CFData blob as a flat little-endian `u32` array.
/// `None` for an empty blob or one whose length is not a whole number of
/// `u32`s — a shape this code does not recognize is reported as absent
/// rather than parsed to whatever prefix happens to fit.
pub fn parse_time_at_soc(bytes: &[u8]) -> Option<Vec<u32>> {
    if bytes.is_empty() || bytes.len() % 4 != 0 {
        return None;
    }
    Some(bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod imp {
    use super::{BatteryHealth, BatterySample, BATTERY_SERVICE_CLASS};
    use crate::host_probe::iokit;
    use std::ffi::c_void;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPSCopyPowerSourcesInfo() -> *const c_void;
        fn IOPSCopyPowerSourcesList(blob: *const c_void) -> *const c_void;
        fn IOPSGetPowerSourceDescription(blob: *const c_void, ps: *const c_void) -> *const c_void;
    }

    /// `IOPSGetPowerSourceDescription`'s key for the OS's own condition
    /// word. Present only on an internal battery, which is what makes it
    /// the marker [`with_internal_power_source`] selects on.
    const K_HEALTH: &str = "BatteryHealth";
    const K_CURRENT_CAPACITY: &str = "Current Capacity";
    const K_MAX_CAPACITY: &str = "Max Capacity";
    const K_IS_CHARGING: &str = "Is Charging";
    const K_POWER_SOURCE_STATE: &str = "Power Source State";
    const K_TIME_TO_EMPTY: &str = "Time to Empty";
    /// `kIOPSACPowerValue` — the `Power Source State` string for "a charger
    /// is attached". Compared verbatim; an unrecognized future value reads
    /// as "not on AC" rather than being guessed at.
    const V_AC_POWER: &str = "AC Power";

    /// Hand each power source's description dictionary to `f`, stopping at
    /// the first that produces a value.
    ///
    /// The list includes UPSs and (on some Macs) attached accessories, so
    /// `f` is responsible for recognizing the internal battery; both
    /// callers do that by requiring the `BatteryHealth` key, which only an
    /// internal battery publishes. A machine with no battery never calls
    /// `f` successfully at all — the `None` that makes every caller degrade
    /// cleanly, and the one #2706's gate reads as "inert".
    fn with_internal_power_source<T>(
        mut f: impl FnMut(iokit::CFDictionaryRef) -> Option<T>,
    ) -> Option<T> {
        // SAFETY: `blob`/`list` are owned CF objects released on every exit
        // path; `IOPSGetPowerSourceDescription` returns a BORROWED
        // dictionary valid while `blob` lives, so it is only read before
        // either release. Every accessor type-checks before dereferencing.
        unsafe {
            let blob = IOPSCopyPowerSourcesInfo();
            if blob.is_null() {
                return None;
            }
            let list = IOPSCopyPowerSourcesList(blob);
            if list.is_null() {
                iokit::release(blob);
                return None;
            }
            let mut found = None;
            for i in 0..iokit::array_count(list) {
                let ps = iokit::array_at(list, i);
                if ps.is_null() {
                    continue;
                }
                let desc = IOPSGetPowerSourceDescription(blob, ps);
                if desc.is_null() || iokit::dict_string(desc, K_HEALTH).is_none() {
                    continue;
                }
                found = f(desc);
                if found.is_some() {
                    break;
                }
            }
            iokit::release(list);
            iokit::release(blob);
            found
        }
    }

    /// One charge reading, from the small `IOPSCopyPowerSourcesInfo`
    /// dictionary (~0.1 ms — see the module doc for why this is NOT the
    /// `AppleSmartBattery` walk the health half uses).
    ///
    /// `None` on a machine with no battery, and also when the source exists
    /// but publishes no usable capacity pair (see `charge_pct_from`) — a
    /// sample with no percent has nothing for the other three fields to
    /// qualify.
    pub fn sample() -> Option<BatterySample> {
        with_internal_power_source(|desc| {
            // SAFETY: `desc` is a live CFDictionary for the duration of the
            // callback, and every accessor type-checks before dereferencing.
            unsafe {
                let charge_pct = super::charge_pct_from(
                    iokit::dict_i64(desc, K_CURRENT_CAPACITY),
                    iokit::dict_i64(desc, K_MAX_CAPACITY),
                )?;
                let on_ac = iokit::dict_string(desc, K_POWER_SOURCE_STATE)
                    .map(|s| s == V_AC_POWER)
                    .unwrap_or(false);
                let charging = iokit::dict_bool(desc, K_IS_CHARGING).unwrap_or(false);
                Some(BatterySample {
                    charge_pct,
                    on_ac,
                    charging,
                    // IOPS reports `-1` while the estimator has not
                    // settled, where `AppleSmartBattery` reports 65535 —
                    // `minutes_to_empty_from` rejects BOTH (`v <= 0` and
                    // `v >= TIME_ESTIMATE_UNAVAILABLE`), so the absent rule
                    // holds whichever source a future revision reads.
                    minutes_to_empty: super::minutes_to_empty_from(
                        iokit::dict_i64(desc, K_TIME_TO_EMPTY),
                        on_ac,
                        charging,
                    ),
                })
            }
        })
    }

    /// The OS's own reported condition word (`BatteryHealth`), verbatim.
    fn reported_condition() -> Option<String> {
        // SAFETY: see `with_internal_power_source` — `desc` is live for the
        // callback and `dict_string` type-checks.
        with_internal_power_source(|desc| unsafe { iokit::dict_string(desc, K_HEALTH) })
    }

    /// One health reading.
    ///
    /// This is the half that pays for the `AppleSmartBattery` registry walk
    /// (~4 ms), because cycle count, the raw capacity counters and the
    /// lifetime blob exist nowhere else — and it runs once an hour, not
    /// once a tick. See the module doc for the measurement behind that
    /// split.
    ///
    /// `None` on a machine with no battery node, and also when the node
    /// exists but answered nothing at all — see [`BatteryHealth::is_empty`]
    /// for why an all-absent row is not recorded as a fact.
    pub fn health() -> Option<BatteryHealth> {
        let mut out: Option<BatteryHealth> = None;
        iokit::for_each_service(BATTERY_SERVICE_CLASS, |props| {
            if out.is_some() {
                return;
            }
            // SAFETY: `props` is a live CFDictionary for the duration of the
            // callback (`for_each_service` releases it after we return), and
            // every accessor type-checks before dereferencing.
            unsafe {
                let lifetime = iokit::dict_dict(props, "BatteryData")
                    .and_then(|bd| iokit::dict_dict(bd, "LifetimeData"));
                out = Some(BatteryHealth {
                    cycle_count: iokit::dict_i64(props, "CycleCount").filter(|n| *n >= 0).map(|n| n as u64),
                    design_capacity_mah: iokit::dict_i64(props, "DesignCapacity")
                        .filter(|n| *n > 0)
                        .map(|n| n as u64),
                    raw_max_capacity_mah: iokit::dict_i64(props, "AppleRawMaxCapacity")
                        .filter(|n| *n > 0)
                        .map(|n| n as u64),
                    nominal_charge_capacity_mah: iokit::dict_i64(props, "NominalChargeCapacity")
                        .filter(|n| *n > 0)
                        .map(|n| n as u64),
                    condition: None,
                    // #2821: the genuine health-verdict signal, read
                    // straight (no `>= 0` filter — a negative value here
                    // would itself be a fact worth keeping, not noise to
                    // discard the way a negative cycle count would be).
                    permanent_failure_status: iokit::dict_i64(props, "PermanentFailureStatus"),
                    temperature_c: iokit::dict_i64(props, "Temperature").map(|n| n as f64 / 100.0),
                    time_at_soc_hours: lifetime
                        .and_then(|ld| iokit::dict_bytes(ld, "TimeAtHighSoc"))
                        .and_then(|b| super::parse_time_at_soc(&b)),
                    total_operating_time_hours: lifetime
                        .and_then(|ld| iokit::dict_i64(ld, "TotalOperatingTime"))
                        .filter(|n| *n >= 0)
                        .map(|n| n as u64),
                });
            }
        });
        let mut h = out?;
        h.condition = reported_condition();
        (!h.is_empty()).then_some(h)
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub use imp::{health, sample};

/// Every non-Apple-Silicon target reports no battery. The arithmetic above
/// (`charge_pct_from`, `minutes_to_empty_from`, `capacity_pct`,
/// `parse_time_at_soc`) stays compiled and unit-tested on every host, so
/// the LOGIC is covered on any CI machine even though the readings are not
/// — the same split `host_probe`'s module doc describes.
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub fn sample() -> Option<BatterySample> {
    None
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub fn health() -> Option<BatteryHealth> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charge_percent_is_the_ratio_so_it_works_on_both_architectures() {
        // Apple Silicon: `CurrentCapacity` is already a percent and
        // `MaxCapacity` is the constant 100.
        assert_eq!(charge_pct_from(Some(100), Some(100)), Some(100));
        assert_eq!(charge_pct_from(Some(42), Some(100)), Some(42));
        // Intel: both are mAh.
        assert_eq!(charge_pct_from(Some(2824), Some(5648)), Some(50));
    }

    #[test]
    fn charge_percent_is_absent_rather_than_a_division_by_nothing() {
        assert_eq!(charge_pct_from(Some(50), None), None, "no denominator, no percent");
        assert_eq!(charge_pct_from(None, Some(100)), None);
        assert_eq!(charge_pct_from(Some(50), Some(0)), None, "must not divide by zero");
        assert_eq!(charge_pct_from(Some(-1), Some(100)), None);
    }

    #[test]
    fn charge_percent_clamps_a_gauge_reading_past_its_own_max() {
        // A pack can briefly read slightly over max right after a full
        // charge; 103% would be a worse answer than 100 for #2706's floor.
        assert_eq!(charge_pct_from(Some(103), Some(100)), Some(100));
    }

    // #2821: `condition_word` must agree with `pmset`/`system_profiler`
    // rather than the unreliable raw `condition` string — this is the
    // regression test for the Step-0 finding (reference machine,
    // 2026-09-23): darkmux's own IOKit `condition` read "Check Battery"
    // while `permanent_failure_status: 0` and `pmset`/`system_profiler`
    // both agreed "Normal".
    #[test]
    fn condition_word_reads_normal_from_permanent_failure_status_even_when_raw_condition_disagrees() {
        let h = BatteryHealth {
            permanent_failure_status: Some(0),
            condition: Some("Check Battery".to_string()),
            ..Default::default()
        };
        assert_eq!(h.condition_word(), Some("Normal"), "must not trust the stale raw condition word");
    }

    #[test]
    fn condition_word_flags_service_battery_on_a_real_permanent_failure() {
        let h = BatteryHealth { permanent_failure_status: Some(3), ..Default::default() };
        assert_eq!(h.condition_word(), Some("Service Battery"));
    }

    #[test]
    fn condition_word_is_absent_without_a_permanent_failure_status_reading() {
        let h = BatteryHealth { permanent_failure_status: None, condition: Some("Good".to_string()), ..Default::default() };
        assert_eq!(h.condition_word(), None, "no computed verdict without the signal it is derived from — never silently \"Normal\"");
    }

    #[test]
    fn time_to_empty_is_absent_on_ac_and_while_charging() {
        assert_eq!(minutes_to_empty_from(Some(180), true, false), None, "on AC there is no discharge to estimate");
        assert_eq!(minutes_to_empty_from(Some(180), false, true), None, "charging is not emptying");
        assert_eq!(minutes_to_empty_from(Some(180), false, false), Some(180));
    }

    #[test]
    fn time_to_empty_reads_absent_not_zero_for_every_no_estimate_case() {
        // The issue's sharpest requirement on this field: the OS declining
        // to give a value must never surface as the number zero.
        assert_eq!(
            minutes_to_empty_from(Some(TIME_ESTIMATE_UNAVAILABLE), false, false),
            None,
            "the 65535 sentinel must read absent"
        );
        assert_eq!(minutes_to_empty_from(Some(70000), false, false), None, "anything past the sentinel too");
        assert_eq!(minutes_to_empty_from(Some(0), false, false), None, "the settling-window zero must read absent");
        assert_eq!(minutes_to_empty_from(None, false, false), None);
    }

    #[test]
    fn both_capacity_readings_are_recorded_and_differ_on_the_reference_machine() {
        // The module doc's measurement, pinned: the raw ratio and the
        // nominal ratio are DIFFERENT numbers, which is why both are
        // recorded and labeled rather than one being called "capacity".
        let h = BatteryHealth {
            design_capacity_mah: Some(6249),
            raw_max_capacity_mah: Some(5648),
            nominal_charge_capacity_mah: Some(5800),
            ..Default::default()
        };
        assert_eq!(h.raw_capacity_pct(), Some(90.4));
        assert_eq!(h.nominal_capacity_pct(), Some(92.8));
        assert_ne!(
            h.raw_capacity_pct(),
            h.nominal_capacity_pct(),
            "the two readings differ materially — recording only one would pick a side"
        );
    }

    #[test]
    fn a_capacity_reading_with_no_denominator_is_absent() {
        let h = BatteryHealth { raw_max_capacity_mah: Some(5648), ..Default::default() };
        assert_eq!(h.raw_capacity_pct(), None);
        let zero = BatteryHealth {
            raw_max_capacity_mah: Some(5648),
            design_capacity_mah: Some(0),
            ..Default::default()
        };
        assert_eq!(zero.raw_capacity_pct(), None, "must not divide by zero");
    }

    #[test]
    fn time_at_soc_parses_the_reference_machines_blob_to_hours() {
        // Captured verbatim from `ioreg -r -c AppleSmartBattery` on the
        // reference machine, 2026-09-15. The SUM is the cross-check that
        // establishes the unit: 5181 against a `TotalOperatingTime` of
        // 5178 hours.
        // Four 56-character rows, one per group of seven counters, so a
        // mangled paste is visible rather than silently shifting every
        // value by a nibble.
        let hex = concat!(
            "000000000e0000005407000059020000000000000000000000000000",
            "000000000d0000000900000002000000000000000000000000000000",
            "00000000470000002300000002000000000000000000000000000000",
            "00000000d706000073020000b4000000000000000000000000000000",
        );
        assert_eq!(hex.len(), 224, "112 bytes = 28 u32 counters; a short literal is a bad paste");
        let bytes: Vec<u8> = (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("valid hex"))
            .collect();
        let buckets = parse_time_at_soc(&bytes).expect("a whole number of u32s");
        assert_eq!(buckets.len(), 28);
        let total: u64 = buckets.iter().map(|b| *b as u64).sum();
        assert_eq!(total, 5181, "the sum is what gives the buckets their unit");
        let operating_time_hours = 5178u64;
        assert!(
            total.abs_diff(operating_time_hours) < 10,
            "buckets ({total}) must reconcile with TotalOperatingTime ({operating_time_hours}) — \
             if this drifts, the hours claim in the module doc is no longer grounded"
        );
    }

    #[test]
    fn an_unrecognized_blob_shape_is_absent_rather_than_a_parsed_prefix() {
        assert_eq!(parse_time_at_soc(&[]), None, "empty is not an array of zero counters");
        assert_eq!(parse_time_at_soc(&[1, 2, 3]), None, "not a whole number of u32s");
        assert_eq!(parse_time_at_soc(&[1, 2, 3, 4, 5]), None);
        assert_eq!(parse_time_at_soc(&[1, 0, 0, 0]), Some(vec![1]));
    }

    #[test]
    fn an_all_absent_health_reading_is_empty_and_not_worth_recording() {
        assert!(BatteryHealth::default().is_empty());
        assert!(!BatteryHealth { cycle_count: Some(26), ..Default::default() }.is_empty());
    }

    #[test]
    fn the_health_poll_cadence_is_hourly() {
        // Pinned because it is a RECORDED KNOB (#2705): this value rides
        // into the emitted payload, so changing it silently would change
        // what an artifact claims about itself.
        assert_eq!(HEALTH_POLL_INTERVAL_MS, 3_600_000);
    }

    /// The live reading, sanity-checked in range. Skips itself (rather than
    /// failing) on a machine with no battery — a desktop has nothing to
    /// read, which is the whole point of the `None` contract, and CI's
    /// macOS runners are VMs with no battery either.
    ///
    /// Everything asserted here is a RANGE or a relationship, never a
    /// specific value: this machine's charge changes between runs.
    #[test]
    fn the_live_charge_reading_is_internally_consistent() {
        let Some(b) = sample() else { return };
        assert!(b.charge_pct <= 100, "charge must be a percent: {b:?}");
        if let Some(mins) = b.minutes_to_empty {
            assert!(mins > 0, "an estimate of zero must have read as absent instead: {b:?}");
            assert!((mins as i64) < TIME_ESTIMATE_UNAVAILABLE, "the sentinel must have read as absent: {b:?}");
            assert!(!b.on_ac && !b.charging, "a time-to-EMPTY estimate on AC is a contradiction: {b:?}");
        }
    }

    /// The live HEALTH reading. Same skip-on-no-battery contract as above.
    ///
    /// Pins the two things the hourly path exists to get that the fast path
    /// cannot: the pack's own cycle/capacity counters (which live only on
    /// the `AppleSmartBattery` node) and the OS's reported condition word
    /// (which lives only on the IOPS description).
    #[test]
    fn the_live_health_reading_carries_both_capacity_ratios_and_a_condition() {
        let Some(h) = health() else { return };
        assert!(!h.is_empty(), "a health reading that resolved must not be all-absent");
        if let Some(pct) = h.raw_capacity_pct() {
            assert!(pct > 0.0 && pct <= 120.0, "raw capacity ratio out of any plausible range: {h:?}");
        }
        if let Some(pct) = h.nominal_capacity_pct() {
            assert!(pct > 0.0 && pct <= 120.0, "nominal capacity ratio out of any plausible range: {h:?}");
        }
        if let Some(c) = &h.cycle_count {
            assert!(*c < 100_000, "cycle count out of any plausible range: {c}");
        }
        if let Some(t) = h.temperature_c {
            assert!(
                (-20.0..=80.0).contains(&t),
                "battery temperature {t}C is outside any plausible range — the hundredths-of-a-degree \
                 conversion is the thing to check"
            );
        }
        if let Some(buckets) = &h.time_at_soc_hours {
            let total: u64 = buckets.iter().map(|b| *b as u64).sum();
            if let Some(op) = h.total_operating_time_hours {
                assert!(
                    total.abs_diff(op) < op / 4 + 24,
                    "the TimeAtHighSoc buckets ({total}) must reconcile with TotalOperatingTime \
                     ({op}) — that reconciliation is the ONLY grounding for the claim that these \
                     counters are in hours, so if it stops holding the doc is wrong, not the test"
                );
            }
        }
    }

    /// The probe's own cost, measured rather than asserted — the
    /// observability contract's "the observer was negligible" as a
    /// checkable fact. Skips itself (rather than failing) on a machine with
    /// no battery, since a desktop has nothing to time.
    #[test]
    fn battery_probe_cost_is_negligible() {
        if sample().is_none() {
            return;
        }
        let start = std::time::Instant::now();
        for _ in 0..10 {
            let _ = sample();
        }
        let per_sample_ms = start.elapsed().as_secs_f64() * 1000.0 / 10.0;
        // Printed, not just asserted (visible under `--no-capture`): the
        // ceiling below only catches a regression, while the NUMBER is what
        // makes "the observer was negligible" a measurement someone can
        // check rather than a claim. Same reasoning as the host probe's own
        // cost-budget test, which reports its measured max/mean.
        eprintln!("battery charge sample: {per_sample_ms:.3} ms (mean of 10)");
        assert!(
            per_sample_ms < 50.0,
            "one charge sample took {per_sample_ms:.2} ms — the in-process read this module \
             exists to provide should be ~1 ms; a regression here means it stopped being one \
             registry walk"
        );
    }
}
