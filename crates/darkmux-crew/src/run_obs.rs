//! The run-observability RAII substrate #1877 (item 2) extracts out of
//! `darkmux_lab::lab::review` — the sink-agnostic emitter seam, the host
//! telemetry sampling LOOP, and the guard that ties a run's samples and
//! `step result` records to its own lifetime.
//!
//! **Why the loop, specifically, was the gap.** The pure host-reading
//! primitive (`crate::telemetry_sampler::sample_host`) has always been
//! shared — `dispatch_internal.rs`'s own always-on sampler thread and
//! review's driver both called it. What neither shared was the LOOP around
//! it: a background thread, a cadence, a stop flag, and — for a caller that
//! can't write straight to the global flow sink (review's driver is
//! sink-agnostic by design; see [`RunEmitter`]) — a channel to carry
//! samples back to whoever CAN decide where they land. [`HostTelemetrySampler`]
//! is that loop, generalized so a second mission reaches for it instead of
//! rebuilding it. `dispatch_internal.rs`'s own sampler thread is a
//! different shape (writes straight to the global sink, no injected
//! emitter to bridge to) and is NOT migrated onto this type by this PR —
//! named here so the asymmetry reads as a decision, not an oversight.
//!
//! **The RAII shape ([`RunObs`]).** Contract 2 (CLAUDE.md's cross-system
//! contract registry) requires liveness bookends on every path that
//! performs model work, on every exit — clean finish, an early `?`-return,
//! or a panic. [`RunObs`] bundles the injected emitter with its
//! [`HostTelemetrySampler`] and drains pending samples before every
//! `step result` it emits AND once more in `Drop`, so a caller that goes
//! through [`RunObs`] cannot hold one without also getting the
//! drain-on-every-exit-path guarantee — there is no method on [`RunObs`]
//! itself that lets a caller emit a step result while skipping the
//! telemetry drain, and no way to reach the sampler's thread handle or
//! receiver directly through it (both stay private on
//! [`HostTelemetrySampler`]; [`HostTelemetrySampler::try_drain`] is the
//! only way to read a sample out).
//!
//! **This guarantee is scoped to [`RunObs`], not to [`HostTelemetrySampler`]
//! itself.** `HostTelemetrySampler::start` is `pub` on purpose: a driver
//! that already owns its own liveness bookend and already interleaves
//! telemetry with its own step-lifecycle record stream — `run_review_graph`
//! is exactly this shape, see its own "(#1349) Host telemetry only — no
//! bookend struct" comment — can hold the sampler directly and drain it by
//! hand instead of going through [`RunObs`]. Nothing at the type level
//! stops that caller from holding the sampler undrained, or dropping it
//! without ever draining; the obligation is the caller's, not the
//! compiler's, for that path. Reach for [`RunObs`] by default — it is the
//! batteries-included wrapper for a driver that does NOT already have this
//! machinery — and reach for the bare sampler only when a caller already
//! owns the interleaving discipline [`RunObs`] would otherwise provide.
//!
//! **Renamed on the move** (`ReviewEmitter` → [`RunEmitter`], `ReviewObs` →
//! [`RunObs`]): both were review-flavored names for genuinely mission-
//! agnostic shapes — a `dyn`-safe sink trait and an RAII bundle that knows
//! nothing about flags or judging. `review.rs` re-exports both under their
//! original names (`pub use darkmux_crew::run_obs::{RunEmitter as
//! ReviewEmitter, ...}`), so every existing `impl ReviewEmitter for X`
//! across the tree keeps compiling unchanged. [`step_result_record`] is the
//! same generalization applied to the record builder: the one hardcoded
//! bit review's original `review_step_result_record` had — `source:
//! Some("review".into())` — is now a parameter, and review.rs's own
//! wrapper passes `"review"` so its output is byte-identical to before.

use crate::telemetry_sampler::{sample_host, HostSample};
use darkmux_types::LoadedModel;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Production sample cadence (#1247 "no blind runs" — the cadence is a
/// recorded knob, never adaptive-silent). Same values `dispatch_internal`'s
/// own always-on sampler uses. `interval` is the time between samples;
/// `poll` is how often the sampler thread re-checks the stop flag while
/// sleeping out `interval`, so teardown is prompt (≤`poll`) instead of
/// blocking for a full tick.
pub const DEFAULT_TELEMETRY_INTERVAL: Duration = Duration::from_millis(2000);
pub const DEFAULT_TELEMETRY_POLL: Duration = Duration::from_millis(500);

// ─── flow-record emission ────────────────────────────────────────────────

/// Sink for a run driver's observability records. The driver only knows
/// how to build [`darkmux_flow::FlowRecord`]s and hand them to `emit` — it
/// never decides where they land, which is what keeps a driver sink-
/// agnostic (the lab-vs-fleet scope boundary: a bench run's records stay
/// per-run-local; a live mission's ride the fleet stream — same driver
/// code, different injected [`RunEmitter`]).
///
/// **Not `Send`, and taken as `&mut dyn`.** A [`RunEmitter`] can only be
/// held and called from one thread at a time — there is no blanket `Sync`
/// bound and no interior locking, deliberately (see [`HostTelemetrySampler`]'s
/// doc for why the sampler routes samples through an `mpsc` channel instead
/// of calling `emit` straight from its background thread). A driver whose
/// steps genuinely run concurrently (a `run_step_graph` wave dispatching
/// several steps in parallel worker threads) cannot hand each worker its
/// own `&mut dyn RunEmitter` — `run_review_graph` (this substrate's own
/// consumer) works around this by keeping the emit call itself on the MAIN
/// thread: the scheduler drains each wave's worker results before invoking
/// its emit closure, so `emitter.emit(...)` is only ever called
/// single-threaded even though the STEPS it reports on ran concurrently.
/// The actual model-dispatch work happening inside those worker threads
/// still emits its own liveness bookends (contract 2) straight to the
/// global `darkmux_flow::record` sink, never through this trait — a
/// `RunEmitter` only ever reports what the main thread already knows.
/// A future driver reaching for this substrate should expect the same
/// shape: [`RunEmitter`] threads records off the MAIN thread only; genuine
/// worker-thread concurrency needs its own bridge back (a channel, same as
/// [`HostTelemetrySampler`]'s) rather than passing the emitter itself
/// across threads.
pub trait RunEmitter {
    fn emit(&mut self, record: darkmux_flow::FlowRecord);
}

/// No-op emitter — the "at minimum a no-op-able sink" default for callers
/// (and tests that don't assert on flow records) that don't want run
/// observability output.
pub struct NullEmitter;

impl RunEmitter for NullEmitter {
    fn emit(&mut self, _record: darkmux_flow::FlowRecord) {}
}

/// Build a generic `step result` flow record — one action/payload
/// vocabulary shared by every step-kind consumer regardless of which
/// mission ran the step. `source` names the mission (review passes
/// `"review"`); `kind`/`step_id` name the step; `payload` carries whatever
/// fields that step's own [`crate::run_record::StepRecord`]-shaped summary
/// wants alongside them.
pub fn step_result_record(
    source: &str,
    kind: &str,
    step_id: &str,
    case_id: &str,
    mission_id: Option<&str>,
    payload: serde_json::Value,
) -> darkmux_flow::FlowRecord {
    let mut full = serde_json::json!({ "step_id": step_id, "kind": kind });
    if let (serde_json::Value::Object(extra), serde_json::Value::Object(base)) = (payload, &mut full) {
        base.extend(extra);
    }
    darkmux_flow::FlowRecord {
        ts: darkmux_flow::ts_utc_now(),
        level: darkmux_flow::Level::Info,
        category: darkmux_flow::Category::Work,
        tier: darkmux_flow::Tier::Local,
        stage: darkmux_flow::Stage::Dispatch,
        action: "step result".to_string(),
        handle: step_id.to_string(),
        phase_id: None,
        session_id: Some(case_id.to_string()),
        source: Some(source.to_string()),
        model: None,
        reasoning: None,
        mission_id: mission_id.map(String::from),
        machine_id: None,
        machine_uid: None,
        prev_hash: None,
        hash: None,
        payload: Some(full),
        work_id: None,
        attempt: None,
    }
}

// ─── host telemetry sampling loop ────────────────────────────────────────

/// (#1247 doctrine surface — "No blind runs") Best-effort host cpu/ram/gpu
/// sampler LOOP for a run driver that bypasses `dispatch_internal` (and so
/// gets none of its always-on sampling for free). Reuses the EXACT
/// host-reading mechanism `dispatch_internal` uses
/// (`crate::telemetry_sampler::sample_host` — shells out to
/// `top`/`vm_stat`+`sysctl`/`ioreg`) and the exact record shape
/// (`crate::dispatch::build_telemetry_record`, `category=telemetry,
/// source="process", action="telemetry.process"`, payload `{cpu, mem,
/// gpu}`), so the run-monitor/viewer code that already renders
/// `telemetry.process` records applies unchanged.
///
/// The sampling FUNCTION is injected (`sample_fn`, a plain fn pointer
/// defaulting to `sample_host` at every production call site — see
/// [`RunObs::new`]) so tests can drive the sampler with an instant fake
/// instead of racing real `top -l 1` subprocess latency (~600-900ms per
/// call) against a scripted deadline on a shared CI runner. The real
/// `sample_host` gets its own direct coverage in this crate (macOS-gated,
/// since the commands it shells to are macOS-only).
///
/// Samples land on an `mpsc` channel rather than being emitted directly
/// from the background thread: a run driver's [`RunEmitter`] is a
/// caller-injected `&mut dyn` trait object — not thread-safe, and
/// deliberately not wrapped in a `Mutex` (that would force every
/// `RunEmitter` impl through lock-guarded access for a feature this
/// narrow). Instead, [`RunObs`] (or a caller managing the sampler
/// directly, as `run_review_graph` does) drains the channel via
/// [`Self::try_drain`] immediately before every `step result` record it
/// emits, so telemetry interleaves with the run's other records close to
/// when it was sampled — never batched at end-of-run.
///
/// All three fields are private: the only way to construct one is
/// [`Self::start`], the only way to read a sample is [`Self::try_drain`],
/// and teardown is unconditional via `Drop` — there is no path that
/// constructs the sampler thread without also getting these guarantees.
pub struct HostTelemetrySampler {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    rx: Receiver<darkmux_flow::FlowRecord>,
}

impl HostTelemetrySampler {
    /// Spawn the sampler thread. Uses `Builder::spawn` (which returns
    /// `io::Result`, unlike the panicking `thread::spawn`) so an OS-level
    /// spawn failure degrades to "no samples" — sampling must never affect
    /// the run it's observing.
    ///
    /// `#[must_use]`: dropping the returned guard immediately stops the
    /// sampler thread before it ever samples (`Drop` joins it right away),
    /// so `HostTelemetrySampler::start(...)` with no binding is silently a
    /// no-op — the cheap structural check for the "held without a drain
    /// plan" mistake this module's doc names as a real, reachable failure
    /// mode now that `start` is `pub`.
    #[must_use]
    pub fn start(
        case_id: String,
        crew: String,
        interval: Duration,
        poll: Duration,
        sample_fn: fn() -> HostSample,
        lms_fn: fn() -> anyhow::Result<Vec<LoadedModel>>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let stop_thread = Arc::clone(&stop);
        // (#1247 follow-up) lms load/unload deltas — the diff-state twin of
        // dispatch_internal.rs's `run_telemetry_sampler`. `sample_host` was
        // already shared, but `lms_diff` wasn't wired up here at all, so a
        // review run's "model (lms)" viewer track always read "no telemetry
        // yet" — the run-detail view has host cpu/mem/gpu (from the
        // sampling below) but never which models were actually resident.
        // `seeded` mirrors dispatch_internal's baseline-emission: the FIRST
        // successful sample diffs against empty so the models already
        // resident when the run started show up immediately, not only on a
        // later load/unload edge. `lms_fn` is injected (same discipline as
        // `sample_fn`, same reason) — the real
        // `darkmux_profiles::lms::list_loaded` shells out to the `lms`
        // CLI, and an un-injected real subprocess call in this loop raced
        // and broke fast-cadence telemetry tests' tight timing margin.
        let mut prev_loaded: Vec<LoadedModel> = Vec::new();
        let mut seeded = false;
        let handle = thread::Builder::new()
            .name("run-telemetry".to_string())
            .spawn(move || loop {
                // Sleep out `interval` FIRST, THEN sample — deliberately
                // NOT sample-then-sleep. A real run's own dispatches are
                // real LMStudio round trips that take real wall-clock, so
                // a run genuinely running past one `interval` gets its
                // first sample right on schedule. The load-bearing side
                // effect: at the PRODUCTION cadence
                // ([`DEFAULT_TELEMETRY_INTERVAL`], 2s), this makes it
                // structurally impossible for a synchronous, sub-
                // millisecond MOCKED test run to race a sample into its
                // recording emitter — the thread can't reach the sample
                // point before `stop()` + `join()` in
                // `HostTelemetrySampler::drop` already won. Only a run
                // whose OWN cadence deliberately shortens `interval` (this
                // crate's own telemetry tests) or a real dispatch that
                // outlives 2s ever observes one.
                let mut slept = Duration::ZERO;
                while slept < interval {
                    if stop_thread.load(Ordering::SeqCst) {
                        return;
                    }
                    let nap = poll.min(interval - slept);
                    thread::sleep(nap);
                    slept += nap;
                }
                // lms load/unload deltas. Only diff against a SUCCESSFUL
                // probe — a failed `list_loaded` is skipped (leaves
                // `prev_loaded` intact) so a transient lms hiccup doesn't
                // emit a flurry of spurious unloads (same guard as
                // dispatch_internal.rs's sampler).
                if let Ok(cur) = lms_fn() {
                    let diffs = if seeded {
                        crate::telemetry_sampler::lms_diff(&prev_loaded, &cur)
                    } else {
                        seeded = true;
                        crate::telemetry_sampler::lms_diff(&[], &cur)
                    };
                    for payload in diffs {
                        let record = crate::dispatch::build_telemetry_record(
                            darkmux_flow::Level::Info,
                            "telemetry.lms",
                            "lms",
                            &crew,
                            &case_id,
                            None,
                            None,
                            None,
                            payload,
                        );
                        let _ = tx.send(record);
                    }
                    prev_loaded = cur;
                }
                let sample = sample_fn();
                if sample.cpu.is_some() || sample.mem.is_some() || sample.gpu.is_some() {
                    let mut payload = serde_json::Map::new();
                    // (#1247 "no blind runs" — the cadence is a recorded
                    // knob, never adaptive-silent) Stamp the ACTUAL
                    // interval this sampler runs at into every sample it
                    // emits, so a second consumer choosing a different
                    // `interval` from `DEFAULT_TELEMETRY_INTERVAL`
                    // produces an artifact that says so, instead of one
                    // indistinguishable from the production cadence.
                    payload.insert("interval_ms".into(), (interval.as_millis() as u64).into());
                    if let Some(c) = sample.cpu {
                        payload.insert("cpu".into(), c.into());
                    }
                    if let Some(m) = sample.mem {
                        payload.insert("mem".into(), m.into());
                    }
                    if let Some(g) = sample.gpu {
                        payload.insert("gpu".into(), g.into());
                    }
                    let record = crate::dispatch::build_telemetry_record(
                        darkmux_flow::Level::Info,
                        "telemetry.process",
                        "process",
                        &crew,
                        &case_id,
                        None,
                        None,
                        None,
                        serde_json::Value::Object(payload),
                    );
                    // A disconnected receiver (the guard already tore
                    // down) just means this sample is lost — best-effort,
                    // never a reason to abort the loop.
                    let _ = tx.send(record);
                }
            })
            .ok();
        Self { stop, handle, rx }
    }

    /// Drain every sample buffered since the last drain, without blocking.
    /// The only way to read a sample out of a running sampler — `rx` never
    /// leaves this type.
    pub fn try_drain(&self) -> Vec<darkmux_flow::FlowRecord> {
        self.rx.try_iter().collect()
    }

    /// Signal the stop flag and join the thread. Called from `Drop` — the
    /// RAII tie-in that guarantees no orphaned sampler thread outlives its
    /// owner, on every exit path (clean finish, early `?`-return, or
    /// panic).
    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for HostTelemetrySampler {
    fn drop(&mut self) {
        self.stop();
    }
}

// ─── the RAII observability guard ────────────────────────────────────────

/// Run-observability helper for a driver's SEQUENTIAL dispatch path (review's
/// `run_judge_only`, the `--charges-file` re-judge). Emits the SAME generic
/// `step result` records a graph-based path emits (via [`step_result_record`])
/// — but through the injected [`RunEmitter`] rather than the global
/// `darkmux_flow::record`, so a driver stays sink-agnostic (a bench emitter
/// suppresses; a fleet emitter writes the real stream).
///
/// It also owns the run's [`HostTelemetrySampler`] (#1247 "no blind runs")
/// and interleaves its samples with the run's own records: [`Self::drain`]
/// fires before every `step result` record it emits (and once more in
/// `Drop`), so telemetry streams alongside the run rather than batching at
/// the end. The sampler thread is torn down by [`HostTelemetrySampler`]'s
/// own `Drop` (a field of this struct, run automatically after this type's
/// own `drop` returns), so it never outlives the run on any exit path.
///
/// There is deliberately NO task-level liveness bookend here — the caller
/// is expected to already wrap the whole dispatch in the canonical
/// `dispatch start`/`dispatch complete`/`dispatch error` record (contract
/// 2); a second bookend inside this guard would be exactly the competing-
/// vocabulary duplication #1349 retired from review's own driver.
pub struct RunObs<'a> {
    case_id: String,
    source: String,
    emitter: &'a mut dyn RunEmitter,
    telemetry: HostTelemetrySampler,
}

impl<'a> RunObs<'a> {
    /// `source` names the mission emitting through this guard (review
    /// passes `"review"`) — stamped on every [`step_result_record`] this
    /// guard builds, the one piece [`step_result_record`]'s generalization
    /// took out of a hardcoded literal.
    ///
    /// `#[must_use]`: `let _ = RunObs::new(...)` drops the guard on the
    /// spot — its `Drop` runs, its `HostTelemetrySampler` stops, and every
    /// step result the caller meant to route through it never gets built.
    /// That compiles today (`let_underscore_drop` is restriction-tier and
    /// off), so this is the cheap structural catch in place of the
    /// compile-time redesign this module's doc correctly declined.
    #[must_use]
    pub fn new(emitter: &'a mut dyn RunEmitter, case_id: &str, crew: &str, source: &str) -> Self {
        Self::new_with_telemetry(
            emitter,
            case_id,
            crew,
            source,
            DEFAULT_TELEMETRY_INTERVAL,
            DEFAULT_TELEMETRY_POLL,
            sample_host,
            darkmux_profiles::lms::list_loaded,
        )
    }

    /// Same as [`Self::new`] but with a caller-chosen telemetry cadence AND
    /// sampling functions — the test-only seam a scripted run uses to
    /// observe deterministic samples without a multi-second sleep and
    /// without shelling to the real (macOS-only, ~600-900ms-per-call)
    /// `top`/`vm_stat`/`ioreg` commands, or to the real `lms` CLI.
    /// Production always goes through `new`, which fixes the cadence at
    /// [`DEFAULT_TELEMETRY_INTERVAL`] and the samplers at the real
    /// `sample_host` / `darkmux_profiles::lms::list_loaded`.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new_with_telemetry(
        emitter: &'a mut dyn RunEmitter,
        case_id: &str,
        crew: &str,
        source: &str,
        telemetry_interval: Duration,
        telemetry_poll: Duration,
        sample_fn: fn() -> HostSample,
        lms_fn: fn() -> anyhow::Result<Vec<LoadedModel>>,
    ) -> Self {
        Self {
            telemetry: HostTelemetrySampler::start(
                case_id.to_string(),
                crew.to_string(),
                telemetry_interval,
                telemetry_poll,
                sample_fn,
                lms_fn,
            ),
            case_id: case_id.to_string(),
            source: source.to_string(),
            emitter,
        }
    }

    /// Drain every telemetry sample buffered since the last drain and emit
    /// each through the injected emitter — called immediately before every
    /// `step result` record so telemetry streams alongside the run rather
    /// than batching at the end.
    pub fn drain(&mut self) {
        for record in self.telemetry.try_drain() {
            self.emitter.emit(record);
        }
    }

    /// Drain pending telemetry, then emit one generic `step result` record —
    /// the SAME shape a graph path's own step kinds emit, just routed
    /// through the injected emitter and stamped with this guard's `source`.
    pub fn step_result(&mut self, kind: &str, step_id: &str, payload: serde_json::Value) {
        self.drain();
        // (#1641) Sequential-path callers mint no Mission — see
        // `step_result_record`'s own doc for the `mission_id` field.
        self.emitter
            .emit(step_result_record(&self.source, kind, step_id, &self.case_id, None, payload));
    }
}

impl Drop for RunObs<'_> {
    fn drop(&mut self) {
        // Flush any sample the sampler produced since the last drain. The
        // sampler thread itself stops right after, via
        // `HostTelemetrySampler`'s own `Drop` (a field, torn down after this
        // body returns), so it never outlives the run on any exit path.
        //
        // Known, accepted loss window: a sample the sampler thread sends
        // AFTER this final drain but BEFORE the join in the sampler's `Drop`
        // completes is dropped with the channel — at most one final-tick
        // sample, consistent with the sampler's best-effort framing.
        self.drain();
    }
}

// ─── #1877 item 2 conformance: liveness + cadence at the new home ────────

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_types::LoadedModel;

    /// Deterministic sample — no subprocess, no timing race. Same shape
    /// `darkmux_lab::lab::review`'s own tests use for the injected
    /// `sample_fn` seam.
    fn fake_sample() -> HostSample {
        HostSample { cpu: Some(42), mem: Some(50), gpu: Some(7) }
    }

    /// No residents — the `lms_fn` seam. Zero IO, zero dispatch: this
    /// function signature (`fn() -> Result<Vec<LoadedModel>>`) cannot name
    /// a model or a chat client, so "no model dispatch on the sampling
    /// path" is a property of the TYPE the sampler is built around, not
    /// just this particular fake's behavior.
    fn fake_lms() -> anyhow::Result<Vec<LoadedModel>> {
        Ok(Vec::new())
    }

    struct RecordingEmitter {
        records: Vec<darkmux_flow::FlowRecord>,
    }

    impl RecordingEmitter {
        fn new() -> Self {
            Self { records: Vec::new() }
        }
    }

    impl RunEmitter for RecordingEmitter {
        fn emit(&mut self, record: darkmux_flow::FlowRecord) {
            self.records.push(record);
        }
    }

    /// The RAII guarantee contract 2 (CLAUDE.md) requires: a panic while
    /// [`RunObs`] is alive must still stop the sampler thread — `Drop` runs
    /// on unwind exactly as it does on a clean return. Spawned on its own
    /// thread (so the panic doesn't fail the test harness itself);
    /// `catch_unwind` there converts the panic into a `Result`, and the
    /// done-channel + bounded `recv_timeout` turns "the sampler thread
    /// wedged" into a loud, bounded test failure instead of hanging
    /// `cargo test` — same discipline
    /// `darkmux_lab::lab::review`'s own sampler tests use for the
    /// clean-finish/early-drop pair; this is their missing THIRD case (a
    /// genuine panic unwind, not just an early scope exit).
    #[test]
    fn run_obs_panic_unwind_stops_the_sampler_thread() {
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut emitter = RecordingEmitter::new();
                let _obs = RunObs::new_with_telemetry(
                    &mut emitter,
                    "case-panic",
                    "crew-panic",
                    "test",
                    Duration::from_millis(5),
                    Duration::from_millis(2),
                    fake_sample,
                    fake_lms,
                );
                panic!("simulated failure while the guard is alive");
            }));
            assert!(result.is_err(), "the panic should have propagated out of catch_unwind");
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("panic unwind did not stop the telemetry sampler thread within 5s — thread leak");
    }

    /// The sampler must actually DELIVER a sample within its cadence, not
    /// merely start and stop cleanly (the clean-finish/early-drop/panic
    /// tests all prove teardown; none of them prove the loop body ever
    /// ran). A 5ms interval with a 5s bounded wait makes "never samples"
    /// fail loud instead of hanging.
    #[test]
    fn run_obs_delivers_a_sample_within_its_cadence() {
        let mut emitter = RecordingEmitter::new();
        {
            let mut obs = RunObs::new_with_telemetry(
                &mut emitter,
                "case-cadence",
                "crew-cadence",
                "test",
                Duration::from_millis(5),
                Duration::from_millis(2),
                fake_sample,
                fake_lms,
            );
            // `obs` holds `&mut emitter` for its whole lifetime, so the
            // records can't be inspected until it drops — a fixed sleep
            // with generous margin (40x the 5ms cadence) rather than a
            // polling loop that would need to read through the same
            // exclusive borrow. `obs.drain()` at the end (mirroring
            // `Drop`'s own final drain) forwards whatever landed during
            // the sleep before this scope ends and `Drop` runs again
            // (idempotent — nothing left to drain the second time).
            thread::sleep(Duration::from_millis(200));
            obs.drain();
        } // `obs` drops here — sampler thread stops, joins.
        assert!(
            !emitter.records.is_empty(),
            "no sample landed within 200ms (40x the 5ms cadence) — sampler did not run"
        );
        let sample = &emitter.records[0];
        assert_eq!(sample.action, "telemetry.process");
        assert_eq!(sample.source.as_deref(), Some("process"));
        let payload = sample.payload.as_ref().expect("telemetry.process record must carry a payload");
        assert_eq!(payload["cpu"], serde_json::json!(42));
        assert_eq!(payload["mem"], serde_json::json!(50));
        assert_eq!(payload["gpu"], serde_json::json!(7));
        // (#1247 "no blind runs" — the cadence must be a recorded knob,
        // never adaptive-silent) This test drives the sampler at a 5ms
        // interval, not the production 2000ms default — if the payload
        // didn't stamp the ACTUAL interval it ran at, this artifact would
        // be indistinguishable from one sampled at production cadence.
        assert_eq!(payload["interval_ms"], serde_json::json!(5));
    }
}
