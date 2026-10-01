//! Payloads of the machine-scoped records the host samplers write
//! (`machine.telemetry`, `machine.thermal`, `machine.battery`,
//! `machine.battery_health`, `machine.rollup`), and the host-reading shapes they
//! share with `GET /machine/resources`.
//!
//! Three surfaces carry a host reading: the `load` block of `/machine/resources`,
//! the periodic `machine.telemetry` payload, and the `machine.rollup` window.
//! Each was built by hand with `json!`; here they are one `Serialize` type, and
//! the TypeScript twin is generated from it.
//!
//! A field the probe could not read serializes as JSON `null`, never as a zero:
//! "not measured" and "measured, and idle" are different claims and the viewer
//! renders them differently. So the `Option` fields of the reading shapes are
//! always present on the wire.

use super::Attribution;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One `hw.perflevelN` cluster (Apple Silicon "Super" / "Performance" /
/// "Efficiency"). `pct` and `mhz` are `null` when IOReport is unavailable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct CpuClusterNow {
    pub name: String,
    pub cores: u32,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub pct: Option<u64>,
    pub mhz: Option<u32>,
}

/// `ProcessInfo.thermalState` and the CPU speed limit, verbatim. `state` is a
/// string, not a closed union: the kernel owns the vocabulary, and a level a
/// later macOS adds must render rather than fail to parse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct ThermalNow {
    pub state: String,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub cpu_speed_limit_pct: u64,
}

/// Instantaneous package power by rail, in milliwatts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct PowerNow {
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub cpu: i64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub gpu: i64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub ane: i64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub total: i64,
}

/// What a battery is doing right now, from observed facts only.
///
/// `Held` is on AC, not charging, not full, with current about zero: macOS is holding the level
/// (Optimized Battery Charging or a charge limit). It is a description of what was observed. The
/// configured limit percent is not readable, so none is stated; the level being held is the
/// reading's own `charge_pct`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
#[serde(rename_all = "snake_case")]
pub enum ChargeState {
    Charging,
    Held,
    Discharging,
    Full,
    /// Facts missing or contradictory; also what a newer producer's unrecognized state reads as.
    #[serde(other)]
    Unknown,
}

/// One battery CHARGE reading. The whole object is `null` on a machine with no
/// battery. `minutes_to_empty` is `null` on AC, while charging, and whenever
/// the OS declines to estimate; never a synthesized zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct BatteryCharge {
    pub charge_pct: u8,
    pub on_ac: bool,
    /// What the pack is doing, derived once by the probe from the facts it observed.
    pub state: ChargeState,
    pub minutes_to_empty: Option<u32>,
}

/// One host reading's full "now" shape, plus the wall-clock it was taken at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct HostSampleNow {
    /// UNIX epoch milliseconds; the caller supplies it (see `epoch_ms_now`).
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub sampled_at_ms: u64,
    /// The probe's own wall cost for this sample: the observer-cost self-stamp.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub sampler_cost_ms: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub cpu_pct: Option<u64>,
    /// One entry per `hw.perflevelN`; `null` when the host reports none.
    pub cpu_clusters: Option<Vec<CpuClusterNow>>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub mem_pct: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub gpu_pct: Option<u64>,
    pub gpu_mhz: Option<u32>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub gpu_mem_bytes: Option<u64>,
    pub thermal: Option<ThermalNow>,
    pub battery: Option<BatteryCharge>,
    pub power_mw: Option<PowerNow>,
}

/// One battery HEALTH reading: a slow-moving machine fact, refreshed on an
/// hourly cadence. `condition` is the raw `BatteryHealth` IOKit word and is
/// unreliable on Apple Silicon; `health_condition` is the authoritative raw
/// signal; `condition_word` is the verdict derived from it (see
/// `BatteryHealth::condition_word`). A UI shows `condition_word` and falls back
/// to `condition`, labeled precisely, only when it is `null`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct BatteryHealthNow {
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub cycle_count: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub design_capacity_mah: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub raw_max_capacity_mah: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub nominal_charge_capacity_mah: Option<u64>,
    pub raw_capacity_pct: Option<f64>,
    pub nominal_capacity_pct: Option<f64>,
    pub condition: Option<String>,
    pub health_condition: Option<String>,
    pub condition_word: Option<String>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub permanent_failure_status: Option<i64>,
    pub temperature_c: Option<f64>,
    #[cfg_attr(feature = "ts-export", ts(type = "Array<number> | null"))]
    pub time_at_soc_ms: Option<Vec<u64>>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub total_operating_ms: Option<u64>,
}

/// One metric's window reduction: mean, nearest-rank p95 and maximum. The same
/// shape carries a percentage metric and a power rail in milliwatts.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct MetricWindow {
    pub mean: Option<f64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub p95: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub max: Option<u64>,
}

/// The power rails' window reductions, in milliwatts.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct PowerWindowWire {
    pub total: MetricWindow,
    pub gpu: MetricWindow,
    pub cpu: MetricWindow,
}

/// One window's thermal summary. `level_ms` answers how LONG the machine spent
/// at each level and `level_entries` how OFTEN it arrived there (transitions,
/// not samples). Both are keyed by the level's own name, because the kernel owns
/// that vocabulary; a missing key means "never observed in this window", which
/// is not the same claim as `0`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct ThermalWindowWire {
    pub worst_state: String,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub above_nominal_ms: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub min_cpu_speed_limit_pct: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, number>"))]
    pub level_ms: BTreeMap<String, u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, number>"))]
    pub level_entries: BTreeMap<String, u64>,
}

/// The window reductions over the sampler's ring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct LoadWindow {
    pub samples: u32,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub span_ms: u64,
    /// The MEASURED mean gap between samples, not the configured cadence; `null` with fewer than
    /// two samples.
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub interval_ms: Option<u64>,
    pub cpu_pct: MetricWindow,
    pub gpu_pct: MetricWindow,
    pub mem_pct: MetricWindow,
    pub power_mw: Option<PowerWindowWire>,
    pub thermal: Option<ThermalWindowWire>,
    /// The integral of total power over the window, in milliwatt-hours.
    pub energy_mwh: Option<f64>,
}

/// The machine lens's own picture: the latest reading, its window, and the slow-moving battery
/// health fact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct MachineLoad {
    /// The slow-moving battery health fact; `null` on a machine with no battery.
    pub battery_health: Option<BatteryHealthNow>,
    pub now: HostSampleNow,
    pub window: LoadWindow,
}

/// A battery transition worth a record, as a stable string a consumer can key on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum BatteryTransition {
    /// Unplugged.
    #[serde(rename = "to-battery")]
    ToBattery,
    /// Plugged in.
    #[serde(rename = "to-ac")]
    ToAc,
    /// Fell below the operator's `power.min_battery_pct` floor, the one transition that changes
    /// what the machine will DO.
    #[serde(rename = "below-floor")]
    BelowFloor,
    /// Recovered to or above the floor.
    #[serde(rename = "at-or-above-floor")]
    AtOrAboveFloor,
    /// A value this build does not name, read from an archive written by another version.
    /// Never written.
    #[serde(other)]
    Unknown,
}

/// The periodic full host reading: the payload of `machine.telemetry`. Machine-scoped: no dispatch,
/// session or model, so at most one emitter per machine writes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct MachineTelemetryPayload {
    /// The reading.
    #[serde(flatten)]
    #[cfg_attr(feature = "ts-export", ts(flatten))]
    pub now: HostSampleNow,
    /// The MEASURED gap since the previous emission of this record, not the configured cadence.
    /// Absent on a record written before it was stamped (2026-09-05).
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub interval_ms: Option<u64>,
    /// The liveness probe this emission's cadence decision depended on: part of this record's own
    /// write cost, stamped so "the observer was negligible" stays a verifiable claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub liveness_probe_ms: Option<u64>,
    /// The PREVIOUS emission's measured write duration; this record's own write has not happened
    /// yet, so it cannot honestly report its own cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub prev_record_write_ms: Option<u64>,
    /// The scenario file behind a record made on scripted readings; absent on a real run, so its
    /// presence alone answers "were these readings real".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub simulated_host_source: Option<String>,
    /// (#3022) The fleet position this machine declares (`fleet.mode`) when
    /// the record was written, so an archive and every consumer of the fleet
    /// stream can tell which machine was the hub without asking it. Absent on
    /// a record written before it was stamped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub fleet_mode: Option<darkmux_types::config::DeclaredFleetMode>,
}

impl Attribution for MachineTelemetryPayload {
    fn host_source_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.simulated_host_source)
    }
}

/// A thermal state transition: the payload of `machine.thermal`. `Warn` only when the state rises
/// into `serious` or `critical`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct MachineThermalPayload {
    /// The state before, as the kernel names it.
    pub from: String,
    /// The state after.
    pub to: String,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub cpu_speed_limit_pct: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub power_mw_total: Option<i64>,
    /// When the transition was observed, epoch milliseconds.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub sampled_at_ms: u64,
    /// The scenario file behind a record made on scripted readings; absent on a real run, so its
    /// presence alone answers "were these readings real".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub simulated_host_source: Option<String>,
}

impl Attribution for MachineThermalPayload {
    fn host_source_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.simulated_host_source)
    }
}

/// A battery charge transition: the payload of `machine.battery`. `Warn` only for `below-floor`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct MachineBatteryPayload {
    pub transitions: Vec<BatteryTransition>,
    pub from: BatteryCharge,
    pub to: BatteryCharge,
    /// The floor this crossing was judged against, recorded so a reader is not left to guess which
    /// config was in force.
    pub floor_pct: u8,
    /// The config field that set the floor.
    pub floor_field: String,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub sampled_at_ms: u64,
    /// The scenario file behind a record made on scripted readings; absent on a real run, so its
    /// presence alone answers "were these readings real".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub simulated_host_source: Option<String>,
}

impl Attribution for MachineBatteryPayload {
    fn host_source_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.simulated_host_source)
    }
}

/// A battery health reading, emitted only when a value actually changed: the payload of
/// `machine.battery_health`. It carries no `simulated_host_source` on purpose: the health read is
/// IOKit, which a scenario file cannot reach.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct MachineBatteryHealthPayload {
    /// The reading.
    #[serde(flatten)]
    #[cfg_attr(feature = "ts-export", ts(flatten))]
    pub health: BatteryHealthNow,
    /// The cadence that produced it, a recorded knob rather than an inference from row spacing.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub poll_interval_ms: u64,
    /// When the change was observed.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub sampled_at_ms: u64,
}

impl Attribution for MachineBatteryHealthPayload {}

/// The periodic machine picture in one record: the payload of `machine.rollup`. Thermal,
/// cpu/gpu/memory, power, battery and residency ride one payload, so a subscriber need not
/// correlate separate feeds. The load block is spliced in at the top level, so a hook predicate
/// says `payload.window.thermal`, not `payload.load.window.thermal`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct MachineRollupPayload {
    /// The CONFIGURED cadence.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub period_ms: u64,
    /// The MEASURED gap since the previous emission.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub emitted_interval_ms: u64,
    /// This rollup's own total cost, the ledger gather included.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub gather_ms: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub sampled_at_ms: u64,
    /// The state the machine was in before the current one; `null` before any transition was
    /// observed, which is a different claim from "it came from nominal".
    pub previous_thermal_state: Option<String>,
    /// What is loaded and how much unified memory is left for AI: the model ledger, as `GET
    /// /machine/resources` serves it, or `null` when it could not be gathered.
    #[cfg_attr(feature = "ts-export", ts(type = "import(\"./ModelLedger\").ModelLedger | null"))]
    pub residency: Option<serde_json::Value>,
    /// The machine lens's picture.
    #[serde(flatten)]
    #[cfg_attr(feature = "ts-export", ts(flatten))]
    pub load: MachineLoad,
    /// The scenario file behind a record made on scripted readings; absent on a real run, so its
    /// presence alone answers "were these readings real".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub simulated_host_source: Option<String>,
}

impl Attribution for MachineRollupPayload {
    fn host_source_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.simulated_host_source)
    }
}

#[cfg(test)]
mod charge_state_tests {
    use super::ChargeState;

    #[test]
    fn charge_state_spells_snake_case_on_the_wire() {
        assert_eq!(serde_json::to_string(&ChargeState::Held).unwrap(), "\"held\"");
        assert_eq!(serde_json::to_string(&ChargeState::Discharging).unwrap(), "\"discharging\"");
    }

    #[test]
    fn an_unrecognized_state_reads_as_unknown_not_an_error() {
        let s: ChargeState = serde_json::from_str("\"trickle\"").unwrap();
        assert_eq!(s, ChargeState::Unknown);
    }
}
