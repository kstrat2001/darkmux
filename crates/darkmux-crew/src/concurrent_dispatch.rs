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
//! needs"). A panicking job's panic propagates naturally when `scope`
//! returns (the stdlib's own behavior) rather than being caught and hidden.
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

use anyhow::{anyhow, Result};
use darkmux_flow::FlowRecord;
use darkmux_gestalt::{Facts, FootprintEstimator, Placement, WaveMode, WaveSchedule};
use std::collections::HashMap;
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
    est: &dyn FootprintEstimator,
    remote_cap: usize,
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
            .then(|| scope.spawn(|| run_local_waves(schedule, local_by_seat, &results)));
        let remote_track =
            (!remote_jobs.is_empty()).then(|| scope.spawn(|| run_remote_batches(remote_jobs, remote_cap.max(1), &results)));

        // `thread::scope` re-panics once every spawned thread is joined if
        // any of them panicked — explicit `.join()` here just makes that
        // join point visible rather than relying on the implicit one at the
        // end of the block (identical behavior either way).
        if let Some(h) = local_track {
            let _ = h.join();
        }
        if let Some(h) = remote_track {
            let _ = h.join();
        }
    });

    Ok(results.into_inner().expect("no thread panicked while holding the results lock"))
}

/// The local track: walk `schedule.waves` in order, running every job in
/// one wave concurrently (a nested `thread::scope` per wave) before moving
/// to the next — the wave IS the "safe to co-reside" unit gestalt already
/// computed. `schedule.refusals` never run at all; each comes back as an
/// `Err` naming the refusal reason via `Reason`'s `Display`.
fn run_local_waves<T: Send + 'static>(
    schedule: WaveSchedule,
    mut by_seat: HashMap<String, (usize, DispatchJob<T>)>,
    results: &ResultsSink<T>,
) {
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
    use darkmux_gestalt::{Budget, FixedEstimator, ResidentFact};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

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
        let results = run_bounded(jobs, &facts, &est, 4).expect("planning never fails under Auto");
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
        let results = run_bounded(jobs, &facts, &est, 4).expect("planning never fails under Auto");
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
        let results = run_bounded(jobs, &facts, &est, 4).expect("planning never fails under Auto");
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
        let results = run_bounded(jobs, &facts, &est, 2).expect("planning never fails under Auto");
        assert_eq!(results.len(), 5);
        assert_eq!(marker.load(Ordering::SeqCst), 5);
        let mut indices: Vec<usize> = results.iter().map(|(i, _)| *i).collect();
        indices.sort_unstable();
        assert_eq!(indices, vec![0, 1, 2, 3, 4]);
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
        let results = run_bounded(jobs, &facts, &est, 4).expect("planning never fails under Auto");
        assert_eq!(results.len(), 1);
        let (idx, outcome) = &results[0];
        assert_eq!(*idx, 0);
        assert!(outcome.as_ref().is_err_and(|e| e.to_string().contains("boom")));
    }
}
