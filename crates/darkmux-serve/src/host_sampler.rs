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
    battery, reduce_host_extras, BatteryHealth, BatterySample, HostExtraAt, HostProbe,
    HostSampleFull, MwStats,
};
use darkmux_crew::telemetry_sampler::{reduce_host_stats, HostSampleAt};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 10 minutes of history at the default 5s cadence. The ring is capacity-
/// bounded by ENTRY COUNT, not by wall-clock span — at a faster-than-default
/// cadence the window is shorter than 10 minutes; at a slower one, longer.
/// That's an intentional, visible tradeoff (the configured cadence is in
/// the payload) rather than a second hidden knob.
const RING_CAPACITY: usize = 120;

/// How often the sampler thread polls its stop flag while napping between
/// ticks — bounds shutdown latency to this, not a full sample interval.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// (#2413) While idle (no dispatch live anywhere on this machine), the
/// singleton sampler emits `machine.telemetry` flow records at this many
/// times the configured `interval_ms` — the issue's own "one tenth of that
/// rate idle" (the multiplier reads as ticks-between-emissions, so a 10x
/// SLOWER rate is expressed as "emit every 10th tick").
const IDLE_EMIT_MULTIPLIER: u64 = 10;

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
    /// (#2705) The most recent battery HEALTH reading — a MACHINE FACT,
    /// not a time series, so it is a single latest value rather than a
    /// ring entry. Refreshed on `battery::HEALTH_POLL_INTERVAL_MS`, which
    /// is three orders of magnitude slower than the ring's own cadence;
    /// keeping it in the ring would mean 1,800 identical copies of one
    /// unchanged reading per poll interval.
    battery_health: Arc<Mutex<Option<BatteryHealth>>>,
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

/// (#2775) One window's thermal summary on the wire — the ONE spelling, so
/// `/machine/resources`' `load.window.thermal` and the `machine.rollup`
/// record's `window.thermal` are the same object built by the same code.
///
/// `level_ms` and `level_entries` answer two different questions that a
/// single "above nominal" bucket collapses: how LONG the machine spent at
/// each level, and how OFTEN it arrived there. See
/// `darkmux_crew::host_probe::ThermalWindow`'s own field docs for why
/// entries count transitions rather than samples.
fn thermal_window_json(t: &darkmux_crew::host_probe::ThermalWindow) -> serde_json::Value {
    serde_json::json!({
        "worst_state": t.worst_state,
        "above_nominal_ms": t.above_nominal_ms,
        "min_cpu_speed_limit_pct": t.min_cpu_speed_limit_pct,
        "level_ms": t.level_ms,
        "level_entries": t.level_entries,
    })
}

impl HostSamplerRing {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAPACITY))),
            configured_interval_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            battery_health: Arc::new(Mutex::new(None)),
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
        // (#2705) Battery HEALTH sits beside `now`/`window` rather than
        // inside either: it is not a reading of this instant (`now`) and
        // not a reduction over the window — it is a slow-moving machine
        // FACT, refreshed hourly. `null` on a machine with no battery.
        let health = self.battery_health.lock().ok().and_then(|h| h.clone());
        Some(serde_json::json!({
            "battery_health": health.as_ref().map(darkmux_crew::host_probe::battery_health_json),
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
                // (#2775) `level_ms`/`level_entries` ride the SAME reduction
                // `above_nominal_ms` comes from, so the machine lens and the
                // `machine.rollup` record cannot disagree about how long the
                // machine spent hot or how often it got there. Additive on
                // the wire — an existing consumer ignores two new keys.
                "thermal": ex.thermal.as_ref().map(thermal_window_json),
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

/// (#2705, #2775) Has `interval_ms` of MEASURED time elapsed since this
/// thing last happened?
///
/// Two callers on two different clocks — the hourly battery-HEALTH poll
/// (#2705) and the `machine.rollup` heartbeat (#2775). Named for the RULE
/// rather than for either caller, because it is one rule: a second copy
/// would be one place for the zero-means-off convention below to be
/// mis-implemented.
///
/// Pure and integer-only ON PURPOSE: one of the cadences it gates is an
/// hour long, and an assertion about an hourly cadence that consulted the
/// wall clock would either take an hour to run or prove nothing. The caller
/// accumulates the MEASURED gap between sampler ticks and hands it here, so
/// the tests drive a scripted clock.
///
/// `interval_ms == 0` is "never" — the same zero-means-off convention
/// `runtime.host_sampler_interval_ms` and `redis.maxlen` use, never
/// "continuously", which is what a naive `>=` would give it.
fn interval_due(ms_since_last: u64, interval_ms: u64) -> bool {
    interval_ms != 0 && ms_since_last >= interval_ms
}

/// (#2705) Build a `machine.battery_health` MACHINE-RECORD flow record.
///
/// Always `Level::Info`: these are inventory numbers, and darkmux does not
/// adjudicate what a cycle count or a capacity ratio means. A `Warn` here
/// would BE the advice the issue rules out.
///
/// `poll_interval_ms` rides in the payload because the observability
/// contract says the cadence is a recorded knob, never adaptive-silent: an
/// artifact says what cadence produced it, and a tightened debug cadence is
/// visible in the data rather than inferred from row spacing.
fn build_battery_health_record(
    health: &BatteryHealth,
    poll_interval_ms: u64,
    sampled_at_ms: u64,
) -> darkmux_flow::FlowRecord {
    let mut payload = darkmux_crew::host_probe::battery_health_json(health);
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("poll_interval_ms".into(), serde_json::json!(poll_interval_ms));
        obj.insert("sampled_at_ms".into(), serde_json::json!(sampled_at_ms));
    }
    let display_name = darkmux_flow::resolve_machine_id().unwrap_or_else(|| "unknown".to_string());
    darkmux_flow::FlowRecord {
        ts: darkmux_flow::ts_utc_now(),
        level: darkmux_flow::Level::Info,
        category: darkmux_flow::Category::Machinery,
        tier: darkmux_flow::Tier::Local,
        stage: darkmux_flow::Stage::Dispatch,
        action: "machine.battery_health".to_string(),
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

/// (#2705) Pure edge detector for battery HEALTH — emit only when a value
/// actually CHANGED, the same edge-triggered shape [`thermal_edge`] uses.
///
/// - `now` absent (no battery, or the poll failed): no record, and the
///   known health is UNCHANGED. An absent reading is not evidence anything
///   moved.
/// - **No prior reading: EMIT.** This is the one deliberate divergence from
///   [`thermal_edge`], which seeds its first reading silently. A thermal
///   record is a TRANSITION and a first reading has nothing to have
///   transitioned from; a health record is an INVENTORY, and the first one
///   at daemon start IS the machine fact. Seeding it silently would mean a
///   battery whose numbers never change never reports its cycle count at
///   all — which is exactly the "the charge number looked healthy every day
///   and the cost was only visible in the slow fields" failure the issue
///   was filed from.
/// - Identical to the last reading: no record. Hour-over-hour capacity
///   differences are below the noise floor, and emitting anyway would add
///   24 identical rows a day to a stream this feature is supposed to keep
///   quiet.
/// - Any field differs: a real movement — emit, stamped with the time it
///   was observed, which is what makes the series answer "when did this
///   start degrading" rather than only "what is it now".
fn battery_health_edge(
    prev: Option<&BatteryHealth>,
    now: Option<&BatteryHealth>,
    poll_interval_ms: u64,
    sampled_at_ms: u64,
) -> (Option<BatteryHealth>, Option<darkmux_flow::FlowRecord>) {
    let Some(h) = now else {
        return (prev.cloned(), None);
    };
    if prev == Some(h) {
        return (Some(h.clone()), None);
    }
    (Some(h.clone()), Some(build_battery_health_record(h, poll_interval_ms, sampled_at_ms)))
}

/// (#2705) The battery TRANSITIONS worth a record, as stable strings a
/// consumer can key on. Pure, and separate from the record builder so every
/// crossing is table-tested without a battery.
///
/// The floor crossings are computed against the SAME
/// `power.min_battery_pct` #2706's gate enforces, so "the stream said it
/// crossed" and "the gate refused" can never disagree about where the line
/// was.
fn battery_transitions(prev: &BatterySample, now: &BatterySample, floor_pct: u8) -> Vec<&'static str> {
    let mut out = Vec::new();
    match (prev.on_ac, now.on_ac) {
        (true, false) => out.push("to-battery"),
        (false, true) => out.push("to-ac"),
        _ => {}
    }
    let was_below = prev.charge_pct < floor_pct;
    let is_below = now.charge_pct < floor_pct;
    if !was_below && is_below {
        out.push("below-floor");
    } else if was_below && !is_below {
        out.push("at-or-above-floor");
    }
    out
}

/// (#2705) Build a `machine.battery` TRANSITION flow record.
///
/// `Level::Warn` only for `below-floor` — the one transition that changes
/// what the machine will DO (runs refuse to start, and an in-flight run
/// pauses, per #2706). Going onto battery or back to AC is `Info`: it is
/// normal laptop life, and warning on it would be the editorializing the
/// issue rules out.
fn build_battery_transition_record(
    transitions: &[&'static str],
    from: &BatterySample,
    to: &BatterySample,
    floor_pct: u8,
    sampled_at_ms: u64,
) -> darkmux_flow::FlowRecord {
    let level = if transitions.contains(&"below-floor") {
        darkmux_flow::Level::Warn
    } else {
        darkmux_flow::Level::Info
    };
    let payload = serde_json::json!({
        "transitions": transitions,
        "from": darkmux_crew::host_probe::battery_sample_json(from),
        "to": darkmux_crew::host_probe::battery_sample_json(to),
        // The floor this crossing was judged against, recorded so a reader
        // is not left to guess which config was in force at the time.
        "floor_pct": floor_pct,
        "floor_field": darkmux_crew::power_policy::FLOOR_FIELD,
        "sampled_at_ms": sampled_at_ms,
    });
    let display_name = darkmux_flow::resolve_machine_id().unwrap_or_else(|| "unknown".to_string());
    darkmux_flow::FlowRecord {
        ts: darkmux_flow::ts_utc_now(),
        level,
        category: darkmux_flow::Category::Machinery,
        tier: darkmux_flow::Tier::Local,
        stage: darkmux_flow::Stage::Dispatch,
        action: "machine.battery".to_string(),
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

/// (#2705) Pure edge detector for battery CHARGE transitions — the same
/// contract [`thermal_edge`] documents, field for field:
///
/// - `sample.battery` absent: no transition, known state UNCHANGED.
/// - No prior state (daemon just started, or a battery just appeared): seed
///   SILENTLY. A first reading is not a transition — unlike health above,
///   which is an inventory rather than an edge.
/// - No qualifying transition: no emit, however much wall-clock passed.
/// - A qualifying transition: emit, whether or not a gap preceded it.
fn battery_edge(
    prev: Option<&BatterySample>,
    sample: &HostSampleFull,
    floor_pct: u8,
    sampled_at_ms: u64,
) -> (Option<BatterySample>, Option<darkmux_flow::FlowRecord>) {
    let Some(b) = sample.battery.as_ref() else {
        return (prev.copied(), None);
    };
    let Some(p) = prev else {
        return (Some(*b), None);
    };
    let transitions = battery_transitions(p, b, floor_pct);
    if transitions.is_empty() {
        return (Some(*b), None);
    }
    let rec = build_battery_transition_record(&transitions, p, b, floor_pct, sampled_at_ms);
    (Some(*b), Some(rec))
}

// ── (#2775) The periodic machine-lens aggregate ────────────────────────
//
// ONE heartbeat carrying the whole machine picture — thermal, cpu/gpu/
// memory, power, battery, residency — rather than a thermal feed a
// subscriber then has to correlate against a separate battery feed and a
// separate memory feed. See `darkmux_types::config::MachineRollupConfig`
// for why this is a periodic RECORD and not a "timed hook", and for the
// `enabled: false` / `period_seconds: 60` defaults.
//
// **Observer doctrine (CLAUDE.md "the observer must not join the
// observed", #1286), point by point:**
//
// 1. ZERO model dispatches. The ring is already sampled (kernel counters,
//    in-process); the residency block reads `lms` metadata and kernel
//    counters through the same ledger `/machine/resources` serves. Nothing
//    on this path asks a model anything.
// 2. The DISPLAY renders off-machine — this emits JSON into the flow
//    stream, and whoever charts it pays for the charting.
// 3. The gatherer stamps its OWN cost (`gather_ms`, plus the ledger's own
//    `residency.gather_ms` inside it), so "the observer was negligible"
//    stays a verifiable claim in the artifact rather than an assumption.
// 4. The cadence is a RECORDED knob: `period_seconds` (configured) and
//    `emitted_interval_ms` (measured) both ride in the payload, so a
//    tightened debug cadence is visible in the data instead of inferred
//    from row spacing.

/// The flow-record action for the periodic aggregate. `machine.*` hook
/// matching already covers it, and dotted `payload.*` predicates already
/// let a rule subscribe to one section — which is exactly why this feature
/// needed no change to the hook layer at all.
pub(crate) const MACHINE_ROLLUP_ACTION: &str = "machine.rollup";

/// (#2775) The residency section — what is loaded and how much unified
/// memory is left for AI, for a subscriber deciding whether to schedule
/// work here.
///
/// Built from the SAME `darkmux_profiles::model_ledger` gather that backs
/// `darkmux machine resources --json` and `GET /machine/resources`, so a
/// consumer reading this record and an operator reading the lens are
/// looking at one implementation's answer, not two.
///
/// Deliberately does NOT reuse the daemon's `/machine/resources` response
/// cache. That cache is guarded by a tokio mutex and this is a plain std
/// sampler thread with no runtime to await on; and a once-a-minute
/// heartbeat wants a reading of its own moment rather than one it inherited
/// from whenever a phone last polled. The cost is one gather per period and
/// it is stamped into the payload.
fn residency_json(ledger: &darkmux_profiles::model_ledger::ModelLedger) -> serde_json::Value {
    let models: Vec<serde_json::Value> = ledger
        .models
        .iter()
        .map(|m| {
            serde_json::json!({
                "identifier": m.identifier,
                "model_key": m.model_key,
                "owner": m.owner,
                "loaded_ctx": m.loaded_ctx,
                "potential_bytes": m.potential_bytes,
                "current_bytes": m.current_bytes,
                "state": m.state,
            })
        })
        .collect();
    serde_json::json!({
        "models": models,
        "limit_bytes": ledger.limit_bytes,
        "pool": ledger.pool.as_ref().map(|p| serde_json::json!({
            "capacity_bytes": p.capacity_bytes,
            "used_bytes": p.used_bytes,
            // The colloquial "how much is left for AI" — the ledger's own
            // `available_bytes`, named here the way the lens names it
            // rather than re-deriving a second figure that could drift.
            "available_bytes": p.available_bytes,
        })),
        "attribution": ledger.attribution,
        // The honesty channel (#1821): a degraded reading says so in the
        // record rather than looking precise. Usually empty.
        "messages": ledger.messages,
        // The ledger's OWN observer cost, kept distinct from the rollup's
        // total below so the expensive half is attributable.
        "gather_ms": ledger.gather_ms,
    })
}

/// (#2775) Build the periodic `machine.rollup` record.
///
/// Pure given its inputs (no probe, no clock, no config read) so the whole
/// payload contract is unit-testable against a scripted snapshot — the same
/// discipline `thermal_edge` / `battery_edge` follow.
///
/// `previous_thermal_state` is the last state the machine was in BEFORE the
/// current one — the operator's explicit ask, so a subscriber that missed an
/// edge-triggered `machine.thermal` can still reconstruct direction from a
/// heartbeat alone. `None` before any transition has been observed (a daemon
/// that started in this state and never left it), which is a different claim
/// from "it came from nominal" and is reported as such rather than guessed.
///
/// Always `Level::Info`: this is a periodic READING, and darkmux describes
/// rather than adjudicates. The edge-triggered `machine.thermal` /
/// `machine.battery` records keep their `Warn` for the transitions that
/// change what the machine will DO; a heartbeat that warned on its own
/// would light the same lamp every minute for a condition already reported.
#[allow(clippy::too_many_arguments)]
fn build_machine_rollup_record(
    load: serde_json::Value,
    residency: Option<serde_json::Value>,
    previous_thermal_state: Option<&str>,
    period_seconds: u64,
    emitted_interval_ms: u64,
    gather_ms: u64,
    sampled_at_ms: u64,
) -> darkmux_flow::FlowRecord {
    let mut payload = serde_json::json!({
        // Constraint 4: the CONFIGURED cadence...
        "period_seconds": period_seconds,
        // ...and the MEASURED gap since the previous emission. The same
        // rule `machine.telemetry` follows: a tick that ran late reports
        // what actually happened rather than restating the knob.
        "emitted_interval_ms": emitted_interval_ms,
        // Constraint 3: this rollup's own total cost, ledger gather
        // included (`residency.gather_ms` breaks out the expensive half).
        "gather_ms": gather_ms,
        "sampled_at_ms": sampled_at_ms,
        "previous_thermal_state": previous_thermal_state,
        "residency": residency,
    });
    // `load` is the machine lens's own `now`/`window`/`battery_health`
    // object, spliced in at the top level rather than nested under a
    // `load` key: the aggregate IS the machine picture, and a consumer
    // writing a hook predicate should say `payload.window.thermal.…`, not
    // `payload.load.window.thermal.…`.
    if let (Some(obj), Some(load_obj)) = (payload.as_object_mut(), load.as_object()) {
        for (k, v) in load_obj {
            obj.insert(k.clone(), v.clone());
        }
    }
    let display_name = darkmux_flow::resolve_machine_id().unwrap_or_else(|| "unknown".to_string());
    darkmux_flow::FlowRecord {
        ts: darkmux_flow::ts_utc_now(),
        level: darkmux_flow::Level::Info,
        category: darkmux_flow::Category::Machinery,
        tier: darkmux_flow::Tier::Local,
        stage: darkmux_flow::Stage::Dispatch,
        action: MACHINE_ROLLUP_ACTION.to_string(),
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
/// (#2413 round 4 MF1) The `interval_ms` value to stamp on THIS
/// `machine.telemetry` emission — one rule, shared with `dispatch_
/// internal.rs`'s `maybe_build_machine_telemetry_record`: the MEASURED
/// gap since the previous emission (`at_ms - last_emit_at_ms`), or, on
/// the very first emission (no prior gap to measure), the configured
/// cadence for whatever live/idle multiplier is in effect at that tick
/// (`interval_ms * emit_every_n_ticks`). Pure and extracted specifically
/// so this can be unit-tested with fake timestamps — a real-thread test
/// driving the actual sampler can't reliably distinguish "measured" from
/// "configured" in the common case, since a healthy real clock makes the
/// two numbers land in the same neighborhood anyway.
fn machine_telemetry_effective_interval_ms(
    last_emit_at_ms: Option<u64>,
    at_ms: u64,
    interval_ms: u64,
    emit_every_n_ticks: u64,
) -> u64 {
    match last_emit_at_ms {
        Some(last) => at_ms.saturating_sub(last),
        None => interval_ms.saturating_mul(emit_every_n_ticks),
    }
}

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
        // (#2705) The battery CHARGE state carried between ticks for
        // `battery_edge`'s edge detection, and the battery HEALTH reading
        // carried between POLLS for `battery_health_edge`'s. Two different
        // clocks on purpose — see the battery module's own doc for why
        // health on the telemetry cadence would be a kernel read per tick
        // for a value unchanged since the last thousand.
        let mut known_battery: Option<BatterySample> = None;
        let mut known_battery_health: Option<BatteryHealth> = None;
        // (#2775) The state the machine was in BEFORE `known_thermal_state`
        // — set only when a real transition fires, so it is the last
        // DIFFERENT state rather than "whatever the previous tick read".
        // `None` until the daemon has seen one transition, which the rollup
        // reports as `null` rather than inventing a plausible predecessor.
        let mut previous_thermal_state: Option<String> = None;
        // (#2775) Accumulated MEASURED time since the last `machine.rollup`
        // emission, seeded past any interval so the FIRST tick emits — a
        // subscriber that just enabled the feature should not wait a full
        // period to learn the machine exists. Same seeding rationale as the
        // health poll above.
        let mut ms_since_rollup: u64 = u64::MAX;
        // The rollup's OWN previous-tick stamp. Deliberately separate from
        // `prev_tick_at_ms`, which the health block above has already
        // advanced to this tick by the time the rollup block runs — reusing
        // it would accumulate a zero gap every time and the heartbeat would
        // never come due after its first emission.
        let mut prev_rollup_tick_at_ms: Option<u64> = None;
        // Measured-gap bookkeeping for the rollup's own `emitted_interval_ms`
        // — `None` before the first emission, which has no prior gap and
        // stamps the configured period instead.
        let mut last_rollup_at_ms: Option<u64> = None;
        // Accumulated MEASURED time since the last health poll, seeded past
        // the interval so the FIRST tick polls — the issue's "plus one read
        // at daemon start, so a short-lived daemon still contributes a
        // reading".
        let mut ms_since_health_poll: u64 = u64::MAX;
        // Previous tick's epoch stamp, for that measured gap. `None` on the
        // first tick, which has no gap to measure and does not need one
        // (the seed above already makes that tick due).
        let mut prev_tick_at_ms: Option<u64> = None;
        // (#2413) This thread is a candidate to be the machine's singleton
        // `machine.telemetry` emitter — see the module doc's "one emitter
        // per machine" section. `None` means it currently does NOT hold
        // the lock (a dispatch process, or nothing yet, holds it); the
        // ring above still samples unconditionally either way, since
        // `/machine/resources` must keep working regardless of who else
        // is emitting flow records.
        let mut sampler_lock: Option<darkmux_crew::host_sampler_lock::SamplerLockGuard> = None;
        // (#2413) Ticks since the last EMITTED `machine.telemetry` record
        // — independent of the ring's OWN cadence, which stays at
        // `interval_ms` unconditionally per the issue's "the ring keeps
        // sampling at the configured rate regardless" rule.
        let mut ticks_since_emit: u64 = 0;
        // (#2413 C3, revised — found live 2026-09-06) The live-vs-idle
        // decision, re-probed every tick (see inside the loop below) rather
        // than only right before an emission — the prior once-per-emission
        // scheme let `cached_live` go stale for up to `IDLE_EMIT_MULTIPLIER`
        // ticks while idle. No eager pre-loop probe needed any more: the
        // loop's first iteration probes (and assigns this) before it reads
        // it for the first time.
        let mut cached_live: bool;
        // (#2413 round 4 MF1) One rule for `interval_ms` on both host-sample
        // producers (this daemon sampler and the dispatch-owned one in
        // `dispatch_internal.rs`): the MEASURED gap since the previous
        // emission, not the configured cadence stamped verbatim. `None`
        // before the first emission — that one has no prior gap to
        // measure, so it stamps the configured cadence for the tick it
        // lands on instead (`interval_ms * emit_every_n_ticks`, matching
        // whatever live/idle multiplier was in effect at that moment).
        let mut last_emit_at_ms: Option<u64> = None;
        loop {
            if stop_flag.load(Ordering::SeqCst) {
                break;
            }

            let sample = probe.sample();
            // (#2111 review finding) The shared epoch-ms read — the same
            // one the machine-scoped telemetry record below (and a
            // dispatch process's own sampler) uses for `sampled_at_ms`, so
            // a viewer strip charting both possible producers is
            // comparing the same clock, not one producer's epoch against
            // another's sampler-relative offset.
            let at_ms = darkmux_crew::host_probe::epoch_ms_now();
            // (#2111) Edge-detect BEFORE the sample moves into the ring —
            // `thermal_edge` only borrows it.
            let (next_state, transition) = thermal_edge(known_thermal_state.as_deref(), &sample, at_ms);
            // (#2775) A transition is the ONLY thing that moves
            // `previous_thermal_state` — captured before `known_thermal_state`
            // is overwritten, so the rollup's "state now / previous state"
            // pair describes a real movement rather than the last tick.
            if transition.is_some() {
                previous_thermal_state = known_thermal_state.clone();
            }
            known_thermal_state = next_state;
            if let Some(rec) = transition {
                let _ = darkmux_flow::record(rec);
            }

            // (#2705) Battery CHARGE transitions — onto battery, back to
            // AC, and crossing the #2706 floor. Same pure-edge shape as
            // thermal above, and the same "an absent sample means no
            // transition" rule: a desktop's permanent `None` emits nothing,
            // ever.
            let floor_pct = darkmux_types::config_access::power_min_battery_pct();
            let (next_battery, battery_transition) =
                battery_edge(known_battery.as_ref(), &sample, floor_pct, at_ms);
            known_battery = next_battery;
            if let Some(rec) = battery_transition {
                let _ = darkmux_flow::record(rec);
            }

            // (#2705) Battery HEALTH — polled on its OWN long cadence off
            // the MEASURED gap between ticks (not a tick count, which would
            // drift with the configured interval and lie across a host
            // sleep), and recorded only when a value actually changed.
            ms_since_health_poll = match prev_tick_at_ms {
                Some(prev) => ms_since_health_poll.saturating_add(at_ms.saturating_sub(prev)),
                None => ms_since_health_poll,
            };
            prev_tick_at_ms = Some(at_ms);
            if interval_due(ms_since_health_poll, battery::HEALTH_POLL_INTERVAL_MS) {
                ms_since_health_poll = 0;
                let (next_health, health_record) = battery_health_edge(
                    known_battery_health.as_ref(),
                    battery::health().as_ref(),
                    battery::HEALTH_POLL_INTERVAL_MS,
                    at_ms,
                );
                known_battery_health = next_health;
                // The machine-facts surface (`/machine/resources`'s
                // `battery_health`) is refreshed on every POLL, not only on
                // an emitted change — a reader asking "what is it now"
                // should get an answer even when nothing moved.
                if let Ok(mut slot) = ring.battery_health.lock() {
                    slot.clone_from(&known_battery_health);
                }
                if let Some(rec) = health_record {
                    let _ = darkmux_flow::record(rec);
                }
            }

            // (#2775) The periodic machine-lens aggregate. Gated OFF by
            // default; when on, it fires on its own MEASURED clock, the
            // same rule the health poll above uses and for the same reason
            // (a tick count would drift with the configured interval and
            // lie across a host sleep).
            //
            // Both knobs are re-read every tick rather than captured at
            // spawn, so `darkmux config set machine_rollup.enabled true`
            // takes effect on the next tick instead of on the next daemon
            // restart. That is two cheap config reads per `interval_ms` —
            // `config()` is a process-cached `DarkmuxConfig` plus an env
            // peek, not a file read — and it is what keeps the operator in
            // the loop without a restart.
            //
            // The ring was pushed regardless (below); only the flow-record
            // WRITE and the ledger gather are gated, per the observer-cost
            // rule — this must not make the machine busier to watch than to
            // use.
            ms_since_rollup = match prev_rollup_tick_at_ms {
                Some(prev) => ms_since_rollup.saturating_add(at_ms.saturating_sub(prev)),
                None => ms_since_rollup,
            };
            prev_rollup_tick_at_ms = Some(at_ms);
            if darkmux_types::config_access::machine_rollup_enabled() {
                let period_seconds = darkmux_types::config_access::machine_rollup_period_seconds();
                let period_ms = period_seconds.saturating_mul(1_000);
                if interval_due(ms_since_rollup, period_ms) {
                    ms_since_rollup = 0;
                    let gather_start = Instant::now();
                    // The ring already holds this tick's sample; `snapshot`
                    // is a mutex lock plus arithmetic. `None` only before
                    // the very first push, which cannot happen here (the
                    // push below has run on every prior iteration) but is
                    // handled rather than unwrapped.
                    if let Some(load) = ring.snapshot() {
                        let ledger = darkmux_profiles::model_ledger::gather();
                        let residency = residency_json(&ledger);
                        let emitted_interval_ms =
                            machine_telemetry_effective_interval_ms(last_rollup_at_ms, at_ms, period_ms, 1);
                        last_rollup_at_ms = Some(at_ms);
                        let rec = build_machine_rollup_record(
                            load,
                            Some(residency),
                            previous_thermal_state.as_deref(),
                            period_seconds,
                            emitted_interval_ms,
                            gather_start.elapsed().as_millis() as u64,
                            at_ms,
                        );
                        let _ = darkmux_flow::record(rec);
                    }
                }
            }

            // (found live 2026-09-06) Re-probe liveness on a FIXED PER-TICK
            // FLOOR — every `interval_ms`, independent of whether this tick
            // actually emits — rather than only right before an emission.
            // The prior "re-probe only right before an emission" scheme
            // meant an idle machine's `cached_live` could go stale for up
            // to `IDLE_EMIT_MULTIPLIER` ticks (~52s at the default 5s
            // cadence): a dispatch that started and finished inside that
            // window got 0-1 samples instead of the 1x cadence it should
            // have gotten from the moment it appeared. The probe itself is
            // ~15ms (already stamped below), so paying it every tick — not
            // just every emission — is cheap relative to the visibility it
            // buys; the switch to live cadence now lands within one
            // interval instead of up to ten.
            let probe_start = Instant::now();
            cached_live = crate::runs::any_dispatch_live_locally(at_ms, crate::runs::stale_after_ms());
            let liveness_probe_ms = probe_start.elapsed().as_millis() as u64;

            // (#2413, C2, and round 3 MF1) Opportunistically (re)acquire
            // the singleton lock every tick this thread doesn't already
            // hold it — a dispatch process that held it may have exited
            // (releasing it via its own `Drop`) since our last attempt,
            // freeing the machine up for the daemon to take over as the
            // steady-state emitter. A fresh, alive lock declining this
            // call is the CORRECT, expected steady state whenever a
            // dispatch legitimately holds the sampler role, not a race —
            // `try_acquire` never records that as contention (the channel
            // is retired, see `host_sampler_lock`'s module doc).
            if sampler_lock.is_none() {
                sampler_lock = darkmux_crew::host_sampler_lock::try_acquire("daemon", interval_ms);
            }
            if let Some(guard) = sampler_lock.as_ref() {
                if guard.heartbeat(interval_ms) {
                    ticks_since_emit += 1;
                    // (#2413) Live-vs-idle cadence: the base
                    // `interval_ms` while at least one dispatch is
                    // running anywhere on this machine, else
                    // `IDLE_EMIT_MULTIPLIER`x that. The ring sampled
                    // above regardless — only the FLOW-RECORD WRITE is
                    // gated, per the observer-cost rule (the sample is
                    // already taken; only the write is the added cost).
                    // `cached_live` is now this TICK's fresh probe result
                    // (probed unconditionally above), so a live/idle
                    // transition gates THIS tick's decision directly.
                    let emit_every_n_ticks = if cached_live { 1 } else { IDLE_EMIT_MULTIPLIER };
                    if ticks_since_emit >= emit_every_n_ticks {
                        ticks_since_emit = 0;
                        // (#2413 round 4 MF1) Measured gap since the LAST
                        // emission, not the knob times the multiplier — a
                        // tick that ran late (a slow probe, a paused
                        // thread) reports what actually happened, same
                        // rule `dispatch_internal.rs`'s dispatch-owned
                        // sampler already follows.
                        let effective_interval_ms =
                            machine_telemetry_effective_interval_ms(last_emit_at_ms, at_ms, interval_ms, emit_every_n_ticks);
                        last_emit_at_ms = Some(at_ms);
                        let mut rec = darkmux_crew::host_probe::build_machine_scoped_telemetry_record(
                            &sample,
                            at_ms,
                            effective_interval_ms,
                        );
                        // (CLAUDE.md "samplers stamp their own cost") The
                        // liveness probe this emission's cadence decision
                        // depended on is part of this record's own write
                        // cost — stamped so "the observer was negligible"
                        // stays a verifiable claim in the data.
                        if let Some(obj) = rec.payload.as_mut().and_then(|p| p.as_object_mut()) {
                            obj.insert("liveness_probe_ms".into(), serde_json::json!(liveness_probe_ms));
                        }
                        let _ = darkmux_flow::record(rec);
                    }
                } else {
                    // Lost the lock to a steal race — stop emitting until
                    // the next tick's opportunistic re-acquire (which will
                    // fail again, harmlessly, for as long as someone else
                    // holds a fresh lock).
                    sampler_lock = None;
                }
            }

            // Best-effort like a dispatch-scoped sampler: a tick where
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
    use tempfile::TempDir;

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

    // ── #2705: battery charge transitions + the hourly health cadence ──

    fn battery_at(pct: u8, on_ac: bool) -> BatterySample {
        BatterySample { charge_pct: pct, on_ac, charging: on_ac, minutes_to_empty: None }
    }

    fn sample_with_battery(b: Option<BatterySample>) -> HostSampleFull {
        HostSampleFull { battery: b, ..Default::default() }
    }

    #[test]
    fn a_machine_with_no_battery_never_emits_a_transition_however_long_it_runs() {
        // A desktop's permanent `None`. This is the daemon-side half of the
        // inertness requirement: the always-on hub must not fill the fleet
        // stream with battery records it has no battery for.
        let mut known: Option<BatterySample> = None;
        for tick in 0..50u64 {
            let (next, rec) = battery_edge(known.as_ref(), &sample_with_battery(None), 50, tick * 5_000);
            known = next;
            assert!(rec.is_none(), "tick {tick}: no battery ⇒ no record, ever");
        }
        assert!(known.is_none(), "an absent reading must not seed a state");
    }

    #[test]
    fn the_first_battery_reading_seeds_silently_because_it_is_not_a_transition() {
        let (known, rec) = battery_edge(None, &sample_with_battery(Some(battery_at(80, true))), 50, 1_000);
        assert!(rec.is_none(), "there is nothing to have transitioned FROM");
        assert_eq!(known.map(|b| b.charge_pct), Some(80), "but the baseline is now known");
    }

    #[test]
    fn going_onto_battery_and_back_to_ac_each_emit_once() {
        let on_ac = battery_at(80, true);
        let unplugged = battery_at(80, false);

        let (known, rec) = battery_edge(Some(&on_ac), &sample_with_battery(Some(unplugged)), 50, 1_000);
        let rec = rec.expect("unplugging is a transition");
        assert_eq!(rec.action, "machine.battery");
        assert!(
            matches!(rec.level, darkmux_flow::Level::Info),
            "normal laptop life is not a warning"
        );
        let p = rec.payload.expect("payload");
        assert_eq!(p["transitions"], serde_json::json!(["to-battery"]));
        assert_eq!(p["from"]["on_ac"], true);
        assert_eq!(p["to"]["on_ac"], false);

        // Staying on battery emits nothing, however many ticks pass.
        let (known, rec) = battery_edge(known.as_ref(), &sample_with_battery(Some(unplugged)), 50, 3_000);
        assert!(rec.is_none(), "an unchanged state is not a transition");

        let (_, rec) = battery_edge(known.as_ref(), &sample_with_battery(Some(on_ac)), 50, 5_000);
        let p = rec.expect("replugging is a transition").payload.expect("payload");
        assert_eq!(p["transitions"], serde_json::json!(["to-ac"]));
    }

    #[test]
    fn crossing_the_floor_downward_warns_and_names_the_floor_it_was_judged_against() {
        let above = battery_at(51, false);
        let below = battery_at(49, false);
        let (_, rec) = battery_edge(Some(&above), &sample_with_battery(Some(below)), 50, 1_000);
        let rec = rec.expect("crossing the floor is a transition");
        assert!(
            matches!(rec.level, darkmux_flow::Level::Warn),
            "below-floor is the one transition that changes what the machine will DO"
        );
        let p = rec.payload.expect("payload");
        assert_eq!(p["transitions"], serde_json::json!(["below-floor"]));
        assert_eq!(p["floor_pct"], 50);
        assert_eq!(p["floor_field"], darkmux_crew::power_policy::FLOOR_FIELD);
    }

    #[test]
    fn exactly_at_the_floor_is_not_a_crossing_matching_the_gate_that_enforces_it() {
        // The stream and the gate must agree about where the line is:
        // `power_policy::start_decision` starts AT the floor, so landing on
        // it is not "below-floor" here either.
        let above = battery_at(51, false);
        let at = battery_at(50, false);
        let (_, rec) = battery_edge(Some(&above), &sample_with_battery(Some(at)), 50, 1_000);
        assert!(rec.is_none(), "50 with a floor of 50 has not crossed anything");
    }

    #[test]
    fn recovering_past_the_floor_emits_and_is_only_info() {
        let below = battery_at(20, false);
        let recovered = battery_at(50, true);
        let (_, rec) = battery_edge(Some(&below), &sample_with_battery(Some(recovered)), 50, 1_000);
        let rec = rec.expect("recovery is a transition");
        assert!(matches!(rec.level, darkmux_flow::Level::Info));
        let p = rec.payload.expect("payload");
        // Plugging in and crossing back up happen together and are both named.
        assert_eq!(p["transitions"], serde_json::json!(["to-ac", "at-or-above-floor"]));
    }

    #[test]
    fn an_absent_reading_mid_series_holds_the_known_state_rather_than_resetting_it() {
        let below = battery_at(20, false);
        let (known, _) = battery_edge(Some(&battery_at(60, false)), &sample_with_battery(Some(below)), 50, 1_000);
        let (held, rec) = battery_edge(known.as_ref(), &sample_with_battery(None), 50, 3_000);
        assert!(rec.is_none());
        assert_eq!(held.map(|b| b.charge_pct), Some(20), "a failed probe is not evidence anything moved");
    }

    // ── The hourly health cadence, driven by an INJECTED clock ──

    #[test]
    fn the_health_poll_is_due_only_once_the_injected_gap_reaches_the_interval() {
        // Integer-only on purpose: an assertion about an HOURLY cadence
        // that consulted the wall clock would either take an hour to run or
        // prove nothing.
        let hour = battery::HEALTH_POLL_INTERVAL_MS;
        assert!(!interval_due(0, hour));
        assert!(!interval_due(hour - 1, hour), "one millisecond short is not due");
        assert!(interval_due(hour, hour), "exactly at the interval is due");
        assert!(interval_due(hour * 3, hour), "a long gap (a host sleep) is still just due");
    }

    #[test]
    fn a_zero_health_interval_means_never_poll_not_poll_continuously() {
        // The zero-means-off convention `host_sampler_interval_ms` and
        // `redis.maxlen` already use — a naive `>=` would read it as
        // "always due", which is the opposite.
        assert!(!interval_due(0, 0));
        assert!(!interval_due(u64::MAX, 0));
    }

    #[test]
    fn the_seeded_accumulator_makes_the_first_tick_due_at_daemon_start() {
        // The issue's "plus one read at daemon start, so a short-lived
        // daemon still contributes a reading" — expressed in the loop as a
        // `u64::MAX` seed, pinned here so a later refactor to `0` (the
        // obvious-looking initial value) is caught.
        assert!(
            interval_due(u64::MAX, battery::HEALTH_POLL_INTERVAL_MS),
            "the daemon's first tick must poll rather than wait an hour"
        );
    }

    fn health_with(cycles: u64) -> BatteryHealth {
        BatteryHealth {
            cycle_count: Some(cycles),
            design_capacity_mah: Some(6249),
            raw_max_capacity_mah: Some(5648),
            ..Default::default()
        }
    }

    #[test]
    fn the_first_health_reading_is_emitted_because_it_is_an_inventory_not_an_edge() {
        // The deliberate divergence from `thermal_edge`: a battery whose
        // numbers never move would otherwise never report its cycle count
        // at all.
        let (known, rec) = battery_health_edge(None, Some(&health_with(26)), 3_600_000, 1_000);
        let rec = rec.expect("the first reading IS the machine fact");
        assert_eq!(rec.action, "machine.battery_health");
        assert!(
            matches!(rec.level, darkmux_flow::Level::Info),
            "inventory numbers are not a verdict"
        );
        assert_eq!(known.map(|h| h.cycle_count), Some(Some(26)));
    }

    #[test]
    fn an_unchanged_health_reading_emits_nothing_however_many_hours_pass() {
        let h = health_with(26);
        let (mut known, _) = battery_health_edge(None, Some(&h), 3_600_000, 0);
        for hour in 1..=24u64 {
            let (next, rec) = battery_health_edge(known.as_ref(), Some(&h), 3_600_000, hour * 3_600_000);
            known = next;
            assert!(
                rec.is_none(),
                "hour {hour}: emitting anyway would add 24 identical rows a day to a stream this \
                 feature is supposed to keep quiet"
            );
        }
    }

    #[test]
    fn a_changed_health_value_emits_with_the_cadence_that_produced_it() {
        let (known, _) = battery_health_edge(None, Some(&health_with(26)), 3_600_000, 0);
        let (_, rec) = battery_health_edge(known.as_ref(), Some(&health_with(27)), 3_600_000, 3_600_000);
        let p = rec.expect("a real movement").payload.expect("payload");
        assert_eq!(p["cycle_count"], 27);
        assert_eq!(
            p["poll_interval_ms"], 3_600_000,
            "the cadence is a RECORDED knob — an artifact must say what produced it"
        );
        assert_eq!(p["sampled_at_ms"], 3_600_000, "stamped with when it was observed, not when it was read back");
        // Both capacity readings ride along, each labeled by its source.
        assert_eq!(p["raw_capacity_pct"], 90.4);
        assert!(p.get("nominal_capacity_pct").is_some(), "the other reading is recorded too: {p}");
    }

    #[test]
    fn a_failed_health_poll_holds_the_last_known_reading_and_emits_nothing() {
        let (known, _) = battery_health_edge(None, Some(&health_with(26)), 3_600_000, 0);
        let (held, rec) = battery_health_edge(known.as_ref(), None, 3_600_000, 3_600_000);
        assert!(rec.is_none(), "an absent reading is not a change");
        assert_eq!(held.map(|h| h.cycle_count), Some(Some(26)));
    }

    #[test]
    fn a_desktop_never_emits_a_health_record_at_all() {
        let mut known: Option<BatteryHealth> = None;
        for hour in 0..48u64 {
            let (next, rec) = battery_health_edge(known.as_ref(), None, 3_600_000, hour * 3_600_000);
            known = next;
            assert!(rec.is_none(), "hour {hour}");
        }
        assert!(known.is_none());
    }

    #[test]
    fn the_snapshot_carries_battery_health_as_a_machine_fact_beside_the_window() {
        let ring = HostSamplerRing::new();
        ring.push(entry(0, 50, 60, 70, 5));
        let v = ring.snapshot().expect("samples present");
        assert!(v["battery_health"].is_null(), "no health polled yet ⇒ null, never a fabricated block");
        assert!(
            v["window"].get("battery_health").is_none(),
            "health is not a time series and must not sit inside the window reduction"
        );

        *ring.battery_health.lock().expect("lock") = Some(health_with(26));
        let v = ring.snapshot().expect("samples present");
        assert_eq!(v["battery_health"]["cycle_count"], 26);
        assert_eq!(v["battery_health"]["raw_capacity_pct"], 90.4);
    }

    #[test]
    fn the_now_block_carries_charge_and_reads_null_on_a_machine_with_no_battery() {
        let ring = HostSamplerRing::new();
        ring.push(RingEntry { at_ms: 0, sample: sample_with_battery(None) });
        let v = ring.snapshot().expect("samples present");
        assert!(v["now"]["battery"].is_null(), "not measured must never serialize as a zero");

        let ring = HostSamplerRing::new();
        ring.push(RingEntry {
            at_ms: 0,
            sample: sample_with_battery(Some(BatterySample {
                charge_pct: 42,
                on_ac: false,
                charging: false,
                minutes_to_empty: None,
            })),
        });
        let v = ring.snapshot().expect("samples present");
        assert_eq!(v["now"]["battery"]["charge_pct"], 42);
        assert_eq!(v["now"]["battery"]["on_ac"], false);
        assert!(
            v["now"]["battery"]["minutes_to_empty"].is_null(),
            "no estimate must read null, never 0 — they are opposite claims"
        );
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
                // (#2705) A laptop on battery, so the `now` block's charge
                // half is exercised by this full-shape snapshot test too.
                battery: Some(BatterySample {
                    charge_pct: 73,
                    on_ac: false,
                    charging: false,
                    minutes_to_empty: Some(184),
                }),
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

    /// (#2762) `spawn` takes the singleton sampler lock, and `lock_path()`
    /// resolves through process-global `DARKMUX_HOME`. Left un-isolated,
    /// this test contended for the FIXED machine-global fallback path
    /// (`dispatch_liveness::darkmux_home_dir_fallback` →
    /// `<test-isolated root>/liveness/host-sampler.lock`) — shared
    /// with every other test binary in the workspace running with
    /// `DARKMUX_HOME` unset — and, being non-serial, could also run
    /// alongside the serial tests below, whose isolated `DARKMUX_HOME` it
    /// would then resolve into. Isolate and serialize it like its
    /// neighbors: this test is about the ring and the teardown, and has no
    /// business touching a lock any other test can see.
    #[serial_test::serial]
    #[test]
    fn spawned_sampler_populates_the_ring_and_stops_promptly() {
        with_isolated_env(|_flows_dir| {
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
        });
    }

    // ─── (#2413) spawn() — the singleton machine.telemetry emitter ─────

    /// Isolate `DARKMUX_HOME` (the lock path) and `DARKMUX_FLOWS_DIR` (where
    /// records land) to fresh tempdirs for the duration of `f`, restoring
    /// both afterward. Every test below mutates process-global env, hence
    /// `#[serial_test::serial]` on each.
    fn with_isolated_env(f: impl FnOnce(&std::path::Path)) {
        let home = TempDir::new().unwrap();
        let flows = TempDir::new().unwrap();
        let prev_home = std::env::var("DARKMUX_HOME").ok();
        let prev_flows = std::env::var("DARKMUX_FLOWS_DIR").ok();
        unsafe {
            std::env::set_var("DARKMUX_HOME", home.path());
            std::env::set_var("DARKMUX_FLOWS_DIR", flows.path());
        }
        f(flows.path());
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("DARKMUX_HOME", v),
                None => std::env::remove_var("DARKMUX_HOME"),
            }
            match prev_flows {
                Some(v) => std::env::set_var("DARKMUX_FLOWS_DIR", v),
                None => std::env::remove_var("DARKMUX_FLOWS_DIR"),
            }
        }
    }

    /// Poll `flows_dir/<today>.jsonl` until a `machine.telemetry` record
    /// appears (bounded), returning it as raw JSON. Panics past the
    /// deadline — every caller expects one to land.
    // ─── (#2413 round 4 MF1) machine_telemetry_effective_interval_ms — the real pin ─

    #[test]
    fn machine_telemetry_effective_interval_ms_first_emission_stamps_the_configured_cadence() {
        assert_eq!(machine_telemetry_effective_interval_ms(None, 12345, 5000, 1), 5000);
        assert_eq!(machine_telemetry_effective_interval_ms(None, 12345, 5000, 10), 50_000, "idle multiplier applies");
    }

    #[test]
    fn machine_telemetry_effective_interval_ms_later_emissions_stamp_the_measured_gap() {
        // A tick that ran LATE (an 8s gap against a 500ms knob) must
        // report the real 8s, not the configured 500ms it would report if
        // this had regressed back to stamping the knob verbatim.
        assert_eq!(machine_telemetry_effective_interval_ms(Some(1_000), 9_000, 500, 1), 8_000);
        assert_eq!(machine_telemetry_effective_interval_ms(Some(1_000), 9_000, 500, 10), 8_000, "knob ignored once a prior emission exists");
    }

    fn wait_for_machine_telemetry_record(flows_dir: &std::path::Path, deadline: Duration) -> serde_json::Value {
        let day_path = flows_dir.join(format!("{}.jsonl", darkmux_flow::day_utc_now()));
        let start = Instant::now();
        loop {
            if let Ok(text) = std::fs::read_to_string(&day_path) {
                for line in text.lines() {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                        if v.get("action").and_then(|a| a.as_str()) == Some("machine.telemetry") {
                            return v;
                        }
                    }
                }
            }
            if start.elapsed() > deadline {
                panic!("no machine.telemetry record landed in {} within {deadline:?}", day_path.display());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// (#2413 round 4 MF1) Same idea as [`wait_for_machine_telemetry_record`]
    /// but collects at least `n` of them — needed to assert the FIRST
    /// emission stamps the configured cadence while the SECOND+ stamps the
    /// measured gap since the previous one.
    fn wait_for_n_machine_telemetry_records(
        flows_dir: &std::path::Path,
        n: usize,
        deadline: Duration,
    ) -> Vec<serde_json::Value> {
        let day_path = flows_dir.join(format!("{}.jsonl", darkmux_flow::day_utc_now()));
        let start = Instant::now();
        loop {
            let mut found = Vec::new();
            if let Ok(text) = std::fs::read_to_string(&day_path) {
                for line in text.lines() {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                        if v.get("action").and_then(|a| a.as_str()) == Some("machine.telemetry") {
                            found.push(v);
                        }
                    }
                }
            }
            if found.len() >= n {
                return found;
            }
            if start.elapsed() > deadline {
                panic!(
                    "only {} of {n} machine.telemetry records landed in {} within {deadline:?}",
                    found.len(),
                    day_path.display()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[serial_test::serial]
    #[test]
    fn spawn_emits_a_machine_scoped_record_with_no_session_fields() {
        with_isolated_env(|flows_dir| {
            let ring = HostSamplerRing::new();
            let stop = Arc::new(AtomicBool::new(false));
            let handle = spawn(50, ring.clone(), Arc::clone(&stop)).unwrap();

            let rec = wait_for_machine_telemetry_record(flows_dir, Duration::from_secs(10));

            stop.store(true, Ordering::SeqCst);
            handle.join().unwrap();

            assert_eq!(rec["source"], "host");
            let absent_or_null = |v: Option<&serde_json::Value>| v.is_none() || v.unwrap().is_null();
            assert!(absent_or_null(rec.get("session_id")), "no session_id: {rec}");
            assert!(absent_or_null(rec.get("model")), "no model: {rec}");
            assert!(absent_or_null(rec.get("mission_id")), "no mission_id: {rec}");
            assert!(absent_or_null(rec.get("phase_id")), "no phase_id: {rec}");
            let payload = &rec["payload"];
            assert!(payload["sampled_at_ms"].is_u64(), "carries sampled_at_ms: {payload}");
            assert!(payload["interval_ms"].is_u64(), "carries the effective interval_ms: {payload}");
            // (#2413 C3) The liveness probe this emission's cadence
            // decision depended on stamps its own cost — "the observer was
            // negligible" stays a verifiable claim in the data rather than
            // an assumption (CLAUDE.md's samplers-stamp-their-own-cost rule).
            assert!(payload["liveness_probe_ms"].is_u64(), "carries liveness_probe_ms: {payload}");
        });
    }

    #[serial_test::serial]
    #[test]
    fn spawn_emits_at_10x_the_base_interval_while_idle() {
        with_isolated_env(|flows_dir| {
            // Nothing live on this machine — `any_dispatch_live_locally`
            // must read false, so emissions land at 10x the base 50ms
            // cadence (every ~500ms). (#2413 round 4 MF1) The FIRST
            // emission has no prior emission to measure a gap against, so
            // it stamps the CONFIGURED cadence (500 = 50 * 10x); the
            // SECOND stamps the MEASURED gap since the first — which,
            // running for real on a real clock, lands close to but not
            // necessarily bit-identical to 500, so this asserts it's in a
            // generous neighborhood rather than exact-equal (a flaky
            // scheduler pause must not fail this test).
            let ring = HostSamplerRing::new();
            let stop = Arc::new(AtomicBool::new(false));
            let handle = spawn(50, ring.clone(), Arc::clone(&stop)).unwrap();
            let recs = wait_for_n_machine_telemetry_records(flows_dir, 2, Duration::from_secs(10));
            stop.store(true, Ordering::SeqCst);
            handle.join().unwrap();
            assert_eq!(recs[0]["payload"]["interval_ms"], 500, "first emission stamps the configured 10x cadence");
            let second = recs[1]["payload"]["interval_ms"].as_u64().expect("interval_ms is a number");
            assert!(
                (400..=2000).contains(&second),
                "second emission must stamp the MEASURED gap, in the neighborhood of 500ms: got {second}"
            );
        });
    }

    #[serial_test::serial]
    #[test]
    fn spawn_emits_at_1x_the_base_interval_while_a_dispatch_is_live() {
        with_isolated_env(|flows_dir| {
            // A fresh, still-open dispatch bookend on THIS machine —
            // `any_dispatch_live_locally` must read true.
            let day_path = flows_dir.join(format!("{}.jsonl", darkmux_flow::day_utc_now()));
            let live_start = darkmux_flow::ts_utc_now();
            std::fs::write(
                &day_path,
                format!(
                    "{}\n",
                    serde_json::json!({
                        "ts": live_start, "level": "info", "category": "work", "tier": "local",
                        "stage": "dispatch", "action": "dispatch start", "handle": "h",
                        "session_id": "live-session-1",
                    })
                ),
            )
            .unwrap();

            let ring = HostSamplerRing::new();
            let stop = Arc::new(AtomicBool::new(false));
            let handle = spawn(50, ring.clone(), Arc::clone(&stop)).unwrap();
            // (#2413 round 4 MF1) First emission stamps the CONFIGURED 1x
            // cadence (no prior gap to measure); the second stamps the
            // MEASURED gap, which on a real clock lands close to but not
            // necessarily bit-identical to 50ms.
            let recs = wait_for_n_machine_telemetry_records(flows_dir, 2, Duration::from_secs(10));
            stop.store(true, Ordering::SeqCst);
            handle.join().unwrap();
            assert_eq!(recs[0]["payload"]["interval_ms"], 50, "first emission stamps the configured 1x cadence");
            let second = recs[1]["payload"]["interval_ms"].as_u64().expect("interval_ms is a number");
            assert!(
                (20..=500).contains(&second),
                "second emission must stamp the MEASURED gap, in the neighborhood of 50ms: got {second}"
            );
        });
    }

    // (found live 2026-09-06) `cached_live` used to be re-probed only right
    // before an emission, so from idle (up to `IDLE_EMIT_MULTIPLIER` x the
    // base interval, ~52s at the default 5s cadence) a dispatch shorter
    // than that window got 0-1 samples. Re-probing on a fixed per-tick
    // floor catches the idle→live transition within one interval instead.
    #[serial_test::serial]
    #[test]
    fn spawn_catches_a_new_dispatch_live_within_a_couple_of_intervals_from_idle() {
        with_isolated_env(|flows_dir| {
            let ring = HostSamplerRing::new();
            let stop = Arc::new(AtomicBool::new(false));
            let interval_ms = 50u64;
            let handle = spawn(interval_ms, ring.clone(), Arc::clone(&stop)).unwrap();

            // Let the sampler land its first (idle, 10x-cadence) emission —
            // confirms it has settled into steady idle state — before the
            // dispatch appears.
            let _first = wait_for_n_machine_telemetry_records(flows_dir, 1, Duration::from_secs(5));

            // A dispatch appears right after.
            let day_path = flows_dir.join(format!("{}.jsonl", darkmux_flow::day_utc_now()));
            let live_start = darkmux_flow::ts_utc_now();
            let appeared_at = Instant::now();
            {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new().append(true).create(true).open(&day_path).unwrap();
                writeln!(
                    f,
                    "{}",
                    serde_json::json!({
                        "ts": live_start, "level": "info", "category": "work", "tier": "local",
                        "stage": "dispatch", "action": "dispatch start", "handle": "h",
                        "session_id": "just-appeared",
                    })
                )
                .unwrap();
            }

            // The next emission after the dispatch appears must land well
            // under the OLD idle cadence (10x interval = 500ms) — with the
            // per-tick re-probe it lands within roughly 1-2 ticks.
            let deadline = Duration::from_secs(5);
            let poll_start = Instant::now();
            let mut waited = None;
            loop {
                if let Ok(text) = std::fs::read_to_string(&day_path) {
                    let count = text
                        .lines()
                        .filter(|l| {
                            serde_json::from_str::<serde_json::Value>(l)
                                .ok()
                                .and_then(|v| v.get("action").and_then(|a| a.as_str().map(str::to_string)))
                                .as_deref()
                                == Some("machine.telemetry")
                        })
                        .count();
                    if count >= 2 {
                        waited = Some(appeared_at.elapsed());
                        break;
                    }
                }
                if poll_start.elapsed() > deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            stop.store(true, Ordering::SeqCst);
            handle.join().unwrap();

            let waited = waited.expect("a second emission landed after the dispatch appeared");
            // Generous slack for scheduler jitter, but well short of the
            // pre-fix ~500ms (10x interval) an idle-cadence wait would take.
            assert!(
                waited <= Duration::from_millis(interval_ms * 6),
                "expected the next emission within a few intervals of the dispatch appearing \
                 (pre-fix this would wait out the whole idle 10x window, ~500ms), took {waited:?}"
            );
        });
    }

    #[serial_test::serial]
    #[test]
    fn spawn_stops_emitting_once_another_process_holds_the_lock() {
        with_isolated_env(|flows_dir| {
            // Simulate a dispatch process already holding the singleton
            // lock, using pid 1 (launchd/init — DETERMINISTICALLY alive on
            // any Unix, unlike `std::process::id() + 1`, which is only
            // alive BY LUCK depending on what else happens to be running)
            // as the "other" pid so `try_acquire`'s alive-check reliably
            // reads true; see `host_sampler_lock::try_acquire_as_for_test`'s
            // own doc for why a distinct pid must be injected at all.
            // A large declared interval (unrelated to the daemon's own
            // 50ms cadence below) so this simulated lock's OWN staleness
            // threshold (3x its interval) vastly outlives the test's
            // wait — a real dispatch holder would keep heartbeating every
            // tick; this test doesn't, so it needs a generous declared
            // interval instead to stay "fresh" without one.
            let other_pid = 1u32;
            // Held for the whole test body — `Drop` releases the lock, so
            // it must outlive the ownership check after the window.
            let _guard = darkmux_crew::host_sampler_lock::try_acquire_as_for_test(other_pid, "dispatch", 60_000)
                .expect("the lock is free at test start");

            let ring = HostSamplerRing::new();
            let stop = Arc::new(AtomicBool::new(false));
            let handle = spawn(50, ring.clone(), Arc::clone(&stop)).unwrap();

            // Give the sampler well past the IDLE emission threshold
            // (`IDLE_EMIT_MULTIPLIER` x the 50ms base interval = 500ms
            // nominal — measured in practice as needing real headroom:
            // each tick's own work pushes real per-tick wall-clock
            // noticeably above the nominal 50ms, so far fewer than the
            // nominal 10 ticks land in a short sleep). 3s is comfortably
            // past the threshold either way, and still fast enough not to
            // slow the suite.
            //
            // (#2475) A real dispatch holder keeps this lock fresh by
            // heartbeating every tick. An earlier round of this fixture
            // imitated that with a 50ms refresh loop that ALSO asserted
            // ownership on every iteration — sixty assertions per run, each
            // one able to abort the test.
            //
            // (#2762) That refresh loop WAS the flake, not the cure. Two
            // things make a per-iteration `guard.heartbeat()` assertion
            // structurally fragile in a threaded test binary:
            //
            //   1. `heartbeat()` re-resolves the lock's ADDRESS on every
            //      call — `lock_path()` reads process-global `DARKMUX_HOME`
            //      live. Any sibling test that mutates that env var while
            //      this window is open sends the heartbeat to a different
            //      file, where it correctly finds no lock of ours and
            //      reports `false`. The holder never lost anything; the
            //      assertion was just pointed somewhere else.
            //   2. With `DARKMUX_HOME` unset, every test build in this
            //      workspace falls back to ONE fixed machine-global path
            //      (`dispatch_liveness::darkmux_home_dir_fallback`), so
            //      "a sibling test" is not even bounded to this binary.
            //
            // Neither is a timing problem, which is why widening the timing
            // tolerance twice never helped. So stop depending on a
            // re-resolved address and a maintained clock at all:
            //
            //   * Freshness is STRUCTURAL, not maintained. The declared
            //     interval is 60s, so this holder's own staleness threshold
            //     (3x = 180s) outlives the 3s window by 60x with no
            //     refreshing whatsoever — which is what the `other_pid`
            //     comment above already said before the refresh loop was
            //     layered on top of it.
            //   * Ownership is checked ONCE, after the window, against the
            //     lock path CAPTURED AT ACQUIRE TIME rather than
            //     re-resolved. A sibling's env mutation can no longer reach
            //     that assertion; a genuine takeover (a different pid
            //     written to that same file) still fails it.
            //
            // One check instead of sixty, and the one that remains cannot
            // be perturbed by anything except the takeover it exists to
            // detect.
            let lock_path_at_acquire = darkmux_types::config_access::host_sampler_lock_path();
            let home_at_acquire = std::env::var("DARKMUX_HOME").ok();
            std::thread::sleep(Duration::from_millis(3000));

            stop.store(true, Ordering::SeqCst);
            handle.join().unwrap();

            // (#2762) The one ownership check — reads the CAPTURED path, so
            // it is about the lock itself, never about where `DARKMUX_HOME`
            // happens to point by now. Without it the no-emission assertion
            // below could pass vacuously: a sampler that never faced a
            // contended lock trivially emits nothing.
            let held: Option<darkmux_crew::host_sampler_lock::LockState> = std::fs::read_to_string(&lock_path_at_acquire)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok());
            assert_eq!(
                held.as_ref().map(|st| st.pid),
                Some(other_pid),
                "the simulated dispatch holder must still own the lock after the window, or the \
                 no-emission assertion below proves nothing.\n  \
                 lock path (captured at acquire): {lock_path_at_acquire:?}\n  \
                 DARKMUX_HOME at acquire: {home_at_acquire:?}\n  \
                 DARKMUX_HOME now:        {now_home:?}\n  \
                 lock file at that path now: {held:?}\n\
                 Read it this way: a present file with a DIFFERENT pid is a real takeover race \
                 (the thing this test guards); an ABSENT file means something deleted it. A \
                 changed DARKMUX_HOME can no longer cause either, since this reads the captured \
                 path — so if you are seeing this, it is NOT an env-isolation problem.",
                now_home = std::env::var("DARKMUX_HOME").ok(),
            );

            let day_path = flows_dir.join(format!("{}.jsonl", darkmux_flow::day_utc_now()));
            let text = std::fs::read_to_string(&day_path).unwrap_or_default();
            let emitted_machine_telemetry = text.lines().any(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .ok()
                    .and_then(|v| v.get("action").and_then(|a| a.as_str().map(str::to_string)))
                    .as_deref()
                    == Some("machine.telemetry")
            });
            // (#2762) The other assertion that a mid-window `DARKMUX_HOME`
            // mutation can trip, and for a DIFFERENT reason than the
            // ownership check above: if the lock's address moves, the
            // daemon finds no holder at the new address, correctly takes
            // over, and emits. Measured — a HOME-only swap for 1.5s of this
            // window produces exactly this failure while the ownership
            // check stays green. Say so here, so that occurrence is not
            // misread as the sampler ignoring a lock it could plainly see.
            assert!(
                !emitted_machine_telemetry,
                "the daemon sampler must not emit while another process holds a fresh lock.\n  \
                 lock path (captured at acquire): {lock_path_at_acquire:?}\n  \
                 lock path now:                   {now_path:?}  (same: {same_path})\n  \
                 DARKMUX_HOME at acquire: {home_at_acquire:?}\n  \
                 DARKMUX_HOME now:        {now_home:?}\n\
                 Read it this way: if the two paths DIFFER, a sibling test mutated process-global \
                 env mid-window and the daemon correctly took over a lock that had moved — \
                 serialize that sibling, this is not a sampler defect. If they are the SAME, the \
                 sampler emitted while a fresh lock it could see was held, which is the real \
                 defect this test exists to catch.",
                now_path = darkmux_types::config_access::host_sampler_lock_path(),
                same_path = darkmux_types::config_access::host_sampler_lock_path() == lock_path_at_acquire,
                now_home = std::env::var("DARKMUX_HOME").ok(),
            );
            // Still samples its OWN ring regardless (the ring is
            // unconditional, per the issue's own rule).
            assert!(ring.snapshot().is_some(), "the ring keeps sampling regardless of who holds the lock");
        });
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
    // ─── (#2775) the periodic machine-lens aggregate ───────────────────

    /// The payload is the MACHINE LENS's own vocabulary at the top level —
    /// `now` / `window` / `battery_health` spliced in rather than nested
    /// under a wrapper key. The operator's settled decision was one
    /// heartbeat carrying the machine's state, and a hook predicate should
    /// read `payload.window.thermal.…`, which is also what the lens calls
    /// it. A wrapper key would give one fact two spellings.
    #[test]
    fn the_rollup_payload_carries_the_machine_lens_shape_at_the_top_level() {
        let ring = HostSamplerRing::new();
        ring.set_configured_interval_for_test(5_000);
        ring.push_for_test(0, 10, 20, 30, 7);
        ring.push_for_test(5_000, 50, 60, 70, 8);
        let load = ring.snapshot().expect("two samples are in the ring");

        let rec = build_machine_rollup_record(
            load,
            Some(serde_json::json!({ "models": [], "gather_ms": 3 })),
            Some("fair"),
            60,
            60_000,
            11,
            5_000,
        );
        assert_eq!(rec.action, MACHINE_ROLLUP_ACTION);
        assert_eq!(rec.source.as_deref(), Some("host-sampler"));
        let p = rec.payload.expect("payload");
        // The lens's own keys, not re-spelled and not wrapped.
        assert!(p.get("now").is_some(), "the lens's `now` block rides at the top level");
        assert_eq!(p["window"]["samples"], 2);
        assert_eq!(p["window"]["cpu_pct"]["max"], 50);
        assert!(p.get("battery_health").is_some(), "present (as null) rather than absent");
        assert_eq!(p["residency"]["gather_ms"], 3);
    }

    /// Observer doctrine, checked as a payload contract rather than as an
    /// intention (#1286 constraints 3 and 4): the emitter stamps its OWN
    /// cost, and the cadence is recorded — both the CONFIGURED period and
    /// the MEASURED gap, so a tightened debug cadence is visible in the
    /// data instead of inferred from row spacing.
    #[test]
    fn the_rollup_stamps_its_own_cost_and_records_both_cadences() {
        let rec = build_machine_rollup_record(
            serde_json::json!({}),
            None,
            None,
            60,
            61_400,
            42,
            9_000,
        );
        let p = rec.payload.expect("payload");
        assert_eq!(p["gather_ms"], 42, "the observer's own cost must be in the artifact");
        assert_eq!(p["period_seconds"], 60, "the configured knob");
        assert_eq!(
            p["emitted_interval_ms"], 61_400,
            "the MEASURED gap — a tick that ran late reports what happened"
        );
        assert_eq!(p["sampled_at_ms"], 9_000);
    }

    /// `previous_thermal_state` exists so a subscriber that missed an
    /// edge-triggered `machine.thermal` can still reconstruct DIRECTION
    /// from a heartbeat alone. Before any transition has been observed
    /// there is no honest predecessor, and the record says `null` rather
    /// than guessing `nominal`.
    #[test]
    fn previous_thermal_state_is_null_until_a_transition_has_been_seen() {
        let rec = build_machine_rollup_record(serde_json::json!({}), None, None, 60, 60_000, 1, 0);
        assert_eq!(rec.payload.unwrap()["previous_thermal_state"], serde_json::Value::Null);

        let rec = build_machine_rollup_record(
            serde_json::json!({}),
            None,
            Some("nominal"),
            60,
            60_000,
            1,
            0,
        );
        assert_eq!(rec.payload.unwrap()["previous_thermal_state"], "nominal");
    }

    /// A heartbeat is a READING, not a verdict. The edge-triggered
    /// `machine.thermal` / `machine.battery` records keep their `Warn` for
    /// the transitions that change what the machine will DO; a periodic
    /// record that warned would light the same lamp every minute for a
    /// condition already reported, which is the editorializing darkmux does
    /// not do.
    #[test]
    fn the_rollup_is_always_info_even_when_the_machine_is_critical() {
        let ring = HostSamplerRing::new();
        ring.set_configured_interval_for_test(5_000);
        ring.push(RingEntry {
            at_ms: 0,
            sample: HostSampleFull {
                thermal: Some(ThermalSample { state: "critical".into(), cpu_speed_limit_pct: 40 }),
                ..Default::default()
            },
        });
        let load = ring.snapshot().expect("one sample");
        let rec = build_machine_rollup_record(load, None, Some("serious"), 60, 60_000, 1, 0);
        assert!(matches!(rec.level, darkmux_flow::Level::Info));
    }

    /// The emission clock is `interval_due`, and `0` there means OFF — this
    /// codebase's zero convention (`runtime.host_sampler_interval_ms`,
    /// `redis.maxlen`), never "emit continuously", which is exactly what a
    /// naive `>=` would give a zero period.
    #[test]
    fn a_zero_period_never_comes_due() {
        assert!(!interval_due(0, 0));
        assert!(!interval_due(60_000, 0));
        assert!(!interval_due(u64::MAX, 0), "an unbounded wait on a 0 period is still off");
        // …and a real period behaves normally.
        assert!(!interval_due(59_999, 60_000));
        assert!(interval_due(60_000, 60_000));
    }

    /// `level_ms`/`level_entries` ride the SAME `/machine/resources` window
    /// block the rollup carries, from one builder — so the machine lens and
    /// a rollup subscriber cannot disagree about how long the machine spent
    /// hot or how often it got there.
    #[test]
    fn the_window_thermal_block_is_one_shape_for_both_carriers() {
        let ring = HostSamplerRing::new();
        ring.set_configured_interval_for_test(5_000);
        for (i, state) in ["nominal", "fair", "fair", "nominal"].iter().enumerate() {
            ring.push(RingEntry {
                at_ms: i as u64 * 5_000,
                sample: HostSampleFull {
                    thermal: Some(ThermalSample {
                        state: (*state).into(),
                        cpu_speed_limit_pct: 100,
                    }),
                    ..Default::default()
                },
            });
        }
        let load = ring.snapshot().expect("samples");
        let from_lens = load["window"]["thermal"].clone();
        assert_eq!(from_lens["level_entries"]["fair"], 1, "one arrival, not two samples");
        assert_eq!(from_lens["level_ms"]["fair"], 10_000);

        let rec = build_machine_rollup_record(load, None, None, 60, 60_000, 1, 0);
        assert_eq!(
            rec.payload.unwrap()["window"]["thermal"],
            from_lens,
            "the rollup must carry the lens's own block, not a second rendering of it"
        );
    }
}
