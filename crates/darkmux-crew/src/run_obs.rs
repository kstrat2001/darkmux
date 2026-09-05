//! The sink-agnostic run-observability emitter seam.
//!
//! This module used to also hold `HostTelemetrySampler` (a background
//! host cpu/ram/gpu + lms sampling loop feeding per-dispatch
//! `telemetry.process` records) and `RunObs` (an RAII guard that bundled
//! that sampler with a driver's `step result` emission). Both are RETIRED
//! (#2413): the per-dispatch `telemetry.process` curve they produced is
//! gone — one machine-scoped sampler (`crate::host_sampler_lock`,
//! `dispatch_internal.rs`'s always-on sampler thread) now owns host
//! telemetry for the whole machine, and every caller that used to
//! construct `HostTelemetrySampler`/`RunObs` directly (the generic launch
//! path in `src/mission_launch.rs`, the ACP ephemeral path in
//! `src/acp_panel.rs`) was deleted along with them — there is no
//! production caller left for either type, so they were removed rather
//! than kept as dead code.
//!
//! What's left is genuinely mission-agnostic and still has a real
//! consumer: [`RunEmitter`] (a `dyn`-safe sink trait a run driver emits
//! `darkmux_flow::FlowRecord`s through, so the SAME driver code can write
//! to a real sink or a suppressing no-op depending on what's injected) and
//! [`NullEmitter`] (the no-op default). `darkmux_lab::lab::review`
//! re-exports both under their pre-rename names (`pub use darkmux_crew::
//! run_obs::{RunEmitter as ReviewEmitter, NullEmitter}`), so existing
//! `impl ReviewEmitter for X` sites across the tree keep compiling
//! unchanged.

/// Sink for a run driver's observability records. The driver only knows
/// how to build [`darkmux_flow::FlowRecord`]s and hand them to `emit` — it
/// never decides where they land, which is what keeps a driver sink-
/// agnostic (the lab-vs-fleet scope boundary: a bench run's records stay
/// per-run-local; a live mission's ride the fleet stream — same driver
/// code, different injected [`RunEmitter`]).
///
/// **Not `Send`, and taken as `&mut dyn`.** A [`RunEmitter`] can only be
/// held and called from one thread at a time — there is no blanket `Sync`
/// bound and no interior locking. A driver whose steps genuinely run
/// concurrently (a `run_step_graph` wave dispatching several steps in
/// parallel worker threads) cannot hand each worker its own `&mut dyn
/// RunEmitter` — the scheduler drains each wave's worker results before
/// invoking its emit closure, so `emitter.emit(...)` is only ever called
/// single-threaded even though the STEPS it reports on ran concurrently.
/// The actual model-dispatch work happening inside those worker threads
/// still emits its own liveness bookends (contract 2) straight to the
/// global `darkmux_flow::record` sink, never through this trait — a
/// `RunEmitter` only ever reports what the main thread already knows.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_emitter_drops_every_record_without_panicking() {
        let mut emitter = NullEmitter;
        emitter.emit(darkmux_flow::FlowRecord {
            ts: darkmux_flow::ts_utc_now(),
            level: darkmux_flow::Level::Info,
            category: darkmux_flow::Category::Work,
            tier: darkmux_flow::Tier::Local,
            stage: darkmux_flow::Stage::Dispatch,
            action: "step result".to_string(),
            handle: "s1".to_string(),
            phase_id: None,
            session_id: None,
            source: None,
            model: None,
            reasoning: None,
            mission_id: None,
            machine_id: None,
            machine_uid: None,
            prev_hash: None,
            hash: None,
            payload: None,
            work_id: None,
            attempt: None,
        });
    }
}
