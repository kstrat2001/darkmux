//! (#2108) The host probe — one in-process reading of what this machine is
//! doing, and the window reduction over a series of them.
//!
//! # Why this replaced the shell-outs
//!
//! Before #2108 a host sample was four process spawns (`top`, `sysctl`,
//! `vm_stat`, `ioreg`) costing ~780 ms, and the CPU number was wrong on top
//! of being expensive: `top -l 1 -n 0` blocks for a full sample interval AND
//! its single "CPU usage" line is a SINCE-BOOT average, so a sampler running
//! every 5 s was recording a number that barely moved no matter what the
//! machine did. Everything here is a kernel-counter or registry read in this
//! process; the same sample now costs ~5-10 ms and the CPU figure is a true
//! mean over the interval since the previous sample.
//!
//! # The observer must not join the observed (CLAUDE.md, #1286)
//!
//! - **Zero model dispatches.** Every source is a kernel counter
//!   (`host_processor_info`, `host_statistics64`), an IORegistry property,
//!   or an IOReport counter delta. No tokens, no Metal work.
//! - **The probe stamps its own cost.** [`HostSampleFull::cost_ms`] is this
//!   probe's measured wall-clock for the sample that carries it, so "the
//!   observer was negligible" is a checkable fact in the artifact rather
//!   than an assumption.
//! - **Every source degrades independently.** A missing framework, a missing
//!   symbol, an absent channel or an unrecognized unit label yields `None`
//!   for that field only. Nothing here panics, and nothing aborts a sampler.
//!
//! # Platform
//!
//! The real implementation is `#[cfg(all(target_os = "macos", target_arch =
//! "aarch64"))]`. Every other target gets a probe that reports `None` for
//! every field — the arithmetic (tick deltas, residency→MHz, energy→mW,
//! window reductions) stays compiled and unit-tested everywhere, so the
//! logic is covered on any CI host even though the readings are not.

pub mod iokit;
pub mod ioreport;
pub mod mach_cpu;
// (#2112) Pre-flight power posture — AC/battery + percent, Low Power Mode,
// thermal state (reusing `thermal::sample`), and recent thermal-emergency
// forced sleeps from `pmset -g log`. Read on demand (doctor + mission
// pre-flight), not part of the periodic telemetry sampler above.
pub mod power_posture;
pub mod thermal;

pub use thermal::ThermalSample;

// Only used by `attach_cluster_mhz` and its direct unit tests, both
// aarch64-gated below (#2108 CI finding — see the comment on
// `attach_cluster_mhz`).
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::collections::BTreeMap;

/// One perf-level cluster's reading. `name`/`cores` come from
/// `hw.perflevelN`; `pct` from mach tick deltas; `mhz` from IOReport DVFS
/// residency. The last two are independently `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuCluster {
    pub name: String,
    pub cores: usize,
    pub pct: Option<u64>,
    pub mhz: Option<u32>,
}

/// The three power rails, in milliwatts, over the sample's interval.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PowerSample {
    pub cpu_mw: f64,
    pub gpu_mw: f64,
    pub ane_mw: f64,
}

impl PowerSample {
    pub fn total_mw(&self) -> f64 {
        self.cpu_mw + self.gpu_mw + self.ane_mw
    }
}

/// One complete host reading. Every field except [`Self::cost_ms`] is
/// independently optional — see the module doc's degradation rule.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostSampleFull {
    /// The probe's own measured wall-clock cost for THIS sample.
    pub cost_ms: u64,
    /// Whole-machine busy percent over the interval since the previous
    /// sample. `None` on the FIRST sample a probe takes — a tick-counter
    /// delta needs two reads, and reporting the since-boot average instead
    /// is precisely the bug this module was written to remove.
    pub cpu_pct: Option<u64>,
    /// One entry per `hw.perflevelN`. `None` (never an empty vec) when the
    /// host reports no perf levels at all.
    pub cpu_clusters: Option<Vec<CpuCluster>>,
    pub mem_pct: Option<u64>,
    pub gpu_pct: Option<u64>,
    pub gpu_mhz: Option<u32>,
    pub gpu_mem_bytes: Option<u64>,
    pub thermal: Option<ThermalSample>,
    /// `None` on the first sample (an energy delta needs two reads) and
    /// whenever IOReport is unavailable.
    pub power: Option<PowerSample>,
}

/// Which sources actually resolved on this host — what `darkmux doctor`
/// reports so an operator can tell "this Mac has no IOReport" from "darkmux
/// forgot to read it".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostProbeSources {
    /// mach kernel counters (CPU ticks + VM statistics).
    pub mach: bool,
    /// The IOReport subscription (power + DVFS residency).
    pub ioreport: bool,
    /// The SoC's DVFS frequency tables from the `pmgr` IORegistry node —
    /// without these, residency is readable but not convertible to MHz.
    pub freq_tables: bool,
    /// `ProcessInfo.thermalState` + `IOPMCopyCPUPowerStatus`.
    pub thermal: bool,
    /// The `IOAccelerator` IORegistry node (GPU utilization + memory).
    pub ioreg_gpu: bool,
}

// ── Window reduction ───────────────────────────────────────────────────────

/// One metric's window reduction in milliwatts. Mirrors
/// [`crate::telemetry_sampler::MetricStats`]'s conventions — mean to one
/// decimal, nearest-rank p95 — so the two reductions can't disagree about
/// what those words mean.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MwStats {
    pub mean_mw: Option<f64>,
    pub p95_mw: Option<u64>,
    pub max_mw: Option<u64>,
}

/// The three rails' window reductions.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PowerWindow {
    pub cpu: MwStats,
    pub gpu: MwStats,
    pub total: MwStats,
}

/// The window's thermal summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThermalWindow {
    /// The most severe state observed in the window.
    pub worst_state: String,
    /// Wall-clock the machine spent in a state other than `nominal`,
    /// measured left-Riemann from each sample's own gap to the next — the
    /// same duty convention [`crate::telemetry_sampler::reduce_metric`]'s
    /// `above_80_ms` uses.
    pub above_nominal_ms: u64,
    /// The lowest CPU speed cap seen. 100 means the kernel never capped.
    pub min_cpu_speed_limit_pct: u64,
}

/// One sample's power/thermal reading plus when it was taken. `at_ms` is on
/// whichever clock the caller's sampler uses (dispatch-relative or epoch) —
/// the reduction only ever uses GAPS, exactly as
/// [`crate::telemetry_sampler::HostSampleAt`] does.
#[derive(Debug, Clone, PartialEq)]
pub struct HostExtraAt {
    pub at_ms: u64,
    pub power: Option<PowerSample>,
    pub thermal: Option<ThermalSample>,
}

/// The window reduction over the power/thermal half of a sample series.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostExtras {
    pub power: Option<PowerWindow>,
    pub thermal: Option<ThermalWindow>,
    /// ∫ total power over the window, in milliwatt-hours.
    pub energy_mwh: Option<f64>,
}

/// Severity rank for `worst_state` comparison. An UNRECOGNIZED state ranks
/// above `critical`: a state this build doesn't know about is not evidence
/// that nothing was wrong, and silently ranking it as `nominal` would let a
/// future macOS state hide real thermal pressure.
fn thermal_severity(state: &str) -> usize {
    thermal::THERMAL_STATES
        .iter()
        .position(|s| *s == state)
        .unwrap_or(thermal::THERMAL_STATES.len())
}

/// Reduce a list of milliwatt readings (in chronological order) into
/// mean/p95/max. Empty input yields an all-`None` [`MwStats`] — "not
/// measured", never a zeroed reading. Pure.
fn reduce_mw(vals: &[f64]) -> MwStats {
    if vals.is_empty() {
        return MwStats::default();
    }
    let mean = vals.iter().sum::<f64>() / vals.len() as f64;
    let mut sorted: Vec<u64> = vals.iter().map(|v| v.max(0.0).round() as u64).collect();
    sorted.sort_unstable();
    // Nearest-rank, matching `reduce_metric`.
    let rank = ((sorted.len() as f64) * 0.95).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    MwStats {
        mean_mw: Some((mean * 10.0).round() / 10.0),
        p95_mw: Some(sorted[idx]),
        max_mw: sorted.last().copied(),
    }
}

/// A pair's dt is capped at this multiple of the configured cadence before
/// it feeds any left-Riemann sum. `3` gives normal jitter (a slow tick, a
/// GC pause, a couple of dropped samples) plenty of room while still
/// catching the failure this guards against: a laptop sleep/wake, a daemon
/// restart, or any other multi-minute-or-longer gap between two ring
/// entries. See [`reduce_host_extras`]'s doc for the bug this closes.
const MAX_GAP_CADENCE_MULTIPLE: u64 = 3;

/// Reduce a sample series' power + thermal readings.
///
/// Pure and independently testable — a scripted list of [`HostExtraAt`] in,
/// exact stats out, with no probe, clock or syscall involved. Mirrors
/// [`crate::telemetry_sampler::reduce_host_stats`]'s shape deliberately: the
/// two are applied to the same sample series by the same callers, and a
/// divergence in convention between them would surface as two numbers on one
/// screen that disagree for no visible reason.
///
/// `configured_interval_ms` is the sampler's OWN configured cadence (never
/// derived from `raw` itself — a derived "typical gap" is exactly the
/// statistic a single huge outlier would corrupt). It bounds how much
/// elapsed time any ONE consecutive pair may contribute to a left-Riemann
/// sum, at [`MAX_GAP_CADENCE_MULTIPLE`]× the cadence. Without this cap, a
/// laptop that slept for 8 hours between two ring entries billed the ENTIRE
/// sleep gap at whatever power/thermal reading was captured the instant
/// before sleep — `energy_mwh` and `above_nominal_ms` both silently
/// inherited hours of a reading that was never actually sustained. `None`
/// (cadence unknown — e.g. a ring the sampler thread never configured) skips
/// the cap entirely rather than guessing one.
pub fn reduce_host_extras(raw: &[HostExtraAt], configured_interval_ms: Option<u64>) -> HostExtras {
    let cap_ms = configured_interval_ms.map(|c| c.saturating_mul(MAX_GAP_CADENCE_MULTIPLE));
    let capped_gap = |a: u64, b: u64| -> u64 {
        let gap = b.saturating_sub(a);
        cap_ms.map_or(gap, |cap| gap.min(cap))
    };

    let cpu: Vec<f64> = raw.iter().filter_map(|s| s.power.map(|p| p.cpu_mw)).collect();
    let gpu: Vec<f64> = raw.iter().filter_map(|s| s.power.map(|p| p.gpu_mw)).collect();
    let total: Vec<f64> = raw
        .iter()
        .filter_map(|s| s.power.map(|p| p.total_mw()))
        .collect();
    let power = (!total.is_empty()).then(|| PowerWindow {
        cpu: reduce_mw(&cpu),
        gpu: reduce_mw(&gpu),
        total: reduce_mw(&total),
    });

    // Energy: left-Riemann over each consecutive pair — the first sample's
    // power is assumed to hold until the next reading, the only honest
    // assumption between two point-in-time measurements. mW × ms / 3.6e6 =
    // mWh. A trailing sample with no successor contributes nothing (it has
    // no measured interval to attribute), which undercounts a window cut off
    // mid-spike rather than overcounting one that wasn't. Each pair's gap is
    // capped (see `capped_gap`) so a sleep/wake or restart between two
    // entries can't bill hours of pre-sleep power as if it held throughout.
    let energy_mwh = power.is_some().then(|| {
        let joules: f64 = raw
            .windows(2)
            .filter_map(|w| {
                let p = w[0].power?;
                let gap = capped_gap(w[0].at_ms, w[1].at_ms) as f64;
                Some(p.total_mw() * gap / 3.6e6)
            })
            .sum();
        // Rounded to microwatt-hours. The sum's full binary expansion
        // (`31.049011483786114`) asserts a precision no sampler at a 1-5s
        // cadence has, and it reads as noise on the wire.
        (joules * 1e3).round() / 1e3
    });

    let thermals: Vec<&HostExtraAt> = raw.iter().filter(|s| s.thermal.is_some()).collect();
    let thermal = (!thermals.is_empty()).then(|| {
        let worst = raw
            .iter()
            .filter_map(|s| s.thermal.as_ref())
            .max_by_key(|t| thermal_severity(&t.state))
            .map(|t| t.state.clone())
            .unwrap_or_else(|| "nominal".to_string());
        // Same cap as the energy sum above — a sleep/wake gap must not bill
        // hours of "above nominal" from a single pre-sleep reading.
        let above_nominal_ms: u64 = raw
            .windows(2)
            .filter(|w| {
                w[0].thermal
                    .as_ref()
                    .is_some_and(|t| t.state != "nominal")
            })
            .map(|w| capped_gap(w[0].at_ms, w[1].at_ms))
            .sum();
        let min_cpu_speed_limit_pct = raw
            .iter()
            .filter_map(|s| s.thermal.as_ref().map(|t| t.cpu_speed_limit_pct))
            .min()
            .unwrap_or(100);
        ThermalWindow {
            worst_state: worst,
            above_nominal_ms,
            min_cpu_speed_limit_pct,
        }
    });

    HostExtras {
        power,
        thermal,
        energy_mwh,
    }
}

// ── The probe ──────────────────────────────────────────────────────────────

/// A stateful host probe.
///
/// **Stateful by necessity**: both the CPU percentage and every power rail
/// are COUNTER DELTAS, so a reading is only meaningful relative to the
/// previous one. Construct one per sampler thread and call
/// [`Self::sample`] on the sampler's cadence; the first sample reports
/// `cpu_pct: None` and `power: None` because there is nothing to difference
/// against yet. (The alternative — sleeping ~100 ms inside the first sample
/// to manufacture a delta — was rejected: it would make the probe's OWN
/// first `cost_ms` two orders of magnitude larger than every later one,
/// which is exactly the observer cost this module exists to keep honest.
/// Callers that need a complete single reading, like `darkmux doctor`, take
/// two samples explicitly.)
pub struct HostProbe {
    levels: Vec<mach_cpu::PerfLevel>,
    ranges: Vec<std::ops::Range<usize>>,
    prev_ticks: Option<Vec<mach_cpu::CoreTicks>>,
    prev_at: Option<std::time::Instant>,
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    ioreport: Option<ioreport::IoReportProbe>,
    sources: HostProbeSources,
}

impl Default for HostProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl HostProbe {
    /// Build a probe. Never fails: a source that can't be reached is simply
    /// absent from [`Self::sources`] and null in every sample. Construction
    /// costs ~80-100 ms on macOS (enumerating IOReport's ~11k channels down
    /// to the 24 darkmux subscribes to, plus the one-time DVFS table read);
    /// each subsequent sample is ~5-10 ms.
    pub fn new() -> Self {
        let levels = mach_cpu::perf_levels();
        let ranges = mach_cpu::core_ranges(&levels);
        // Not `mut`: only the aarch64 branch below ever assigns
        // `sources.ioreport`/`sources.freq_tables` — on every other target
        // this binding is read-only, and a `mut` here would be a dead-code
        // warning under `-D warnings` on those targets (#2108 CI finding).
        let sources = HostProbeSources {
            mach: mach_cpu::per_core_ticks().is_some(),
            thermal: thermal::sample().is_some(),
            ioreg_gpu: platform::gpu_read().is_some(),
            ..Default::default()
        };
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        let (ioreport, sources) = {
            let mut sources = sources;
            let p = ioreport::IoReportProbe::new();
            if p.is_none() {
                log_once(
                    &UNAVAILABLE_IOREPORT,
                    "host probe: IOReport unavailable on this host — power and CPU/GPU frequency will be reported as null",
                );
            }
            sources.ioreport = p.is_some();
            sources.freq_tables = p.as_ref().is_some_and(|p| p.has_freq_tables());
            if sources.ioreport && !sources.freq_tables {
                log_once(
                    &UNAVAILABLE_FREQS,
                    "host probe: the SoC DVFS frequency tables could not be read — CPU/GPU frequency will be reported as null",
                );
            }
            (p, sources)
        };
        if !sources.mach {
            log_once(
                &UNAVAILABLE_MACH,
                "host probe: mach CPU counters unavailable — CPU load will be reported as null",
            );
        }
        Self {
            levels,
            ranges,
            prev_ticks: None,
            prev_at: None,
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            ioreport,
            sources,
        }
    }

    /// Which sources resolved. Read by `darkmux doctor`.
    pub fn sources(&self) -> HostProbeSources {
        self.sources
    }

    /// Take one reading, stamping the probe's own wall-clock cost into it.
    pub fn sample(&mut self) -> HostSampleFull {
        let t0 = std::time::Instant::now();
        let interval_ms = self
            .prev_at
            .map(|p| t0.duration_since(p).as_millis() as u64)
            .unwrap_or(0);

        // CPU: per-core tick deltas, grouped into perf-level clusters.
        let cur_ticks = mach_cpu::per_core_ticks();
        let (cpu_pct, clusters) = match (&self.prev_ticks, &cur_ticks) {
            (Some(prev), Some(cur)) => {
                let whole = mach_cpu::range_busy_pct(prev, cur, &(0..cur.len().min(prev.len())));
                let per_level = self
                    .levels
                    .iter()
                    .zip(&self.ranges)
                    .map(|(l, r)| CpuCluster {
                        name: l.name.clone(),
                        cores: l.logical_cpus,
                        pct: mach_cpu::range_busy_pct(prev, cur, r),
                        mhz: None,
                    })
                    .collect::<Vec<_>>();
                (whole, per_level)
            }
            _ => (
                None,
                self.levels
                    .iter()
                    .map(|l| CpuCluster {
                        name: l.name.clone(),
                        cores: l.logical_cpus,
                        pct: None,
                        mhz: None,
                    })
                    .collect(),
            ),
        };
        if cur_ticks.is_some() {
            self.prev_ticks = cur_ticks;
        }

        // IOReport: DVFS frequency per cluster + the power rails. Not `mut`
        // out here: only the aarch64 branch below ever reassigns
        // `gpu_mhz`/`power`/`clusters` — on every other target these
        // bindings are read-only, and declaring them `mut` unconditionally
        // is a dead-code warning under `-D warnings` (#2108 CI finding).
        let gpu_mhz = None;
        // `power_from_rails(None, None, None)` is always `None` — written
        // this way (not a bare `None`) so the helper is a genuine call on
        // EVERY target, not dead code the non-aarch64 lib build would flag
        // under `-D warnings`, matching the module doc's "tested (and, here,
        // exercised) everywhere" promise.
        let power = power_from_rails(None, None, None);
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        let (clusters, gpu_mhz, power) = {
            let mut clusters = clusters;
            let mut gpu_mhz = gpu_mhz;
            let mut power = power;
            if let Some(p) = self.ioreport.as_mut() {
                if let Some(s) = p.sample(interval_ms) {
                    gpu_mhz = s.gpu_mhz;
                    attach_cluster_mhz(&mut clusters, &s.cluster_mhz, &s.cluster_cores);
                    power = power_from_rails(s.cpu_mw, s.gpu_mw, s.ane_mw);
                }
            }
            (clusters, gpu_mhz, power)
        };
        let _ = interval_ms; // unused on non-Apple-Silicon builds

        let gpu = platform::gpu_read();
        self.prev_at = Some(t0);

        HostSampleFull {
            // `elapsed()` from `t0`, which is also the interval anchor — so
            // the stamped cost is exactly the probe's own work.
            cost_ms: t0.elapsed().as_millis() as u64,
            cpu_pct,
            cpu_clusters: (!clusters.is_empty()).then_some(clusters),
            mem_pct: mach_cpu::mem_pct(),
            gpu_pct: gpu.map(|g| g.0),
            gpu_mhz,
            gpu_mem_bytes: gpu.and_then(|g| g.1),
            thermal: thermal::sample(),
            power,
        }
    }
}

/// Combine the three IOReport energy rails into one [`PowerSample`] — ALL
/// three or none (#2108 review finding).
///
/// A rail's unit can independently fail to match the closed set
/// `energy_delta_to_mw` recognizes (`ioreport::energy_delta_to_mw`'s doc:
/// "an unrecognized unit yields `None`, never a guessed scale"). Reporting
/// the other two real numbers alongside a zeroed one for that rail is
/// indistinguishable on the wire from "this rail genuinely measured zero
/// milliwatts" — exactly the degradation contract
/// `a_sample_missing_power_is_skipped_not_zeroed` (below, one reduction
/// layer up) exists to prevent. `None` — "not measured this sample" —
/// rather than mixing two real readings with one fabricated zero. Pure,
/// unlike its caller's aarch64-only branch, so it's unit-tested on every
/// target the way the module doc promises the arithmetic is.
fn power_from_rails(cpu_mw: Option<f64>, gpu_mw: Option<f64>, ane_mw: Option<f64>) -> Option<PowerSample> {
    match (cpu_mw, gpu_mw, ane_mw) {
        (Some(cpu_mw), Some(gpu_mw), Some(ane_mw)) => Some(PowerSample { cpu_mw, gpu_mw, ane_mw }),
        _ => None,
    }
}

/// Attach each IOReport cluster's mean MHz to the perf-level cluster it
/// serves, matching by core count (see
/// [`ioreport::assign_groups_to_levels`]). A cluster with no matching
/// IOReport group keeps `mhz: None` rather than borrowing another's.
///
/// Only called from [`HostProbe::sample`]'s aarch64 branch — cfg-gated the
/// same way so non-Apple-Silicon targets don't carry it as dead code under
/// `-D warnings` (#2108 CI finding).
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn attach_cluster_mhz(
    clusters: &mut [CpuCluster],
    cluster_mhz: &BTreeMap<String, u32>,
    cluster_cores: &BTreeMap<String, usize>,
) {
    if clusters.is_empty() || cluster_mhz.is_empty() {
        return;
    }
    // BTreeMap iteration is sorted, so `keys`/`sizes`/`mean_mhz` are
    // index-aligned and deterministic across samples. `mean_mhz` feeds
    // `assign_groups_to_levels`'s same-core-count tie-break (#2108 review
    // finding — matching by count alone silently swapped Performance's and
    // Efficiency's MHz on a chip whose perf levels have equal core counts).
    let keys: Vec<&String> = cluster_cores.keys().collect();
    let sizes: Vec<usize> = keys.iter().map(|k| cluster_cores[*k]).collect();
    let mean_mhz: Vec<u32> = keys.iter().map(|k| cluster_mhz.get(*k).copied().unwrap_or(0)).collect();
    let level_cores: Vec<usize> = clusters.iter().map(|c| c.cores).collect();
    for (c, g) in clusters
        .iter_mut()
        .zip(ioreport::assign_groups_to_levels(&sizes, &mean_mhz, &level_cores))
    {
        c.mhz = g.and_then(|i| cluster_mhz.get(keys[i]).copied());
    }
}

// Log-once latches: an unavailable source is a fact about the HOST, so
// repeating it every tick would be noise proportional to uptime.
// UNAVAILABLE_IOREPORT/UNAVAILABLE_FREQS are only read from the aarch64
// branch of `HostProbe::new` — cfg-gated so non-Apple-Silicon targets don't
// carry them as dead code under `-D warnings` (#2108 CI finding).
// UNAVAILABLE_MACH is read unconditionally (mach counters are checked on
// every target), so it stays ungated.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
static UNAVAILABLE_IOREPORT: std::sync::Once = std::sync::Once::new();
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
static UNAVAILABLE_FREQS: std::sync::Once = std::sync::Once::new();
static UNAVAILABLE_MACH: std::sync::Once = std::sync::Once::new();

fn log_once(latch: &std::sync::Once, msg: &str) {
    latch.call_once(|| eprintln!("{msg}"));
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod platform {
    use crate::host_probe::iokit;

    /// GPU utilization percent and in-use system memory, from the
    /// `IOAccelerator` IORegistry node's `PerformanceStatistics` dict.
    ///
    /// Read IN PROCESS (~1.0 ms) rather than by shelling `ioreg -r -d 1 -c
    /// IOAccelerator` (~21 ms) — same node, same two keys, same
    /// max-across-accelerators rule as
    /// [`crate::telemetry_sampler::gpu_percent_from_ioreg`], one twentieth
    /// of the observer cost and no process spawn on the measured machine.
    pub fn gpu_read() -> Option<(u64, Option<u64>)> {
        let mut util: Option<u64> = None;
        let mut mem: Option<u64> = None;
        iokit::for_each_service("IOAccelerator", |props| {
            // SAFETY: `props` is live for the callback and every accessor
            // type-checks before reading.
            unsafe {
                let Some(ps) = iokit::dict_dict(props, "PerformanceStatistics") else {
                    return;
                };
                if let Some(u) = iokit::dict_i64(ps, "Device Utilization %") {
                    let u = u.clamp(0, 100) as u64;
                    util = Some(util.map_or(u, |b| b.max(u)));
                }
                if let Some(m) = iokit::dict_i64(ps, "In use system memory") {
                    if m >= 0 {
                        mem = Some(mem.map_or(m as u64, |b: u64| b.max(m as u64)));
                    }
                }
            }
        });
        util.map(|u| (u, mem))
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
mod platform {
    pub fn gpu_read() -> Option<(u64, Option<u64>)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(cpu: f64, gpu: f64, ane: f64) -> Option<PowerSample> {
        Some(PowerSample {
            cpu_mw: cpu,
            gpu_mw: gpu,
            ane_mw: ane,
        })
    }

    fn t(state: &str, limit: u64) -> Option<ThermalSample> {
        Some(ThermalSample {
            state: state.to_string(),
            cpu_speed_limit_pct: limit,
        })
    }

    fn at(at_ms: u64, power: Option<PowerSample>, thermal: Option<ThermalSample>) -> HostExtraAt {
        HostExtraAt {
            at_ms,
            power,
            thermal,
        }
    }

    #[test]
    fn empty_series_reduces_to_all_none() {
        let x = reduce_host_extras(&[], None);
        assert_eq!(x, HostExtras::default());
        assert!(x.power.is_none() && x.thermal.is_none() && x.energy_mwh.is_none());
    }

    #[test]
    fn power_window_reduces_each_rail() {
        let raw = vec![
            at(0, p(1000.0, 10.0, 0.0), None),
            at(1000, p(3000.0, 20.0, 0.0), None),
            at(2000, p(2000.0, 30.0, 0.0), None),
        ];
        let w = reduce_host_extras(&raw, None).power.expect("power measured");
        assert_eq!(w.cpu.mean_mw, Some(2000.0));
        assert_eq!(w.cpu.max_mw, Some(3000));
        assert_eq!(w.cpu.p95_mw, Some(3000), "nearest-rank on 3 samples is the max");
        assert_eq!(w.gpu.mean_mw, Some(20.0));
        // total = cpu + gpu + ane per sample: 1010, 3020, 2030 → mean 2020
        assert_eq!(w.total.mean_mw, Some(2020.0));
        assert_eq!(w.total.max_mw, Some(3020));
    }

    #[test]
    fn energy_is_left_riemann_over_measured_gaps() {
        // 3600 mW held for 1000 ms → 3600 * 1000 / 3.6e6 = 1.0 mWh.
        let raw = vec![at(0, p(3600.0, 0.0, 0.0), None), at(1000, p(0.0, 0.0, 0.0), None)];
        let e = reduce_host_extras(&raw, None).energy_mwh.expect("energy measured");
        assert!((e - 1.0).abs() < 1e-9, "got {e}");
    }

    #[test]
    fn energy_is_rounded_to_microwatt_hours_not_full_float_noise() {
        // A gap that produces a repeating expansion: 1000 mW for 1111 ms.
        let raw = vec![
            at(0, p(1000.0, 0.0, 0.0), None),
            at(1111, p(0.0, 0.0, 0.0), None),
        ];
        let e = reduce_host_extras(&raw, None).energy_mwh.expect("energy");
        assert_eq!(
            e, 0.309,
            "no sampler at a 1-5s cadence justifies more than microwatt-hour precision"
        );
    }

    #[test]
    fn energy_ignores_a_trailing_sample_with_no_successor() {
        // One sample alone bounds no interval → 0 mWh, not a fabricated one.
        let raw = vec![at(0, p(3600.0, 0.0, 0.0), None)];
        assert_eq!(reduce_host_extras(&raw, None).energy_mwh, Some(0.0));
    }

    #[test]
    fn energy_uses_the_measured_gap_not_a_nominal_cadence() {
        // Same power, a 2x longer gap → 2x the energy.
        let a = reduce_host_extras(
            &[at(0, p(3600.0, 0.0, 0.0), None), at(1000, p(0.0, 0.0, 0.0), None)],
            None,
        )
        .energy_mwh
        .unwrap();
        let b = reduce_host_extras(
            &[at(0, p(3600.0, 0.0, 0.0), None), at(2000, p(0.0, 0.0, 0.0), None)],
            None,
        )
        .energy_mwh
        .unwrap();
        assert!((b - 2.0 * a).abs() < 1e-9, "{b} should be 2x {a}");
    }

    // (#2108 review finding) A laptop sleeping for hours between two ring
    // entries is exactly the shape a left-Riemann sum can't tell apart from
    // "the pre-sleep reading held continuously the whole time" — these three
    // tests pin the cap that closes it.
    #[test]
    fn energy_caps_a_gap_that_outruns_the_configured_cadence() {
        // Configured cadence 5000 ms → cap at 3x = 15000 ms. Without the
        // cap, this pair would report 3600 * 28_800_000 / 3.6e6 = 28800 mWh
        // — 8 hours of "held continuously" from one pre-sleep reading.
        let eight_hours_ms = 8 * 60 * 60 * 1000;
        let raw = vec![
            at(0, p(3600.0, 0.0, 0.0), None),
            at(eight_hours_ms, p(0.0, 0.0, 0.0), None),
        ];
        let e = reduce_host_extras(&raw, Some(5000)).energy_mwh.expect("energy measured");
        assert_eq!(e, 15.0, "capped at 3x the 5000ms cadence (15000ms), not the 8-hour gap");
    }

    #[test]
    fn energy_is_uncapped_when_the_cadence_is_unknown() {
        // `None` (a ring the sampler thread never configured — e.g. a
        // `push_for_test`-only ring in a test) skips the cap entirely,
        // rather than guessing a cadence from data a single huge gap would
        // itself corrupt.
        let eight_hours_ms = 8 * 60 * 60 * 1000;
        let raw = vec![
            at(0, p(3600.0, 0.0, 0.0), None),
            at(eight_hours_ms, p(0.0, 0.0, 0.0), None),
        ];
        let e = reduce_host_extras(&raw, None).energy_mwh.expect("energy measured");
        assert_eq!(e, 28800.0, "no configured cadence ⇒ no cap");
    }

    #[test]
    fn thermal_above_nominal_caps_a_sleep_gap_too() {
        // Same cap, same reasoning, the OTHER left-Riemann sum in this
        // function — a pre-sleep "serious" reading must not bill 8 hours of
        // "above nominal" against a machine that was actually asleep.
        let eight_hours_ms = 8 * 60 * 60 * 1000;
        let raw = vec![at(0, None, t("serious", 62)), at(eight_hours_ms, None, t("nominal", 100))];
        let w = reduce_host_extras(&raw, Some(5000)).thermal.expect("thermal measured");
        assert_eq!(w.above_nominal_ms, 15_000, "capped, not the full 8-hour sleep gap");
    }

    #[test]
    fn thermal_window_takes_the_worst_state_and_the_lowest_cap() {
        let raw = vec![
            at(0, None, t("nominal", 100)),
            at(1000, None, t("serious", 62)),
            at(2000, None, t("fair", 80)),
        ];
        let w = reduce_host_extras(&raw, None).thermal.expect("thermal measured");
        assert_eq!(w.worst_state, "serious");
        assert_eq!(w.min_cpu_speed_limit_pct, 62);
        // Left-Riemann: the `serious` sample at 1000 holds until 2000.
        assert_eq!(w.above_nominal_ms, 1000);
    }

    #[test]
    fn an_unrecognized_thermal_state_outranks_critical() {
        let raw = vec![
            at(0, None, t("critical", 50)),
            at(1000, None, t("unknown-9", 100)),
        ];
        let w = reduce_host_extras(&raw, None).thermal.expect("thermal measured");
        assert_eq!(
            w.worst_state, "unknown-9",
            "a state this build doesn't know is not evidence that nothing was wrong"
        );
    }

    #[test]
    fn thermal_absent_from_every_sample_reduces_to_none() {
        let raw = vec![at(0, p(1.0, 1.0, 1.0), None), at(1000, p(1.0, 1.0, 1.0), None)];
        let x = reduce_host_extras(&raw, None);
        assert!(x.thermal.is_none(), "no thermal source ⇒ no thermal block");
        assert!(x.power.is_some(), "…but power still reduces");
    }

    #[test]
    fn power_absent_from_every_sample_reduces_to_none() {
        let raw = vec![at(0, None, t("nominal", 100)), at(1000, None, t("nominal", 100))];
        let x = reduce_host_extras(&raw, None);
        assert!(x.power.is_none() && x.energy_mwh.is_none());
        assert!(x.thermal.is_some());
    }

    #[test]
    fn a_sample_missing_power_is_skipped_not_zeroed() {
        // The first sample of any probe has no power (no delta yet).
        let raw = vec![
            at(0, None, None),
            at(1000, p(2000.0, 0.0, 0.0), None),
            at(2000, p(2000.0, 0.0, 0.0), None),
        ];
        let w = reduce_host_extras(&raw, None).power.expect("power measured");
        assert_eq!(w.cpu.mean_mw, Some(2000.0), "the null sample must not drag the mean to 1333");
    }

    // `attach_cluster_mhz` itself is aarch64-gated (#2108 CI finding — see
    // its definition), so these two direct-call tests are a known, narrower
    // coverage than the module doc's stated "arithmetic is unit-tested
    // everywhere" goal: revisit if the DVFS-matching arithmetic is ever
    // split out into a target-independent helper.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn attach_cluster_mhz_matches_by_core_count() {
        let mut clusters = vec![
            CpuCluster { name: "Super".into(), cores: 6, pct: Some(10), mhz: None },
            CpuCluster { name: "Performance".into(), cores: 12, pct: Some(20), mhz: None },
        ];
        let mhz = BTreeMap::from([("MCPU".to_string(), 2038u32), ("PCPU".to_string(), 2655)]);
        let cores = BTreeMap::from([("MCPU".to_string(), 12usize), ("PCPU".to_string(), 6)]);
        attach_cluster_mhz(&mut clusters, &mhz, &cores);
        assert_eq!(clusters[0].mhz, Some(2655), "Super(6) ← PCPU(6)");
        assert_eq!(clusters[1].mhz, Some(2038), "Performance(12) ← MCPU(12)");
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn attach_cluster_mhz_leaves_an_unmatched_cluster_null() {
        let mut clusters = vec![
            CpuCluster { name: "Super".into(), cores: 6, pct: None, mhz: None },
            CpuCluster { name: "Efficiency".into(), cores: 4, pct: None, mhz: None },
        ];
        let mhz = BTreeMap::from([("PCPU".to_string(), 3000u32)]);
        let cores = BTreeMap::from([("PCPU".to_string(), 6usize)]);
        attach_cluster_mhz(&mut clusters, &mhz, &cores);
        assert_eq!(clusters[0].mhz, Some(3000));
        assert_eq!(clusters[1].mhz, None, "no 4-core IOReport group ⇒ no frequency claim");
    }

    // (#2108 review finding) A hypothetical 4-Performance + 4-Efficiency
    // chip: both perf levels have the SAME core count, so IOReport's two
    // matching groups (ECPU, PCPU — both count 4) can't be told apart by
    // count alone. Before the fix, `attach_cluster_mhz` matched by count in
    // `cluster_cores`' BTreeMap-sorted (alphabetical) order, so ECPU (which
    // sorts before PCPU) always won the FIRST cluster asking, regardless of
    // tier — silently giving the Performance cluster Efficiency's 1200 MHz
    // instead of Performance's own 3000 MHz.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn attach_cluster_mhz_tie_breaks_equal_counts_by_mean_mhz() {
        let mut clusters = vec![
            CpuCluster { name: "Performance".into(), cores: 4, pct: Some(10), mhz: None },
            CpuCluster { name: "Efficiency".into(), cores: 4, pct: Some(5), mhz: None },
        ];
        let mhz = BTreeMap::from([("ECPU".to_string(), 1200u32), ("PCPU".to_string(), 3000)]);
        let cores = BTreeMap::from([("ECPU".to_string(), 4usize), ("PCPU".to_string(), 4)]);
        attach_cluster_mhz(&mut clusters, &mhz, &cores);
        assert_eq!(clusters[0].mhz, Some(3000), "Performance must get PCPU's 3000 MHz, not ECPU's 1200");
        assert_eq!(clusters[1].mhz, Some(1200), "Efficiency gets what's left: ECPU's 1200 MHz");
    }

    #[test]
    fn total_mw_sums_the_three_rails() {
        let p = PowerSample { cpu_mw: 1200.0, gpu_mw: 30.0, ane_mw: 5.0 };
        assert_eq!(p.total_mw(), 1235.0);
    }

    // (#2108 review finding) `power_from_rails`: an unrecognized unit on
    // ONE rail (e.g. `ioreport::energy_delta_to_mw` sees a unit outside its
    // closed set) must not zero that rail while reporting the other two —
    // that is indistinguishable on the wire from "this rail genuinely
    // measured zero milliwatts".
    #[test]
    fn power_from_rails_requires_all_three_or_none() {
        assert_eq!(
            power_from_rails(Some(1200.0), Some(30.0), Some(5.0)),
            Some(PowerSample { cpu_mw: 1200.0, gpu_mw: 30.0, ane_mw: 5.0 }),
            "all three present ⇒ a real reading"
        );
        assert_eq!(power_from_rails(None, None, None), None, "none present ⇒ no reading at all");
    }

    #[test]
    fn power_from_rails_an_unknown_unit_rail_is_not_zeroed() {
        // ANE's unit failed to parse (e.g. an unrecognized energy unit);
        // CPU and GPU read fine. The pre-fix code reported
        // `Some(PowerSample { cpu_mw: 1200.0, gpu_mw: 30.0, ane_mw: 0.0 })`
        // — a fabricated "ANE draws 0 mW" that looks exactly like a real
        // measurement. The fix reports no power at all for this sample
        // rather than mixing two real numbers with one invented zero.
        assert_eq!(
            power_from_rails(Some(1200.0), Some(30.0), None),
            None,
            "one rail unmeasured ⇒ the WHOLE sample is unmeasured, never partially zeroed"
        );
        assert_eq!(power_from_rails(Some(1200.0), None, Some(5.0)), None, "same for the GPU rail");
        assert_eq!(power_from_rails(None, Some(30.0), Some(5.0)), None, "same for the CPU rail");
    }

    // ── The one test that runs the REAL probe ─────────────────────────────
    //
    // macOS/aarch64-gated: every source it exercises is Apple-Silicon-only,
    // and on any other target the probe correctly reports nothing, which
    // would make these assertions vacuous. `#[serial]` because the probe
    // measures the machine — a parallel test hammering the CPU would not
    // make it FAIL (nothing here asserts an idle host), but it would make
    // the cost assertion noisier than it needs to be.
    //
    // `DARKMUX_EXPECT_IOREPORT=1` gates the assertions that IOReport itself
    // (and everything downstream of it — freq tables, power, GPU MHz)
    // actually resolved. A real Apple Silicon Mac always resolves it, but a
    // GitHub-hosted macOS runner's VM has no IOReport channels / `pmgr`
    // IORegistry node — the CI failure this env knob fixes (#2108): a
    // panic at `IOReport did not load` on every macOS run, on a test that
    // was never wrong about a developer's own machine. Set the env var
    // locally (or in a workflow running on real Apple Silicon) to restore
    // the strict check; documented as a test-only knob in
    // docs/ENVIRONMENT.md. Unconditional either way: no panic, the cost
    // budget, and every field in range or `None` — the degradation
    // contract this module exists to guarantee.
    // Only called from the aarch64-gated live-probe test below — cfg-gated
    // the same way so a non-Apple-Silicon TEST build doesn't flag it as
    // dead code under `-D warnings` (the same class of finding as #1's
    // `attach_cluster_mhz`, caught the same way: `cargo clippy --target
    // x86_64-apple-darwin --all-targets`, not just `--lib`).
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn expect_ioreport() -> bool {
        std::env::var("DARKMUX_EXPECT_IOREPORT").as_deref() == Ok("1")
    }

    #[test]
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[serial_test::serial]
    fn real_probe_samples_within_the_cost_budget_and_in_range() {
        let mut probe = HostProbe::new();
        // First sample seeds the deltas; it reports no cpu_pct/power by design.
        let first = probe.sample();
        assert_eq!(
            first.cpu_pct, None,
            "the first sample has no previous counters to difference against"
        );
        assert_eq!(first.power, None, "…and no energy delta either");

        // Twenty samples at a realistic cadence, measuring the probe's own
        // stamped cost. The pre-#2108 shell-out path measured ~780 ms.
        let mut costs = Vec::new();
        let mut complete = None;
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let s = probe.sample();
            costs.push(s.cost_ms);
            complete = Some(s);
        }
        let s = complete.expect("twenty samples");
        let mean = costs.iter().sum::<u64>() as f64 / costs.len() as f64;
        let max = *costs.iter().max().unwrap();
        assert!(
            max < 60,
            "the observer must stay negligible: max {max} ms, mean {mean:.1} ms over {} samples",
            costs.len()
        );

        // Every field that IS present must be in range. Absence is allowed
        // (a future macOS could move any of these); a nonsense value is not.
        let cpu = s.cpu_pct.expect("mach tick deltas are always available on macOS");
        assert!(cpu <= 100, "cpu_pct out of range: {cpu}");
        for v in [s.mem_pct, s.gpu_pct].into_iter().flatten() {
            assert!(v <= 100, "percent field out of range: {v}");
        }
        let src = probe.sources();
        assert!(src.mach, "mach counters are always available on macOS");
        assert!(src.thermal, "ProcessInfo.thermalState did not resolve");
        // Apple Silicon + IOReport is the configuration darkmux is marketed
        // for. Asserting the sources RESOLVED — not merely that they didn't
        // crash — is what makes this test fail if the IOReport half silently
        // stops loading (a private framework whose path has already moved
        // once between macOS releases). Without these, breaking
        // `IOREPORT_PATHS` leaves the whole suite green while every power
        // and frequency number goes permanently null. BUT a GitHub-hosted
        // macOS runner's VM genuinely has no IOReport channels / `pmgr`
        // IORegistry node — a fact about the VM, not a regression — so
        // these assertions are opt-in via `DARKMUX_EXPECT_IOREPORT=1`
        // (`expect_ioreport`, above). Everything outside this block still
        // runs unconditionally: no panic, the cost budget, and every field
        // in range or `None`.
        if expect_ioreport() {
            assert!(
                src.ioreport,
                "IOReport did not load — if this fires on a new macOS, the framework moved again; \
                 see `ioreport::IOREPORT_PATHS`"
            );
            assert!(src.freq_tables, "the pmgr DVFS frequency tables did not resolve");
            assert!(src.ioreg_gpu, "the IOAccelerator IORegistry node did not resolve");
            assert!(
                s.power.is_some(),
                "IOReport resolved, so the Energy Model rails must produce a reading"
            );
            assert!(
                s.gpu_mhz.is_some(),
                "IOReport + freq tables resolved, so the GPU perf state must produce a frequency"
            );
        }

        let clusters = s.cpu_clusters.as_ref().expect("hw.perflevelN is reported on Apple Silicon");
        assert!(!clusters.is_empty(), "cpu_clusters is null when empty, never an empty vec");
        if expect_ioreport() {
            assert!(
                clusters.iter().any(|c| c.mhz.is_some()),
                "at least one perf-level cluster must match an IOReport group and get a frequency"
            );
        }
        for c in clusters {
            assert!(c.cores > 0, "a reported cluster has cores");
            if let Some(pct) = c.pct {
                assert!(pct <= 100, "cluster {} pct out of range: {pct}", c.name);
            }
            if let Some(mhz) = c.mhz {
                assert!(
                    (200..=10_000).contains(&mhz),
                    "cluster {} mhz outside any plausible SoC table: {mhz}",
                    c.name
                );
            }
        }
        if let Some(mhz) = s.gpu_mhz {
            assert!((50..=5_000).contains(&mhz), "gpu_mhz out of range: {mhz}");
        }
        if let Some(t) = &s.thermal {
            assert!(!t.state.is_empty());
            assert!(t.cpu_speed_limit_pct <= 100);
        }
        if let Some(p) = &s.power {
            // A Mac drawing more than 1 kW, or negative power, means the unit
            // scaling is wrong — the exact failure the closed unit set guards.
            for (name, mw) in [("cpu", p.cpu_mw), ("gpu", p.gpu_mw), ("ane", p.ane_mw)] {
                assert!(
                    (0.0..=1_000_000.0).contains(&mw),
                    "{name} power implausible ({mw} mW) — check the energy unit scaling"
                );
            }
        }
    }
}
