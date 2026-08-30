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

use darkmux_crew::host_probe::{reduce_host_extras, HostExtraAt, HostProbe, HostSampleFull, MwStats};
use darkmux_crew::telemetry_sampler::{reduce_host_stats, HostSampleAt};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    /// The sampler thread's configured cadence in ms — `0` until [`spawn`]
    /// sets it (or forever, for a ring [`spawn`] never ran against). Feeds
    /// [`reduce_host_extras`]'s sleep-gap cap: `snapshot` reads this and
    /// passes it through as `Some`/`None` so a laptop that slept for hours
    /// between two ring entries can't bill the whole gap at the pre-sleep
    /// reading (#2108 review finding). An `AtomicU64`, not a plain field, so
    /// every clone of this `Arc`-backed ring sees the same value the
    /// sampler thread stored, without a second lock.
    configured_interval_ms: Arc<std::sync::atomic::AtomicU64>,
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

impl HostSamplerRing {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAPACITY))),
            configured_interval_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
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

    /// Test-only: set the cadence [`spawn`] would otherwise record, without
    /// actually spawning a sampler thread. Needed to exercise
    /// `snapshot`'s sleep-gap cap (`reduce_host_extras`'s
    /// `configured_interval_ms`) against a scripted ring built with
    /// `push`/`push_for_test` directly.
    #[cfg(test)]
    pub(crate) fn set_configured_interval_for_test(&self, ms: u64) {
        self.configured_interval_ms.store(ms, Ordering::Relaxed);
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
        // `0` means "spawn never configured this ring" (a `push_for_test`-
        // only ring in a test, or a snapshot taken before `spawn` ran) —
        // translated to `None` so the sleep-gap cap has nothing to guess a
        // cadence from.
        let configured_interval_ms = match self.configured_interval_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(ms),
        };
        let ex = reduce_host_extras(&extras, configured_interval_ms);
        // `unwrap`s below are safe: `raw` is non-empty because `latest`
        // (from `g.back()?` above) proved the ring holds at least one entry.
        let span_ms = raw.last().unwrap().at_ms.saturating_sub(raw.first().unwrap().at_ms);
        drop(g);

        // (#2111) The "now" shape is the shared `sample_full_json` mapping —
        // the same one `dispatch_internal::run_telemetry_sampler` uses for
        // the periodic `machine.telemetry` flow record's payload — so the
        // two never independently drift on what a host reading's JSON shape
        // means.
        Some(serde_json::json!({
            "now": darkmux_crew::host_probe::sample_full_json(&latest.sample, latest.at_ms),
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

/// (#2111) Build a `machine.thermal` TRANSITION flow record. `Level::Warn`
/// only when the state RISES into `serious` or `critical` — an
/// operator-actionable event; every other transition (recovering, or a
/// lateral move between non-elevated states) is `Level::Info`. No
/// mission/session context (the daemon sampler runs independently of any
/// dispatch); `machine_id`/`machine_uid` are left `None` so
/// `darkmux_flow::record`'s write-time auto-stamp fills them from this
/// machine's own provenance, the same as `machine.online`/`machine.offline`.
fn build_thermal_transition_record(
    from: &str,
    to: &str,
    sample: &HostSampleFull,
    sampled_at_ms: u64,
) -> darkmux_flow::FlowRecord {
    use darkmux_crew::host_probe::thermal_severity;
    let rising_into_elevated =
        thermal_severity(to) > thermal_severity(from) && thermal_severity(to) >= thermal_severity("serious");
    let level = if rising_into_elevated {
        darkmux_flow::Level::Warn
    } else {
        darkmux_flow::Level::Info
    };
    let payload = serde_json::json!({
        "from": from,
        "to": to,
        "cpu_speed_limit_pct": sample.thermal.as_ref().map(|t| t.cpu_speed_limit_pct),
        "power_mw_total": sample.power.as_ref().map(|p| p.total_mw().round() as i64),
        "sampled_at_ms": sampled_at_ms,
    });
    let display_name = darkmux_flow::resolve_machine_id().unwrap_or_else(|| "unknown".to_string());
    darkmux_flow::FlowRecord {
        ts: darkmux_flow::ts_utc_now(),
        level,
        category: darkmux_flow::Category::Machinery,
        tier: darkmux_flow::Tier::Local,
        stage: darkmux_flow::Stage::Dispatch,
        action: "machine.thermal".to_string(),
        handle: display_name,
        phase_id: None,
        session_id: None,
        source: Some("host-sampler".to_string()),
        model: None,
        reasoning: None,
        mission_id: None,
        machine_id: None,
        machine_uid: None,
        prev_hash: None,
        hash: None,
        payload: Some(payload),
        work_id: None,
        attempt: None,
    }
}

/// (#2111) Pure edge detector: given the previously KNOWN thermal state (or
/// `None` before any reading has landed) and this tick's own reading,
/// decide whether a `machine.thermal` TRANSITION should fire, and what the
/// known state becomes for the NEXT call. Testable without a probe, a ring,
/// or a thread — `spawn`'s loop below is a thin, unit-untestable wrapper
/// around this.
///
/// - `sample.thermal` absent (the probe couldn't read it this tick): no
///   transition, and the known state is UNCHANGED — an absent reading is
///   not evidence the state changed, or that it didn't.
/// - No prior known state (daemon just started): this reading seeds the
///   baseline SILENTLY. The first reading is never a "transition" — there
///   is nothing to have transitioned FROM.
/// - Same state as before: no emit, REGARDLESS of how much wall-clock
///   elapsed since the prior tick — including across a sleep/wake gap the
///   ring's own gap-capping logic (`reduce_host_extras`) would separately
///   flag downstream. A state that reads the same on both sides of a gap
///   never fires.
/// - Different state: a genuine transition — emit, whether or not a gap
///   preceded it. "The state actually differs" is the only condition that
///   matters here.
fn thermal_edge(
    prev: Option<&str>,
    sample: &HostSampleFull,
    sampled_at_ms: u64,
) -> (Option<String>, Option<darkmux_flow::FlowRecord>) {
    let Some(t) = sample.thermal.as_ref() else {
        return (prev.map(str::to_string), None);
    };
    match prev {
        None => (Some(t.state.clone()), None),
        Some(p) if p == t.state => (Some(t.state.clone()), None),
        Some(p) => {
            let rec = build_thermal_transition_record(p, &t.state, sample, sampled_at_ms);
            (Some(t.state.clone()), Some(rec))
        }
    }
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
    // `snapshot`'s sleep-gap cap (`reduce_host_extras`) reads this back —
    // recorded here, once, rather than re-resolved from config on every
    // request.
    ring.configured_interval_ms.store(interval_ms, Ordering::Relaxed);
    let interval = Duration::from_millis(interval_ms);
    Some(std::thread::spawn(move || {
        let mut probe = HostProbe::new();
        // (#2111) The known thermal state carried between ticks for
        // `thermal_edge`'s edge detection — see that function's own doc.
        let mut known_thermal_state: Option<String> = None;
        loop {
            if stop_flag.load(Ordering::SeqCst) {
                break;
            }

            let sample = probe.sample();
            // (#2111 review finding) The shared epoch-ms read — the same
            // one `dispatch_internal::run_telemetry_sampler` now uses for
            // `machine.telemetry`'s `sampled_at_ms`, so a viewer strip
            // charting both producers' records is comparing the same
            // clock, not one producer's epoch against another's
            // sampler-relative offset.
            let at_ms = darkmux_crew::host_probe::epoch_ms_now();
            // (#2111) Edge-detect BEFORE the sample moves into the ring —
            // `thermal_edge` only borrows it.
            let (next_state, transition) = thermal_edge(known_thermal_state.as_deref(), &sample, at_ms);
            known_thermal_state = next_state;
            if let Some(rec) = transition {
                let _ = darkmux_flow::record(rec);
            }
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
    use darkmux_crew::host_probe::{CpuCluster, PowerSample, ThermalSample};
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
        // A real daemon ring always has this set — `spawn` is the only
        // production path that populates the ring with real probe reads,
        // and it records the cadence before the first sample lands. Setting
        // it here makes this test representative of what `snapshot`
        // actually returns in production, including the sleep-gap cap
        // below (#2108 review finding).
        ring.set_configured_interval_for_test(5000);
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
        // A full hour apart — far past the 5000ms cadence's 3x/15000ms cap,
        // deliberately: this pins that the cap engages, not the pre-fix
        // "held continuously for the whole gap" arithmetic.
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
        // (#2108 review finding) 1230 mW held for the FULL one-hour gap
        // would be 1230 mWh — that was the bug: a left-Riemann sum with no
        // cap bills a sleep/wake gap as if the pre-sleep reading held
        // throughout. Capped at 3x the 5000ms cadence (15000ms):
        // 1230 * 15000 / 3.6e6 = 5.125 mWh.
        let e = v["window"]["energy_mwh"].as_f64().expect("energy");
        assert!((e - 5.125).abs() < 0.001, "got {e}, expected the capped 5.125 mWh, not 1230");
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

    // ─── (#2111) thermal_edge — pure edge detection ─────────────────────

    fn thermal_sample(state: &str) -> HostSampleFull {
        HostSampleFull {
            thermal: Some(ThermalSample { state: state.to_string(), cpu_speed_limit_pct: 100 }),
            ..Default::default()
        }
    }

    #[test]
    fn thermal_edge_emits_only_on_real_transitions_with_correct_levels() {
        // nominal (seeds the baseline, no emit) -> nominal (no-op) -> fair
        // (Info: rising, but not into serious/critical) -> serious (Warn:
        // rising INTO serious) -> nominal (Info: falling). Mirrors the
        // original issue's acceptance criteria exactly (3 records from this
        // sequence).
        let sequence = ["nominal", "nominal", "fair", "serious", "nominal"];
        let mut prev: Option<String> = None;
        let mut records = Vec::new();
        for (i, state) in sequence.iter().enumerate() {
            let sample = thermal_sample(state);
            let (next, rec) = thermal_edge(prev.as_deref(), &sample, i as u64 * 1000);
            prev = next;
            if let Some(r) = rec {
                records.push(r);
            }
        }
        assert_eq!(records.len(), 3, "expected exactly 3 transitions, got {records:?}");

        let p0 = records[0].payload.as_ref().unwrap();
        assert_eq!(p0["from"], "nominal");
        assert_eq!(p0["to"], "fair");
        assert!(matches!(records[0].level, darkmux_flow::Level::Info), "rising to fair is Info");

        let p1 = records[1].payload.as_ref().unwrap();
        assert_eq!(p1["from"], "fair");
        assert_eq!(p1["to"], "serious");
        assert!(matches!(records[1].level, darkmux_flow::Level::Warn), "rising INTO serious is Warn");

        let p2 = records[2].payload.as_ref().unwrap();
        assert_eq!(p2["from"], "serious");
        assert_eq!(p2["to"], "nominal");
        assert!(matches!(records[2].level, darkmux_flow::Level::Info), "falling back to nominal is Info");

        for r in &records {
            assert!(matches!(r.category, darkmux_flow::Category::Machinery));
            assert!(matches!(r.tier, darkmux_flow::Tier::Local));
            assert_eq!(r.action, "machine.thermal");
            assert!(r.mission_id.is_none(), "no mission context — the daemon runs independently of any dispatch");
            assert!(r.session_id.is_none());
        }
    }

    #[test]
    fn thermal_edge_first_reading_seeds_baseline_silently() {
        let (next, rec) = thermal_edge(None, &thermal_sample("critical"), 0);
        assert!(rec.is_none(), "the very first reading is never a transition");
        assert_eq!(next.as_deref(), Some("critical"));
    }

    #[test]
    fn thermal_edge_same_state_across_a_large_gap_emits_nothing() {
        let (prev, seed_rec) = thermal_edge(None, &thermal_sample("nominal"), 0);
        assert!(seed_rec.is_none());
        // A sample taken a very long time later (simulating a sleep/wake
        // gap) that reports the SAME state must not emit — live edge
        // detection only cares whether the state differs; the ring's own
        // gap-capping logic (`reduce_host_extras`) is a separate,
        // retrospective concern over the reduced window, not this
        // per-tick detector.
        let (_next, rec) = thermal_edge(prev.as_deref(), &thermal_sample("nominal"), 3_600_000);
        assert!(rec.is_none(), "same state across any gap must not emit a transition");
    }

    #[test]
    fn thermal_edge_missing_reading_does_not_reset_baseline_or_emit() {
        let (prev, _) = thermal_edge(None, &thermal_sample("fair"), 0);
        assert_eq!(prev.as_deref(), Some("fair"));
        let missing = HostSampleFull::default(); // thermal: None — probe read failure
        let (prev2, rec) = thermal_edge(prev.as_deref(), &missing, 1000);
        assert!(rec.is_none(), "an absent reading is not evidence of a transition");
        assert_eq!(
            prev2.as_deref(),
            Some("fair"),
            "known state stays unchanged when the probe couldn't read thermal this tick"
        );
    }

    #[test]
    fn thermal_edge_payload_carries_speed_limit_power_and_sampled_at() {
        let (prev, _) = thermal_edge(None, &thermal_sample("nominal"), 0);
        let sample = HostSampleFull {
            thermal: Some(ThermalSample { state: "serious".into(), cpu_speed_limit_pct: 62 }),
            power: Some(PowerSample { cpu_mw: 1000.0, gpu_mw: 200.0, ane_mw: 30.0 }),
            ..Default::default()
        };
        let (_next, rec) = thermal_edge(prev.as_deref(), &sample, 5000);
        let rec = rec.expect("nominal -> serious is a real transition");
        let payload = rec.payload.unwrap();
        assert_eq!(payload["cpu_speed_limit_pct"], 62);
        assert_eq!(payload["power_mw_total"], 1230);
        assert_eq!(payload["sampled_at_ms"], 5000);
    }
}
