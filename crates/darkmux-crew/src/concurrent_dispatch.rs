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
use crate::step_kinds::SeatClaim;
use darkmux_flow::FlowRecord;
use darkmux_gestalt::{
    plan_acquire, Action, AcquireOpts, AcquireScope, CallerIntent, Deadline, Facts,
    FootprintEstimator, HostError, ModelHost, Placement, Plan, Reason, ResourceProbe, WaveMode,
    WaveSchedule,
};
use darkmux_profiles::gestalt_host::{resolved_load_deadline, LmsHost, MacProbe};
use darkmux_types::residency_lease;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

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

/// Spawn a scoped worker thread that inherits the CURRENT thread's name
/// instead of `std::thread::scope`'s default unnamed. Purely a debugging
/// nicety in production (readable thread names in panics/backtraces) — but
/// load-bearing for the #2632 env-read audit: `darkmux_types::env_audit::
/// audit_env_read` attributes a read to `std::thread::current().name()`,
/// so an unnamed worker thread made every env read inside it unattributable
/// to the test that ultimately caused it (`scripts/env-audit-report.py`
/// used to silently drop those lines; it now treats an unattributable read
/// of a mutated key as a loud failure — see that script's module doc).
/// Proven case: a `procedural.shell` job reads
/// `DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS` three `thread::scope` hops below
/// the test's own thread (this module's outer scope spawn, then
/// [`run_local_waves`]'s per-wave `wave_scope.spawn` or
/// [`run_capped_batches`]'s `batch_scope.spawn`) — without name propagation
/// at every hop the read showed up as `<unnamed>` and a real race against
/// `bounded_command`'s two `DARKMUX_STEP_COMMAND_TIMEOUT_SECONDS`-mutating
/// tests was invisible to the audit (seven `scheduler.rs` tests were
/// unguarded readers of it).
pub(crate) fn spawn_scoped_named<'scope, 'env, F, T>(
    scope: &'scope std::thread::Scope<'scope, 'env>,
    f: F,
) -> std::thread::ScopedJoinHandle<'scope, T>
where
    F: FnOnce() -> T + Send + 'scope,
    T: Send + 'scope,
{
    let name = std::thread::current()
        .name()
        .unwrap_or("darkmux-worker")
        .to_string();
    std::thread::Builder::new()
        .name(name)
        .spawn_scoped(scope, f)
        .expect("darkmux: failed to spawn scoped worker thread")
}

/// One job queued for [`run_bounded`]. `index` is the CALLER's own
/// bookkeeping key (e.g. a future Step id's position) — results come back
/// tagged with it rather than assuming the job list itself is
/// index-addressable after it's been partitioned into the three tracks.
///
/// (#2394) `seat` was a two-variant `Residency` (`Local(Placement)` |
/// `Remote`) whose `Remote` arm was reached both by a genuine hosted
/// endpoint AND by every job that consumes no model at all, so a wave of
/// `procedural.shell` steps queued behind a cap meant for hosted endpoints.
/// It is now [`crate::step_kinds::SeatClaim`], the same exhaustive type the
/// `StepKind::seat` hook returns — one vocabulary from the kind's
/// declaration to this executor's partition, with no lossy step between.
pub struct QueuedJob<T> {
    pub index: usize,
    pub seat: SeatClaim,
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

/// Run `jobs` to completion on THREE independent, genuinely interleaved
/// tracks (#2394), one per seat class:
///
/// - [`SeatClaim::LocalModel`] — gestalt's co-residency wave packing.
/// - [`SeatClaim::RemoteEndpoint`] — a `remote_cap`-bounded concurrent
///   batch. Also where [`SeatClaim::LocalModelUnresolved`] lands: #1509's
///   fail-open behavior, unchanged, but the CALLER has already said so out
///   loud (see `scheduler::run_step_graph`'s classification block).
/// - [`SeatClaim::NoModel`] — a `dispatch_free_cap`-bounded concurrent
///   batch of its OWN. A step that speaks to no model has no business
///   queueing behind a cap that exists to protect a hosted endpoint's rate
///   limit; #2394 is what that cost live.
///
/// Returns one entry per job, in COMPLETION order (not input order —
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
    // (#2394) The concurrency ceiling for `SeatClaim::NoModel` jobs — the
    // caller-resolved `config_access::dispatch_free_concurrency()`. Clamped
    // to >= 1 here, same as `remote_cap` (a 0 cap would mean "run nothing,
    // forever").
    dispatch_free_cap: usize,
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
    // (#2394) The dispatch-free track. Its own vec, its own cap, its own
    // sibling thread — never merged into `remote_jobs`.
    let mut dispatch_free_jobs: Vec<(usize, DispatchJob<T>)> = Vec::new();

    // (#1452) Every queued index, captured BEFORE `jobs` is partitioned and
    // consumed below. A job whose body panics never pushes a result into
    // `results`; after both tracks join we reconcile any index absent from
    // `results` back to a terminal `Err` (see the join/reconcile block).
    let all_indices: Vec<usize> = jobs.iter().map(|q| q.index).collect();

    // (#2394) Exhaustive, and deliberately WITHOUT a `_` arm: a new
    // `SeatClaim` variant must fail to compile HERE, where the decision
    // about which track it runs on actually lives, rather than silently
    // inheriting whatever the catch-all happened to do. That silent
    // inheritance is the entire bug this replaced.
    for q in jobs {
        match q.seat {
            SeatClaim::LocalModel(mut placement) => {
                placement.seat = format!("{}#job{}", placement.seat, q.index);
                local_by_seat.insert(placement.seat.clone(), (q.index, q.job));
                placements.push(placement);
            }
            SeatClaim::RemoteEndpoint => remote_jobs.push((q.index, q.job)),
            // #1509's fail-open, unchanged: a local seat we could not place
            // still runs, just without a wave load or a residency lease. The
            // caller has already surfaced it loudly by the time it gets here.
            SeatClaim::LocalModelUnresolved { .. } => remote_jobs.push((q.index, q.job)),
            SeatClaim::NoModel => dispatch_free_jobs.push((q.index, q.job)),
        }
    }

    // ── ONE synchronous planning call — see module doc ──
    let schedule = darkmux_gestalt::plan_waves(&placements, facts, est, WaveMode::Auto)
        .map_err(|e| anyhow!("darkmux: unexpected wave refusal under Auto mode: {e}"))?;

    let results: ResultsSink<T> = Mutex::new(Vec::new());

    std::thread::scope(|scope| {
        // Sibling scoped threads — genuinely interleaved wall-clock windows
        // (module doc: "interleaved with, never blocked behind").
        let local_track = (!local_by_seat.is_empty() || !schedule.refusals.is_empty()).then(|| {
            spawn_scoped_named(scope, || run_local_waves(schedule, local_by_seat, &results, est, host_factory))
        });
        let remote_track = (!remote_jobs.is_empty())
            .then(|| spawn_scoped_named(scope, || run_capped_batches(remote_jobs, remote_cap.max(1), &results)));
        // (#2394) The third sibling. Same batching mechanism as the remote
        // track, a DIFFERENT cap — and running on its own thread means a
        // long dispatch-free wait never occupies a hosted-endpoint slot.
        let dispatch_free_track = (!dispatch_free_jobs.is_empty()).then(|| {
            spawn_scoped_named(scope, || run_capped_batches(dispatch_free_jobs, dispatch_free_cap.max(1), &results))
        });

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
        if let Some(h) = dispatch_free_track {
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
    // (#1487 PR2; same-process aggregation #2651) Held for this WHOLE local
    // track's lifetime — a normal return or a panic-unwind through this
    // function both run `Drop`, removing THIS guard's own contribution to
    // the process's residency lease (never the whole file — see
    // `residency_lease`'s module doc) so a concurrent darkmux command's own
    // reconcile no longer sees this track's models pinned. Only a hard
    // crash (SIGKILL) skips `Drop`, leaving the lease for the pid-liveness
    // sweep in `residency_lease::live_leased_models` to reclaim.
    // `ensure_wave_loaded` itself refreshes the LEASE CONTENT
    // (`lease_guard.write`) once per wave — passed down explicitly (#2651)
    // rather than resolved implicitly by pid, since ANOTHER local track
    // (a concurrent same-process dispatch) may hold its OWN `LeaseGuard`
    // at the same time; the on-disk file is the union of every live guard's
    // contribution, never a single wholesale overwrite.
    let lease_guard = residency_lease::LeaseGuard::acquire();
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
        if let Err(e) = ensure_wave_loaded(wave, est, host.as_mut(), &lease_guard) {
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
                spawn_scoped_named(wave_scope, move || {
                    let outcome = job();
                    results.lock().expect("results mutex poisoned").push((index, outcome));
                });
            }
        });
    }
}

/// (#1487 PR2) Bounded retry-hold for a wave blocked ONLY by a CONCURRENT
/// darkmux command's pinned (leased) resident — the addendum's
/// "hold-not-fail" feasibility contract, deliberately kept simple for v1
/// (see [`ensure_wave_loaded`]'s doc). This is NOT a scheduler: it is a
/// few short, bounded attempts so a same-machine overlap that is about to
/// free RAM (the other command finishing its own dispatch) has a real
/// chance to clear before this wave gives up. The full hold/serialize
/// admission scheduler (wait indefinitely, or admit other ready work
/// meanwhile) is deferred to a follow-up (named PR 3 in the #1487 arc).
const BLOCKED_BY_HOLDER_RETRY_ATTEMPTS: u32 = 3;
const BLOCKED_BY_HOLDER_RETRY_DELAY: Duration = Duration::from_millis(300);

/// One attempt's outcome from dispatching `plan.actions` to a real host —
/// distinguishes a planning-level refusal (never transient — see
/// [`ensure_wave_loaded`]) from a live host-call failure (POSSIBLY
/// transient when it is a resource shortfall a concurrent lease-holder
/// explains).
enum PlanExecOutcome {
    Loaded,
    /// `reason` is the TYPED `Reason`, not a rendered string (#2669) — so
    /// the caller can distinguish [`Reason::ClaimedResidentInsufficientCtx`]
    /// (the one Block reason that can genuinely resolve with time, since
    /// its cause is a live claim that may clear) from every other Block
    /// reason (unknown model key, a foreign duplicate over capacity, a
    /// load that alone exceeds the whole budget), none of which any amount
    /// of waiting ever fixes.
    Blocked { model_key: String, reason: Reason },
    HostFailed { detail: String, host_error: HostError },
}

/// Dispatch every action in `plan` to `host`, in order (the plan's own
/// free-then-load ordering contract — every Unload precedes every Load).
/// Stops at the first failure; a partially-executed plan on failure is the
/// same behavior `ensure_wave_loaded` always had (the caller's fresh-facts
/// re-plan on the next attempt/wave is the recovery path, not a rollback).
/// A `Block` mid-plan stops here too, same as a live host error — #2669/
/// #2672 made `Action::Block` reachable via `Reason::
/// ClaimedResidentInsufficientCtx` in the ORDINARY concurrent-session case
/// (not just a capacity/catalog error), widening this pre-existing
/// partial-execution exposure's practical blast radius; tracked as a
/// separate follow-up, #2674.
fn execute_plan(plan: &Plan, host: &mut dyn ModelHost, deadline: Deadline) -> PlanExecOutcome {
    for planned in &plan.actions {
        match &planned.action {
            Action::Reuse { .. } => {}
            Action::Unload { target } => {
                if let Err(host_error) = host.unload(target, deadline) {
                    return PlanExecOutcome::HostFailed {
                        detail: format!("unload failed for \"{}\": {host_error}", target.identifier()),
                        host_error,
                    };
                }
            }
            Action::Load { model_key, identifier, min_ctx } => {
                if let Err(host_error) = host.load(model_key, identifier, *min_ctx, deadline) {
                    return PlanExecOutcome::HostFailed {
                        detail: format!(
                            "load failed for \"{model_key}\" (\"{identifier}\"): {host_error}"
                        ),
                        host_error,
                    };
                }
            }
            Action::Block { model_key, .. } => {
                return PlanExecOutcome::Blocked {
                    model_key: model_key.clone(),
                    reason: planned.reason.clone(),
                };
            }
        }
    }
    PlanExecOutcome::Loaded
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
///
/// # Reconcile-to-need (#1487 PR2)
///
/// Scope was `AcquireScope::Additive` — load what this wave wants, touch
/// nothing else — which is exactly why residency only ever grew across
/// waves/phases/runs (the stale-orphan bug this PR fixes). It is now
/// `Exclusive`: pass 1 unloads darkmux-owned residents this wave does NOT
/// desire, before any load. Concurrency-safe via the residency-lease
/// registry (`darkmux_types::residency_lease`, #1487 PR2 part A): every
/// OTHER live holder's leased `darkmux:*` model ids — an OTHER PROCESS
/// AND a same-process sibling alike (#2663; `LeaseGuard::
/// all_live_leased_models`, never the bare `live_leased_models` free
/// function, which excludes only `own_pid` and so missed a live
/// same-process sibling entirely) — are read fresh on every attempt and
/// fed in as `AcquireOpts.pinned` (#1487 PR1) — `plan_acquire` never
/// pass-1-unloads a pinned resident as not-desired, always counts it
/// as occupied, and (#2669) never unloads it via the per-desired
/// `Reconcile` arm either — a pinned identifier that shares a model key
/// with a desired placement at insufficient context now `Block`s that
/// placement (`Reason::ClaimedResidentInsufficientCtx`) instead of being
/// unloaded-and-reloaded out from under the command that pinned it,
/// whether that pin comes from a different process or a sibling dispatch
/// in THIS one — see this function's own retry-hold section below for how
/// that specific Block still gets a bounded chance to clear rather than
/// failing the wave outright. This process's OWN lease is written/refreshed to
/// this wave's placements BEFORE planning, via the CALLER-SUPPLIED `lease`
/// guard's own [`residency_lease::LeaseGuard::write`] (#2651 — never the
/// old bare `write_lease` free function, which clobbered a concurrent
/// same-process holder's contribution), so OTHER commands protect them
/// symmetrically; [`run_local_waves`] holds the guard for the whole local
/// track's lifetime, so a normal return or a panic-unwind releases ONLY
/// this guard's own contribution (see `residency_lease`'s module doc for
/// how two concurrent same-process guards union rather than clobber), and
/// a hard crash leaves it for the pid-liveness sweep to reclaim.
///
/// # The honest ceiling — hold-not-fail, kept simple (v1)
///
/// The #1487 addendum's feasibility guarantee: a mission always completes
/// as long as every individual profile fits the machine alone, so RAM
/// pressure between CONCURRENT commands is a scheduling question, never a
/// mission failure. `plan_acquire` has no budget engaged on this path yet
/// (see the PR body — #1243 isn't wired to a live config source), so a
/// resource shortfall surfaces here as a live `HostError::InsufficientResources`
/// (the #1139 fast-fail) rather than a typed `Block`. Two cases:
///
///   - A live PINNED holder explains the shortfall → transient contention,
///     not a failure: [`BLOCKED_BY_HOLDER_RETRY_ATTEMPTS`] bounded retries
///     (the holder may free) before surfacing loudly, naming the pinned
///     model(s) — never evicting them.
///   - No pin explains it → this placement likely does not fit the machine
///     AT ALL, alone; retrying can never help, so this fails immediately.
///
/// A planning-level `Action::Block` is never retried EXCEPT for one reason:
/// [`Reason::ClaimedResidentInsufficientCtx`] (#2669) — a resident sharing
/// the desired model key at insufficient context, but already claimed by a
/// live pinned dispatch. Its cause is a live claim that may clear (the
/// pinning command finishing its own dispatch), so it gets the SAME bounded
/// hold-not-fail retry as the analogous `HostFailed`+pinned shortfall below,
/// never an eviction of the claimed resident. Every OTHER Block reason
/// (unknown model key, a foreign duplicate with no capacity, or — once
/// #1243 is wired — a single profile that exceeds the WHOLE budget) is
/// still never retried: no wait changes what `plan_acquire` already refused
/// to plan for those.
///
/// `pub(crate)` (#2628): [`crate::dispatch_reconciled`] calls this directly
/// for a single-placement "wave" of one, giving a standalone (non-graph,
/// non-wave) dispatch the SAME Exclusive-reconcile + lease-write regime a
/// mission/coder-phase/review step gets via [`run_local_waves`] — without
/// pulling in the wave-batch executor's own multi-placement machinery it
/// doesn't need. Safe ONLY for callers that are never one placement among
/// several CONCURRENT wave siblings: a caller whose seat is already
/// resolved via `resolve_local_seat` (i.e. is itself a `StepKind`) must
/// NEVER also call this — its residency is already reconciled by the wave
/// that placed it, and a second independent single-placement reconcile
/// here would see its concurrent siblings' models as "not desired" and
/// evict them. See `dispatch_reconciled`'s own module doc for the full
/// list of which callers this is and is not safe for.
///
/// `lease` (#2651) is the CALLER's own already-acquired
/// [`residency_lease::LeaseGuard`] — never acquired here, and never a bare
/// pid-keyed write. Threading the specific guard through explicitly is what
/// makes two concurrent callers in the SAME process (two ACP sessions each
/// running an ephemeral panel dispatch, say) safe: each writes through its
/// OWN guard, and the on-disk lease is the union of every currently-live
/// guard's contribution rather than a single caller-agnostic overwrite.
pub(crate) fn ensure_wave_loaded(
    placements: &[Placement],
    est: &(dyn FootprintEstimator + Sync),
    host: &mut dyn ModelHost,
    lease: &residency_lease::LeaseGuard,
) -> Result<()> {
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

    // (#1487 PR2; #2651) Write/refresh THIS guard's own lease contribution
    // BEFORE planning — the models this wave is actively dispatching to, so
    // a CONCURRENT darkmux command's own reconcile sees them as pinned as
    // early as possible. Wholesale overwrite of THIS guard's OWN
    // contribution, never a delta against its own last write
    // (`LeaseGuard::write`'s contract): `run_local_waves`'s wave loop only
    // reaches the NEXT wave after every job in THIS wave has completed (the
    // `thread::scope` join there), so there is no cross-wave overlap WITHIN
    // one local track to protect against. There CAN be a concurrent
    // SIBLING local track in the same process (a second ACP session's own
    // ephemeral dispatch, holding its own `LeaseGuard`) — `LeaseGuard::
    // write` unions this contribution with every other currently-live
    // guard's own, rather than overwriting the shared per-pid file
    // outright (#2651).
    let own_models: Vec<String> = unique.iter().map(|p| p.identifier.clone()).collect();
    lease.write(&own_models)
        .map_err(|e| anyhow!("darkmux: could not write this wave's residency lease: {e}"))?;

    let deadline = resolved_load_deadline();
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let residents = host
            .list_resident()
            .map_err(|e| anyhow!("darkmux: could not read LMStudio residents (`lms ps`): {e}"))?;
        let pools = MacProbe.pools().unwrap_or_default();
        let facts = Facts { residents, pools, ..Default::default() };

        // Every OTHER live holder's lease, read fresh every attempt (the
        // blocker this attempt is retrying past may free between
        // attempts) — other PROCESSES via `live_leased_models`, UNIONED
        // with every other SAME-PROCESS holder's own contribution,
        // excluding THIS guard's own token (#2663; see
        // `LeaseGuard::all_live_leased_models`'s doc for why a live
        // same-process sibling must be protected too, and why this
        // guard's own just-written contribution must not be).
        //
        // (#2672 MUST FIX 1) Excluded from that raw pinned set: any of
        // THIS wave's own desired identifiers where every OTHER live
        // holder currently claiming it is itself still only ACQUIRING
        // (never confirmed `loaded`) and this guard's own priority is the
        // lowest among them — see `identifiers_i_should_lead`'s doc for
        // the full mechanism. Without this, two concurrent waves that
        // both merely INTEND to acquire the same identifier each see the
        // OTHER as an already-claimed pin and both fail — a regression
        // from pre-#2669, where both simply succeeded. An identifier any
        // live holder has confirmed `loaded` (genuinely mid-generation)
        // is never excluded this way, regardless of priority — #2669's
        // protection against evicting a live sibling is untouched.
        let mut pinned = lease.all_live_leased_models();
        let led = lease.identifiers_i_should_lead(&own_models);
        pinned.retain(|id| !led.contains(id));
        let opts = AcquireOpts {
            pinned: pinned.clone(),
            ..AcquireOpts::new(CallerIntent::Auto, AcquireScope::Exclusive)
        };
        let plan = plan_acquire(&unique, &facts, opts, est);

        match execute_plan(&plan, host, deadline) {
            PlanExecOutcome::Loaded => {
                // (#2672) This wave's own models are now genuinely
                // resident — flip this guard's lease entry for them from
                // "acquiring" to "loaded" so a CONCURRENT sibling's own
                // `identifiers_i_should_lead` never treats this dispatch
                // as a mere racing acquirer once it starts generating.
                // Best-effort: a lease write failure here must not fail an
                // otherwise-successful load — the lease is a busy-overlay,
                // never the source of truth (see the module doc's "`lms
                // ps` stays the truth" section), so a stale "acquiring"
                // entry only costs a future sibling its leader-election
                // fast path, never correctness (#2669's own Block-on-claim
                // guard still protects a resident that fresh facts show is
                // actually loaded).
                if let Err(e) = lease.mark_loaded(&own_models) {
                    eprintln!(
                        "darkmux: could not mark this wave's residency lease as loaded (non-fatal): {e}"
                    );
                }
                return Ok(());
            }
            PlanExecOutcome::Blocked { model_key, reason } => {
                // (#2669; narrowed #2672 CONSIDER 3) The one Block reason
                // that can genuinely resolve with time: the claimed
                // resident's holder may finish and release its lease
                // before the next attempt — but ONLY when `clearable` is
                // true (the claim's origin is an external pin). A
                // `clearable: false` claim (a same-plan collision — this
                // SAME `desired` list already targeted the identical
                // stale resident via an earlier decision) can never
                // resolve by waiting: `plan_acquire` decides from one
                // fixed `facts` snapshot, so retrying with the identical
                // input regenerates the identical Block every time —
                // burning the whole retry budget on a deterministic
                // failure was the CONSIDER 3 finding this narrows. Same
                // bounded hold-not-fail budget as the analogous
                // HostFailed+pinned shortfall below for the clearable
                // case — never an unbounded wait, never an eviction of
                // the claimed resident either way.
                if matches!(reason, Reason::ClaimedResidentInsufficientCtx { clearable: true, .. })
                    && attempt < BLOCKED_BY_HOLDER_RETRY_ATTEMPTS
                {
                    std::thread::sleep(BLOCKED_BY_HOLDER_RETRY_DELAY);
                    continue;
                }
                bail!(
                    "darkmux: cannot load \"{model_key}\" for this wave after {attempt} \
                     attempt(s) — {reason}"
                );
            }
            PlanExecOutcome::HostFailed { detail, host_error } => {
                let insufficient = matches!(host_error, HostError::InsufficientResources { .. });
                if insufficient && !pinned.is_empty() {
                    if attempt < BLOCKED_BY_HOLDER_RETRY_ATTEMPTS {
                        std::thread::sleep(BLOCKED_BY_HOLDER_RETRY_DELAY);
                        continue;
                    }
                    bail!(
                        "darkmux: could not load for this wave after {attempt} attempt(s) — \
                         blocked by (a) concurrent live darkmux command holding {pinned:?} \
                         ({detail}); this is transient contention, not a capacity failure — \
                         retry once the other command frees its model (the full hold/serialize \
                         scheduler is deferred, #1487 PR3)"
                    );
                }
                bail!(
                    "darkmux: could not load for this wave: {detail} (no concurrent darkmux \
                     command has this RAM pinned — this placement likely does not fit the \
                     machine at all, on its own)"
                );
            }
        }
    }
}

/// The batching mechanism BOTH cap-bounded tracks use (#2394 — this was
/// `run_remote_batches` when the remote track was the only one): chunk
/// `jobs` into `cap`-sized batches (in input order — no wave-style
/// co-residency arithmetic applies to a seat with no local placement, so a
/// simple fixed-size batch is the whole mechanism) and run each batch
/// concurrently via a nested `thread::scope`, moving to the next batch once
/// the current one finishes.
///
/// `cap` is the caller-resolved ceiling for THAT track —
/// `config_access::remote_concurrent_cap()` for the hosted-endpoint track,
/// `config_access::dispatch_free_concurrency()` for the dispatch-free one —
/// already clamped to >= 1 by [`run_bounded`] (a 0 cap would otherwise mean
/// "run nothing, forever"). The two tracks share this code and share
/// nothing else: separate vecs, separate caps, separate threads.
fn run_capped_batches<T: Send + 'static>(
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
                spawn_scoped_named(batch_scope, move || {
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
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Barrier};
    use tempfile::TempDir;

    /// Conformance (#2666 CONSIDER 2): the bare
    /// `residency_lease::live_leased_models` free function excludes only
    /// `own_pid` — it is right for `LeaseGuard::all_live_leased_models`'s
    /// own internal use (which additionally unions in same-process
    /// siblings) and wrong for anything computing `AcquireOpts.pinned`
    /// directly, which is exactly the #2663 bug shape (a caller reaching
    /// past `all_live_leased_models` and missing same-process siblings).
    /// Scans this file's and `dispatch_reconciled.rs`'s own PRODUCTION
    /// source (everything before their `mod tests` boundary, with
    /// comments skipped) for a bare call to the free function — i.e. an
    /// occurrence of `live_leased_models(` not part of
    /// `all_live_leased_models(`. A future edit that reintroduces #2663 by
    /// wiring `pinned` straight to the free function fails this test
    /// before it ever gets to a real dispatch.
    #[test]
    fn no_production_pinned_computation_bypasses_all_live_leased_models() {
        for (name, src) in [
            ("concurrent_dispatch.rs", include_str!("concurrent_dispatch.rs")),
            ("dispatch_reconciled.rs", include_str!("dispatch_reconciled.rs")),
        ] {
            let production = src.split("\nmod tests").next().unwrap_or(src);
            for (line_no, line) in production.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue; // doc/line comments reference the free function by name
                }
                if let Some(idx) = line.find("live_leased_models(") {
                    let prefix = &line[..idx];
                    assert!(
                        prefix.ends_with("all_"),
                        "{name}:{}: production code calls the bare `live_leased_models` \
                         free function directly — every `AcquireOpts.pinned` computation \
                         must go through `LeaseGuard::all_live_leased_models` instead \
                         (#2663): {line:?}",
                        line_no + 1,
                    );
                }
            }
        }
    }

    /// Hermetic `host_factory` for these fixtures — synthetic `Facts`/
    /// placements (fake model keys like "m"/"small-a") were never intended
    /// to touch a real LMStudio; `ensure_wave_loaded`'s host is injected
    /// (#1360 follow-up) specifically so this stays true.
    fn mock_host_factory() -> Box<dyn ModelHost> {
        Box::new(MockHost::new())
    }

    /// (#1487 PR2) `ensure_wave_loaded` now writes/reads a residency lease
    /// on every call — every test that reaches it (directly, or via
    /// `run_bounded` with a `Residency::Local` job) MUST point
    /// `DARKMUX_HOME` at a throwaway tempdir, never the real
    /// `~/.darkmux/residency/`. Pair with `#[serial_test::serial]` on the
    /// test (env vars are process-global; `cargo test`'s default
    /// multi-threaded runner would otherwise race this against every
    /// other test in this file that also touches it).
    struct LeaseTestEnv {
        _tmp: TempDir,
        prev: Option<String>,
    }
    impl LeaseTestEnv {
        fn new() -> Self {
            let tmp = TempDir::new().expect("tempdir for DARKMUX_HOME");
            let prev = std::env::var("DARKMUX_HOME").ok();
            unsafe { std::env::set_var("DARKMUX_HOME", tmp.path()) };
            Self { _tmp: tmp, prev }
        }
        fn home(&self) -> &Path {
            self._tmp.path()
        }
    }
    impl Drop for LeaseTestEnv {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("DARKMUX_HOME", v),
                    None => std::env::remove_var("DARKMUX_HOME"),
                }
            }
        }
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
    #[serial_test::serial]
    #[test]
    fn ensure_wave_loaded_collapses_duplicate_placements_before_planning() {
        let _env = LeaseTestEnv::new();
        let est = FixedEstimator(BTreeMap::from([("m".to_string(), 1_000u64)]));
        let mut host = MockHost::new()
            .resident("darkmux:m", "m", 32_000, Some(1_000))
            .cataloged("m", 1_000);
        let wave = vec![
            Placement { model_key: "m".into(), identifier: "darkmux:m".into(), min_ctx: 68_000, seat: "step:probe-0".into() },
            Placement { model_key: "m".into(), identifier: "darkmux:m".into(), min_ctx: 64_000, seat: "step:probe-1".into() },
        ];
        let lease_guard = residency_lease::LeaseGuard::acquire();
        ensure_wave_loaded(&wave, &est, &mut host, &lease_guard)
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

    /// (#1487 PR2) The headline orphan-eviction fix: a darkmux-owned
    /// resident NOT in this wave's desired set is unloaded (Exclusive
    /// scope's pass 1) rather than left to grow residency forever
    /// (Additive's old behavior). The desired model still loads.
    #[serial_test::serial]
    #[test]
    fn ensure_wave_loaded_evicts_a_darkmux_owned_orphan_not_in_the_desired_set() {
        let _env = LeaseTestEnv::new();
        let est = FixedEstimator(BTreeMap::from([("m".to_string(), 1_000u64)]));
        let mut host = MockHost::new()
            .resident("darkmux:orphan", "orphan", 32_000, Some(1_000))
            .cataloged("m", 1_000);
        let wave = vec![placement("m", 8_000)];

        let lease_guard = residency_lease::LeaseGuard::acquire();
        ensure_wave_loaded(&wave, &est, &mut host, &lease_guard).expect("the wanted model loads");

        assert_eq!(
            host.ops,
            vec![
                darkmux_gestalt::mock::HostOp::ListResident,
                darkmux_gestalt::mock::HostOp::Unload { identifier: "darkmux:orphan".to_string() },
                darkmux_gestalt::mock::HostOp::Load {
                    model_key: "m".to_string(),
                    identifier: "darkmux:m".to_string(),
                    min_ctx: 8_000,
                },
            ],
            "the stale orphan is evicted (pass 1) before the desired model loads"
        );
    }

    /// (#1487 PR2) The concurrency-safety delta test: the SAME orphan as
    /// above, but a DIFFERENT live darkmux process has it leased (pinned).
    /// It must survive — never evicted out from under a command actively
    /// dispatching to it — while the wave's own desired model still loads.
    #[serial_test::serial]
    #[test]
    fn ensure_wave_loaded_never_evicts_a_model_pinned_by_a_live_external_lease() {
        let env = LeaseTestEnv::new();
        // A genuinely live OTHER process (this test's own pid would be
        // excluded as "own" by the `live_leased_models(self.pid)` read
        // inside `LeaseGuard::all_live_leased_models` — the lease must
        // belong to some OTHER live pid to prove the cross-process path).
        let mut holder = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawning a short-lived holder process");
        let holder_pid = holder.id();
        let residency_dir = env.home().join("residency");
        std::fs::create_dir_all(&residency_dir).unwrap();
        std::fs::write(
            residency_dir.join(format!("{holder_pid}.lease")),
            format!(r#"{{"pid":{holder_pid},"models":["darkmux:orphan"]}}"#),
        )
        .expect("hand-writing a lease for the external holder pid");

        let est = FixedEstimator(BTreeMap::from([("m".to_string(), 1_000u64)]));
        let mut host = MockHost::new()
            .resident("darkmux:orphan", "orphan", 32_000, Some(1_000))
            .cataloged("m", 1_000);
        let wave = vec![placement("m", 8_000)];

        let lease_guard = residency_lease::LeaseGuard::acquire();
        ensure_wave_loaded(&wave, &est, &mut host, &lease_guard).expect("the wanted model loads");

        let unloads: Vec<_> = host
            .ops
            .iter()
            .filter(|op| matches!(op, darkmux_gestalt::mock::HostOp::Unload { .. }))
            .collect();
        assert!(unloads.is_empty(), "a pinned resident must never be evicted: {:?}", host.ops);
        assert!(
            host.residents.iter().any(|r| r.identifier == "darkmux:orphan"),
            "the pinned orphan must still be resident afterward: {:?}",
            host.residents
        );
        assert!(
            host.ops.iter().any(|op| matches!(
                op,
                darkmux_gestalt::mock::HostOp::Load { identifier, .. } if identifier == "darkmux:m"
            )),
            "the wave's own desired model still loads: {:?}",
            host.ops
        );

        let _ = holder.kill();
        let _ = holder.wait();
    }

    // ── #2669: the Reconcile arm must honor a pin too ────────────────────
    //
    // #1487/#2663 closed the pass-1 not-desired eviction hazard for a
    // pinned resident, same-process and cross-process alike — but
    // `plan_acquire`'s per-desired `Reconcile` arm (unload + reload at a
    // higher context for the SAME model key) never consulted `pinned` at
    // all. The exact issue repro: a sibling guard holds `["darkmux:m"]`,
    // the host has `darkmux:m` resident at ctx 32_000, and this wave wants
    // `placement("m", 68_000)` — pre-fix this produced `[ListResident,
    // Unload { identifier: "darkmux:m" }, Load { model_key: "m",
    // identifier: "darkmux:m", min_ctx: 68_000 }]`, killing the sibling's
    // in-flight dispatch mid-generation.

    /// RED-PROVE (same-process): a live same-process sibling's lease pins
    /// "darkmux:m" — resident at ctx 32_000 — while this wave wants it at
    /// 68_000. Must Block, never Unload.
    ///
    /// The sibling calls `mark_loaded` right after `write` (#2672): this
    /// scenario means "the sibling is genuinely MID-GENERATION on
    /// darkmux:m" (its own doc: "the pinned sibling's model must still be
    /// resident... afterward"), never "the sibling is ALSO still racing to
    /// acquire it" — MUST FIX 1's leader/follower exclusion applies only
    /// to the latter, so this red-prove must keep blocking regardless of
    /// which guard's token happens to be lower.
    #[serial_test::serial]
    #[test]
    fn ensure_wave_loaded_never_reconciles_over_a_live_same_process_sibling_at_insufficient_ctx_2669(
    ) {
        let _env = LeaseTestEnv::new();

        let sibling_guard = residency_lease::LeaseGuard::acquire();
        sibling_guard.write(&["darkmux:m".to_string()]).expect("sibling writes its lease");
        sibling_guard
            .mark_loaded(&["darkmux:m".to_string()])
            .expect("sibling confirms it is genuinely resident, not merely acquiring");

        let est = FixedEstimator(BTreeMap::from([("m".to_string(), 1_000u64)]));
        let mut host = MockHost::new()
            .resident("darkmux:m", "m", 32_000, Some(1_000))
            .cataloged("m", 1_000);
        let wave = vec![placement("m", 68_000)];

        let own_guard = residency_lease::LeaseGuard::acquire();
        let err = ensure_wave_loaded(&wave, &est, &mut host, &own_guard)
            .expect_err("a live sibling's model at insufficient ctx must Block, not reconcile");

        assert!(
            host.ops.iter().all(|op| !matches!(op, darkmux_gestalt::mock::HostOp::Unload { .. })),
            "the pinned sibling's model must never be unloaded: {:?}",
            host.ops
        );
        assert!(
            host.residents.iter().any(|r| r.identifier == "darkmux:m" && r.ctx == 32_000),
            "the pinned sibling's model must still be resident, at its original ctx, afterward: \
             {:?}",
            host.residents
        );
        assert!(err.to_string().contains("darkmux:m"), "the error names the claimed model: {err:#}");

        drop(sibling_guard);
        drop(own_guard);
    }

    /// RED-PROVE (cross-process): the identical shape, but the pin comes
    /// from a hand-written `<pid>.lease` file belonging to a genuinely
    /// different, live process (never `ACTIVE_LEASES` — this exercises the
    /// `live_leased_models` half of `all_live_leased_models`'s union, not
    /// the same-process half the test above exercises). Must Block, never
    /// Unload — the SAME outcome, proving the fix is process-shape-agnostic.
    ///
    /// The hand-written lease names `darkmux:m` in BOTH `models` AND
    /// `loaded` (#2672): this scenario is "the external process is
    /// genuinely mid-generation," never "the external process is ALSO
    /// still racing to acquire it" — the latter is what MUST FIX 1's
    /// leader/follower exclusion is FOR, and a foreign pid is otherwise
    /// unpredictable relative to this test process's own pid (a real
    /// spawned child), so this must stay pinned regardless of which side
    /// happens to have the lower pid.
    #[serial_test::serial]
    #[test]
    fn ensure_wave_loaded_never_reconciles_over_a_live_external_process_lease_at_insufficient_ctx_2669(
    ) {
        let env = LeaseTestEnv::new();
        let mut holder = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawning a short-lived holder process");
        let holder_pid = holder.id();
        let residency_dir = env.home().join("residency");
        std::fs::create_dir_all(&residency_dir).unwrap();
        std::fs::write(
            residency_dir.join(format!("{holder_pid}.lease")),
            format!(r#"{{"pid":{holder_pid},"models":["darkmux:m"],"loaded":["darkmux:m"]}}"#),
        )
        .expect("hand-writing a lease for the external holder pid");

        let est = FixedEstimator(BTreeMap::from([("m".to_string(), 1_000u64)]));
        let mut host = MockHost::new()
            .resident("darkmux:m", "m", 32_000, Some(1_000))
            .cataloged("m", 1_000);
        let wave = vec![placement("m", 68_000)];

        let lease_guard = residency_lease::LeaseGuard::acquire();
        let err = ensure_wave_loaded(&wave, &est, &mut host, &lease_guard).expect_err(
            "a live EXTERNAL process's pinned model at insufficient ctx must Block, not reconcile",
        );

        assert!(
            host.ops.iter().all(|op| !matches!(op, darkmux_gestalt::mock::HostOp::Unload { .. })),
            "the externally-pinned model must never be unloaded: {:?}",
            host.ops
        );
        assert!(
            host.residents.iter().any(|r| r.identifier == "darkmux:m" && r.ctx == 32_000),
            "the externally-pinned model must still be resident, at its original ctx, afterward: \
             {:?}",
            host.residents
        );
        assert!(err.to_string().contains("darkmux:m"), "the error names the claimed model: {err:#}");

        let _ = holder.kill();
        let _ = holder.wait();
    }

    /// INVERTED direction: the identical stale-ctx resident, but nothing
    /// has it pinned or leased — this must still reconcile normally (the
    /// #1135 class the Reconcile arm exists to close), never Block just
    /// because the #2669 guard now exists.
    #[serial_test::serial]
    #[test]
    fn ensure_wave_loaded_still_reconciles_an_unpinned_stale_resident_2669() {
        let _env = LeaseTestEnv::new();
        let est = FixedEstimator(BTreeMap::from([("m".to_string(), 1_000u64)]));
        let mut host = MockHost::new()
            .resident("darkmux:m", "m", 32_000, Some(1_000))
            .cataloged("m", 1_000);
        let wave = vec![placement("m", 68_000)];

        let lease_guard = residency_lease::LeaseGuard::acquire();
        ensure_wave_loaded(&wave, &est, &mut host, &lease_guard)
            .expect("an unpinned stale resident still reconciles");

        assert_eq!(
            host.ops,
            vec![
                darkmux_gestalt::mock::HostOp::ListResident,
                darkmux_gestalt::mock::HostOp::Unload { identifier: "darkmux:m".to_string() },
                darkmux_gestalt::mock::HostOp::Load {
                    model_key: "m".to_string(),
                    identifier: "darkmux:m".to_string(),
                    min_ctx: 68_000,
                },
            ],
            "the #2669 guard must not block a routine, un-pinned reconcile"
        );
    }

    /// The retry-hold half of #2669: a Block from `Reason::
    /// ClaimedResidentInsufficientCtx` gets the SAME bounded hold-not-fail
    /// retry as the analogous `HostFailed`+pinned shortfall
    /// (`ensure_wave_loaded_retries_past_a_transient_shortfall_when_a_holder_is_pinned`
    /// below) rather than failing the wave on the very first Block — a
    /// sibling that holds its claim for the WHOLE test (never drops) must
    /// still see `BLOCKED_BY_HOLDER_RETRY_ATTEMPTS` attempts (one
    /// `ListResident` per attempt — the claimed resident is never touched)
    /// before failing loud, naming the claimed model.
    #[serial_test::serial]
    #[test]
    fn ensure_wave_loaded_retries_a_permanently_claimed_reconcile_before_failing_loud_2669() {
        let _env = LeaseTestEnv::new();

        // (#2672) `mark_loaded` after `write`: a "permanently claimed"
        // sibling is genuinely mid-generation, never merely racing to
        // acquire — MUST FIX 1's leader/follower exclusion must not apply
        // here regardless of token order, so this retries-then-fails-loud
        // guarantee must keep holding.
        let sibling_guard = residency_lease::LeaseGuard::acquire();
        sibling_guard.write(&["darkmux:m".to_string()]).expect("sibling writes its lease");
        sibling_guard
            .mark_loaded(&["darkmux:m".to_string()])
            .expect("sibling confirms it is genuinely resident, not merely acquiring");

        let est = FixedEstimator(BTreeMap::from([("m".to_string(), 1_000u64)]));
        let mut host = MockHost::new()
            .resident("darkmux:m", "m", 32_000, Some(1_000))
            .cataloged("m", 1_000);
        let wave = vec![placement("m", 68_000)];

        let own_guard = residency_lease::LeaseGuard::acquire();
        let err = ensure_wave_loaded(&wave, &est, &mut host, &own_guard)
            .expect_err("a permanently claimed resident never becomes reconcilable");

        let list_attempts = host
            .ops
            .iter()
            .filter(|op| matches!(op, darkmux_gestalt::mock::HostOp::ListResident))
            .count();
        assert_eq!(
            list_attempts as u32, BLOCKED_BY_HOLDER_RETRY_ATTEMPTS,
            "the claimed-reconcile Block retries the same bounded budget as the analogous \
             HostFailed+pinned shortfall before giving up: {:?}",
            host.ops
        );
        assert!(
            host.ops.iter().all(|op| !matches!(op, darkmux_gestalt::mock::HostOp::Unload { .. })),
            "the claimed resident must never be unloaded across any attempt: {:?}",
            host.ops
        );
        assert!(err.to_string().contains("darkmux:m"), "{err:#}");

        drop(sibling_guard);
        drop(own_guard);
    }

    // ── #2672 MUST FIX 1: two racing ACQUIRERS must not mutually Block ──
    //
    // The regression a reviewer's adversarial probe proved live: #2669
    // (above) correctly Blocks a Reconcile that would evict an ALREADY-
    // LOADED sibling — but its guard also fired for two waves that BOTH
    // merely INTEND to acquire the identical identifier, neither one
    // loaded yet. Both write their lease before planning (existing #1487/
    // #2651 discipline), each then sees the OTHER as an already-claimed
    // pin, and both hit `Reason::ClaimedResidentInsufficientCtx` — where
    // pre-#2669 both had simply succeeded. `identifiers_i_should_lead`
    // (#2672) closes this: exactly one racing guard (the lowest-priority
    // live holder) excludes the identifier from its own `pinned` set and
    // proceeds with the real reconcile; the other keeps it pinned and
    // takes the existing bounded retry-hold path, converging to a plain
    // `Reuse` once the leader's reconcile lands.

    /// A [`ModelHost`] both probe threads share, wrapping one [`MockHost`]
    /// behind a mutex — the faithful model of production: two concurrent
    /// `ensure_wave_loaded` callers (two ACP sessions, two `darkmux
    /// dispatch` processes) each hold their OWN `LmsHost`/`Box<dyn
    /// ModelHost>`, but both ultimately talk to the SAME real LMStudio
    /// server. Two independent, unsynchronized `MockHost` instances would
    /// miss that: the follower's retry-and-reuse convergence only works
    /// because its `list_resident()` call can actually observe the
    /// leader's completed reconcile.
    #[derive(Clone)]
    struct SharedMockHost(Arc<Mutex<MockHost>>);

    impl ModelHost for SharedMockHost {
        fn list_resident(&mut self) -> std::result::Result<Vec<ResidentFact>, HostError> {
            self.0.lock().expect("shared mock host mutex poisoned").list_resident()
        }
        fn list_catalog(&mut self) -> std::result::Result<Vec<darkmux_gestalt::CatalogFact>, HostError> {
            self.0.lock().expect("shared mock host mutex poisoned").list_catalog()
        }
        fn load(
            &mut self,
            model_key: &str,
            identifier: &str,
            min_ctx: u32,
            deadline: Deadline,
        ) -> std::result::Result<darkmux_gestalt::LoadReport, HostError> {
            self.0
                .lock()
                .expect("shared mock host mutex poisoned")
                .load(model_key, identifier, min_ctx, deadline)
        }
        fn unload(
            &mut self,
            target: &darkmux_gestalt::OwnedTarget,
            deadline: Deadline,
        ) -> std::result::Result<(), HostError> {
            self.0.lock().expect("shared mock host mutex poisoned").unload(target, deadline)
        }
    }

    /// RED-PROVE, the reviewer's exact probe: two threads, a barrier
    /// AFTER each writes its own lease for `["darkmux:m"]` (forcing both
    /// to be symmetrically visible to each other at planning time — the
    /// worst case), a shared host with `darkmux:m` resident at ctx
    /// 32_000, and both waves wanting it at 68_000. Five runs, both
    /// siblings must succeed on EVERY run — pre-#2672 (the #2669 guard
    /// with no leader/follower distinction) this failed at least one side
    /// on every run (`A ERR B ERR`, `A ERR B OK`, `B ERR A OK` ×2, `B ERR
    /// A ERR` in the reviewer's own five); disabling the #2669 guard
    /// entirely (the pre-#2669 baseline) gave `A OK B OK` every time —
    /// this test proves #2672 restores that same "both OK" outcome
    /// WITHOUT disabling the #2669 protection (the sibling-mid-generation
    /// red-proves above still pass unmodified).
    #[serial_test::serial]
    #[test]
    fn two_concurrent_same_process_acquirers_of_the_same_identifier_both_succeed_2672() {
        for run in 0..5 {
            let _env = LeaseTestEnv::new();
            let shared_host =
                SharedMockHost(Arc::new(Mutex::new(MockHost::new()
                    .resident("darkmux:m", "m", 32_000, Some(1_000))
                    .cataloged("m", 1_000))));
            let est = FixedEstimator(BTreeMap::from([("m".to_string(), 1_000u64)]));
            let wave = vec![placement("m", 68_000)];

            // Both guards acquired BEFORE spawning, so token order (and
            // therefore which side leads) is fixed per run but not
            // predetermined across runs — either side may end up the
            // leader; the assertion is symmetric on purpose.
            let guard_a = residency_lease::LeaseGuard::acquire();
            let guard_b = residency_lease::LeaseGuard::acquire();

            // The reviewer's exact setup: each side writes its OWN lease
            // for `["darkmux:m"]` FIRST — `ensure_wave_loaded` would do
            // this same write as its own first action, so doing it here
            // ahead of time (its own internal write then just repeats the
            // identical content, a no-op difference) lets a barrier force
            // the deterministic worst case: BOTH writes are guaranteed
            // visible to EACH OTHER before either side's first planning
            // pass, every run — never left to chance on which thread the
            // OS schedules first.
            let own_models = vec!["darkmux:m".to_string()];
            guard_a.write(&own_models).expect("A writes its lease");
            guard_b.write(&own_models).expect("B writes its lease");
            let barrier = Barrier::new(2);

            let result = std::thread::scope(|scope| {
                let host_a = shared_host.clone();
                let host_b = shared_host.clone();
                let wave_a = wave.clone();
                let wave_b = wave.clone();
                let est_ref = &est;
                let barrier_ref = &barrier;
                let guard_a_ref = &guard_a;
                let guard_b_ref = &guard_b;

                let handle_a = scope.spawn(move || {
                    let mut host_a = host_a;
                    barrier_ref.wait();
                    ensure_wave_loaded(&wave_a, est_ref, &mut host_a, guard_a_ref)
                });
                let handle_b = scope.spawn(move || {
                    let mut host_b = host_b;
                    barrier_ref.wait();
                    ensure_wave_loaded(&wave_b, est_ref, &mut host_b, guard_b_ref)
                });

                (handle_a.join().expect("thread A joins"), handle_b.join().expect("thread B joins"))
            });

            drop(guard_a);
            drop(guard_b);

            let (a, b) = result;
            assert!(a.is_ok(), "run {run}: sibling A must succeed: {:?}", a.err());
            assert!(b.is_ok(), "run {run}: sibling B must succeed: {:?}", b.err());

            let residents = shared_host.0.lock().expect("shared mock host mutex poisoned").residents.clone();
            assert!(
                residents
                    .iter()
                    .any(|r| r.identifier == "darkmux:m" && r.ctx >= 68_000),
                "run {run}: darkmux:m must end up resident at a sufficient ctx: {residents:?}"
            );
        }
    }

    // ── #2663: same-process sibling protection ──────────────────────────
    //
    // #2651 made the on-disk `<pid>.lease` file the correct union of every
    // concurrent in-process `LeaseGuard`'s own contribution, but
    // `ensure_wave_loaded`'s `pinned` set was still computed from
    // `residency_lease::live_leased_models(own_pid)` alone, which EXCLUDES
    // `own_pid` by construction — so a live SAME-PROCESS sibling holder's
    // model (correctly present in the on-disk union) was never actually
    // read back by this process's own reconcile. The reviewer's exact
    // repro (#2662): a sibling guard writes `darkmux:sibling`, then
    // `ensure_wave_loaded` for a different model in the SAME process
    // produces `[ListResident, Unload { identifier: "darkmux:sibling" },
    // Load { .. }]` — the sibling's model is evicted mid-generation by a
    // plan the same process itself generated.

    /// RED-PROVE (reviewer's exact probe, #2662): a live same-process
    /// sibling `LeaseGuard` (still held, still resident) must never be
    /// evicted by a DIFFERENT guard's `ensure_wave_loaded` reconcile
    /// running in the same process. Pre-fix, this fails with exactly the
    /// reviewer's repro — `host.ops` contains `Unload { identifier:
    /// "darkmux:sibling" }`.
    #[serial_test::serial]
    #[test]
    fn ensure_wave_loaded_never_evicts_a_live_same_process_sibling_lease() {
        let _env = LeaseTestEnv::new();

        // The sibling: a second concurrent local track in the SAME
        // process (same pid), holding its own guard for a DIFFERENT
        // model, still live (not dropped) for the whole test.
        let sibling_guard = residency_lease::LeaseGuard::acquire();
        sibling_guard.write(&["darkmux:sibling".to_string()]).expect("sibling writes its lease");

        let est = FixedEstimator(BTreeMap::from([("m".to_string(), 1_000u64)]));
        let mut host = MockHost::new()
            .resident("darkmux:sibling", "sibling", 32_000, Some(1_000))
            .cataloged("m", 1_000);
        let wave = vec![placement("m", 8_000)];

        // A SEPARATE guard — this call's own local track, reconciling for
        // a completely different model, in the SAME process as the
        // sibling above.
        let own_guard = residency_lease::LeaseGuard::acquire();
        ensure_wave_loaded(&wave, &est, &mut host, &own_guard).expect("the wanted model loads");

        let unloads: Vec<_> = host
            .ops
            .iter()
            .filter(|op| matches!(op, darkmux_gestalt::mock::HostOp::Unload { .. }))
            .collect();
        assert!(
            unloads.is_empty(),
            "a live SAME-PROCESS sibling's model must never be evicted by this process's own \
             reconcile: {:?}",
            host.ops
        );
        assert!(
            host.residents.iter().any(|r| r.identifier == "darkmux:sibling"),
            "the sibling's model must still be resident afterward: {:?}",
            host.residents
        );
        assert!(
            host.ops.iter().any(|op| matches!(
                op,
                darkmux_gestalt::mock::HostOp::Load { identifier, .. } if identifier == "darkmux:m"
            )),
            "the wave's own desired model still loads: {:?}",
            host.ops
        );

        drop(sibling_guard);
        drop(own_guard);
    }

    /// INVERTED direction: once the same-process sibling's guard has
    /// actually DROPPED (its dispatch finished, or it withdrew), its model
    /// must no longer stay pinned — a later reconcile in the same process
    /// (a different guard) must be free to evict it as an orphan not in
    /// its own desired set. Guards against a fix that protects same-process
    /// siblings but never lets go once they're gone (which would mean a
    /// wave that should free memory never does, and the next dispatch
    /// fails to fit).
    #[serial_test::serial]
    #[test]
    fn ensure_wave_loaded_evicts_a_same_process_orphan_once_its_holder_has_dropped() {
        let _env = LeaseTestEnv::new();

        {
            let withdrawn_guard = residency_lease::LeaseGuard::acquire();
            withdrawn_guard
                .write(&["darkmux:withdrawn".to_string()])
                .expect("the withdrawing guard writes its lease");
            // Guard drops here — its dispatch is over, its contribution
            // must be released (Drop-only release, #2651).
        }

        let est = FixedEstimator(BTreeMap::from([("m".to_string(), 1_000u64)]));
        let mut host = MockHost::new()
            .resident("darkmux:withdrawn", "withdrawn", 32_000, Some(1_000))
            .cataloged("m", 1_000);
        let wave = vec![placement("m", 8_000)];

        let own_guard = residency_lease::LeaseGuard::acquire();
        ensure_wave_loaded(&wave, &est, &mut host, &own_guard).expect("the wanted model loads");

        assert!(
            host.ops.iter().any(|op| matches!(
                op,
                darkmux_gestalt::mock::HostOp::Unload { identifier } if identifier == "darkmux:withdrawn"
            )),
            "a withdrawn (dropped) same-process holder's model must not stay pinned forever — \
             it must be evicted as an orphan: {:?}",
            host.ops
        );
    }

    /// Panic path: a same-process sibling that PANICS mid-dispatch (its
    /// guard's `Drop` still runs during unwind, releasing ONLY its own
    /// contribution — #2651's guarantee) must (a) still protect a
    /// DIFFERENT, genuinely live survivor sibling from eviction, and (b)
    /// not itself stay wrongfully pinned forever once its own `Drop` has
    /// run. Exercises the same self-token exclusion + panic-release
    /// contract this fix adds on top of #2651's own panic-path proof in
    /// `residency_lease`'s test suite.
    #[serial_test::serial]
    #[test]
    fn ensure_wave_loaded_after_a_same_process_sibling_panics_protects_the_survivor_and_frees_the_doomed(
    ) {
        let _env = LeaseTestEnv::new();

        let survivor_guard = residency_lease::LeaseGuard::acquire();
        survivor_guard.write(&["darkmux:survivor".to_string()]).expect("survivor writes its lease");

        let unwound = std::panic::catch_unwind(|| {
            let doomed_guard = residency_lease::LeaseGuard::acquire();
            doomed_guard.write(&["darkmux:doomed".to_string()]).expect("doomed writes its lease");
            panic!("simulated mid-dispatch panic (#2663 panic-path proof)");
        });
        assert!(unwound.is_err(), "precondition: the simulated panic must actually have unwound");

        let est = FixedEstimator(BTreeMap::from([("m".to_string(), 1_000u64)]));
        let mut host = MockHost::new()
            .resident("darkmux:survivor", "survivor", 32_000, Some(1_000))
            .resident("darkmux:doomed", "doomed", 32_000, Some(1_000))
            .cataloged("m", 1_000);
        let wave = vec![placement("m", 8_000)];

        let own_guard = residency_lease::LeaseGuard::acquire();
        ensure_wave_loaded(&wave, &est, &mut host, &own_guard).expect("the wanted model loads");

        let unloaded: Vec<String> = host
            .ops
            .iter()
            .filter_map(|op| match op {
                darkmux_gestalt::mock::HostOp::Unload { identifier } => Some(identifier.clone()),
                _ => None,
            })
            .collect();
        assert!(
            !unloaded.contains(&"darkmux:survivor".to_string()),
            "the genuinely live survivor must never be evicted by this process's own reconcile: \
             {unloaded:?}"
        );
        assert!(
            unloaded.contains(&"darkmux:doomed".to_string()),
            "the panicked holder's model must be evicted once its Drop released it during unwind \
             — never wrongfully pinned forever: {unloaded:?}"
        );

        drop(survivor_guard);
        drop(own_guard);
    }

    /// (#1487 PR2) `ensure_wave_loaded` writes its OWN lease before
    /// planning, naming exactly the (deduplicated) models this wave holds
    /// — so a DIFFERENT concurrent darkmux command reading the registry
    /// sees them pinned.
    #[serial_test::serial]
    #[test]
    fn ensure_wave_loaded_writes_its_own_lease_for_the_wave() {
        let _env = LeaseTestEnv::new();
        let est = FixedEstimator(BTreeMap::from([("m".to_string(), 1_000u64)]));
        let mut host = MockHost::new().cataloged("m", 1_000);
        let wave = vec![placement("m", 8_000)];

        let lease_guard = residency_lease::LeaseGuard::acquire();
        ensure_wave_loaded(&wave, &est, &mut host, &lease_guard).expect("the model loads");

        // Read back as if from a DIFFERENT process — own-pid exclusion is
        // exactly what `residency_lease`'s own unit tests already prove, so
        // this test only needs to confirm the CONTENT this call site wrote.
        let other_own_pid = std::process::id().wrapping_add(1);
        let leased = residency_lease::live_leased_models(other_own_pid);
        assert_eq!(
            leased,
            vec!["darkmux:m".to_string()],
            "this wave's own lease names exactly its (deduplicated) desired models"
        );
    }

    /// (#1487 PR2 addendum) The hold-not-fail feasibility contract: a
    /// resource shortfall (`HostError::InsufficientResources`, the #1139
    /// fast-fail) that a LIVE pinned holder explains is transient — the
    /// bounded retry-hold re-plans and succeeds once the scripted failure
    /// drains, without ever surfacing an error to the caller.
    #[serial_test::serial]
    #[test]
    fn ensure_wave_loaded_retries_past_a_transient_shortfall_when_a_holder_is_pinned() {
        let env = LeaseTestEnv::new();
        let mut holder = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawning a short-lived holder process");
        let holder_pid = holder.id();
        let residency_dir = env.home().join("residency");
        std::fs::create_dir_all(&residency_dir).unwrap();
        std::fs::write(
            residency_dir.join(format!("{holder_pid}.lease")),
            format!(r#"{{"pid":{holder_pid},"models":["darkmux:busy-elsewhere"]}}"#),
        )
        .expect("hand-writing a lease for the external holder pid");

        let est = FixedEstimator(BTreeMap::from([("m".to_string(), 1_000u64)]));
        let mut host = MockHost::new().cataloged("m", 1_000);
        host.fail_next_load = Some(HostError::InsufficientResources { detail: "no room right now".into() });
        let wave = vec![placement("m", 8_000)];

        let lease_guard = residency_lease::LeaseGuard::acquire();
        ensure_wave_loaded(&wave, &est, &mut host, &lease_guard)
            .expect("a pinned-external shortfall retries past the scripted failure and succeeds");

        let load_attempts = host
            .ops
            .iter()
            .filter(|op| matches!(op, darkmux_gestalt::mock::HostOp::Load { .. }))
            .count();
        assert_eq!(load_attempts, 2, "one failed attempt, one successful retry: {:?}", host.ops);

        let _ = holder.kill();
        let _ = holder.wait();
    }

    /// (#1487 PR2 addendum) The OTHER half of the ceiling distinction: the
    /// SAME `InsufficientResources` shortfall, but with NO live holder
    /// pinned to explain it — this fails IMMEDIATELY (no retry; nothing a
    /// wait could fix), naming that no concurrent holder is blocking it.
    #[serial_test::serial]
    #[test]
    fn ensure_wave_loaded_fails_immediately_with_no_pinned_holder_to_blame() {
        let _env = LeaseTestEnv::new();
        let est = FixedEstimator(BTreeMap::from([("m".to_string(), 1_000u64)]));
        let mut host = MockHost::new().cataloged("m", 1_000);
        host.fail_next_load = Some(HostError::InsufficientResources { detail: "too big alone".into() });
        let wave = vec![placement("m", 8_000)];

        let lease_guard = residency_lease::LeaseGuard::acquire();
        let err = ensure_wave_loaded(&wave, &est, &mut host, &lease_guard)
            .expect_err("no pinned holder explains the shortfall — this can never be transient");
        assert!(
            err.to_string().contains("no concurrent darkmux"),
            "the error must name that no concurrent holder is pinned: {err:#}"
        );

        let load_attempts = host
            .ops
            .iter()
            .filter(|op| matches!(op, darkmux_gestalt::mock::HostOp::Load { .. }))
            .count();
        assert_eq!(load_attempts, 1, "no retry when nothing explains the shortfall as transient: {:?}", host.ops);
    }

    /// (#2672 CONSIDER 5) Conformance: the retry-hold at
    /// `ensure_wave_loaded` is gated on the SPECIFIC `Reason::
    /// ClaimedResidentInsufficientCtx { clearable: true, .. }` match, never
    /// on "any `ClaimedResidentInsufficientCtx`, regardless of
    /// `clearable`" (never mind "any `Action::Block`" at all). Two
    /// placements in ONE wave sharing a model key but under DIFFERENT
    /// identifiers (so `ensure_wave_loaded`'s own model_key+identifier
    /// dedup does not collapse them) both resolve against the identical
    /// stale resident; the second Blocks with `clearable: false` (a
    /// same-plan collision — see `Reason::ClaimedResidentInsufficientCtx`'s
    /// own doc) — this must fail on the FIRST attempt, never retried,
    /// since no amount of waiting ever changes a deterministic re-plan of
    /// the identical fixed input. Pins the exact boundary the #2672
    /// narrowing checks: a mutant that widens the match to `Reason::
    /// ClaimedResidentInsufficientCtx { .. }` (dropping the `clearable:
    /// true` sub-pattern, retrying BOTH origins alike) would still pass
    /// every #2669 red-prove above (all `clearable: true` fixtures) but
    /// fails this one — it would see `ListResident` called
    /// `BLOCKED_BY_HOLDER_RETRY_ATTEMPTS` times instead of once.
    #[serial_test::serial]
    #[test]
    fn ensure_wave_loaded_never_retries_a_same_plan_collision_2672() {
        let _env = LeaseTestEnv::new();
        let est = FixedEstimator(BTreeMap::from([("shared".to_string(), 1_000u64)]));
        let mut host = MockHost::new()
            .resident("darkmux:shared", "shared", 4_096, Some(1_000))
            .cataloged("shared", 1_000);
        // Same model_key, DIFFERENT identifiers — `ensure_wave_loaded`'s
        // own dedup only collapses an EXACT (model_key, identifier) match,
        // so both placements reach `plan_acquire` in the same call.
        let wave = vec![
            Placement {
                model_key: "shared".into(),
                identifier: "darkmux:shared".into(),
                min_ctx: 8_000,
                seat: "probe-a".into(),
            },
            Placement {
                model_key: "shared".into(),
                identifier: "custom-alias".into(),
                min_ctx: 68_000,
                seat: "probe-b".into(),
            },
        ];

        let lease_guard = residency_lease::LeaseGuard::acquire();
        let err = ensure_wave_loaded(&wave, &est, &mut host, &lease_guard)
            .expect_err("a same-plan collision is never satisfiable by waiting");
        assert!(
            err.to_string().contains("never resolve by waiting"),
            "the error must name the real (non-clearable) reason: {err:#}"
        );

        let list_attempts = host
            .ops
            .iter()
            .filter(|op| matches!(op, darkmux_gestalt::mock::HostOp::ListResident))
            .count();
        assert_eq!(
            list_attempts, 1,
            "a same-plan (clearable: false) collision must fail on the very first attempt, \
             never retried: {:?}",
            host.ops
        );
    }

    /// The plan sketch's headline test: `run_bounded` respects
    /// `plan_waves`'s own partitioning under a byte budget that fits two
    /// small models together but not a third — mirrors `waves.rs`'s own
    /// table tests (two-fit-together, third overflows to a second wave),
    /// just exercised through the executor instead of `plan_waves` directly.
    #[serial_test::serial]
    #[test]
    fn run_bounded_respects_wave_partitioning_under_budget() {
        let _env = LeaseTestEnv::new();
        let est = FixedEstimator(BTreeMap::from([
            ("small-a".to_string(), 10_000_000_000),
            ("small-b".to_string(), 10_000_000_000),
            ("big-c".to_string(), 10_000_000_000),
        ]));
        // Budget fits any two of the three (20GB) but not all three (30GB).
        let facts = Facts { budget: Budget { max_darkmux_bytes: Some(20_000_000_000) }, ..Default::default() };
        let marker = Arc::new(AtomicU32::new(0));
        let jobs = vec![
            QueuedJob { index: 0, seat: SeatClaim::LocalModel(placement("small-a", 8_000)), job: ok_job(0, marker.clone()) },
            QueuedJob { index: 1, seat: SeatClaim::LocalModel(placement("small-b", 8_000)), job: ok_job(1, marker.clone()) },
            QueuedJob { index: 2, seat: SeatClaim::LocalModel(placement("big-c", 8_000)), job: ok_job(2, marker.clone()) },
        ];
        let results = run_bounded(jobs, &facts, &est, 4, 4, &mock_host_factory).expect("planning never fails under Auto");
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
    #[serial_test::serial]
    #[test]
    fn run_bounded_never_runs_a_refused_placement() {
        let _env = LeaseTestEnv::new();
        let est = FixedEstimator(BTreeMap::from([
            ("fits".to_string(), 5_000_000_000),
            ("too-big".to_string(), 50_000_000_000),
        ]));
        let facts = Facts { budget: Budget { max_darkmux_bytes: Some(10_000_000_000) }, ..Default::default() };
        let marker = Arc::new(AtomicU32::new(0));
        let jobs = vec![
            QueuedJob { index: 0, seat: SeatClaim::LocalModel(placement("fits", 8_000)), job: ok_job(0, marker.clone()) },
            QueuedJob { index: 1, seat: SeatClaim::LocalModel(placement("too-big", 8_000)), job: ok_job(1, marker.clone()) },
        ];
        let results = run_bounded(jobs, &facts, &est, 4, 4, &mock_host_factory).expect("planning never fails under Auto");
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
    #[serial_test::serial]
    #[test]
    fn run_bounded_runs_both_jobs_sharing_one_resident_placement() {
        let _env = LeaseTestEnv::new();
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
            QueuedJob { index: 0, seat: SeatClaim::LocalModel(placement("shared", 8_000)), job: ok_job(0, marker.clone()) },
            QueuedJob { index: 1, seat: SeatClaim::LocalModel(placement("shared", 8_000)), job: ok_job(1, marker.clone()) },
        ];
        let results = run_bounded(jobs, &facts, &est, 4, 4, &mock_host_factory).expect("planning never fails under Auto");
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
            .map(|i| QueuedJob { index: i, seat: SeatClaim::RemoteEndpoint, job: ok_job(i, marker.clone()) })
            .collect();
        let results = run_bounded(jobs, &facts, &est, 2, 4, &mock_host_factory).expect("planning never fails under Auto");
        assert_eq!(results.len(), 5);
        assert_eq!(marker.load(Ordering::SeqCst), 5);
        let mut indices: Vec<usize> = results.iter().map(|(i, _)| *i).collect();
        indices.sort_unstable();
        assert_eq!(indices, vec![0, 1, 2, 3, 4]);
    }

    /// (#2394) The two cap-bounded tracks are INDEPENDENT: a
    /// `SeatClaim::NoModel` job is bounded by `dispatch_free_cap`, and
    /// `remote_cap` — even at 1, the mission-launch value — does not touch
    /// it. Timed, because the whole bug was a timing one: four jobs each
    /// sleeping 200ms under `remote_cap: 1` must finish in ~200ms, not
    /// ~800ms.
    ///
    /// **Red before the fix**: there was no third track. Every dispatch-free
    /// job was a `Residency::Remote` job, so this ran in four sequential
    /// 200ms batches. The scheduler-level twin of this
    /// (`dispatch_free_siblings_do_not_serialize_behind_the_remote_cap`)
    /// measured 12.16s against a 3s expectation on the real
    /// `procedural.shell` kind.
    #[test]
    fn dispatch_free_jobs_are_bounded_by_their_own_cap_not_the_remote_one() {
        let est = FixedEstimator::default();
        let facts = Facts::default();
        let marker = Arc::new(AtomicU32::new(0));
        let jobs = (0..4)
            .map(|i| QueuedJob {
                index: i,
                seat: SeatClaim::NoModel,
                job: {
                    let marker = marker.clone();
                    Box::new(move || {
                        std::thread::sleep(Duration::from_millis(200));
                        marker.fetch_add(1, Ordering::SeqCst);
                        Ok((i, vec![]))
                    })
                },
            })
            .collect();
        let t0 = std::time::Instant::now();
        // remote_cap = 1 (a mission launch's value); dispatch_free_cap = 4.
        let results = run_bounded(jobs, &facts, &est, 1, 4, &mock_host_factory).expect("planning never fails under Auto");
        let elapsed = t0.elapsed();
        assert_eq!(results.len(), 4);
        assert_eq!(marker.load(Ordering::SeqCst), 4);
        assert!(
            elapsed < Duration::from_millis(600),
            "four dispatch-free jobs at 200ms each must overlap under their OWN cap, never \
             serialize behind remote_cap=1 — got {elapsed:?}"
        );
    }

    /// (#2394) And the dispatch-free cap is a REAL bound, not decoration: at
    /// `dispatch_free_cap: 1` the same four jobs serialize. Without this,
    /// the test above would pass equally well against an unbounded track,
    /// and `mods.gate` running a `test_command` per mod would have no
    /// ceiling at all.
    #[test]
    fn the_dispatch_free_cap_actually_bounds() {
        let est = FixedEstimator::default();
        let facts = Facts::default();
        let marker = Arc::new(AtomicU32::new(0));
        let jobs = (0..4)
            .map(|i| QueuedJob {
                index: i,
                seat: SeatClaim::NoModel,
                job: {
                    let marker = marker.clone();
                    Box::new(move || {
                        std::thread::sleep(Duration::from_millis(120));
                        marker.fetch_add(1, Ordering::SeqCst);
                        Ok((i, vec![]))
                    })
                },
            })
            .collect();
        let t0 = std::time::Instant::now();
        let results = run_bounded(jobs, &facts, &est, 8, 1, &mock_host_factory).expect("planning never fails under Auto");
        let elapsed = t0.elapsed();
        assert_eq!(results.len(), 4);
        assert!(
            elapsed >= Duration::from_millis(400),
            "dispatch_free_cap=1 must serialize all four (~480ms) — a track that ignored its \
             cap would finish in ~120ms; got {elapsed:?}"
        );
        assert_eq!(marker.load(Ordering::SeqCst), 4);
    }

    /// (#2394 / #1509) An UNRESOLVED local seat keeps its historical
    /// behavior — it rides the remote track, cap and all — rather than
    /// silently gaining the dispatch-free track's much wider ceiling. The
    /// class is new; the scheduling of this case is not.
    #[test]
    fn an_unresolved_local_seat_still_rides_the_remote_cap() {
        let est = FixedEstimator::default();
        let facts = Facts::default();
        let marker = Arc::new(AtomicU32::new(0));
        let jobs = (0..3)
            .map(|i| QueuedJob {
                index: i,
                seat: SeatClaim::LocalModelUnresolved { reason: "no active profile".to_string() },
                job: {
                    let marker = marker.clone();
                    Box::new(move || {
                        std::thread::sleep(Duration::from_millis(120));
                        marker.fetch_add(1, Ordering::SeqCst);
                        Ok((i, vec![]))
                    })
                },
            })
            .collect();
        let t0 = std::time::Instant::now();
        // remote_cap=1 serializes them; dispatch_free_cap=8 would not — so a
        // finish under ~240ms would prove they took the wrong track.
        let results = run_bounded(jobs, &facts, &est, 1, 8, &mock_host_factory).expect("planning never fails under Auto");
        let elapsed = t0.elapsed();
        assert_eq!(results.len(), 3);
        assert_eq!(marker.load(Ordering::SeqCst), 3);
        assert!(
            elapsed >= Duration::from_millis(300),
            "an unresolved LOCAL seat must stay on the remote track (serialized at \
             remote_cap=1, ~360ms), never fall through to the dispatch-free one — got {elapsed:?}"
        );
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
            QueuedJob { index: 0, seat: SeatClaim::RemoteEndpoint, job: Box::new(|| panic!("boom in a remote job")) },
            QueuedJob { index: 1, seat: SeatClaim::RemoteEndpoint, job: Box::new(|| Ok(((), vec![]))) },
        ];
        let results = run_bounded(jobs, &facts, &est, 4, 4, &mock_host_factory).expect("planning never fails under Auto");
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
    #[serial_test::serial]
    #[test]
    fn run_bounded_reconciles_a_panicking_local_job_to_a_terminal_error() {
        let _env = LeaseTestEnv::new();
        let est = FixedEstimator::default();
        let facts = Facts::default();
        let marker = Arc::new(AtomicU32::new(0));
        let jobs: Vec<QueuedJob<usize>> = vec![
            QueuedJob {
                index: 0,
                seat: SeatClaim::LocalModel(placement("m", 8_000)),
                job: Box::new(|| panic!("boom in a local wave job")),
            },
            QueuedJob { index: 1, seat: SeatClaim::LocalModel(placement("m2", 8_000)), job: ok_job(1, marker.clone()) },
        ];
        let results = run_bounded(jobs, &facts, &est, 4, 4, &mock_host_factory).expect("planning never fails under Auto");
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
            seat: SeatClaim::RemoteEndpoint,
            job: Box::new(|| Err(anyhow!("boom"))),
        }];
        let results = run_bounded(jobs, &facts, &est, 4, 4, &mock_host_factory).expect("planning never fails under Auto");
        assert_eq!(results.len(), 1);
        let (idx, outcome) = &results[0];
        assert_eq!(*idx, 0);
        assert!(outcome.as_ref().is_err_and(|e| e.to_string().contains("boom")));
    }
}
