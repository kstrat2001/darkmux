//! (#2107, #1833) Daemon-side continuous host sampler for the machine
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
//! restated #1833):** this sampler contains ZERO model dispatches — it
//! reads kernel counters (`top`/`vm_stat`/`sysctl`) and `ioreg` only, via
//! the exact same [`darkmux_crew::telemetry_sampler::sample_host`] the
//! dispatch-scoped sampler uses (one mechanism, not two that could drift).
//! It writes NO flow records — a continuous background sampler emitting a
//! record every tick would double (or worse) the fleet stream's size for a
//! signal that's daemon-local by nature, which is exactly the "casual
//! observability path grows a durable-storage cost" failure this rule
//! guards against. It only ever feeds an in-memory ring this process holds
//! for its own `/machine/resources` handler to read.
//!
//! Constraint 3 ("samplers stamp their own cost") and constraint 4
//! ("cadence is a recorded knob") are honored explicitly: each ring entry
//! carries its OWN measured `sample_host()` cost
//! (`HostSamplerRing::snapshot`'s `sampler_cost_ms_mean`), and the
//! configured cadence (`runtime.host_sampler_interval_ms`,
//! `config_access::host_sampler_interval_ms`) rides into the payload
//! alongside the MEASURED mean gap between samples (`window.interval_ms`,
//! via the shared `reduce_host_stats` reduction) — so "the observer was
//! negligible" and "the cadence is what it claims" are both verifiable
//! facts in the response, not assumptions.

use darkmux_crew::telemetry_sampler::{reduce_host_stats, sample_host, HostSampleAt};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 10 minutes of history at the default 5s cadence. The ring is capacity-
/// bounded by ENTRY COUNT, not by wall-clock span — at a faster-than-default
/// cadence the window is shorter than 10 minutes; at a slower one, longer.
/// That's an intentional, visible tradeoff (the configured cadence is in
/// the payload) rather than a second hidden knob.
const RING_CAPACITY: usize = 120;

/// How often the sampler thread polls its stop flag while napping between
/// ticks — bounds shutdown latency to this, not a full sample interval.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// One ring entry: a host reading plus how long THIS sampler's own gather
/// took to produce it (the self-stamped observer cost).
#[derive(Debug, Clone, Copy)]
struct RingEntry {
    /// Wall-clock epoch ms this sample was taken — unlike the dispatch-
    /// scoped sampler's `Instant`-relative clock, this sampler has no
    /// single dispatch start to be relative to, and the drawer wants an
    /// absolute `sampled_at_ms` for its "now" reading anyway.
    at_ms: u64,
    cpu: Option<u64>,
    mem: Option<u64>,
    gpu: Option<u64>,
    /// Measured wall-clock cost of the `sample_host()` call that produced
    /// this entry (the observer-cost self-stamp).
    cost_ms: u64,
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
    /// ring, bypassing `spawn`'s real `sample_host()` shell-outs entirely.
    /// This is the "injected sampler" the `/machine/resources` route test
    /// (`lib_tests.rs`) uses so the route's `load` shape is exercised
    /// without a real `ioreg`/`top`/`vm_stat` call in CI. `pub(crate)` (not
    /// `pub`) — the crate's own `mod tests` is the only external caller.
    #[cfg(test)]
    pub(crate) fn push_for_test(&self, at_ms: u64, cpu: u64, mem: u64, gpu: u64, cost_ms: u64) {
        self.push(RingEntry { at_ms, cpu: Some(cpu), mem: Some(mem), gpu: Some(gpu), cost_ms });
    }

    /// The `load` block for `GET /machine/resources` — `None` when no
    /// sample has landed yet (the sampler just started, or is disabled via
    /// `runtime.host_sampler_interval_ms: 0`, in which case the caller
    /// never spawned the thread and this ring simply stays empty forever).
    pub(crate) fn snapshot(&self) -> Option<serde_json::Value> {
        let g = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let latest = g.back()?;
        let raw: Vec<HostSampleAt> = g
            .iter()
            .map(|e| HostSampleAt { at_ms: e.at_ms, cpu: e.cpu, mem: e.mem, gpu: e.gpu })
            .collect();
        let stats = reduce_host_stats(&raw);
        // `unwrap`s below are safe: `raw` is non-empty because `latest`
        // (from `g.back()?` above) proved the ring holds at least one entry.
        let span_ms = raw.last().unwrap().at_ms.saturating_sub(raw.first().unwrap().at_ms);
        let cost_sum: u64 = g.iter().map(|e| e.cost_ms).sum();
        let sampler_cost_ms_mean = (cost_sum as f64 / g.len() as f64 * 10.0).round() / 10.0;

        fn metric_json(m: &darkmux_crew::telemetry_sampler::MetricStats) -> serde_json::Value {
            serde_json::json!({
                "mean_pct": m.mean_pct,
                "p95_pct": m.p95_pct,
                "max_pct": m.peak_pct,
            })
        }

        Some(serde_json::json!({
            "now": {
                "cpu_pct": latest.cpu,
                "mem_pct": latest.mem,
                "gpu_pct": latest.gpu,
                "sampled_at_ms": latest.at_ms,
            },
            "window": {
                "cpu": metric_json(&stats.cpu),
                "mem": metric_json(&stats.mem),
                "gpu": metric_json(&stats.gpu),
                "samples": stats.samples,
                // MEASURED mean gap between samples, not the nominal
                // configured cadence — see this module's own doc.
                "interval_ms": stats.sample_interval_ms,
                "span_ms": span_ms,
            },
            "sampler_cost_ms_mean": sampler_cost_ms_mean,
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
        loop {
            if stop_flag.load(Ordering::SeqCst) {
                break;
            }

            let t0 = Instant::now();
            let sample = sample_host();
            let cost_ms = t0.elapsed().as_millis() as u64;
            let at_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            // Best-effort like the dispatch-scoped sampler: a tick where
            // every field failed still gets recorded (with `cost_ms`
            // stamped) rather than skipped, since the cost of the failed
            // gather is itself part of the observer-cost claim.
            ring.push(RingEntry { at_ms, cpu: sample.cpu, mem: sample.mem, gpu: sample.gpu, cost_ms });

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

    fn entry(at_ms: u64, cpu: u64, mem: u64, gpu: u64, cost_ms: u64) -> RingEntry {
        RingEntry { at_ms, cpu: Some(cpu), mem: Some(mem), gpu: Some(gpu), cost_ms }
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

        assert_eq!(v["window"]["samples"], 3);
        assert_eq!(v["window"]["span_ms"], 4000);
        assert_eq!(v["window"]["interval_ms"], 2000, "measured mean gap");
        assert_eq!(v["window"]["cpu"]["max_pct"], 90, "peak reused via reduce_host_stats");
        assert_eq!(v["window"]["cpu"]["mean_pct"], 60.0);

        // cost_ms mean of [5, 6, 4] = 5.0 — the observer's self-stamped cost.
        assert_eq!(v["sampler_cost_ms_mean"], 5.0);
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

        // Wait (bounded) for at least one sample.
        let deadline = Instant::now() + Duration::from_secs(5);
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
