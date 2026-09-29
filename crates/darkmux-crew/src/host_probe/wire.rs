//! The wire shapes of a host reading: one typed definition for every consumer.
//!
//! Three surfaces carry these objects: `GET /machine/resources`' `load` block,
//! the periodic `machine.telemetry` flow record's payload, and the
//! `machine.rollup` record's window. Each used to build the object by hand with
//! `json!`, so the wire shape lived in the builders and in a hand-written
//! TypeScript copy. Here it is a `Serialize` type, and the TypeScript twin is
//! generated from it.
//!
//! A field the probe could not read serializes as JSON `null`, never as a zero:
//! "not measured" and "measured, and idle" are different claims and the viewer
//! renders them differently. So the `Option` fields here are always present on
//! the wire.

use serde::Serialize;
use std::collections::BTreeMap;

use super::battery::{BatteryHealth, BatterySample};
use super::{CpuCluster, HostSampleFull, MwStats, PowerSample, ThermalSample, ThermalWindow};

/// One `hw.perflevelN` cluster (Apple Silicon "Super" / "Performance" /
/// "Efficiency"). `pct` and `mhz` are `null` when IOReport is unavailable.
#[derive(Debug, Clone, PartialEq, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct ThermalNow {
    pub state: String,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub cpu_speed_limit_pct: u64,
}

/// Instantaneous package power by rail, in milliwatts.
#[derive(Debug, Clone, PartialEq, Serialize)]
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

/// One battery CHARGE reading. The whole object is `null` on a machine with no
/// battery. `minutes_to_empty` is `null` on AC, while charging, and whenever
/// the OS declines to estimate; never a synthesized zero.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct BatteryCharge {
    pub charge_pct: u8,
    pub on_ac: bool,
    pub charging: bool,
    pub minutes_to_empty: Option<u32>,
}

/// One host reading's full "now" shape, plus the wall-clock it was taken at.
#[derive(Debug, Clone, PartialEq, Serialize)]
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

impl HostSampleNow {
    pub fn new(s: &HostSampleFull, sampled_at_ms: u64) -> Self {
        Self {
            sampled_at_ms,
            sampler_cost_ms: s.cost_ms,
            cpu_pct: s.cpu_pct,
            cpu_clusters: s.cpu_clusters.as_ref().map(|cs| cs.iter().map(CpuClusterNow::from).collect()),
            mem_pct: s.mem_pct,
            gpu_pct: s.gpu_pct,
            gpu_mhz: s.gpu_mhz,
            gpu_mem_bytes: s.gpu_mem_bytes,
            thermal: s.thermal.as_ref().map(ThermalNow::from),
            battery: s.battery.as_ref().map(BatteryCharge::from),
            power_mw: s.power.as_ref().map(PowerNow::from),
        }
    }
}

impl From<&CpuCluster> for CpuClusterNow {
    fn from(c: &CpuCluster) -> Self {
        Self { name: c.name.clone(), cores: c.cores as u32, pct: c.pct, mhz: c.mhz }
    }
}

impl From<&ThermalSample> for ThermalNow {
    fn from(t: &ThermalSample) -> Self {
        Self { state: t.state.clone(), cpu_speed_limit_pct: t.cpu_speed_limit_pct }
    }
}

impl From<&PowerSample> for PowerNow {
    fn from(p: &PowerSample) -> Self {
        Self {
            cpu: p.cpu_mw.round() as i64,
            gpu: p.gpu_mw.round() as i64,
            ane: p.ane_mw.round() as i64,
            total: p.total_mw().round() as i64,
        }
    }
}

impl From<&BatterySample> for BatteryCharge {
    fn from(b: &BatterySample) -> Self {
        Self { charge_pct: b.charge_pct, on_ac: b.on_ac, charging: b.charging, minutes_to_empty: b.minutes_to_empty }
    }
}

/// One battery HEALTH reading: a slow-moving machine fact, refreshed on an
/// hourly cadence. `condition` is the raw `BatteryHealth` IOKit word and is
/// unreliable on Apple Silicon; `health_condition` is the authoritative raw
/// signal; `condition_word` is the verdict derived from it (see
/// `BatteryHealth::condition_word`). A UI shows `condition_word` and falls back
/// to `condition`, labeled precisely, only when it is `null`.
#[derive(Debug, Clone, PartialEq, Serialize)]
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
    pub time_at_soc_hours: Option<Vec<u32>>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub total_operating_time_hours: Option<u64>,
}

impl From<&BatteryHealth> for BatteryHealthNow {
    fn from(h: &BatteryHealth) -> Self {
        Self {
            cycle_count: h.cycle_count,
            design_capacity_mah: h.design_capacity_mah,
            raw_max_capacity_mah: h.raw_max_capacity_mah,
            nominal_charge_capacity_mah: h.nominal_charge_capacity_mah,
            raw_capacity_pct: h.raw_capacity_pct(),
            nominal_capacity_pct: h.nominal_capacity_pct(),
            condition: h.condition.clone(),
            health_condition: h.health_condition.clone(),
            condition_word: h.condition_word(),
            permanent_failure_status: h.permanent_failure_status,
            temperature_c: h.temperature_c,
            time_at_soc_hours: h.time_at_soc_hours.clone(),
            total_operating_time_hours: h.total_operating_time_hours,
        }
    }
}

/// One metric's window reduction: mean, nearest-rank p95 and maximum. The same
/// shape carries a percentage metric and a power rail in milliwatts.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct MetricWindow {
    pub mean: Option<f64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub p95: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub max: Option<u64>,
}

impl From<&crate::telemetry_sampler::MetricStats> for MetricWindow {
    fn from(m: &crate::telemetry_sampler::MetricStats) -> Self {
        Self { mean: m.mean_pct, p95: m.p95_pct, max: m.peak_pct }
    }
}

impl From<&MwStats> for MetricWindow {
    fn from(m: &MwStats) -> Self {
        Self { mean: m.mean_mw, p95: m.p95_mw, max: m.max_mw }
    }
}

/// The power rails' window reductions, in milliwatts.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct PowerWindowWire {
    pub total: MetricWindow,
    pub gpu: MetricWindow,
    pub cpu: MetricWindow,
}

impl From<&super::PowerWindow> for PowerWindowWire {
    fn from(p: &super::PowerWindow) -> Self {
        Self { total: (&p.total).into(), gpu: (&p.gpu).into(), cpu: (&p.cpu).into() }
    }
}

/// One window's thermal summary. `level_ms` answers how LONG the machine spent
/// at each level and `level_entries` how OFTEN it arrived there (transitions,
/// not samples). Both are keyed by the level's own name, because the kernel owns
/// that vocabulary; a missing key means "never observed in this window", which
/// is not the same claim as `0`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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

impl From<&ThermalWindow> for ThermalWindowWire {
    fn from(t: &ThermalWindow) -> Self {
        Self {
            worst_state: t.worst_state.clone(),
            above_nominal_ms: t.above_nominal_ms,
            min_cpu_speed_limit_pct: t.min_cpu_speed_limit_pct,
            level_ms: t.level_ms.clone(),
            level_entries: t.level_entries.clone(),
        }
    }
}
