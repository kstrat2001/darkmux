//! The conversions from a host probe's readings to the wire shapes of a host reading.
//!
//! The shapes themselves are `darkmux_flow::payload` types, one typed definition for every
//! consumer.
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

pub use darkmux_flow::payload::{
    BatteryCharge, BatteryHealthNow, CpuClusterNow, HostSampleNow, LoadWindow, MachineLoad, MetricWindow, PowerNow,
    PowerWindowWire, ThermalNow, ThermalWindowWire,
};

use super::battery::{BatteryHealth, BatterySample};
use super::{CpuCluster, HostSampleFull, MwStats, PowerSample, ThermalSample, ThermalWindow};

/// One host reading's "now" shape, plus the wall-clock it was taken at.
pub fn host_sample_now(s: &HostSampleFull, sampled_at_ms: u64) -> HostSampleNow {
    HostSampleNow {
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
            time_at_soc_ms: h.time_at_soc_ms.clone(),
            total_operating_ms: h.total_operating_ms,
        }
    }
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

impl From<&super::PowerWindow> for PowerWindowWire {
    fn from(p: &super::PowerWindow) -> Self {
        Self { total: (&p.total).into(), gpu: (&p.gpu).into(), cpu: (&p.cpu).into() }
    }
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
