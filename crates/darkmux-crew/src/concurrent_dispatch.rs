//! Bounded concurrent-dispatch executor (#1230 Packet 1).
//!
//! Built directly on `darkmux_gestalt::planner::plan_waves` — the wave
//! scheduler ALREADY partitions N desired model placements into the
//! largest co-resident-safe sets that fit a byte budget, which is exactly
//! the RAM-safety mechanism a batch of ready-to-run local dispatches needs.
//! This module does not reinvent that arithmetic; it adds the missing
//! EXECUTION half: given a batch of jobs (each either bound to a local
//! model placement or unbound/remote), run them for real, honoring the
//! wave partitioning for local jobs and a separate concurrency cap for
//! remote ones.
//!
//! # Planning vs execution
//!
//! `plan_waves` is called EXACTLY ONCE per `run_bounded` call, synchronously,
//! before any job runs. `darkmux-gestalt` documents itself as having no
//! internal locking or cross-caller claim tracking — it is pure
//! snapshot-in/plan-out — so this executor never calls it concurrently from
//! multiple threads; planning is a fast one-shot batch step. EXECUTION of
//! the resulting waves is what's actually concurrent: every job in one wave
//! runs at once (gestalt has already judged that wave's placements safe to
//! co-reside), the executor waits for the whole wave to finish, then moves
//! to the next wave.
//!
//! # Technology: `std::thread::scope`, not tokio
//!
//! Every dispatch path in darkmux today is synchronous blocking I/O
//! (`std::process::Command`, blocking HTTP via `ureq`) and neither
//! `darkmux-crew` nor `darkmux-lab` depends on tokio. `std::thread::scope`
//! (stable since Rust 1.63, workspace MSRV 1.80 covers it) gives bounded,
//! borrow-checked concurrency for exactly this shape without pulling in an
//! async runtime — see this repo's CLAUDE.md dependency-discipline
//! convention ("a 10-line inline module beats a crate for small one-off
//! needs"). A panicking job unwinds its wave's inner `thread::scope` (which
//! re-panics on its own IMPLICIT join), which in turn unwinds the track
//! thread; `run_bounded` joins that track EXPLICITLY and reconciles the
//! panicked job into a terminal `Err` result for its index (#1452) rather
//! than letting the panic vanish and strand the job's Step `Running` — see
//! `run_bounded`'s reconcile step.
//!
//! # Local waves vs the remote batch
//!
//! Local jobs execute wave-by-wave: each wave IS the gestalt-computed "safe
//! to co-reside" set, run concurrently via a nested `thread::scope`: then
//! the executor moves to the next wave. Remote/hosted jobs aren't RAM-bound
//! (the #1177/#1260 residency-free design — a remote seat consumes zero
//! local pool), so they run in their OWN `remote_cap`-bounded batch,
//! **interleaved with the local wave track rather than blocked behind it**:
//! both tracks are spawned as sibling scoped threads inside one outer
//! `thread::scope`, so their wall-clock windows genuinely overlap.
//!
//! # Flow-record ordering under concurrency
//!
//! Each worker returns `(T, Vec<FlowRecord>)` rather than emitting flow
//! records directly. This is deliberate: the emitter/sink types the rest of
//! darkmux uses (`darkmux_flow::bookend::BookendGuard` and friends, #1230
//! Packet 0) are not `Send`/`Sync` — a worker thread cannot hold one. The
//! caller drains `run_bounded`'s returned `Vec` (already in COMPLETION
//! order — see below) and emits each job's records through its own
//! single-owned sink on the main thread as results land.
//!
//! # What this packet does NOT build
//!
//! No `Task`/`Step` schema and no dependency-graph scheduler — that is
//! Packet 2's `run_step_graph`, which this executor is the primitive
//! underneath. No CLI verb. Nothing here is wired into the review
//! yet (Packet 4) or `mission run` (Packet 3); this module has zero
//! production callers in this packet, matching how `darkmux-gestalt`
//! itself shipped as a fully-tested, uncalled crate ahead of its own
//! cutover.
//!
//! # Open item — `same_local_model` concurrent-request safety
//!
//! Whether ONE resident LMStudio/llama.cpp model can safely serve two
//! concurrent chat-completion requests is genuinely unknown (no evidence
//! either way has been gathered). `plan_waves` governs which MODELS are
//! resident, not how many concurrent requests one resident model may
//! safely take — that is orthogonal and out of scope here. Until an
//! empirical check runs, callers of this module should serialize requests
//! against the same resident model themselves (e.g. a mutex/counter keyed
//! by model identifier) rather than relying on this executor for that
//! guarantee; a wave with two placements that happen to share one
//! identifier still schedules both of that identifier's jobs into the SAME
//! wave (their `Placement`s collapse to one `Reuse` decision — see
//! `desired::ingest`'s dedup precedent) and this executor runs them
//! concurrently against it.

use anyhow::{anyhow, bail, Result};
use darkmux_flow::FlowRecord;
use darkmux_gestalt::{
    plan_acquire, Action, AcquireOpts, AcquireScope, CallerIntent, Facts, FootprintEstimator,
    ModelHost, Placement, ResourceProbe, WaveMode, WaveSchedule,
};
use darkmux_profiles::gestalt_host::{resolved_load_deadline, LmsHost, MacProbe};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// One job's completed outcome: its own value plus every flow record it
/// produced (see the module doc's "Flow-record ordering" section).
pub type JobOutcome<T> = Result<(T, Vec<FlowRecord>)>;

/// One dispatch job's body: runs to completion off the main thread and
/// returns a [`JobOutcome`].
pub type DispatchJob<T> = Box<dyn FnOnce() -> JobOutcome<T> + Send>;

/// The shared results collector every worker thread pushes into as its job
/// completes — factored into a named alias (clippy's `type_complexity`)
/// rather than spelled inline at every call site.
type ResultsSink<T> = Mutex<Vec<(usize, JobOutcome<T>)>>;

/// Where a job's model runs. `Local` names the exact
/// [`darkmux_gestalt::Placement`] gestalt's wave scheduler should reason
/// about for this job (the model it needs resident before it can run) —
/// callers building this from a crew staffing typically use the same
/// `model_key`/`identifier`/`min_ctx` they'd hand `ModelCycler`. `Remote`
/// jobs carry no placement: a hosted-endpoint seat consumes zero local pool
/// (#1177/#1260) and never reaches gestalt's planner.
pub enum Residency {
    Local(Placement),
    Remote,
}

/// One job queued for [`run_bounded`]. `index` is the CALLER's own
/// bookkeeping key (e.g. a future Step id's position) — results come back
/// tagged with it rather than assuming the job list itself is
/// index-addressable after it's been partitioned into local/remote tracks.
pub struct QueuedJob<T> {
    pub index: usize,
    pub residency: Residency,
    pub job: DispatchJob<T>,
}

/// The canonical production [`ModelHost`] factory for [`run_bounded`]'s
/// `host_factory` parameter — a real `LmsHost` per call. A plain `fn`, not a
/// closure, so callers pass it directly as `&concurrent_dispatch::
/// lms_host_factory` with no allocation. Tests exercising `run_bounded`'s
/// wave-partitioning logic in isolation (synthetic `Facts`/placements, no
/// real LMStudio intended) pass a `darkmux_gestalt::mock::MockHost`-backed
/// factory instead — see [`ensure_wave_loaded`]'s doc.
pub fn lms_host_factory() -> Box<dyn ModelHost> {
    Box::new(LmsHost::new())
}

/// Run `jobs` to completion, honoring gestalt's co-residency wave packing
/// for every [`Residency::Local`] job and a separate `remote_cap`-bounded
/// concurrent batch for every [`Residency::Remote`] job (see the module
/// doc). Returns one entry per job, in COMPLETION order (not input order —
/// the caller pairs a result back to its origin via the tagged `index`); a
/// local job whose placement `plan_waves` could never fit any wave (see
/// [`darkmux_gestalt::WaveRefusal`]) never runs at all and comes back as an
/// `Err` naming the refusal reason.
///
/// The top-level `Result` is reserved for a planning-stage failure (today,
/// realistically unreachable — `plan_waves` under [`WaveMode::Auto`] never
/// returns `Err`, that variant exists only for `WaveMode::ForceParallel`,
/// which this function does not use); a per-job failure is always carried
/// in that job's own `Result` slot in the returned `Vec`, never surfaced
/// here.
pub fn run_bounded<T: Send + 'static>(
    jobs: Vec<QueuedJob<T>>,
    facts: &Facts,
    est: &(dyn FootprintEstimator + Sync),
    remote_cap: usize,
    host_factory: &(dyn Fn() -> Box<dyn ModelHost> + Sync),
) -> Result<Vec<(usize, JobOutcome<T>)>> {
    // ── partition + stamp local placements with a job-unique seat label ──
    // `plan_waves` returns `Vec<Placement>` BY VALUE; two jobs wanting
    // byte-identical placements (same model/ctx/seat) would otherwise be
    // indistinguishable once scheduled. `seat` is documented
    // never-decision-bearing provenance (`darkmux_gestalt::desired`), so
    // stamping a job-unique suffix here cannot change what the planner
    // decides — only how this executor re-associates its own output.
    let mut local_by_seat: HashMap<String, (usize, DispatchJob<T>)> = HashMap::new();
    let mut placements: Vec<Placement> = Vec::new();
    let mut remote_jobs: Vec<(usize, DispatchJob<T>)> = Vec::new();

    // (#1452) Every queued index, captured BEFORE `jobs` is partitioned and
    // consumed below. A job whose body panics never pushes a result into
    // `results`; after both tracks join we reconcile any index absent from
    // `results` back to a terminal `Err` (see the join/reconcile block).
    let all_indices: Vec<usize> = jobs.iter().map(|q| q.index).collect();

    for q in jobs {
        match q.residency {
            Residency::Local(mut placement) => {
                placement.seat = format!("{}#job{}", placement.seat, q.index);
                local_by_seat.insert(placement.seat.clone(), (q.index, q.job));
                placements.push(placement);
            }
            Residency::Remote => remote_jobs.push((q.index, q.job)),
        }
    }

    // ── ONE synchronous planning call — see module doc ──
    let schedule = darkmux_gestalt::plan_waves(&placements, facts, est, WaveMode::Auto)
        .map_err(|e| anyhow!("darkmux: unexpected wave refusal under Auto mode: {e}"))?;

    let results: ResultsSink<T> = Mutex::new(Vec::new());

    std::thread::scope(|scope| {
        // Sibling scoped threads — genuinely interleaved wall-clock windows
        // (module doc: "interleaved with, never blocked behind").
        let local_track = (!local_by_seat.is_empty() || !schedule.refusals.is_empty())
            .then(|| scope.spawn(|| run_local_waves(schedule, local_by_seat, &results, est, host_factory)));
        let remote_track =
            (!remote_jobs.is_empty()).then(|| scope.spawn(|| run_remote_batches(remote_jobs, remote_cap.max(1), &results)));

        // (#1452) Join each track EXPLICITLY. A track thread panics when one
        // of its jobs panics — the job's own wave `thread::scope` re-panics
        // on its IMPLICIT end-of-block join, unwinding the track. Crucially,
        // an EXPLICIT `.join()` on a scoped handle does NOT re-propagate that
        // panic the way the outer scope's own implicit join would, so here we
        // deliberately absorb it (`let _ = h.join()`): a wave panic must not
        // abort the WHOLE batch and lose the OTHER jobs' already-pushed
        // terminal results. The panicked job left no result of its own, so we
        // reconcile it to a terminal `Err` right after the scope (below) —
        // the earlier code's claim that this join re-panicked "identical
        // behavior either way" was factually wrong (#1452), which is exactly
        // how a panicked job used to vanish and strand its Step `Running`.
        if let Some(h) = local_track {
            let _ = h.join();
        }
        if let Some(h) = remote_track {
            let _ = h.join();
        }
    });

    let mut results = results.into_inner().expect("no thread panicked while holding the results lock");

    // (#1452) Reconcile absent indices. On every NON-panic path each queued
    // job pushes exactly one result (normal completion, a wave-load failure,
    // or a co-residency refusal), so an index still missing here can only
    // mean its job PANICKED before pushing. Synthesize a terminal `Err` for
    // it: the caller (`scheduler::run_step_graph`) then flips that Step to
    // `Error` and persists it terminal through its ordinary per-job error
    // arm, so a job panic surfaces as an errored step AND an errored run
    // (contract 2, dispatch liveness — a terminal record on every exit path),
    // never a silent `Running` skip inside a run reported as success. Loud
    // beats quiet; the thread's default panic hook already printed the panic
    // payload to stderr.
    let seen: HashSet<usize> = results.iter().map(|(i, _)| *i).collect();
    for index in all_indices {
        if !seen.contains(&index) {
            results.push((
                index,
                Err(anyhow!(
                    "darkmux: a dispatch job panicked mid-wave and produced no terminal \
                     result (see stderr for the panic payload) — recorded as a step error so \
                     the run fails loud rather than stranding the step Running (#1452)"
                )),
            ));
        }
    }
    Ok(results)
}

/// The local track: walk `schedule.waves` in order, running every job in
/// one wave concurrently (a nested `thread::scope` per wave) before moving
/// to the next — the wave IS the "safe to co-reside" unit gestalt already
/// computed. `schedule.refusals` never run at all; each comes back as an
/// `Err` naming the refusal reason via `Reason`'s `Display`.
///
/// (#1360) Before dispatching a wave's jobs, [`ensure_wave_loaded`] makes
/// its placements ACTUALLY resident — `plan_waves` above only decided which
/// placements are SAFE to co-reside, it performs no I/O. A wave whose
/// load-ensure fails never dispatches at all; every job in it comes back as
/// the same attributable `Err` instead of each one independently hitting a
/// confusing "Invalid model identifier" from LMStudio's own auto-load
/// fallback (which cannot resolve darkmux's namespaced alias).
fn run_local_waves<T: Send + 'static>(
    schedule: WaveSchedule,
    mut by_seat: HashMap<String, (usize, DispatchJob<T>)>,
    results: &ResultsSink<T>,
    est: &(dyn FootprintEstimator + Sync),
    host_factory: &(dyn Fn() -> Box<dyn ModelHost> + Sync),
) {
    // One host instance for this whole local track — waves within it run
    // strictly sequentially (the `for wave in &schedule.waves` loop below),
    // so a single mutable host handles every `ensure_wave_loaded` call
    // safely without needing to reconstruct one per wave.
    let mut host = host_factory();
    for refusal in &schedule.refusals {
        if let Some((index, _job)) = by_seat.remove(&refusal.placement.seat) {
            results.lock().expect("results mutex poisoned").push((
                index,
                Err(anyhow!(
                    "darkmux: \"{}\" never fits a co-residency wave — {}",
                    refusal.placement.model_key,
                    refusal.reason
                )),
            ));
        }
    }
    for wave in &schedule.waves {
        if let Err(e) = ensure_wave_loaded(wave, est, host.as_mut()) {
            for placement in wave {
                if let Some((index, _job)) = by_seat.remove(&placement.seat) {
                    results.lock().expect("results mutex poisoned").push((
                        index,
                        Err(anyhow!("darkmux: could not load \"{}\" for this wave: {e:#}", placement.model_key)),
                    ));
                }
            }
            continue;
        }
        std::thread::scope(|wave_scope| {
            for placement in wave {
                let Some((index, job)) = by_seat.remove(&placement.seat) else { continue };
                wave_scope.spawn(move || {
                    let outcome = job();
                    results.lock().expect("results mutex poisoned").push((index, outcome));
                });
            }
        });
    }
}

/// Make every placement in one wave actually resident before its jobs
/// dispatch. `plan_waves` (called once, up front, in [`run_bounded`]) only
/// PARTITIONS placements into co-resident-safe groups — it performs no I/O.
/// Without this, a cold model's job silently depended on LMStudio's own
/// auto-load-on-request fallback, which only resolves BARE catalog
/// identifiers, never darkmux's namespaced alias — and fails loud with
/// "Invalid model identifier" the moment a wave's model isn't already warm
/// from some earlier, unrelated dispatch (#1360, reproduced live twice,
/// identically, via a 3-seat concurrent probe wave where one seat's model
/// was cold).
///
/// Facts are gathered FRESH here on every call — never reused from
/// [`run_bounded`]'s caller-supplied snapshot — because a later wave in the
/// same `run_bounded` invocation can only trust residency state as of right
/// before ITS OWN dispatch: an earlier wave's loads/unloads have already
/// changed what's actually resident by then. Mirrors
/// `darkmux_lab::lab::review::LmsCycler::ensure_loaded`'s per-call
/// fresh-facts discipline, generalized here to a whole wave's placements in
/// one `plan_acquire` call instead of one placement at a time.
///
/// `host` is CALLER-INJECTED (via `run_bounded`'s `host_factory`), never
/// constructed here — production passes a real `LmsHost`; hermetic tests of
/// `run_bounded`'s wave-partitioning logic (which construct `Facts`/
/// synthetic placements directly, never intending any real LMStudio
/// interaction) pass `darkmux_gestalt::mock::MockHost` instead. Matches
/// `plan_waves`/`plan_acquire` themselves already being pure snapshot-in
/// functions — this keeps the one place that DOES real host I/O equally
/// injectable rather than silently reaching past the caller's test double.
fn ensure_wave_loaded(
    placements: &[Placement],
    est: &(dyn FootprintEstimator + Sync),
    host: &mut dyn ModelHost,
) -> Result<()> {
    let residents = host
        .list_resident()
        .map_err(|e| anyhow!("darkmux: could not read LMStudio residents (`lms ps`): {e}"))?;
    let pools = MacProbe.pools().unwrap_or_default();
    let facts = Facts { residents, pools, ..Default::default() };
    let opts = AcquireOpts::new(CallerIntent::Auto, AcquireScope::Additive);
    // (#1442 ship-2b, found live) A wave's placements are per-STEP, and the
    // seats x k fan-out makes SAME-MODEL duplicates the norm (k sibling
    // `dispatch.map` steps all place the same model, differing only in
    // their `seat` provenance string). `plan_acquire` decides per
    // placement against one facts snapshot, so a duplicated placement
    // whose resident needs a stale-ctx reconcile would plan the SAME
    // unload+load once PER DUPLICATE — the second unload then hard-fails
    // with "not resident" and takes the whole wave down (reproduced live
    // on the first seats x k validation run). The loader's job is per
    // MODEL, not per step: collapse duplicates before planning, keeping
    // the MAX `min_ctx` across the duplicates so every sibling's need is
    // still satisfied by the one load.
    let mut unique: Vec<Placement> = Vec::with_capacity(placements.len());
    for p in placements {
        match unique
            .iter_mut()
            .find(|u| u.model_key == p.model_key && u.identifier == p.identifier)
        {
            Some(u) => u.min_ctx = u.min_ctx.max(p.min_ctx),
            None => unique.push(p.clone()),
        }
    }
    let plan = plan_acquire(&unique, &facts, opts, est);
    let deadline = resolved_load_deadline();
    for planned in &plan.actions {
        match &planned.action {
            Action::Reuse { .. } => {}
            Action::Unload { target } => {
                host.unload(target, deadline)
                    .map_err(|e| anyhow!("darkmux: unload failed for \"{}\": {e}", target.identifier()))?;
            }
            Action::Load { model_key, identifier, min_ctx } => {
                host.load(model_key, identifier, *min_ctx, deadline)
                    .map_err(|e| anyhow!("darkmux: load failed for \"{model_key}\" (\"{identifier}\"): {e}"))?;
            }
            Action::Block { model_key, .. } => {
                bail!("darkmux: cannot load \"{model_key}\" for this wave — {}", planned.reason)
            }
        }
    }
    Ok(())
}

/// The remote track: chunk `remote_jobs` into `cap`-sized batches (in input
/// order — no wave-style co-residency arithmetic applies to remote seats,
/// so a simple fixed-size batch is the whole mechanism) and run each batch
/// concurrently via a nested `thread::scope`, moving to the next batch once
/// the current one finishes. `cap` is the caller-resolved
/// `config_access::remote_concurrent_cap()`, already clamped to >= 1 by
/// [`run_bounded`] (a 0 cap would otherwise mean "run nothing, forever").
fn run_remote_batches<T: Send + 'static>(
    mut remote_jobs: Vec<(usize, DispatchJob<T>)>,
    cap: usize,
    results: &ResultsSink<T>,
) {
    for batch in remote_jobs.chunks_mut(cap.max(1)) {
        std::thread::scope(|batch_scope| {
            for (index, job) in batch {
                let index = *index;
                // `job` is `DispatchJob<T>` (owned `Box<dyn FnOnce +
                // Send>`) sitting behind a `&mut` chunk slot — `take()` its
                // place with a no-op so the closure can move the real one
                // into the spawned thread without fighting the borrow
                // checker over a `chunks_mut` slice element.
                let job: DispatchJob<T> = std::mem::replace(job, Box::new(|| unreachable!()));
                batch_scope.spawn(move || {
                    let outcome = job();
                    results.lock().expect("results mutex poisoned").push((index, outcome));
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use darkmux_gestalt::{mock::MockHost, Budget, FixedEstimator, ResidentFact};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    /// Hermetic `host_factory` for these fixtures — synthetic `Facts`/
    /// placements (fake model keys like "m"/"small-a") were never intended
    /// to touch a real LMStudio; `ensure_wave_loaded`'s host is injected
    /// (#1360 follow-up) specifically so this stays true.
    fn mock_host_factory() -> Box<dyn ModelHost> {
        Box::new(MockHost::new())
    }

    fn placement(model_key: &str, min_ctx: u32) -> Placement {
        Placement {
            model_key: model_key.to_string(),
            identifier: format!("darkmux:{model_key}"),
            min_ctx,
            seat: "probe".to_string(),
        }
    }

    fn ok_job(index: usize, marker: Arc<AtomicU32>) -> DispatchJob<usize> {
        Box::new(move || {
            marker.fetch_add(1, Ordering::SeqCst);
            Ok((index, vec![]))
        })
    }

    /// (#1442 ship-2b, reproduced live on the first seats x k validation
    /// run) A wave whose placements DUPLICATE one model (k sibling
    /// `dispatch.map` steps, distinct `seat` strings) while that model is
    /// resident at a STALE (too-small) ctx: the loader must reconcile the
    /// model ONCE — one unload, one load at the duplicates' MAX `min_ctx` —
    /// never once per duplicate (the second unload would hit the mock's
    /// enforced #1279 NotResident error, exactly the live failure).
    #[test]
    fn ensure_wave_loaded_collapses_duplicate_placements_before_planning() {
        let est = FixedEstimator(BTreeMap::from([("m".to_string(), 1_000u64)]));
        let mut host = MockHost::new()
            .resident("darkmux:m", "m", 32_000, Some(1_000))
            .cataloged("m", 1_000);
        let wave = vec![
            Placement { model_key: "m".into(), identifier: "darkmux:m".into(), min_ctx: 68_000, seat: "step:probe-0".into() },
            Placement { model_key: "m".into(), identifier: "darkmux:m".into(), min_ctx: 64_000, seat: "step:probe-1".into() },
        ];
        ensure_wave_loaded(&wave, &est, &mut host)
            .expect("duplicate placements reconcile once, never a second NotResident unload");

        let unloads: Vec<_> = host
            .ops
            .iter()
            .filter(|op| matches!(op, darkmux_gestalt::mock::HostOp::Unload { .. }))
            .collect();
        assert_eq!(unloads.len(), 1, "one unload for the stale resident: {:?}", host.ops);
        let loads: Vec<_> = host
            .ops
            .iter()
            .filter_map(|op| match op {
                darkmux_gestalt::mock::HostOp::Load { min_ctx, .. } => Some(*min_ctx),
                _ => None,
            })
            .collect();
        assert_eq!(loads, vec![68_000], "one load, at the duplicates' MAX min_ctx: {:?}", host.ops);
    }

    /// The plan sketch's headline test: `run_bounded` respects
    /// `plan_waves`'s own partitioning under a byte budget that fits two
    /// small models together but not a third — mirrors `waves.rs`'s own
    /// table tests (two-fit-together, third overflows to a second wave),
    /// just exercised through the executor instead of `plan_waves` directly.
    #[test]
    fn run_bounded_respects_wave_partitioning_under_budget() {
        let est = FixedEstimator(BTreeMap::from([
            ("small-a".to_string(), 10_000_000_000),
            ("small-b".to_string(), 10_000_000_000),
            ("big-c".to_string(), 10_000_000_000),
        ]));
        // Budget fits any two of the three (20GB) but not all three (30GB).
        let facts = Facts { budget: Budget { max_darkmux_bytes: Some(20_000_000_000) }, ..Default::default() };
        let marker = Arc::new(AtomicU32::new(0));
        let jobs = vec![
            QueuedJob { index: 0, residency: Residency::Local(placement("small-a", 8_000)), job: ok_job(0, marker.clone()) },
            QueuedJob { index: 1, residency: Residency::Local(placement("small-b", 8_000)), job: ok_job(1, marker.clone()) },
            QueuedJob { index: 2, residency: Residency::Local(placement("big-c", 8_000)), job: ok_job(2, marker.clone()) },
        ];
        let results = run_bounded(jobs, &facts, &est, 4, &mock_host_factory).expect("planning never fails under Auto");
        assert_eq!(results.len(), 3, "every job ran (none refused — pool-less budget-only case never blocks)");
        assert_eq!(marker.load(Ordering::SeqCst), 3, "every job's body actually executed");
        let mut indices: Vec<usize> = results.iter().map(|(i, _)| *i).collect();
        indices.sort_unstable();
        assert_eq!(indices, vec![0, 1, 2], "every original index is accounted for exactly once");
        for (_, r) in &results {
            assert!(r.is_ok(), "no job should fail in this fixture");
        }
    }

    /// A local job whose placement can never fit ANY wave (its estimate
    /// alone exceeds the whole budget) never runs — it comes back as an
    /// `Err` naming the refusal, and every OTHER job still completes.
    #[test]
    fn run_bounded_never_runs_a_refused_placement() {
        let est = FixedEstimator(BTreeMap::from([
            ("fits".to_string(), 5_000_000_000),
            ("too-big".to_string(), 50_000_000_000),
        ]));
        let facts = Facts { budget: Budget { max_darkmux_bytes: Some(10_000_000_000) }, ..Default::default() };
        let marker = Arc::new(AtomicU32::new(0));
        let jobs = vec![
            QueuedJob { index: 0, residency: Residency::Local(placement("fits", 8_000)), job: ok_job(0, marker.clone()) },
            QueuedJob { index: 1, residency: Residency::Local(placement("too-big", 8_000)), job: ok_job(1, marker.clone()) },
        ];
        let results = run_bounded(jobs, &facts, &est, 4, &mock_host_factory).expect("planning never fails under Auto");
        assert_eq!(results.len(), 2);
        assert_eq!(marker.load(Ordering::SeqCst), 1, "only the fitting job's body ran");
        let refused = results.iter().find(|(i, _)| *i == 1).expect("index 1 present");
        assert!(refused.1.is_err(), "the too-big placement is refused, never run");
        let fitting = results.iter().find(|(i, _)| *i == 0).expect("index 0 present");
        assert!(fitting.1.is_ok(), "the other job still completes");
    }

    /// Two jobs that want the SAME identifier collapse to one `Reuse`
    /// decision inside `plan_waves` (gestalt's own dedup/reuse semantics),
    /// and this executor still runs BOTH job bodies — the open
    /// `same_local_model` concurrency question the module doc names is
    /// exactly this shape; today the executor does not serialize them
    /// itself, matching the documented open item.
    #[test]
    fn run_bounded_runs_both_jobs_sharing_one_resident_placement() {
        let est = FixedEstimator::default();
        let facts = Facts {
            residents: vec![ResidentFact {
                identifier: "darkmux:shared".to_string(),
                model_key: "shared".to_string(),
                ctx: 32_000,
                est_bytes: Some(1_000),
            }],
            ..Default::default()
        };
        let marker = Arc::new(AtomicU32::new(0));
        let jobs = vec![
            QueuedJob { index: 0, residency: Residency::Local(placement("shared", 8_000)), job: ok_job(0, marker.clone()) },
            QueuedJob { index: 1, residency: Residency::Local(placement("shared", 8_000)), job: ok_job(1, marker.clone()) },
        ];
        let results = run_bounded(jobs, &facts, &est, 4, &mock_host_factory).expect("planning never fails under Auto");
        assert_eq!(results.len(), 2);
        assert_eq!(marker.load(Ordering::SeqCst), 2, "both jobs ran despite sharing one resident placement");
    }

    /// Remote jobs never touch `plan_waves`'s local-model arithmetic at
    /// all — an empty local set plus an unconfigured budget/catalog is a
    /// legal, always-fits input.
    #[test]
    fn run_bounded_runs_remote_jobs_capped_and_independent_of_local() {
        let est = FixedEstimator::default();
        let facts = Facts::default();
        let marker = Arc::new(AtomicU32::new(0));
        let jobs = (0..5)
            .map(|i| QueuedJob { index: i, residency: Residency::Remote, job: ok_job(i, marker.clone()) })
            .collect();
        let results = run_bounded(jobs, &facts, &est, 2, &mock_host_factory).expect("planning never fails under Auto");
        assert_eq!(results.len(), 5);
        assert_eq!(marker.load(Ordering::SeqCst), 5);
        let mut indices: Vec<usize> = results.iter().map(|(i, _)| *i).collect();
        indices.sort_unstable();
        assert_eq!(indices, vec![0, 1, 2, 3, 4]);
    }

    /// (#1452) A REMOTE job whose body PANICS must not vanish. Before the
    /// fix, the panicked job's wave scope re-panicked, its track thread
    /// unwound, and the outer `let _ = h.join()` discarded the panic — so the
    /// job's index came back ABSENT from `results`, which stranded its Step
    /// `Running` in a run the scheduler reported as success. Now the absent
    /// index is reconciled into a terminal `Err`, and a sibling job in the
    /// same batch still completes.
    #[test]
    fn run_bounded_reconciles_a_panicking_remote_job_to_a_terminal_error() {
        let est = FixedEstimator::default();
        let facts = Facts::default();
        let jobs: Vec<QueuedJob<()>> = vec![
            QueuedJob { index: 0, residency: Residency::Remote, job: Box::new(|| panic!("boom in a remote job")) },
            QueuedJob { index: 1, residency: Residency::Remote, job: Box::new(|| Ok(((), vec![]))) },
        ];
        let results = run_bounded(jobs, &facts, &est, 4, &mock_host_factory).expect("planning never fails under Auto");
        assert_eq!(results.len(), 2, "both indices accounted for — the panicked one is not dropped");
        let panicked = results.iter().find(|(i, _)| *i == 0).expect("index 0 present despite the panic");
        assert!(panicked.1.is_err(), "the panicked job's index comes back as a terminal Err");
        let survivor = results.iter().find(|(i, _)| *i == 1).expect("index 1 present");
        assert!(survivor.1.is_ok(), "a sibling job in the same batch still completes");
    }

    /// (#1452) The LOCAL-track twin of the remote panic test — a wave job
    /// unwinds through a different code path (the per-wave nested
    /// `thread::scope` inside `run_local_waves`), so the reconcile is proven
    /// on both tracks. The sibling local job in the same wave still completes.
    #[test]
    fn run_bounded_reconciles_a_panicking_local_job_to_a_terminal_error() {
        let est = FixedEstimator::default();
        let facts = Facts::default();
        let marker = Arc::new(AtomicU32::new(0));
        let jobs: Vec<QueuedJob<usize>> = vec![
            QueuedJob {
                index: 0,
                residency: Residency::Local(placement("m", 8_000)),
                job: Box::new(|| panic!("boom in a local wave job")),
            },
            QueuedJob { index: 1, residency: Residency::Local(placement("m2", 8_000)), job: ok_job(1, marker.clone()) },
        ];
        let results = run_bounded(jobs, &facts, &est, 4, &mock_host_factory).expect("planning never fails under Auto");
        assert_eq!(results.len(), 2, "both local indices accounted for — the panicked one is not dropped");
        let panicked = results.iter().find(|(i, _)| *i == 0).expect("index 0 present despite the panic");
        assert!(panicked.1.is_err(), "the panicked local job's index comes back as a terminal Err");
    }

    /// A job's own `Err` return (not a panic) is carried through untouched
    /// — the executor never masks or reclassifies a job's own failure.
    #[test]
    fn run_bounded_propagates_a_jobs_own_error() {
        let est = FixedEstimator::default();
        let facts = Facts::default();
        let jobs = vec![QueuedJob::<()> {
            index: 0,
            residency: Residency::Remote,
            job: Box::new(|| Err(anyhow!("boom"))),
        }];
        let results = run_bounded(jobs, &facts, &est, 4, &mock_host_factory).expect("planning never fails under Auto");
        assert_eq!(results.len(), 1);
        let (idx, outcome) = &results[0];
        assert_eq!(*idx, 0);
        assert!(outcome.as_ref().is_err_and(|e| e.to_string().contains("boom")));
    }
}
