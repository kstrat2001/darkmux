//! (#2107, #1833, #2108) Daemon-side continuous host sampler for the machine
//! stats drawer (phone bottom tab, desktop modal).
//!
//! Before this module, the only host samples anywhere in darkmux were the
//! per-dispatch `telemetry.process` flow records
//! `darkmux-crew::dispatch_internal::run_telemetry_sampler` writes WHILE a
//! dispatch is running — so the drawer read `idle · no samples in the last
//! 10 min` between dispatches, even though the operator wants to glance at
//! machine load at any time. This module gives `darkmux serve` its own
//! background sampler, independent of any dispatch, feeding
//! `GET /machine/resources`' `load` block.
//!
//! **CLAUDE.md "the observer must not join the observed" (#1286 origin,
//! restated #1833):** this sampler contains ZERO model dispatches. As of
//! #2108 it also contains zero PROCESS SPAWNS: it owns a
//! [`darkmux_crew::host_probe::HostProbe`], which reads mach kernel
//! counters, the IORegistry and IOReport IN PROCESS. The previous mechanism
//! (`top`/`vm_stat`/`sysctl`/`ioreg`) cost ~780 ms of the measured machine's
//! time per tick — a sixth of a 5 s cadence spent on four process spawns
//! just to watch the machine — and its CPU figure was `top -l 1`'s
//! since-boot average rather than a reading of the interval. It writes NO
//! flow records: a continuous background sampler emitting a record every
//! tick would double (or worse) the fleet stream's size for a signal that's
//! daemon-local by nature, which is exactly the "casual observability path
//! grows a durable-storage cost" failure this rule guards against. It only
//! ever feeds an in-memory ring this process holds for its own
//! `/machine/resources` handler to read.
//!
//! Constraint 3 ("samplers stamp their own cost") and constraint 4
//! ("cadence is a recorded knob") are honored explicitly: each ring entry
//! carries the probe's OWN measured cost for that sample, surfaced verbatim
//! as `load.now.sampler_cost_ms` (host-sample-shape v2 replaced the pre-v2
//! running `sampler_cost_ms_mean` with this per-sample figure), and the
//! configured cadence (`runtime.host_sampler_interval_ms`,
//! `config_access::host_sampler_interval_ms`) rides into the payload
//! alongside the MEASURED mean gap between samples (`window.interval_ms`,
//! via the shared `reduce_host_stats` reduction) — so "the observer was
//! negligible" and "the cadence is what it claims" are both verifiable
//! facts in the response, not assumptions.

use darkmux_crew::host_probe::{
    reduce_host_extras, HostExtraAt, HostProbe, HostSampleFull, MwStats, PowerSample, ThermalSample,
};
use darkmux_crew::telemetry_sampler::{reduce_host_stats, HostSampleAt};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 10 minutes of history at the default 5s cadence. The ring is capacity-
/// bounded by ENTRY COUNT, not by wall-clock span — at a faster-than-default
/// cadence the window is shorter than 10 minutes; at a slower one, longer.
/// That's an intentional, visible tradeoff (the configured cadence is in
/// the payload) rather than a second hidden knob.
const RING_CAPACITY: usize = 120;

/// How often the sampler thread polls its stop flag while napping between
/// ticks — bounds shutdown latency to this, not a full sample interval.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// One ring entry: a host reading plus the wall-clock it was taken at.
///
/// The probe stamps its own cost INTO the sample
/// (`HostSampleFull::cost_ms`), so unlike the pre-#2108 shape there is no
/// separate `cost_ms` field for the ring to keep in sync with the reading it
/// describes.
#[derive(Debug, Clone)]
struct RingEntry {
    /// Wall-clock epoch ms this sample was taken — unlike the dispatch-
    /// scoped sampler's `Instant`-relative clock, this sampler has no
    /// single dispatch start to be relative to, and the drawer wants an
    /// absolute `sampled_at_ms` for its "now" reading anyway.
    at_ms: u64,
    sample: HostSampleFull,
}

/// Thread-safe fixed-capacity ring of the daemon's own host samples. Cheap
/// to `Clone` (an `Arc` around the actual storage) so it can live in
/// `AppState` and be shared between the sampler thread (writer) and every
/// `/machine/resources` request (reader) without contention beyond a brief
/// mutex hold.
#[derive(Clone)]
pub(crate) struct HostSamplerRing {
    inner: Arc<Mutex<VecDeque<RingEntry>>>,
}

/// `mean`/`p95`/`max` — the ROUTE's wire names for one metric's window
/// reduction. Deliberately different from the internal
/// `peak_pct`/`mean_pct`/`p95_pct` naming: this is what
/// `/machine/resources` promises callers (`ui/src/types/handwritten.ts`'s
/// `MachineLoadMetric`), not a re-export of the reduction type. The internal
/// numbers FEED these; they are never re-derived here.
fn metric_json(m: &darkmux_crew::telemetry_sampler::MetricStats) -> serde_json::Value {
    serde_json::json!({ "mean": m.mean_pct, "p95": m.p95_pct, "max": m.peak_pct })
}

/// The same wire shape for a power rail, in milliwatts.
fn mw_json(m: &MwStats) -> serde_json::Value {
    serde_json::json!({ "mean": m.mean_mw, "p95": m.p95_mw, "max": m.max_mw })
}

fn thermal_now_json(t: &ThermalSample) -> serde_json::Value {
    serde_json::json!({ "state": t.state, "cpu_speed_limit_pct": t.cpu_speed_limit_pct })
}

/// `now.power_mw`. `total` is emitted rather than left to the client so
/// every consumer adds the rails the same way.
fn power_now_json(p: &PowerSample) -> serde_json::Value {
    serde_json::json!({
        "cpu": p.cpu_mw.round() as i64,
        "gpu": p.gpu_mw.round() as i64,
        "ane": p.ane_mw.round() as i64,
        "total": p.total_mw().round() as i64,
    })
}

impl HostSamplerRing {
    pub(crate) fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAPACITY))) }
    }

    fn push(&self, entry: RingEntry) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if g.len() >= RING_CAPACITY {
            g.pop_front();
        }
        g.push_back(entry);
    }

    /// Test-only injection hook: push a fabricated sample straight into the
    /// ring, bypassing `spawn`'s real [`HostProbe`] reads entirely. This is
    /// the "injected sampler" the `/machine/resources` route test
    /// (`lib_tests.rs`) uses so the route's `load` shape is exercised
    /// without touching the machine. `pub(crate)` (not `pub`) — the crate's
    /// own tests are the only external callers.
    #[cfg(test)]
    pub(crate) fn push_for_test(&self, at_ms: u64, cpu: u64, mem: u64, gpu: u64, cost_ms: u64) {
        self.push(RingEntry {
            at_ms,
            sample: HostSampleFull {
                cost_ms,
                cpu_pct: Some(cpu),
                mem_pct: Some(mem),
                gpu_pct: Some(gpu),
                ..Default::default()
            },
        });
    }

    /// The `load` block for `GET /machine/resources` (host-sample-shape v2 —
    /// the contract mirrored in `ui/src/types/handwritten.ts`'s
    /// `MachineLoad`). `None` when no sample has landed yet (the sampler
    /// just started, or is disabled via `runtime.host_sampler_interval_ms:
    /// 0`, in which case the caller never spawned the thread and this ring
    /// simply stays empty forever).
    ///
    /// Every field the probe could not read is emitted as JSON `null` rather
    /// than as a zero — "not measured" and "measured, and idle" are
    /// different claims, and the viewer renders them differently.
    pub(crate) fn snapshot(&self) -> Option<serde_json::Value> {
        let g = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let latest = g.back()?.clone();
        // cpu/mem/gpu go through the SAME reduction the dispatch envelope's
        // `host` block uses, so the two surfaces can never silently disagree
        // about what "mean"/"p95"/"max" mean.
        let raw: Vec<HostSampleAt> = g
            .iter()
            .map(|e| HostSampleAt {
                at_ms: e.at_ms,
                cpu: e.sample.cpu_pct,
                mem: e.sample.mem_pct,
                gpu: e.sample.gpu_pct,
            })
            .collect();
        let stats = reduce_host_stats(&raw);
        // …and power/thermal/energy through its #2108 sibling, which follows
        // the same conventions (left-Riemann duty, nearest-rank p95).
        let extras: Vec<HostExtraAt> = g
            .iter()
            .map(|e| HostExtraAt {
                at_ms: e.at_ms,
                power: e.sample.power,
                thermal: e.sample.thermal.clone(),
            })
            .collect();
        let ex = reduce_host_extras(&extras);
        // `unwrap`s below are safe: `raw` is non-empty because `latest`
        // (from `g.back()?` above) proved the ring holds at least one entry.
        let span_ms = raw.last().unwrap().at_ms.saturating_sub(raw.first().unwrap().at_ms);
        drop(g);

        let s = &latest.sample;
        let clusters = s.cpu_clusters.as_ref().map(|cs| {
            cs.iter()
                .map(|c| {
                    serde_json::json!({
                        "name": c.name,
                        "cores": c.cores,
                        "pct": c.pct,
                        "mhz": c.mhz,
                    })
                })
                .collect::<Vec<_>>()
        });

        Some(serde_json::json!({
            "now": {
                "sampled_at_ms": latest.at_ms,
                "sampler_cost_ms": s.cost_ms,
                "cpu_pct": s.cpu_pct,
                "cpu_clusters": clusters,
                "mem_pct": s.mem_pct,
                "gpu_pct": s.gpu_pct,
                "gpu_mhz": s.gpu_mhz,
                "gpu_mem_bytes": s.gpu_mem_bytes,
                "thermal": s.thermal.as_ref().map(thermal_now_json),
                "power_mw": s.power.as_ref().map(power_now_json),
            },
            "window": {
                "samples": stats.samples,
                "span_ms": span_ms,
                // MEASURED mean gap between samples, not the nominal
                // configured cadence — see this module's own doc.
                "interval_ms": stats.sample_interval_ms,
                "cpu_pct": metric_json(&stats.cpu),
                "gpu_pct": metric_json(&stats.gpu),
                "mem_pct": metric_json(&stats.mem),
                "power_mw": ex.power.as_ref().map(|p| serde_json::json!({
                    "total": mw_json(&p.total),
                    "cpu": mw_json(&p.cpu),
                    "gpu": mw_json(&p.gpu),
                })),
                "thermal": ex.thermal.as_ref().map(|t| serde_json::json!({
                    "worst_state": t.worst_state,
                    "above_nominal_ms": t.above_nominal_ms,
                    "min_cpu_speed_limit_pct": t.min_cpu_speed_limit_pct,
                })),
                "energy_mwh": ex.energy_mwh,
            },
        }))
    }
}

/// The process-wide ring `run()` spawns the sampler thread against and
/// `machine_resources_handler` reads from — same `OnceLock`-backed static
/// shape this crate already uses for `MACHINE_RESOURCES_CACHE`'s gather
/// lock. One ring per process; `HostSamplerRing::clone()` is cheap (an
/// `Arc` clone), so the sampler thread and every request handler share the
/// same underlying storage without either side owning the daemon's startup
/// sequencing.
pub(crate) fn ring() -> &'static HostSamplerRing {
    static RING: std::sync::OnceLock<HostSamplerRing> = std::sync::OnceLock::new();
    RING.get_or_init(HostSamplerRing::new)
}

/// Spawn the daemon-side host sampler thread. `interval_ms` is the
/// resolved `config_access::host_sampler_interval_ms()` cadence; `0`
/// disables the sampler entirely and this returns `None` without spawning
/// anything (the "0 means hard off" convention shared with
/// `remote.max_tokens_per_execution`). The returned thread runs until
/// `stop_flag` is set, polling it every [`STOP_POLL_INTERVAL`] so shutdown
/// is prompt rather than blocking for a full sample interval — the same
/// teardown shape `dispatch_internal::run_telemetry_sampler` uses.
///
/// (#2108) The [`HostProbe`] is constructed INSIDE the thread and owned by
/// it: the probe is stateful (CPU percent and every power rail are counter
/// deltas), so a private probe gives this sampler deltas that line up with
/// its own cadence rather than with whoever sampled last. Construction costs
/// ~80-100 ms once; each subsequent sample is ~5-10 ms. The FIRST sample
/// therefore carries no `cpu_pct` and no `power` — it is the one that seeds
/// the deltas, and the drawer renders those two as "not measured" for that
/// one tick.
pub(crate) fn spawn(
    interval_ms: u64,
    ring: HostSamplerRing,
    stop_flag: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    if interval_ms == 0 {
        return None;
    }
    let interval = Duration::from_millis(interval_ms);
    Some(std::thread::spawn(move || {
        let mut probe = HostProbe::new();
        loop {
            if stop_flag.load(Ordering::SeqCst) {
                break;
            }

            let sample = probe.sample();
            let at_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            // Best-effort like the dispatch-scoped sampler: a tick where
            // every field failed still gets recorded (with the probe's own
            // cost stamped) rather than skipped, since the cost of the
            // failed gather is itself part of the observer-cost claim.
            ring.push(RingEntry { at_ms, sample });

            let mut slept = Duration::ZERO;
            while slept < interval {
                if stop_flag.load(Ordering::SeqCst) {
                    return;
                }
                let nap = STOP_POLL_INTERVAL.min(interval - slept);
                std::thread::sleep(nap);
                slept += nap;
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_crew::host_probe::CpuCluster;
    use std::time::Instant;

    fn entry(at_ms: u64, cpu: u64, mem: u64, gpu: u64, cost_ms: u64) -> RingEntry {
        RingEntry {
            at_ms,
            sample: HostSampleFull {
                cost_ms,
                cpu_pct: Some(cpu),
                mem_pct: Some(mem),
                gpu_pct: Some(gpu),
                ..Default::default()
            },
        }
    }

    #[test]
    fn empty_ring_snapshots_to_none() {
        let ring = HostSamplerRing::new();
        assert!(ring.snapshot().is_none(), "no samples yet ⇒ no load block");
    }

    #[test]
    fn snapshot_reflects_latest_now_and_reduced_window() {
        let ring = HostSamplerRing::new();
        ring.push(entry(0, 50, 60, 70, 5));
        ring.push(entry(2000, 90, 65, 85, 6));
        ring.push(entry(4000, 40, 62, 30, 4));

        let v = ring.snapshot().expect("samples present");
        assert_eq!(v["now"]["cpu_pct"], 40, "now reflects the LATEST sample");
        assert_eq!(v["now"]["mem_pct"], 62);
        assert_eq!(v["now"]["gpu_pct"], 30);
        assert_eq!(v["now"]["sampled_at_ms"], 4000);
        // (#2108, v2) the per-sample observer cost, not a running mean.
        assert_eq!(v["now"]["sampler_cost_ms"], 4);

        assert_eq!(v["window"]["samples"], 3);
        assert_eq!(v["window"]["span_ms"], 4000);
        assert_eq!(v["window"]["interval_ms"], 2000, "measured mean gap");
        assert_eq!(v["window"]["cpu_pct"]["max"], 90, "peak reused via reduce_host_stats");
        assert_eq!(v["window"]["cpu_pct"]["mean"], 60.0);
    }

    #[test]
    fn unmeasured_fields_are_null_never_zero() {
        let ring = HostSamplerRing::new();
        ring.push(entry(0, 50, 60, 70, 5));
        let v = ring.snapshot().expect("samples present");
        for key in ["cpu_clusters", "gpu_mhz", "gpu_mem_bytes", "thermal", "power_mw"] {
            assert!(
                v["now"][key].is_null(),
                "`now.{key}` must be null when unmeasured, got {}",
                v["now"][key]
            );
        }
        for key in ["power_mw", "thermal", "energy_mwh"] {
            assert!(
                v["window"][key].is_null(),
                "`window.{key}` must be null when unmeasured"
            );
        }
    }

    #[test]
    fn snapshot_carries_the_2108_blocks_when_the_probe_read_them() {
        let ring = HostSamplerRing::new();
        let mk = |at_ms: u64| RingEntry {
            at_ms,
            sample: HostSampleFull {
                cost_ms: 7,
                cpu_pct: Some(19),
                cpu_clusters: Some(vec![
                    CpuCluster { name: "Super".into(), cores: 6, pct: Some(40), mhz: Some(4200) },
                    CpuCluster {
                        name: "Performance".into(),
                        cores: 12,
                        pct: Some(12),
                        mhz: Some(3100),
                    },
                ]),
                mem_pct: Some(51),
                gpu_pct: Some(0),
                gpu_mhz: Some(338),
                gpu_mem_bytes: Some(51_003_392),
                thermal: Some(ThermalSample {
                    state: "nominal".into(),
                    cpu_speed_limit_pct: 100,
                }),
                power: Some(PowerSample { cpu_mw: 1200.0, gpu_mw: 30.0, ane_mw: 0.0 }),
            },
        };
        ring.push(mk(0));
        // Exactly one hour apart, so the energy integral is a round number.
        ring.push(mk(3_600_000));

        let v = ring.snapshot().expect("samples present");
        assert_eq!(v["now"]["sampler_cost_ms"], 7);
        assert_eq!(v["now"]["cpu_clusters"][0]["name"], "Super");
        assert_eq!(v["now"]["cpu_clusters"][0]["cores"], 6);
        assert_eq!(v["now"]["cpu_clusters"][0]["mhz"], 4200);
        assert_eq!(v["now"]["cpu_clusters"][1]["pct"], 12);
        assert_eq!(v["now"]["gpu_mhz"], 338);
        assert_eq!(v["now"]["gpu_mem_bytes"], 51_003_392u64);
        assert_eq!(v["now"]["thermal"]["state"], "nominal");
        assert_eq!(v["now"]["thermal"]["cpu_speed_limit_pct"], 100);
        assert_eq!(v["now"]["power_mw"]["cpu"], 1200);
        assert_eq!(v["now"]["power_mw"]["gpu"], 30);
        assert_eq!(
            v["now"]["power_mw"]["total"], 1230,
            "total is emitted, never left to the client to add"
        );

        assert_eq!(v["window"]["power_mw"]["total"]["max"], 1230);
        assert_eq!(v["window"]["thermal"]["worst_state"], "nominal");
        assert_eq!(v["window"]["thermal"]["min_cpu_speed_limit_pct"], 100);
        // 1230 mW held for exactly one hour → 1230 mWh.
        let e = v["window"]["energy_mwh"].as_f64().expect("energy");
        assert!((e - 1230.0).abs() < 0.001, "got {e}");
    }

    #[test]
    fn ring_drops_oldest_beyond_capacity() {
        let ring = HostSamplerRing::new();
        for i in 0..(RING_CAPACITY + 5) {
            ring.push(entry(i as u64 * 1000, 1, 1, 1, 1));
        }
        let v = ring.snapshot().expect("samples present");
        assert_eq!(v["window"]["samples"], RING_CAPACITY as u64, "capped at RING_CAPACITY");
        // The oldest 5 entries (at_ms 0..5000) must have been evicted — the
        // window's span should reflect only the newest RING_CAPACITY entries.
        let expected_span = (RING_CAPACITY as u64 - 1) * 1000;
        assert_eq!(v["window"]["span_ms"], expected_span);
    }

    #[test]
    fn interval_zero_disables_the_sampler_no_thread_spawned() {
        let ring = HostSamplerRing::new();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn(0, ring.clone(), stop);
        assert!(handle.is_none(), "0ms interval must not spawn a thread");
        assert!(ring.snapshot().is_none(), "and the ring stays empty");
    }

    #[test]
    fn spawned_sampler_populates_the_ring_and_stops_promptly() {
        let ring = HostSamplerRing::new();
        let stop = Arc::new(AtomicBool::new(false));
        // A tight interval so the test doesn't wait long for a sample to land.
        let handle = spawn(50, ring.clone(), Arc::clone(&stop));
        assert!(handle.is_some(), "non-zero interval spawns a thread");

        // Wait (bounded) for at least one sample. The bound allows for the
        // probe's one-time construction cost (~100ms on macOS).
        let deadline = Instant::now() + Duration::from_secs(10);
        while ring.snapshot().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(ring.snapshot().is_some(), "at least one sample landed within the deadline");

        let t0 = Instant::now();
        stop.store(true, Ordering::SeqCst);
        handle.unwrap().join().expect("sampler thread joins cleanly");
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "teardown must be prompt (bounded by STOP_POLL_INTERVAL), not a full interval wait"
        );
    }
}
