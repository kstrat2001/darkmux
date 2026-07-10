//! (#1222 Phase B packet 4) Review funnel — the validated review pipeline:
//! bundles → probe seats ×k draws → dedup → double-confirm judge → a
//! three-tier envelope.
//!
//! ```text
//! bundle → probe(k draws × seat, temp 0.2) → dedup → judge pass-1(every flag)
//!        → judge pass-2(pass-1 confirms only) → {confirmed, needs_check, archived}
//! ```
//!
//! This module is the DRIVER: given a resolved crew (packet 1's
//! `darkmux_profiles::crews::resolve_crew`), a diff, and an intent, it runs
//! the whole pipeline and returns a [`FunnelEnvelope`]. Dispatch itself goes
//! through a caller-injected `chat` closure (the container-free single-shot
//! primitive from packet 2, `darkmux_crew::single_shot::single_shot_chat`,
//! in production) and a caller-injected [`ModelCycler`] (real `lms` calls in
//! production, a recording mock in tests) — so the whole pipeline is
//! unit-testable without a live LMStudio or a real dispatch.
//!
//! ## Double-confirm judge (the load-bearing design choice)
//!
//! Every probe flag gets a judge pass-1 ruling. Only a `confirmed` pass-1
//! gets a pass-2 — a FRESH judge call over the identical prompt. Agreement
//! (confirmed → confirmed) promotes the flag to [`Tier::Confirmed`];
//! disagreement demotes it to [`Tier::NeedsCheck`] rather than shipping a
//! coin-flip as a defect report. This mirrors the CLAUDE.md "recheck vs
//! rethink" doctrine at judge scale: a single judge call is one context's
//! opinion; two independent calls voting the same way is real signal.
//!
//! ## Bundling — the packet 3 seam
//!
//! [`BundleInput`] is deliberately this module's OWN shape, decoupled from
//! `darkmux_lab::lab::bundle::{Bundle, BundleSet, build_bundles, slice_code,
//! external_bundles, FileSource}` (Phase B packet 3), which had not landed
//! on `main` when this packet was written. [`bundles_from_diff`] is the
//! PROVISIONAL bundler standing in for the real one — see its doc comment
//! for what it stands in for. Every other piece of this module (probe/
//! dedup/judge/envelope) is written entirely against `BundleInput` and
//! needed no changes once the real bundler landed.
//!
//! **Reconciled in packet 5** (`darkmux pr-review run`, `src/pr_review.rs`
//! in the binary crate): rather than editing `bundles_from_diff`'s body
//! in place, [`FunnelInputs::bundles`] is the injection seam — packet 5
//! builds real bundles via `build_bundles`/`external_bundles` + `slice_code`
//! and passes `Some(..)`; [`run_funnel`]/[`run_judge_only`] use those
//! directly and never call the provisional bundler. `bundles_from_diff`
//! survives only as the `None` fallback this module's own pre-packet-3
//! tests still rely on — no production caller uses it.
//!
//! Parsers and the dedup/double-confirm state machine are pure and
//! unit-tested; dispatching goes through caller-provided closures/traits so
//! the whole chain is testable without containers or a live LMStudio —
//! same discipline as `super::dialectic`.
//!
//! ## Flow-record emission (#1247 Part 1)
//!
//! The driver (`run_funnel`/`run_judge_only`/`finish_funnel`/`probe_phase`/
//! `dispatch_probe_staffing`) emits [`darkmux_flow::FlowRecord`]s through a
//! caller-injected [`FunnelEmitter`] — same injection discipline as `chat`/
//! `cycler` above, so a scripted test can assert the exact record SEQUENCE
//! via a recording mock. The driver is deliberately SINK-AGNOSTIC: it never
//! calls `darkmux_flow::record` itself and has no idea whether the records
//! land on the real engagement-scoped flow stream or a per-run-local file —
//! that choice belongs to the caller (`darkmux pr-review run` wires the real
//! stream; `darkmux lab review-bench --funnel` wires a per-run-local JSONL
//! file, per the lab-vs-fleet scope boundary — a bench's hundreds of
//! per-flag ruling records must never spam an operator's engagement
//! stream). Three action families, vocabulary aligned with #1230/#1240's
//! Mission → Sprint → Task → Step hierarchy so the records forward-port to
//! the generic mission-flow graph view unchanged:
//!
//! - `funnel.task` — one funnel RUN's bookends (`payload.status` = `started`
//!   | `finished` | `error`): case id, crew, exec mode, bundle count on
//!   start; confirmed/needs_check/archived counts + `degenerate` reason
//!   (when set) on finish. `error` is the [`FunnelBookendGuard`]'s Drop-path
//!   terminal record — emitted when the driver `?`-returns or panics after
//!   `started`, so no consumer ever sees an orphaned, perpetually-in-flight
//!   run (the same guarantee `darkmux-crew`'s `DispatchBookendGuard`, #717,
//!   gives `dispatch.start`).
//! - `funnel.step` — a step transition, payload shape matching #1230's
//!   named substrate exactly: `{step_id, kind: "procedural"|"dispatch",
//!   items_in, items_out, status: "started"|"finished", wall_ms}` (plus
//!   `status: "error"` from the guard's Drop path, closing any step still
//!   open at an abort — innermost-first, so start/terminal pairing holds on
//!   every path).
//!   `step_id` is `bundle` | `probe` | `probe:<staffing-name>` (one per
//!   probe seat — a future graph engine renders these as PARALLEL sibling
//!   steps under `probe`, #1230's parallel-step vision) | `dedup` |
//!   `judge-pass1` | `judge-pass2`. Seat-level (`probe:*`) records carry
//!   extra `model`/`draws_done`/`draws_total`/`tokens` fields. A `confirmed`
//!   pass-1 gets its pass-2 ruling immediately, interleaved within the SAME
//!   per-flag judge loop as pass-1 — so `judge-pass2`'s `started` record
//!   opens the moment the FIRST pass-2 ruling actually fires (`items_in` is
//!   the running count at that point, not the final docket size), rather
//!   than waiting for the whole loop to finish. Opening it late let a live
//!   observer see `funnel.ruling{pass:2}` records stream in while the
//!   `judge-pass2` step still read "not started" — a real contradiction
//!   caught live in the lab lens. `finished` closes once the loop
//!   completes, carrying the real final docket size and elapsed `wall_ms`.
//! - `funnel.ruling` — the live ticker: one record per judge ruling (every
//!   pass-1, plus pass-2 when it ran) with `bundle_id`/`pass`/`ruling`/
//!   `seconds`.
//!
//! Emission happens ONLY in the driver — never inside the pure protocol
//! functions (`dedup_flags`, `mechanism_family`, `parse_judge_ruling`,
//! `judge_prompt`, etc.) or the per-flag dispatch helper `judge_one_flag`
//! (its [`JudgeOutcome`] is emitted from by the caller in `finish_funnel`'s
//! loop, after the call returns).
//!
//! ## Host telemetry sampling (#1247 doctrine surface — "No blind runs")
//!
//! `run_funnel`/`run_judge_only` also start a background host cpu/ram/gpu
//! sampler for the run's whole lifetime — see [`FunnelBookendGuard`] and
//! [`HostTelemetrySampler`]. Samples emit as `telemetry.process` records
//! through the SAME injected [`FunnelEmitter`] the `funnel.*` action family
//! above uses (so a bench run's samples stay per-run-local and a
//! `pr-review run`'s samples ride the fleet stream, same split), with the
//! identical field shape `darkmux_crew::dispatch_internal`'s always-on
//! sampler already produces — the run-monitor/viewer code that renders
//! `telemetry.process` today applies unchanged.

use anyhow::{anyhow, bail, Context, Result};
use darkmux_crew::single_shot::SingleShotReply;
use darkmux_crew::telemetry_sampler::{sample_host, HostSample};
use darkmux_profiles::crews::{ResolvedCrew, ResolvedSeatStaffing};
use darkmux_profiles::{lms, swap};
use darkmux_types::{BundleSelector, ProfileModel};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

// ─── execution mode ───────────────────────────────────────────────────────

/// How probe/judge models are cycled through LMStudio across the funnel's
/// dispatches. `Auto` resolves once, up front, to `Sequential` or
/// `Parallel` (see [`resolve_mode`]) — the resolved choice is what
/// `FunnelEnvelope::mode` records, so an operator reading the envelope
/// never has to wonder which one actually ran.
///
/// This governs LMStudio RESIDENCY (which models stay loaded), not
/// concurrent network dispatch — `Sequential` loads one member, runs every
/// draw for it, releases it, then moves on; `Parallel` loads every member
/// up front and dispatches each staffing's draws in turn without
/// releasing between them (dispatches themselves still run one at a time
/// through the injected `chat` closure — true concurrent dispatch is a
/// separate, unaddressed concern).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecMode {
    Sequential,
    Parallel,
    Auto,
}

fn mode_label(mode: ExecMode) -> &'static str {
    match mode {
        ExecMode::Sequential => "sequential",
        ExecMode::Parallel => "parallel",
        // `resolve_mode` always turns `Auto` into one of the above before
        // this is ever read into an envelope; kept for exhaustiveness.
        ExecMode::Auto => "auto",
    }
}

// ─── probe flags ──────────────────────────────────────────────────────────

/// One probe draw's finding, post-parse but pre-dedup. `anchor` starts
/// `None` at construction — [`dedup_flags`] is where anchor extraction
/// happens (it needs the diff to validate a quote against, so doing the
/// extraction there keeps ONE place responsible for both jobs at once).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeFlag {
    pub bundle_id: String,
    pub fact_family: String,
    /// The probe staffing that produced this draw — the darkmux-namespaced
    /// LMStudio identifier (e.g. `darkmux:qwen3.6-35b-a3b`), so a mixed-
    /// model probe seat's flags stay attributable.
    pub member: String,
    pub draw: u32,
    pub charge_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
}

/// Bookkeeping [`dedup_flags`] returns alongside the deduped list — the
/// raw/deduped counts an envelope's `raw_flags`/`deduped_flags` fields are
/// sourced from.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DedupStats {
    pub raw: usize,
    pub deduped: usize,
}

// ─── judge rulings ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunnelRuling {
    Confirmed,
    NeedsCheck,
    FalsePositive,
    /// The judge's reply carried no recognizable fenced JSON ruling (after
    /// one retry — see [`judge_pass_with_retry`]).
    Unparsed,
    /// The dispatch itself failed (propagated up from `chat`, wrapped here
    /// rather than aborting the whole docket over one bad call).
    Error,
}

/// One judge call's outcome. `pass` is `1` or `2` (double-confirm); one
/// `JudgeRecord` per actual dispatch — a retried pass-1 produces TWO
/// records internally but only the retry's outcome survives into a
/// [`JudgedFlag`] (the first, unparsed attempt is discarded, not hidden —
/// see `judge_pass_with_retry`'s doc for why that's honest).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeRecord {
    pub ruling: FunnelRuling,
    pub decisive_evidence: String,
    pub note_for_author: String,
    pub pass: u8,
    pub seconds: f64,
}

/// The three-tier envelope outcome for one flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Confirmed,
    NeedsCheck,
    Archived,
}

/// One flag's full judge record: pass-1 always present, pass-2 present iff
/// pass-1 was `confirmed`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgedFlag {
    pub flag: ProbeFlag,
    pub pass1: JudgeRecord,
    pub pass2: Option<JudgeRecord>,
    pub tier: Tier,
    /// `true` iff a pass-1 `confirmed` was demoted to `needs_check` because
    /// pass-2 disagreed — the specific signal an operator scanning the
    /// envelope wants to find first (a flag the judge itself wasn't sure
    /// about, not one the harness is guessing on).
    pub demoted_by_pass2: bool,
}

// ─── telemetry ────────────────────────────────────────────────────────────

/// Per-model resource accounting — one row per probe staffing plus one for
/// the judge seat.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemberRecord {
    pub model: String,
    pub seat: String,
    pub draws: u32,
    pub wall_ms: u64,
    pub total_tokens: u64,
}

/// One pipeline step's in/out counts + wall time — the issue #1230 bridge:
/// a future flow-record consumer can render the funnel as a step timeline
/// without re-deriving it from the envelope's nested arrays. Realized by
/// the `funnel.step` flow record (#1247 Part 1, see the module doc) — the
/// live-run counterpart of this end-of-run summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    /// `bundle` | `probe` | `dedup` | `judge-pass1` | `judge-pass2`.
    pub step_id: String,
    /// `procedural` (no dispatch — bundling, dedup) | `dispatch` (LMStudio
    /// calls).
    pub kind: String,
    pub items_in: usize,
    pub items_out: usize,
    pub wall_ms: u64,
}

// ─── flow-record emission (#1247 Part 1 — see the module doc) ───────────

/// Sink for the funnel driver's run-observability records. The driver only
/// knows how to build [`darkmux_flow::FlowRecord`]s and hand them to
/// `emit` — it never decides where they land. See the module doc's
/// "Flow-record emission" section for the action/payload vocabulary and
/// why the driver stays sink-agnostic (lab-vs-fleet scope boundary).
pub trait FunnelEmitter {
    fn emit(&mut self, record: darkmux_flow::FlowRecord);
}

/// No-op emitter — the "at minimum a no-op-able sink" default for callers
/// (and this module's own tests that don't assert on flow records) that
/// don't want funnel observability output.
pub struct NullEmitter;

impl FunnelEmitter for NullEmitter {
    fn emit(&mut self, _record: darkmux_flow::FlowRecord) {}
}

const FUNNEL_TASK_ACTION: &str = "funnel.task";
const FUNNEL_STEP_ACTION: &str = "funnel.step";
const FUNNEL_RULING_ACTION: &str = "funnel.ruling";

/// Build one funnel observability record. `handle` = the crew name (this
/// funnel's addressable identity, the role `handle` plays for `crew
/// dispatch`'s per-role records); `session_id` = the case id (one funnel
/// RUN's identity, the role `session_id` plays for a single dispatch).
/// `source = "funnel"` distinguishes these from `crew_dispatch`/
/// `sprint_review` records that may share the same sink. `category = Work`
/// / `tier = Local` / `stage = Dispatch` mirror `crew dispatch`'s own
/// per-turn records (`dispatch.tool`, `dispatch.turn`) — the funnel is,
/// mechanically, a multi-dispatch alternative shape of the same "produce a
/// local review" job.
fn funnel_flow_record(
    case_id: &str,
    crew_name: &str,
    action: &str,
    level: darkmux_flow::Level,
    payload: serde_json::Value,
) -> darkmux_flow::FlowRecord {
    darkmux_flow::FlowRecord {
        ts: darkmux_flow::ts_utc_now(),
        level,
        category: darkmux_flow::Category::Work,
        tier: darkmux_flow::Tier::Local,
        stage: darkmux_flow::Stage::Dispatch,
        action: action.to_string(),
        handle: crew_name.to_string(),
        sprint_id: None,
        session_id: Some(case_id.to_string()),
        source: Some("funnel".to_string()),
        model: None,
        reasoning: None,
        mission_id: None,
        machine_id: None,
        machine_uid: None,
        orchestrator: None,
        prev_hash: None,
        hash: None,
        payload: Some(payload),
        work_id: None,
        attempt: None,
    }
}

/// The `funnel.task` "finished" record's payload + level, shared by every
/// return point (`run_funnel`'s two early degenerate returns,
/// `run_judge_only`'s one, and `finish_funnel`'s normal end) so the shape
/// can't drift between call sites. `Level::Warn` when `env.degenerate` is
/// set — a degenerate run is a loud, scoreable outcome, never quietly
/// `Info`.
fn task_finished_record(env: &FunnelEnvelope) -> darkmux_flow::FlowRecord {
    let mut payload = json!({
        "status": "finished",
        "case_id": env.case_id,
        "crew": env.crew,
        "confirmed": env.confirmed,
        "needs_check": env.needs_check,
        "archived": env.archived,
    });
    if let Some(reason) = &env.degenerate {
        payload["degenerate"] = serde_json::Value::String(reason.clone());
    }
    let level = if env.degenerate.is_some() { darkmux_flow::Level::Warn } else { darkmux_flow::Level::Info };
    funnel_flow_record(&env.case_id, &env.crew, FUNNEL_TASK_ACTION, level, payload)
}

// ─── host telemetry sampling (#1247 doctrine surface) ────────────────────

/// Production sample cadence — identical to `dispatch_internal`'s always-on
/// sampler (`TELEMETRY_SAMPLE_INTERVAL`/`SAMPLER_POLL_INTERVAL`). `interval`
/// is the time between samples; `poll` is how often the sampler thread
/// re-checks the stop flag while sleeping out `interval`, so teardown is
/// prompt (≤`poll`) instead of blocking for a full tick.
const FUNNEL_TELEMETRY_INTERVAL: Duration = Duration::from_millis(2000);
const FUNNEL_TELEMETRY_POLL: Duration = Duration::from_millis(500);

/// (#1247 doctrine surface — "No blind runs") Best-effort host cpu/ram/gpu
/// sampler for the funnel driver. The container dispatch path
/// (`darkmux_crew::dispatch_internal`) has always sampled host load at
/// ~2s cadence; the funnel path bypasses `dispatch_internal` entirely
/// (it dispatches through the container-free single-shot primitive) and
/// so, until now, produced zero host telemetry — a funnel envelope
/// recorded per-step wall-clock with no visibility into concurrent
/// machine load. Measured motivation (#1247 host-telemetry comment): a
/// 35B judge's tok/s dropped ~12–15% exactly when concurrent
/// `cargo test`/build bursts began on the same machine, invisible in the
/// envelope.
///
/// Reuses the EXACT host-reading mechanism `dispatch_internal` uses
/// (`darkmux_crew::telemetry_sampler::sample_host` — shells out to
/// `top`/`vm_stat`+`sysctl`/`ioreg`, extracted there for this reuse) and
/// the exact record shape (`darkmux_crew::dispatch::build_telemetry_record`,
/// `category=telemetry, source="process", action="telemetry.process"`,
/// payload `{cpu, mem, gpu}`), so the run-monitor/viewer code that already
/// renders `telemetry.process` records applies unchanged. `handle`/
/// `session_id` carry the crew name / case id — the same identity fields
/// `funnel_flow_record` stamps on the `funnel.*` action family, so a
/// telemetry record for this run groups with its other records under the
/// same `session_id`.
///
/// The sampling FUNCTION is injected (`sample_fn`, a plain fn pointer
/// defaulting to `sample_host` at every production call site — see
/// [`FunnelBookendGuard::new`]) so tests can drive the sampler with an
/// instant fake instead of racing real `top -l 1` subprocess latency
/// (~600-900ms per call) against a scripted deadline on a shared CI
/// runner — the same injection discipline as `chat`/`cycler`/`emitter`.
/// The real `sample_host` gets its own direct coverage in
/// `darkmux-crew` (macOS-gated, since the commands it shells to are
/// macOS-only).
///
/// Samples land on an `mpsc` channel rather than being emitted directly
/// from the background thread: the funnel driver's [`FunnelEmitter`] is a
/// caller-injected `&mut dyn` trait object — not thread-safe, and
/// deliberately not wrapped in a `Mutex` (that would force every
/// `FunnelEmitter` impl and every existing emission call site in this file
/// through lock-guarded access for a feature this narrow). Instead,
/// [`FunnelBookendGuard`] drains the channel immediately before every
/// record it already emits (`funnel.task`/`funnel.step`/`funnel.ruling`)
/// and once more in its own `Drop`, so telemetry interleaves with the
/// run's other records close to when it was sampled — never batched at
/// end-of-run, which is exactly the failure the doctrine calls out
/// ("per-event records stream durably as they happen").
struct HostTelemetrySampler {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    rx: Receiver<darkmux_flow::FlowRecord>,
}

impl HostTelemetrySampler {
    /// Spawn the sampler thread. Uses `Builder::spawn` (which returns
    /// `io::Result`, unlike the panicking `thread::spawn`) so an OS-level
    /// spawn failure degrades to "no samples" — sampling must never affect
    /// the funnel run it's observing.
    fn start(
        case_id: String,
        crew: String,
        interval: Duration,
        poll: Duration,
        sample_fn: fn() -> HostSample,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let stop_thread = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("funnel-telemetry".to_string())
            .spawn(move || loop {
                // Sleep out `interval` FIRST, THEN sample — deliberately
                // NOT sample-then-sleep. A funnel run's own dispatches
                // (bundling, probe draws, judge passes) are real LMStudio
                // round trips that take real wall-clock, so a run genuinely
                // running past one `interval` gets its first sample right
                // on schedule. The load-bearing side effect: at the
                // PRODUCTION cadence ([`FUNNEL_TELEMETRY_INTERVAL`], 2s),
                // this makes it structurally impossible for a synchronous,
                // sub-millisecond MOCKED test run (of which this file has
                // 20+) to race a sample into its `RecordingEmitter` — the
                // thread can't reach the sample point before `stop()` +
                // `join()` in `HostTelemetrySampler::drop` already won.
                // Only a run whose OWN cadence deliberately shortens
                // `interval` (this module's telemetry tests) or a real
                // dispatch that outlives 2s ever observes one.
                let mut slept = Duration::ZERO;
                while slept < interval {
                    if stop_thread.load(Ordering::SeqCst) {
                        return;
                    }
                    let nap = poll.min(interval - slept);
                    thread::sleep(nap);
                    slept += nap;
                }
                let sample = sample_fn();
                if sample.cpu.is_some() || sample.mem.is_some() || sample.gpu.is_some() {
                    let mut payload = serde_json::Map::new();
                    if let Some(c) = sample.cpu {
                        payload.insert("cpu".into(), c.into());
                    }
                    if let Some(m) = sample.mem {
                        payload.insert("mem".into(), m.into());
                    }
                    if let Some(g) = sample.gpu {
                        payload.insert("gpu".into(), g.into());
                    }
                    let record = darkmux_crew::dispatch::build_telemetry_record(
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

    /// Signal the stop flag and join the thread. Called from `Drop` — the
    /// RAII tie-in that guarantees no orphaned sampler thread outlives a
    /// [`FunnelBookendGuard`], on every exit path (clean finish, early
    /// `?`-return, or panic).
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

/// (#1247 review round) Bookend guard for the funnel's flow-record
/// lifecycle — same class of problem `darkmux-crew`'s
/// `DispatchBookendGuard` (#717) solves for `dispatch.start`: once
/// `funnel.task started` is emitted, the driver can still `?`-return before
/// the clean `finished` bookend (a probe dispatch error, a cycler
/// load/release failure) — or panic. Without a terminal record that leaves
/// an orphaned task (rendering as perpetually in-flight to any consumer)
/// plus whatever step-`started` records were open at the abort point.
///
/// All driver emission routes THROUGH this guard so it can track the task
/// bookend and the stack of currently-open steps: on `Drop` while still
/// armed, it closes each open step innermost-first with a `funnel.step`
/// record carrying `status: "error"`, then emits a terminal `funnel.task`
/// record with `status: "error"` — every `started` gets a matching terminal
/// event, on every path. The clean finish (`task_finished`, which every
/// success/degenerate return point calls) disarms it, so a run that reached
/// its own terminal record is never double-counted.
///
/// Emission on `Drop` is best-effort by construction — [`FunnelEmitter`]
/// impls are already infallible (`emit` returns nothing), so a sink problem
/// can't mask the original error propagating out.
///
/// Also owns the run's [`HostTelemetrySampler`] (#1247 doctrine surface):
/// started in [`Self::new`]/[`Self::new_with_telemetry`] and
/// stopped by its own `Drop` — which Rust runs automatically as a field of
/// this struct, right after `FunnelBookendGuard`'s own `Drop::drop` body
/// returns, so the sampler thread never outlives the guard on any exit
/// path.
struct FunnelBookendGuard<'a> {
    emitter: &'a mut dyn FunnelEmitter,
    case_id: String,
    crew: String,
    armed: bool,
    /// `(step_id, kind)` of every step with a `started` record and no
    /// `finished` yet — a stack, since seat-level `probe:<name>` steps nest
    /// inside the phase-level `probe` step.
    open_steps: Vec<(String, String)>,
    telemetry: HostTelemetrySampler,
}

impl<'a> FunnelBookendGuard<'a> {
    fn new(emitter: &'a mut dyn FunnelEmitter, case_id: &str, crew: &str) -> Self {
        Self::new_with_telemetry(
            emitter,
            case_id,
            crew,
            FUNNEL_TELEMETRY_INTERVAL,
            FUNNEL_TELEMETRY_POLL,
            sample_host,
        )
    }

    /// Same as [`Self::new`] but with a caller-chosen telemetry cadence
    /// AND sampling function — the test-only seam a scripted run uses to
    /// observe deterministic samples without a multi-second sleep and
    /// without shelling to the real (macOS-only, ~600-900ms-per-call)
    /// `top`/`vm_stat`/`ioreg` commands. Production always goes through
    /// `new`, which fixes the cadence at [`FUNNEL_TELEMETRY_INTERVAL`]
    /// and the sampler at the real `sample_host`.
    fn new_with_telemetry(
        emitter: &'a mut dyn FunnelEmitter,
        case_id: &str,
        crew: &str,
        telemetry_interval: Duration,
        telemetry_poll: Duration,
        sample_fn: fn() -> HostSample,
    ) -> Self {
        Self {
            telemetry: HostTelemetrySampler::start(
                case_id.to_string(),
                crew.to_string(),
                telemetry_interval,
                telemetry_poll,
                sample_fn,
            ),
            emitter,
            case_id: case_id.to_string(),
            crew: crew.to_string(),
            armed: false,
            open_steps: Vec::new(),
        }
    }

    /// Drain every telemetry sample buffered since the last drain and emit
    /// each through the same [`FunnelEmitter`] the driver's own records go
    /// through — called immediately before every record this guard emits
    /// (see [`Self::emit_now`]) so telemetry streams alongside the run
    /// rather than batching at the end.
    fn drain_telemetry(&mut self) {
        let records: Vec<darkmux_flow::FlowRecord> = self.telemetry.rx.try_iter().collect();
        for record in records {
            self.emitter.emit(record);
        }
    }

    /// Drain pending telemetry, then emit `record`. Every direct emission
    /// in this guard routes through here (instead of calling
    /// `self.emitter.emit` directly) so telemetry ordering stays close to
    /// wall-clock without needing the sampler thread to touch the emitter
    /// itself.
    fn emit_now(&mut self, record: darkmux_flow::FlowRecord) {
        self.drain_telemetry();
        self.emitter.emit(record);
    }

    /// Emit the `funnel.task started` bookend and ARM the guard — from here
    /// until `task_finished`, an early return or panic fires the Drop path.
    fn task_started(&mut self, payload: serde_json::Value) {
        self.armed = true;
        self.emit_now(funnel_flow_record(
            &self.case_id,
            &self.crew,
            FUNNEL_TASK_ACTION,
            darkmux_flow::Level::Info,
            payload,
        ));
    }

    /// Emit a `funnel.step` `status: "started"` record and track the step
    /// as open until [`Self::step_finished`] closes it.
    fn step_started(&mut self, step_id: &str, kind: &str, payload: serde_json::Value) {
        self.open_steps.push((step_id.to_string(), kind.to_string()));
        self.emit_now(funnel_flow_record(
            &self.case_id,
            &self.crew,
            FUNNEL_STEP_ACTION,
            darkmux_flow::Level::Info,
            payload,
        ));
    }

    /// Emit a `funnel.step` `status: "finished"` record and close the step.
    /// Also the entry point for one-shot steps that emit `finished` with no
    /// prior `started` (`bundle`, `dedup` — instantaneous procedural steps);
    /// the close is then a no-op on the open-step stack.
    fn step_finished(&mut self, step_id: &str, payload: serde_json::Value) {
        self.open_steps.retain(|(id, _)| id != step_id);
        self.emit_now(funnel_flow_record(
            &self.case_id,
            &self.crew,
            FUNNEL_STEP_ACTION,
            darkmux_flow::Level::Info,
            payload,
        ));
    }

    /// Emit a `funnel.ruling` ticker record (no open/close semantics).
    fn ruling(&mut self, payload: serde_json::Value) {
        self.emit_now(funnel_flow_record(
            &self.case_id,
            &self.crew,
            FUNNEL_RULING_ACTION,
            darkmux_flow::Level::Info,
            payload,
        ));
    }

    /// Emit the terminal `funnel.task` record for `env` (finished, or
    /// degenerate-finished — see [`task_finished_record`]) and DISARM the
    /// guard: this run reached its own terminal record.
    fn task_finished(&mut self, env: &FunnelEnvelope) {
        self.armed = false;
        self.open_steps.clear();
        self.emit_now(task_finished_record(env));
    }
}

impl Drop for FunnelBookendGuard<'_> {
    fn drop(&mut self) {
        // Flush any sample the sampler produced since the last drain —
        // even on the clean-finish path (`task_finished` already disarmed
        // `self.armed`), a sample can land in the brief window between
        // that drain and this `Drop` running. The sampler thread itself
        // stops right after, via `HostTelemetrySampler`'s own `Drop`
        // (a field of this struct, torn down once this function returns).
        //
        // Known, accepted loss window: a sample the sampler thread sends
        // AFTER this final drain but BEFORE the join in the sampler's
        // `Drop` completes is dropped with the channel — at most one
        // final-tick sample, consistent with the sampler's best-effort
        // framing (telemetry never blocks or extends teardown to chase
        // one more data point).
        self.drain_telemetry();
        if !self.armed {
            return;
        }
        // Close every still-open step, innermost-first, so the stream's
        // start/terminal pairing stays consistent even on the abort path.
        while let Some((step_id, kind)) = self.open_steps.pop() {
            self.emit_now(funnel_flow_record(
                &self.case_id,
                &self.crew,
                FUNNEL_STEP_ACTION,
                darkmux_flow::Level::Error,
                json!({ "step_id": step_id, "kind": kind, "status": "error" }),
            ));
        }
        self.emit_now(funnel_flow_record(
            &self.case_id,
            &self.crew,
            FUNNEL_TASK_ACTION,
            darkmux_flow::Level::Error,
            json!({
                "status": "error",
                "case_id": self.case_id,
                "crew": self.crew,
                "error": "funnel terminated before completion (early return or panic)",
            }),
        ));
    }
}

// ─── the envelope ─────────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FunnelEnvelope {
    pub case_id: String,
    pub crew: String,
    pub mode: String,
    pub members: Vec<MemberRecord>,
    pub steps: Vec<StepRecord>,
    pub bundles: usize,
    pub raw_flags: usize,
    pub deduped_flags: usize,
    pub flags: Vec<ProbeFlag>,
    pub judged: Vec<JudgedFlag>,
    pub confirmed: usize,
    pub needs_check: usize,
    pub archived: usize,
    /// Set (never silently left empty) when the docket produced zero raw
    /// flags (every probe drew nothing usable) — a degenerate run is a
    /// LOUD, scoreable outcome, never a silent pass. `None` on a normal run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degenerate: Option<String>,
    /// Judge model + temperature + persona hash + protocol version — what
    /// two envelopes need to share before their tiers are comparable.
    pub fingerprint: serde_json::Value,
    /// The RESOLVED per-seat staffing this run actually used — post any
    /// `--k` override the caller applied to the crew before dispatch.
    /// `FunnelEnvelope::crew` is only the crew's NAME; if the operator
    /// edits or renames that crew's staffing between runs, a series
    /// comparison keyed on the name alone silently corrupts. This snapshot
    /// makes the run's knob config self-contained in its own artifact — an
    /// experiment-series lab view can diff two runs' `staffing` fields
    /// directly, never re-reading a registry that may have since changed.
    /// `Option` (not a bare `Default`) so pre-#1247 envelopes deserialize
    /// as `None` rather than a misleadingly-empty snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staffing: Option<CrewStaffingSnapshot>,
}

/// One seat staffing's resolved config, snapshotted as ACTUALLY used —
/// see [`FunnelEnvelope::staffing`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaffingSnapshot {
    pub name: String,
    /// The darkmux-namespaced LMStudio identifier — the same form
    /// [`MemberRecord::model`] records, so the two line up at a glance.
    pub model: String,
    pub k: u32,
    /// The resolved `ProfileModel`'s DECLARED context length — settings
    /// provenance per run, so "what context was this model loaded at" is
    /// never a forensic question (a sibling concern to the config-vs-
    /// measured-context mismatch class of bug #1135 shipped). `#[serde(default)]`
    /// so a pre-#1256 snapshot (staffing existed, this field didn't)
    /// deserializes as `0` rather than a hard parse failure — the same
    /// schema-lenience discipline every field in this module follows.
    #[serde(default)]
    pub n_ctx: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<BundleSelector>,
}

/// Per-seat resolved staffing snapshot — `review-probe` (one or more
/// staffings) + `review-judge` (exactly one). See [`FunnelEnvelope::staffing`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrewStaffingSnapshot {
    pub probes: Vec<StaffingSnapshot>,
    pub judge: Option<StaffingSnapshot>,
}

fn staffing_snapshot(probes: &[ResolvedSeatStaffing], judge: &ResolvedSeatStaffing) -> CrewStaffingSnapshot {
    fn one(s: &ResolvedSeatStaffing) -> StaffingSnapshot {
        StaffingSnapshot {
            name: s.name.clone(),
            model: swap::namespaced_identifier(&s.pm),
            k: s.k,
            n_ctx: s.pm.n_ctx,
            max_tokens: s.max_tokens,
            selector: s.selector.clone(),
        }
    }
    CrewStaffingSnapshot {
        probes: probes.iter().map(one).collect(),
        judge: Some(one(judge)),
    }
}

// ─── model cycling ────────────────────────────────────────────────────────

/// Load/release one [`ProfileModel`] into/out of LMStudio. Injected so
/// tests can assert on cycling ORDER via a recording mock without a live
/// LMStudio; production dispatch uses [`LmsCycler`].
pub trait ModelCycler {
    fn ensure_loaded(&mut self, pm: &ProfileModel) -> Result<()>;
    fn release(&mut self, pm: &ProfileModel) -> Result<()>;
}

/// Production [`ModelCycler`]: real `lms` calls, namespaced under
/// `darkmux:` (the same operator-sovereignty guard `swap::swap` uses — a
/// model NOT in the namespace is user state and is never unloaded) and
/// context-sufficiency aware (a model already loaded with >= the wanted
/// context is left in place, mirroring `swap::ctx_sufficient` — no
/// needless reload-down).
pub struct LmsCycler;

/// (#1271) What [`LmsCycler::ensure_loaded`] should do about a seat's model,
/// given the CURRENT `lms ps` residents. Factored out as a pure function so
/// the reconciliation logic is unit-testable without shelling to a real
/// `lms` binary — same "pure decision, impure execution" split the rest of
/// this module favors (`resolve_mode`, `dedup_flags`, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResidencyDecision {
    /// No resident shares this model's modelKey — load fresh.
    LoadFresh,
    /// A resident darkmux may manage already satisfies the ctx requirement —
    /// nothing to load. Carries the resident's identity + actual ctx so the
    /// caller can leave a declared-vs-actual breadcrumb when they diverge
    /// (interim provenance until #1257 lands).
    Reuse { identifier: String, resident_ctx: u64 },
    /// A resident darkmux may manage shares the modelKey but is loaded at an
    /// insufficient ctx — unload it, then load fresh at the required ctx.
    /// Silently reusing the wrong ctx is the #1135 bug class and is not an
    /// option; attempting a second concurrent `lms load` of the same
    /// weights is the #1271 bug class (OOM) and isn't either.
    Reconcile { stale_identifier: String, stale_ctx: u64 },
    /// A resident shares the modelKey but is NOT one darkmux may manage —
    /// operator state the cycler must never touch. Fail loud before
    /// spending a load attempt LMStudio's own guardrail would refuse anyway.
    Blocked { resident_identifier: String },
}

/// Inspect `loaded` for a resident sharing `pm`'s modelKey (`LoadedModel::model`,
/// the `lms ps` field derived from `modelKey`/`model`/`id` — see
/// `lms::model_from_json`) and decide what `ensure_loaded` should do.
/// Matching on modelKey rather than the darkmux-namespaced identifier is the
/// point: two different profiles/crews can reference the SAME catalog model
/// under different identifiers (or different `n_ctx`), and LMStudio can't
/// hold two full concurrent loads of the same weights on a RAM-constrained
/// machine — the identifier-only check missed that collision and let a
/// doomed second `lms load` reach LMStudio's own OOM guardrail (#1271).
///
/// A resident counts as darkmux's own when its identifier is in the
/// `darkmux:` namespace OR equals the exact identifier THIS call would load
/// under — the second arm covers a `ProfileModel.identifier` explicit alias,
/// the documented namespace opt-out (`swap::namespaced_identifier` passes it
/// through verbatim), whose resident must not misclassify as foreign user
/// state and get Blocked against darkmux's own load.
///
/// Multiple residents sharing the modelKey: the FIRST match (in `lms ps`
/// order) decides. A first-match user-owned resident blocks even when a
/// darkmux-owned instance also sits further down the list — the operator's
/// copy of the weights is resident either way, and any load attempt in that
/// state still risks the double-footprint LMStudio's guardrail refuses.
fn decide_residency(loaded: &[darkmux_types::LoadedModel], pm: &ProfileModel) -> ResidencyDecision {
    let Some(found) = loaded.iter().find(|l| l.model == pm.id) else {
        return ResidencyDecision::LoadFresh;
    };
    let own_identifier = swap::namespaced_identifier(pm);
    let ours = swap::is_darkmux_owned(&found.identifier) || found.identifier == own_identifier;
    if !ours {
        return ResidencyDecision::Blocked { resident_identifier: found.identifier.clone() };
    }
    if swap::ctx_sufficient(found.context, pm.n_ctx) {
        ResidencyDecision::Reuse {
            identifier: found.identifier.clone(),
            resident_ctx: found.context,
        }
    } else {
        ResidencyDecision::Reconcile {
            stale_identifier: found.identifier.clone(),
            stale_ctx: found.context,
        }
    }
}

impl ModelCycler for LmsCycler {
    fn ensure_loaded(&mut self, pm: &ProfileModel) -> Result<()> {
        let identifier = swap::namespaced_identifier(pm);
        let loaded = lms::list_loaded()?;
        match decide_residency(&loaded, pm) {
            ResidencyDecision::Reuse { identifier: resident, resident_ctx } => {
                if resident_ctx > u64::from(pm.n_ctx) {
                    // (#1271 review round) Declared-vs-actual ctx divergence
                    // now happens ACROSS profiles (a bigger load from another
                    // profile satisfies this seat's minimum) — leave a trace
                    // until #1257's full load-config provenance lands.
                    println!(
                        "cycler: reusing {resident} at ctx={resident_ctx} (declared {})",
                        pm.n_ctx
                    );
                }
                Ok(())
            }
            ResidencyDecision::LoadFresh => lms::load_with_identifier(&pm.id, pm.n_ctx, &identifier, true),
            ResidencyDecision::Reconcile { stale_identifier, stale_ctx } => {
                // (#1271) Reconcile rather than attempt a doomed second load:
                // unload the stale-ctx darkmux instance first, matching the
                // style of `swap::swap`'s own unload-then-load logging.
                println!(
                    "cycler: unload {stale_identifier} (was ctx={stale_ctx}) — reconciling to ctx={} for {}",
                    pm.n_ctx, pm.id
                );
                lms::unload(&stale_identifier)?;
                lms::load_with_identifier(&pm.id, pm.n_ctx, &identifier, true)
            }
            ResidencyDecision::Blocked { resident_identifier } => bail!(
                "darkmux: model \"{}\" is already resident under \"{}\", which is NOT darkmux-owned \
                 (user/operator state) — darkmux won't unload it. Free it yourself first: \
                 `darkmux model eject` (if it's actually stale darkmux state under a legacy \
                 identifier) or `lms unload {}`, then re-run.",
                pm.id,
                resident_identifier,
                resident_identifier
            ),
        }
    }

    fn release(&mut self, pm: &ProfileModel) -> Result<()> {
        let identifier = swap::namespaced_identifier(pm);
        if !swap::is_darkmux_owned(&identifier) {
            return Ok(());
        }
        lms::unload(&identifier)
    }
}

// ─── constants ────────────────────────────────────────────────────────────

const PROBE_TEMPERATURE: f32 = 0.2;
const JUDGE_TEMPERATURE: f32 = 0.2;
const DEFAULT_PROBE_MAX_TOKENS: u32 = 4_000;
const DEFAULT_JUDGE_MAX_TOKENS: u32 = 20_000;
const FUNNEL_PROTOCOL: &str = "double-confirm-v1";

/// A hardware-tier concurrency budget for [`resolve_auto`]: the review
/// funnel is a light, occasional dispatch (not throughput-critical
/// infrastructure), so a coarse rule beats a tuned cost model — KISS per
/// CLAUDE.md doctrine. `distinct_models` counts unique model ids across
/// every probe staffing plus the judge — the number that would need to be
/// simultaneously resident under `Parallel`.
fn resolve_auto(distinct_models: usize, hw: &darkmux_hardware::HardwareSpec) -> ExecMode {
    let budget = match hw.ram_tier() {
        darkmux_hardware::RamTier::Xl | darkmux_hardware::RamTier::Large => 3,
        darkmux_hardware::RamTier::Medium => 2,
        darkmux_hardware::RamTier::Small => 1,
    };
    if distinct_models <= budget {
        ExecMode::Parallel
    } else {
        ExecMode::Sequential
    }
}

fn resolve_mode(mode: ExecMode, probes: &[ResolvedSeatStaffing], judge: &ResolvedSeatStaffing) -> ExecMode {
    match mode {
        ExecMode::Auto => {
            let mut ids: Vec<&str> = probes.iter().map(|s| s.pm.id.as_str()).collect();
            ids.push(judge.pm.id.as_str());
            ids.sort_unstable();
            ids.dedup();
            resolve_auto(ids.len(), &darkmux_hardware::detect())
        }
        other => other,
    }
}

// ─── crew validation (funnel-owned seat requirements) ───────────────────

/// Validate `crew` carries what the funnel needs: seat `"review-probe"`
/// with >= 1 staffing, seat `"review-judge"` with EXACTLY 1 staffing.
/// `resolve_crew` (packet 1) validates the crew schema is well-formed and
/// every model resolvable; it deliberately does NOT know about
/// pipeline-specific seat requirements — that's this function's job, and
/// it runs at funnel start so a misconfigured crew fails loud before any
/// dispatch spends a token.
///
/// `pub` (not private) since #1222 Phase B packet 7 review round: the
/// `review-bench --funnel` preflight (`darkmux_lab::lab::review_bench::
/// resolve_funnel_ctx`) calls this directly, ahead of `run_funnel`'s own
/// internal call, so a misconfigured crew fails at bench START (before the
/// per-case loop even begins) rather than at the first case's dispatch.
pub fn validate_funnel_crew(crew: &ResolvedCrew) -> Result<(&Vec<ResolvedSeatStaffing>, &ResolvedSeatStaffing)> {
    let probes = crew
        .seats
        .get("review-probe")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "darkmux: crew \"{}\" is missing seat \"review-probe\" (the review \
                 funnel needs >= 1 staffing) — add one under crews.\"{}\".seats.\"review-probe\"",
                crew.name,
                crew.name
            )
        })?;
    let judges = crew.seats.get("review-judge").ok_or_else(|| {
        anyhow!(
            "darkmux: crew \"{}\" is missing seat \"review-judge\" (the review \
             funnel needs exactly 1 staffing)",
            crew.name
        )
    })?;
    if judges.len() != 1 {
        bail!(
            "darkmux: crew \"{}\" seat \"review-judge\" must have EXACTLY 1 staffing \
             (got {}) — the double-confirm judge is a single seat, unlike \"review-probe\"",
            crew.name,
            judges.len()
        );
    }
    Ok((probes, &judges[0]))
}

// ─── mechanism-family keyword table (for dedup) ──────────────────────────

/// Lowercased alphanumeric word tokens of `text` — the unit
/// [`mechanism_family`] matches on. Splitting on every non-alphanumeric
/// char means `Date.now()` tokenizes as `["date", "now"]` and `copy-paste`
/// as `["copy", "paste"]`, so punctuation variants match without any
/// substring tricks.
fn word_tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// True when `seq` appears in `tokens` as CONSECUTIVE whole tokens.
fn contains_token_seq(tokens: &[String], seq: &[&str]) -> bool {
    !seq.is_empty()
        && tokens.len() >= seq.len()
        && tokens
            .windows(seq.len())
            .any(|w| w.iter().zip(seq).all(|(a, b)| a == b))
}

/// Classify a charge's prose into a coarse mechanism family for dedup —
/// deliberately coarse (a keyword table, not a classifier): dedup only
/// needs "these two flags are probably the same finding," not a precise
/// taxonomy.
///
/// Matching is WHOLE-TOKEN (word-boundary), never substring — the naive
/// `.contains()` form classified "tenant", "covenant", and "finance" as
/// `null/nan` (all contain "nan"), so two DISTINCT unanchored charges on a
/// billing corpus collapsed in dedup and a real defect was silently
/// dropped (frontier QA should-fix on this packet's PR). Plural/variant
/// forms are listed explicitly rather than stemmed — transparent beats
/// clever for a table this small.
fn mechanism_family(charge_text: &str) -> &'static str {
    const TABLE: &[(&str, &[&[&str]])] = &[
        (
            "timezone/ambient-time",
            &[
                &["timezone"],
                &["timezones"],
                &["time", "zone"],
                &["time", "zones"],
                &["utc"],
                &["date", "now"],
                &["new", "date"],
                &["ambient", "time"],
                &["local", "time"],
                &["dst"],
                &["daylight", "saving"],
                &["daylight", "savings"],
            ],
        ),
        (
            "arity/param",
            &[
                &["argument"],
                &["arguments"],
                &["arg"],
                &["args"],
                &["parameter"],
                &["parameters"],
                &["param"],
                &["params"],
                &["arity"],
                &["wrong", "number", "of"],
            ],
        ),
        (
            "null/nan",
            &[&["null"], &["undefined"], &["nan"], &["none"], &["nil"]],
        ),
        (
            "async/await",
            &[
                &["async"],
                &["await"],
                &["promise"],
                &["promises"],
                &["race", "condition"],
                &["event", "loop"],
                &["callback"],
                &["callbacks"],
                &["unhandled", "rejection"],
            ],
        ),
        (
            "provenance/sibling",
            &[
                &["sibling"],
                &["siblings"],
                &["duplicate", "logic"],
                &["other", "implementation"],
                &["diverge"],
                &["diverges"],
                &["diverged"],
                &["copy", "paste"],
                &["provenance"],
            ],
        ),
    ];
    let tokens = word_tokens(charge_text);
    for (family, keyword_seqs) in TABLE {
        if keyword_seqs.iter().any(|seq| contains_token_seq(&tokens, seq)) {
            return family;
        }
    }
    "other"
}

// ─── anchor extraction (reuses dialectic's matching discipline) ─────────

/// The first backtick-quoted span in `charge_text` that matches a NEW-side
/// diff line (context or `+`; never a deleted `-` line — an anchor should
/// point at code that still exists). Reuses `super::dialectic`'s
/// normalization (leading `+`/`-` strip, whitespace-collapse fallback for
/// a diff-wrapped logical line) so both matchers share ONE discipline
/// rather than re-deriving the wrapped-line/marker-strip fixes twice —
/// including its [`dialectic::MIN_EVIDENCE_SPAN`] floor, so a trivial
/// span (`0`, `}`) is inline code styling, never an anchor / dedup key.
fn extract_new_side_anchor(charge_text: &str, diff: &str) -> Option<String> {
    use super::dialectic::{
        backtick_spans, collapse_ws, diff_line_content, normalize_anchor, MIN_EVIDENCE_SPAN,
    };
    let new_side_lines: Vec<&str> = diff.lines().filter(|l| !l.starts_with('-')).collect();
    let collapsed = collapse_ws(
        &new_side_lines
            .iter()
            .map(|l| diff_line_content(l))
            .collect::<Vec<_>>()
            .join(" "),
    );
    for span in backtick_spans(charge_text) {
        let a = normalize_anchor(&span);
        if a.trim().len() < MIN_EVIDENCE_SPAN {
            continue;
        }
        let found = new_side_lines.iter().any(|l| diff_line_content(l).contains(a))
            || collapsed.contains(&collapse_ws(a));
        if found {
            return Some(span);
        }
    }
    None
}

// ─── dedup ────────────────────────────────────────────────────────────────

/// Dedup raw probe flags. Key = `(bundle_id, anchor-or-none, mechanism
/// family)` — flags from different members/draws that land on the same key
/// collapse to ONE surviving flag (the first seen, in input order).
/// Anchor extraction (see [`extract_new_side_anchor`]) happens HERE,
/// populating `ProbeFlag::anchor` on the surviving flags — `diff` is why
/// this function needs it.
pub fn dedup_flags(flags: Vec<ProbeFlag>, diff: &str) -> (Vec<ProbeFlag>, DedupStats) {
    let raw = flags.len();
    let mut seen: std::collections::HashSet<(String, Option<String>, &'static str)> =
        std::collections::HashSet::new();
    let mut out = Vec::new();
    for mut f in flags {
        let anchor = extract_new_side_anchor(&f.charge_text, diff);
        let family = mechanism_family(&f.charge_text);
        let key = (f.bundle_id.clone(), anchor.clone(), family);
        if seen.insert(key) {
            f.anchor = anchor;
            out.push(f);
        }
    }
    let deduped = out.len();
    (out, DedupStats { raw, deduped })
}

// ─── judge prompt + ruling parser ────────────────────────────────────────

/// The frozen one-fenced-JSON instruction tail — byte-identical to
/// `judge-runner.py`'s `judge_one` f-string tail (Phase A parity, #1256).
/// No leading blank line of its own; callers that need one add it (see
/// [`judge_prompt`]'s assembly, which needs a bare `\n` before this, not
/// `\n\n`).
const JUDGE_TAIL_INSTRUCTION: &str = "Investigate the flagged item against the code above. End your reply with exactly one fenced JSON block:\n```json\n{\"ruling\": \"confirmed\" | \"needs_check\" | \"false_positive\", \"decisive_evidence\": \"<the specific code line or checked claim that decided it>\", \"note_for_author\": \"<one or two sentences the author reads>\"}\n```";

/// Build the judge's prompt — byte-identical to `judge-runner.py`'s
/// `judge_one`'s `user` f-string assembly, given the same inputs (#1256):
/// the author's stated case (title + description, each independently
/// defaulted/stripped exactly as Python does — see below), the code under
/// review (fenced ```` ```typescript ````, matching the Python template
/// literally), the fact sheet (when non-empty, header + raw `- `-free
/// lines — Phase A's fact sheet has NO bullet prefix, unlike the probe's),
/// the flagged item, then the frozen fenced-JSON instruction tail.
///
/// Phase A has no MANIFEST section (`bundler.py`'s bundles carry no such
/// field and `judge_one` never renders one) — the Rust funnel's `manifest`
/// input is Rust-only and, per the "match Phase A exactly" operator
/// decision (#1256), is DROPPED from this prompt entirely, not silently
/// kept. `BundleInput.manifest` still exists (available to a future
/// synthesis/reporting consumer) — it just never reaches this prompt.
///
/// `intent_title`/`intent_body` mirror `judge_one`'s two SEPARATE inputs
/// (`lab.get('intent_title', '')` / `lab.get('intent_body') or default,
/// .strip()`-ed) rather than one pre-joined string — this is what lets a
/// title-present-body-absent case byte-match Python exactly (title still
/// renders, only the body line defaults), a case a single combined field
/// can't distinguish from "everything blank".
pub fn judge_prompt(intent_title: &str, intent_body: &str, code: &str, facts: &[String], charge: &str) -> String {
    let body = intent_body.trim();
    let body = if body.is_empty() { "(no description provided)" } else { body };
    let mut out = String::new();
    out.push_str("## The author's stated case (the pull request description)\n");
    out.push_str(intent_title);
    out.push('\n');
    out.push_str(body);
    out.push_str("\n\n## The code under review\n```typescript\n");
    out.push_str(code);
    out.push_str("\n```\n");
    if !facts.is_empty() {
        out.push_str("\n## Fact sheet given to the flagging reviewer\n");
        out.push_str(&facts.join("\n"));
        out.push('\n');
    }
    out.push_str("\n## The flagged item to investigate\n");
    out.push_str(charge);
    out.push_str("\n\n");
    out.push_str(JUDGE_TAIL_INSTRUCTION);
    out
}

#[derive(Debug, Deserialize)]
struct RawJudgeRuling {
    ruling: String,
    #[serde(default)]
    decisive_evidence: String,
    #[serde(default)]
    note_for_author: String,
}

/// Candidate JSON substrings, LAST fenced block first (a judge's prose may
/// itself quote code in a fence ahead of its real ruling — trying fences
/// last-to-first, then the whole text, then a first-`{`..last-`}` span,
/// mirrors `dialectic::judge_json_candidates`'s discipline).
fn judge_json_candidates(text: &str) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        let Some(close) = after.find("```") else { break };
        let block = &after[..close];
        let inner = block.strip_prefix("json").unwrap_or(block).trim();
        if !inner.is_empty() {
            chunks.push(inner.to_string());
        }
        rest = &after[close + 3..];
    }
    let mut out: Vec<String> = chunks.into_iter().rev().collect();
    let s = text.trim();
    out.push(s.to_string());
    if let (Some(a), Some(b)) = (s.find('{'), s.rfind('}')) {
        if b > a {
            out.push(s[a..=b].to_string());
        }
    }
    out
}

/// Parse a judge reply into `(ruling, decisive_evidence, note_for_author)`.
/// `None` when no candidate carries a recognized `ruling` value — the
/// caller treats that as [`FunnelRuling::Unparsed`].
pub fn parse_judge_ruling(text: &str) -> Option<(FunnelRuling, String, String)> {
    for cand in judge_json_candidates(text) {
        if let Ok(raw) = serde_json::from_str::<RawJudgeRuling>(&cand) {
            let ruling = match raw.ruling.trim().to_ascii_lowercase().as_str() {
                "confirmed" => FunnelRuling::Confirmed,
                "needs_check" => FunnelRuling::NeedsCheck,
                "false_positive" => FunnelRuling::FalsePositive,
                _ => continue,
            };
            return Some((ruling, raw.decisive_evidence, raw.note_for_author));
        }
    }
    None
}

// ─── bundling (packet 3 seam) ─────────────────────────────────────────────

/// One unit the probe seat examines: a bounded code slice plus its fact
/// sheet. Deliberately THIS module's own shape — see the module doc's
/// "Bundling — the packet 3 seam" section for why, and [`bundles_from_diff`]
/// for the reconciliation point.
#[derive(Debug, Clone)]
pub struct BundleInput {
    pub id: String,
    pub fact_family: String,
    /// The JUDGE seat's code rendering — `bundle::slice_code`'s
    /// `// path (lines a-b)` raw-text format, matching `judge-runner.py`'s
    /// own `slice_code` (#1256).
    pub code: String,
    /// The PROBE seat's code rendering — `bundle::slice_code_probe`'s
    /// ``### `path` (lines a-b)`` + ```` ```typescript ````-fenced blocks,
    /// matching `probe-runner.py`'s `read_code_excerpt` (#1256 correction
    /// round). Phase A formatted the two seats' code DIFFERENTLY; per-seat
    /// parity means carrying both renderings, not unifying them.
    /// [`probe_user_message`] reads this; [`judge_prompt`] reads `code`.
    pub probe_code: String,
    pub facts: Vec<String>,
    /// Symbols referenced but not defined in `code` — a Rust-only addition
    /// Phase A never had (`bundler.py`'s bundles carry no such field). Per
    /// the "match Phase A exactly" operator decision (#1256), [`judge_prompt`]
    /// no longer reads this field — it's dropped from the prompt, not
    /// silently threaded through. Still populated by the real bundler and
    /// kept here for a future synthesis/reporting consumer.
    pub manifest: Vec<String>,
}

/// PROVISIONAL bundler standing in for `darkmux_lab::lab::bundle`'s
/// `Bundle`/`BundleSet`/`build_bundles`/`slice_code`/`external_bundles`/
/// `FileSource` (Phase B packet 3), which had not landed on `main` as of
/// this packet. One [`BundleInput`] per changed file — `code` is that
/// file's diff hunks verbatim; `facts`/`manifest` are empty (both need
/// repo-tree reads the real bundler brings). `fact_family` is always
/// `"unscoped"`, so [`BundleSelector::fact_families`] filtering degrades to
/// "no restriction matches" until real fact families exist.
///
/// **Reconciliation seam**: replace this function's body with
/// `build_bundles`/`slice_code`/`external_bundles`/`FileSource` calls once
/// packet 3 lands (either populating `BundleInput` from the real `Bundle`,
/// or promoting `BundleInput` to a thin wrapper around it). Every other
/// piece of this module is written entirely against `BundleInput` and
/// needs no further changes.
fn bundles_from_diff(diff: &str) -> Vec<BundleInput> {
    let mut out = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_lines: Vec<&str> = Vec::new();
    let flush = |path: &mut Option<String>, lines: &mut Vec<&str>, out: &mut Vec<BundleInput>| {
        if let Some(p) = path.take() {
            if !lines.is_empty() {
                let code = lines.join("\n");
                out.push(BundleInput {
                    id: p,
                    fact_family: "unscoped".to_string(),
                    // Test-only fallback (no repo tree to re-slice from):
                    // both seats see the same hunk text. Production callers
                    // always render `probe_code` via `slice_code_probe`.
                    probe_code: code.clone(),
                    code,
                    facts: Vec::new(),
                    manifest: Vec::new(),
                });
            }
        }
        lines.clear();
    };
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            flush(&mut current_path, &mut current_lines, &mut out);
            current_path = Some(rest.trim().to_string());
        } else if line.starts_with("+++ ") || line.starts_with("--- ") || line.starts_with("diff --git") {
            // File-header noise between hunks — not code.
        } else if current_path.is_some() {
            current_lines.push(line);
        }
    }
    flush(&mut current_path, &mut current_lines, &mut out);
    out
}

/// (#1222 Phase B packet 5 reconciliation) `inputs.bundles` when the caller
/// supplied real ones (production), else the provisional [`bundles_from_diff`]
/// (this module's own pre-packet-3 tests only — see [`FunnelInputs::bundles`]).
fn resolve_bundles(inputs: &FunnelInputs) -> Vec<BundleInput> {
    match &inputs.bundles {
        Some(b) => b.clone(),
        None => bundles_from_diff(inputs.diff),
    }
}

/// A staffing with a `bundle_selector` runs only on bundles whose
/// `fact_family` is named in `fact_families` (empty `fact_families` = no
/// restriction), capped at `max_bundles`, prioritizing `"param-flow"`
/// bundles first (stable order otherwise — Rust's `sort_by_key` is a
/// stable sort). A staffing with no selector runs on every bundle.
fn select_bundles_for_staffing<'a>(
    bundles: &'a [BundleInput],
    selector: Option<&BundleSelector>,
) -> Vec<&'a BundleInput> {
    let Some(sel) = selector else {
        return bundles.iter().collect();
    };
    let mut matched: Vec<&BundleInput> = bundles
        .iter()
        .filter(|b| sel.fact_families.is_empty() || sel.fact_families.iter().any(|f| f == &b.fact_family))
        .collect();
    matched.sort_by_key(|b| if b.fact_family == "param-flow" { 0u8 } else { 1u8 });
    if let Some(max) = sel.max_bundles {
        matched.truncate(max as usize);
    }
    matched
}

// ─── dispatch primitive ───────────────────────────────────────────────────

/// One single-shot chat call the funnel wants dispatched. Test closures
/// assert on these fields directly; production wiring turns this into a
/// `darkmux_crew::single_shot::SingleShotRequest` (the caller resolves
/// `base_url`).
pub struct ChatCall<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub user: &'a str,
    pub temperature: f32,
    pub max_tokens: u32,
}

// ─── funnel inputs ────────────────────────────────────────────────────────

/// Everything [`run_funnel`]/[`run_judge_only`] need beyond the injected
/// `chat`/`cycler`. Role-prompt resolution (`review-probe.md` /
/// `review-judge.md`) is the caller's job — `darkmux-lab` already depends
/// on `darkmux-crew`, but pulling role-manifest resolution INTO this
/// module would couple the pure pipeline to `darkmux_crew::loader`'s
/// filesystem/embedded-role search order for no benefit the caller
/// couldn't provide more simply.
pub struct FunnelInputs<'a> {
    pub case_id: String,
    pub crew: &'a ResolvedCrew,
    /// The author's stated case (PR title). Fed into [`judge_prompt`] only
    /// — Phase A never showed the probe seat the intent (#1256), so
    /// [`probe_user_message`] never reads this field.
    pub intent_title: &'a str,
    /// The author's stated case (PR description). Same [`judge_prompt`]-
    /// only scope as `intent_title` — see its doc comment.
    pub intent_body: &'a str,
    pub diff: &'a str,
    pub mode: ExecMode,
    /// The probe seat's PRIOR text (`review-probe.md`) — injected as the
    /// FIRST line of the probe's user message (#1256's `probe_user_message`
    /// assembly), never as a system-role message: Phase A's probe protocol
    /// (`probe-runner.py`'s `call_model`) sends ONE user-role message with
    /// no system message at all, and [`dispatch_probe_staffing`] now sends
    /// an empty `ChatCall::system` for probe calls to match (which
    /// `darkmux_crew::single_shot::local_chat_body` then omits from the
    /// wire entirely).
    pub probe_system: &'a str,
    /// The judge seat's PERSONA — still sent as a genuine system-role
    /// message (`judge-runner.py`'s `call_judge` does the same).
    pub judge_system: &'a str,
    /// (#1222 Phase B packet 5 reconciliation) Caller-supplied bundles from
    /// the REAL bundler (`darkmux_lab::lab::bundle::build_bundles`/
    /// `external_bundles`, packet 3), already mapped `Bundle` ->
    /// [`BundleInput`] (via `slice_code` for the code text). `None` falls
    /// back to the provisional [`bundles_from_diff`] — kept ONLY so this
    /// module's own tests (written before packet 3 landed) keep working
    /// unchanged. Production callers (`darkmux pr-review run`, packet 5)
    /// always pass `Some` and never invoke the provisional bundler.
    pub bundles: Option<Vec<BundleInput>>,
}

fn fingerprint(judge_identifier: &str, judge_system: &str) -> serde_json::Value {
    serde_json::json!({
        "judge_model": judge_identifier,
        "judge_temperature": JUDGE_TEMPERATURE,
        "judge_persona_blake3": blake3::hash(judge_system.as_bytes()).to_hex().to_string(),
        "protocol": FUNNEL_PROTOCOL,
    })
}

// ─── probe phase ──────────────────────────────────────────────────────────

/// Build the probe's user message — byte-identical to `probe-runner.py`'s
/// `build_prompt`, given the same inputs (#1256): `prior` (the seat's
/// review-probe.md text, standing in for Python's hardcoded `STRONG_PRIOR`
/// — see the golden test's provenance comment for how the two relate)
/// first, a blank line, `Code:`, a blank line, the code section
/// (`bundle.probe_code` — `read_code_excerpt`-format blocks:
/// ``### `path` (lines a-b)`` + ```` ```typescript ```` fences, joined by
/// blank lines, rendered by `bundle::slice_code_probe`; the PROBE format,
/// distinct from the judge's `// path` raw format in `bundle.code`), then
/// IF facts: a blank line, the fact-sheet header, a blank line, `- fact`
/// lines. Deliberately NO intent anywhere in this prompt — Phase A's
/// `build_prompt` never saw one; `FunnelInputs::intent_title`/
/// `intent_body` are dropped here on purpose (kept for [`judge_prompt`]
/// only), not silently threaded through.
fn probe_user_message(prior: &str, bundle: &BundleInput) -> String {
    let mut parts: Vec<String> =
        vec![prior.to_string(), String::new(), "Code:".to_string(), String::new(), bundle.probe_code.clone()];
    if !bundle.facts.is_empty() {
        parts.push(String::new());
        parts.push("Computed facts about this code (mechanically extracted, not interpreted):".to_string());
        parts.push(String::new());
        parts.push(bundle.facts.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n"));
    }
    parts.join("\n")
}

/// One probe draw, retried once on empty content, then skipped (`Ok(None)`)
/// — never recorded as a flag. A dispatch-level `Err` propagates
/// immediately (the shared single-shot primitive already carries its own
/// backoff/retry — a second-guessing retry here would be redundant AND
/// would hide a real infra problem behind a "skipped" label).
fn probe_one_draw(
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
) -> Result<Option<(String, u64)>> {
    for _ in 0..2 {
        let call = ChatCall {
            model,
            system,
            user,
            temperature: PROBE_TEMPERATURE,
            max_tokens,
        };
        let reply = chat(&call)?;
        let trimmed = reply.content.trim();
        if !trimmed.is_empty() {
            return Ok(Some((trimmed.to_string(), reply.total_tokens.unwrap_or(0))));
        }
    }
    Ok(None)
}

/// One probe seat's dispatch — a `funnel.step` pair (`step_id =
/// "probe:<staffing-name>"`, #1247 Part 1) brackets the seat's whole
/// draw loop so a live ticker sees per-seat progress inside a multi-seat
/// probe phase, not just the phase-level aggregate `probe_phase` records.
fn dispatch_probe_staffing(
    s: &ResolvedSeatStaffing,
    bundles: &[BundleInput],
    inputs: &FunnelInputs,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
    flags: &mut Vec<ProbeFlag>,
    guard: &mut FunnelBookendGuard<'_>,
) -> Result<MemberRecord> {
    let identifier = swap::namespaced_identifier(&s.pm);
    let max_tokens = s.max_tokens.unwrap_or(DEFAULT_PROBE_MAX_TOKENS);
    let selected = select_bundles_for_staffing(bundles, s.selector.as_ref());
    let step_id = format!("probe:{}", s.name);
    let draws_total = selected.len() as u32 * s.k;
    guard.step_started(
        &step_id,
        "dispatch",
        json!({
            "step_id": step_id, "kind": "dispatch", "status": "started",
            "items_in": selected.len(), "items_out": 0, "wall_ms": 0,
            "model": identifier, "draws_done": 0, "draws_total": draws_total,
        }),
    );
    let t0 = Instant::now();
    let mut draws = 0u32;
    let mut tokens = 0u64;
    let flags_before = flags.len();
    for bundle in &selected {
        let user = probe_user_message(inputs.probe_system, bundle);
        for draw in 0..s.k {
            draws += 1;
            // Empty system — Phase A parity (#1256): probe-runner.py's
            // `call_model` sends ONE user-role message, no system message
            // at all. `single_shot::local_chat_body` omits the system
            // entry entirely when it's empty, so this is the wire-level
            // no-system-message behavior, not a system message with blank
            // content.
            if let Some((text, tok)) = probe_one_draw(chat, &identifier, "", &user, max_tokens)? {
                tokens += tok;
                flags.push(ProbeFlag {
                    bundle_id: bundle.id.clone(),
                    fact_family: bundle.fact_family.clone(),
                    member: identifier.clone(),
                    draw,
                    charge_text: text,
                    anchor: None,
                });
            }
        }
    }
    let wall_ms = t0.elapsed().as_millis() as u64;
    let flags_produced = flags.len() - flags_before;
    guard.step_finished(
        &step_id,
        json!({
            "step_id": step_id, "kind": "dispatch", "status": "finished",
            "items_in": selected.len(), "items_out": flags_produced, "wall_ms": wall_ms,
            "model": identifier, "draws_done": draws, "draws_total": draws_total, "tokens": tokens,
        }),
    );
    Ok(MemberRecord {
        model: identifier,
        seat: "review-probe".to_string(),
        draws,
        wall_ms,
        total_tokens: tokens,
    })
}

#[allow(clippy::too_many_arguments)]
fn probe_phase(
    bundles: &[BundleInput],
    probes: &[ResolvedSeatStaffing],
    inputs: &FunnelInputs,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
    cycler: &mut dyn ModelCycler,
    members: &mut Vec<MemberRecord>,
    mode: ExecMode,
    guard: &mut FunnelBookendGuard<'_>,
) -> Result<Vec<ProbeFlag>> {
    let mut flags = Vec::new();
    if mode == ExecMode::Parallel {
        for s in probes {
            cycler.ensure_loaded(&s.pm)?;
        }
        for s in probes {
            members.push(dispatch_probe_staffing(s, bundles, inputs, chat, &mut flags, guard)?);
        }
        for s in probes {
            cycler.release(&s.pm)?;
        }
    } else {
        // Sequential (the only other resolved mode by the time this runs —
        // `resolve_mode` never leaves `Auto` unresolved): load member → all
        // its draws → release → next.
        for s in probes {
            cycler.ensure_loaded(&s.pm)?;
            members.push(dispatch_probe_staffing(s, bundles, inputs, chat, &mut flags, guard)?);
            cycler.release(&s.pm)?;
        }
    }
    Ok(flags)
}

// ─── judge phase (double-confirm) ─────────────────────────────────────────

fn run_judge_pass(
    pass: u8,
    model: &str,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
) -> (JudgeRecord, u64) {
    let t0 = Instant::now();
    let call = ChatCall {
        model,
        system,
        user: prompt,
        temperature: JUDGE_TEMPERATURE,
        max_tokens,
    };
    match chat(&call) {
        Ok(reply) => {
            let seconds = t0.elapsed().as_secs_f64();
            let tokens = reply.total_tokens.unwrap_or(0);
            match parse_judge_ruling(&reply.content) {
                Some((ruling, decisive_evidence, note_for_author)) => (
                    JudgeRecord { ruling, decisive_evidence, note_for_author, pass, seconds },
                    tokens,
                ),
                None => (
                    JudgeRecord {
                        ruling: FunnelRuling::Unparsed,
                        decisive_evidence: String::new(),
                        note_for_author: String::new(),
                        pass,
                        seconds,
                    },
                    tokens,
                ),
            }
        }
        // A dispatch-level failure is recorded as `Error`, not propagated —
        // one bad judge call must not abort the whole docket (the funnel's
        // job is to be loud PER-FLAG, not to be fragile).
        Err(_) => (
            JudgeRecord {
                ruling: FunnelRuling::Error,
                decisive_evidence: String::new(),
                note_for_author: String::new(),
                pass,
                seconds: t0.elapsed().as_secs_f64(),
            },
            0,
        ),
    }
}

/// One judge pass's resource accounting alongside its surviving record:
/// tokens spent, wall time, and the number of ACTUAL dispatches made
/// (2 when the unparsed-retry fired, else 1) — the member/step telemetry
/// counts real calls, not logical passes (frontier QA minor on this
/// packet's PR).
struct PassOutcome {
    record: JudgeRecord,
    tokens: u64,
    wall_ms: u64,
    calls: u32,
}

/// One judge pass, retried ONCE if the reply was [`FunnelRuling::Unparsed`]
/// (the retry keeps the same `pass` number — a retried pass-1 is still
/// pass-1, just a second attempt at it). Still unparsed after the retry:
/// the retry's record survives (the first attempt's record is discarded,
/// not hidden — it added no information a clean retry didn't already
/// supersede). Tokens/wall/calls account for BOTH attempts.
fn judge_pass_with_retry(
    pass: u8,
    model: &str,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
) -> PassOutcome {
    let t0 = Instant::now();
    let (r1, t1) = run_judge_pass(pass, model, system, prompt, max_tokens, chat);
    if r1.ruling == FunnelRuling::Unparsed {
        let (r2, t2) = run_judge_pass(pass, model, system, prompt, max_tokens, chat);
        PassOutcome {
            record: r2,
            tokens: t1 + t2,
            wall_ms: t0.elapsed().as_millis() as u64,
            calls: 2,
        }
    } else {
        PassOutcome {
            record: r1,
            tokens: t1,
            wall_ms: t0.elapsed().as_millis() as u64,
            calls: 1,
        }
    }
}

/// One flag's full double-confirm outcome, with per-pass resource
/// accounting so the envelope's `judge-pass1` / `judge-pass2` step rows
/// carry HONEST per-pass wall times (an all-confirm docket previously
/// booked its whole elapsed under pass-2, reading as pass-1 = 0ms).
struct JudgeOutcome {
    pass1: JudgeRecord,
    pass2: Option<JudgeRecord>,
    tier: Tier,
    demoted_by_pass2: bool,
    tokens: u64,
    pass1_ms: u64,
    pass2_ms: u64,
    /// Actual dispatches made across both passes, unparsed retries
    /// included.
    calls: u32,
}

/// The double-confirm state machine for one flag: pass-1 (with the
/// unparsed-retry above) always runs; a `confirmed` pass-1 gets a pass-2
/// (also with the retry) — agreement → [`Tier::Confirmed`]; ANY other
/// pass-2 outcome (needs_check, false_positive, unparsed, error) demotes
/// to [`Tier::NeedsCheck`], never silently to `confirmed`. A non-confirmed
/// pass-1 needs no pass-2: `needs_check` stays `NeedsCheck`; everything
/// else (`false_positive`, `unparsed`, `error`) is `Archived` — the
/// specific ruling is still preserved on the record (loud), just tiered
/// out of the author-facing report.
fn judge_one_flag(
    prompt: &str,
    model: &str,
    system: &str,
    max_tokens: u32,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
) -> JudgeOutcome {
    let p1 = judge_pass_with_retry(1, model, system, prompt, max_tokens, chat);
    match p1.record.ruling {
        FunnelRuling::Confirmed => {
            let p2 = judge_pass_with_retry(2, model, system, prompt, max_tokens, chat);
            let (tier, demoted) = if p2.record.ruling == FunnelRuling::Confirmed {
                (Tier::Confirmed, false)
            } else {
                (Tier::NeedsCheck, true)
            };
            JudgeOutcome {
                pass1: p1.record,
                pass2: Some(p2.record),
                tier,
                demoted_by_pass2: demoted,
                tokens: p1.tokens + p2.tokens,
                pass1_ms: p1.wall_ms,
                pass2_ms: p2.wall_ms,
                calls: p1.calls + p2.calls,
            }
        }
        FunnelRuling::NeedsCheck => JudgeOutcome {
            tier: Tier::NeedsCheck,
            demoted_by_pass2: false,
            tokens: p1.tokens,
            pass1_ms: p1.wall_ms,
            pass2_ms: 0,
            calls: p1.calls,
            pass1: p1.record,
            pass2: None,
        },
        FunnelRuling::FalsePositive | FunnelRuling::Unparsed | FunnelRuling::Error => JudgeOutcome {
            tier: Tier::Archived,
            demoted_by_pass2: false,
            tokens: p1.tokens,
            pass1_ms: p1.wall_ms,
            pass2_ms: 0,
            calls: p1.calls,
            pass1: p1.record,
            pass2: None,
        },
    }
}

// ─── shared finish (probe→dedup→judge→envelope), reused by run_judge_only ─

#[allow(clippy::too_many_arguments)]
fn finish_funnel(
    mut env: FunnelEnvelope,
    raw_flags: Vec<ProbeFlag>,
    bundles: &[BundleInput],
    inputs: &FunnelInputs,
    judge: &ResolvedSeatStaffing,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
    cycler: &mut dyn ModelCycler,
    guard: &mut FunnelBookendGuard<'_>,
) -> Result<FunnelEnvelope> {
    env.raw_flags = raw_flags.len();

    let t_dedup = Instant::now();
    let (deduped, _stats) = dedup_flags(raw_flags, inputs.diff);
    let dedup_ms = t_dedup.elapsed().as_millis() as u64;
    env.steps.push(StepRecord {
        step_id: "dedup".to_string(),
        kind: "procedural".to_string(),
        items_in: env.raw_flags,
        items_out: deduped.len(),
        wall_ms: dedup_ms,
    });
    guard.step_finished(
        "dedup",
        json!({
            "step_id": "dedup", "kind": "procedural", "status": "finished",
            "items_in": env.raw_flags, "items_out": deduped.len(), "wall_ms": dedup_ms,
        }),
    );
    env.deduped_flags = deduped.len();

    let judge_identifier = swap::namespaced_identifier(&judge.pm);
    let judge_max_tokens = judge.max_tokens.unwrap_or(DEFAULT_JUDGE_MAX_TOKENS);

    cycler.ensure_loaded(&judge.pm)?;
    guard.step_started(
        "judge-pass1",
        "dispatch",
        json!({
            "step_id": "judge-pass1", "kind": "dispatch", "status": "started",
            "items_in": deduped.len(), "items_out": 0, "wall_ms": 0,
        }),
    );
    let mut judged = Vec::with_capacity(deduped.len());
    let mut pass1_ms = 0u64;
    let mut pass2_ms = 0u64;
    let mut pass2_flags = 0usize;
    let mut judge_calls = 0u32;
    let mut judge_tokens = 0u64;
    for flag in &deduped {
        let bundle = bundles.iter().find(|b| b.id == flag.bundle_id);
        let code = bundle.map(|b| b.code.as_str()).unwrap_or_default();
        let facts: &[String] = bundle.map(|b| b.facts.as_slice()).unwrap_or_default();
        let prompt = judge_prompt(inputs.intent_title, inputs.intent_body, code, facts, &flag.charge_text);
        let outcome =
            judge_one_flag(&prompt, &judge_identifier, inputs.judge_system, judge_max_tokens, chat);
        judge_tokens += outcome.tokens;
        judge_calls += outcome.calls;
        pass1_ms += outcome.pass1_ms;
        pass2_ms += outcome.pass2_ms;
        // The per-ruling ticker (#1247 Part 1) — one record per judge
        // dispatch outcome, emitted BEFORE `outcome`'s fields move into the
        // `JudgedFlag` below.
        guard.ruling(json!({
            "bundle_id": flag.bundle_id, "pass": 1,
            "ruling": outcome.pass1.ruling, "seconds": outcome.pass1.seconds,
        }));
        if let Some(p2) = &outcome.pass2 {
            pass2_flags += 1;
            if pass2_flags == 1 {
                // A `confirmed` pass-1 gets its pass-2 ruling immediately,
                // interleaved within THIS per-flag loop (see the module
                // doc) — so the `judge-pass2` step must open the moment the
                // FIRST pass-2 ruling actually fires, not once the whole
                // docket is known. Opening it only after the loop finishes
                // (the prior behavior) let `funnel.ruling{pass:2}` records
                // stream to a live observer while the `judge-pass2` step
                // still read "not started" — a contradiction observed live
                // in the lab lens. `items_in` here is the running count (1
                // so far); `step_finished` below reports the real final
                // docket size.
                guard.step_started(
                    "judge-pass2",
                    "dispatch",
                    json!({
                        "step_id": "judge-pass2", "kind": "dispatch", "status": "started",
                        "items_in": pass2_flags, "items_out": 0, "wall_ms": 0,
                    }),
                );
            }
            guard.ruling(json!({
                "bundle_id": flag.bundle_id, "pass": 2,
                "ruling": p2.ruling, "seconds": p2.seconds,
            }));
        }
        judged.push(JudgedFlag {
            flag: flag.clone(),
            pass1: outcome.pass1,
            pass2: outcome.pass2,
            tier: outcome.tier,
            demoted_by_pass2: outcome.demoted_by_pass2,
        });
    }
    cycler.release(&judge.pm)?;

    env.members.push(MemberRecord {
        model: judge_identifier,
        seat: "review-judge".to_string(),
        // Actual dispatches, unparsed retries included — never fewer calls
        // than the operator paid for.
        draws: judge_calls,
        wall_ms: pass1_ms + pass2_ms,
        total_tokens: judge_tokens,
    });
    env.steps.push(StepRecord {
        step_id: "judge-pass1".to_string(),
        kind: "dispatch".to_string(),
        items_in: deduped.len(),
        items_out: deduped.len(),
        wall_ms: pass1_ms,
    });
    guard.step_finished(
        "judge-pass1",
        json!({
            "step_id": "judge-pass1", "kind": "dispatch", "status": "finished",
            "items_in": deduped.len(), "items_out": deduped.len(), "wall_ms": pass1_ms,
        }),
    );
    if pass2_flags > 0 {
        env.steps.push(StepRecord {
            step_id: "judge-pass2".to_string(),
            kind: "dispatch".to_string(),
            items_in: pass2_flags,
            items_out: pass2_flags,
            wall_ms: pass2_ms,
        });
        // `started` was already emitted above, in the per-flag loop, the
        // moment the first pass-2 ruling fired — this only closes it, now
        // that the loop has finished and the real final docket size +
        // elapsed `wall_ms` are known.
        guard.step_finished(
            "judge-pass2",
            json!({
                "step_id": "judge-pass2", "kind": "dispatch", "status": "finished",
                "items_in": pass2_flags, "items_out": pass2_flags, "wall_ms": pass2_ms,
            }),
        );
    }

    env.confirmed = judged.iter().filter(|j| j.tier == Tier::Confirmed).count();
    env.needs_check = judged.iter().filter(|j| j.tier == Tier::NeedsCheck).count();
    env.archived = judged.iter().filter(|j| j.tier == Tier::Archived).count();

    // The judge-dead honesty gate (#1222 packet 5 review): per-flag judge
    // failures are deliberately swallowed to `Error`/`Unparsed` →
    // `Tier::Archived` (one bad call must not abort the docket), but when
    // NO flag got a usable pass-1 ruling the whole judge phase produced no
    // signal — confirmed=0/needs_check=0 would render downstream as an
    // honest-looking "none confirmed" green comment while the judge was
    // dead or off-contract the entire run. Mark the envelope degenerate so
    // synthesis routes it to "degraded" (the workflow's exit-1 path). A
    // genuine all-false-positive docket has usable rulings and keeps the
    // honest comment.
    let usable = judged
        .iter()
        .filter(|j| {
            matches!(
                j.pass1.ruling,
                FunnelRuling::Confirmed | FunnelRuling::NeedsCheck | FunnelRuling::FalsePositive
            )
        })
        .count();
    if !judged.is_empty() && usable == 0 {
        env.degenerate = Some(format!(
            "judge produced no usable ruling on any of {} flags (all errored/unparsed)",
            judged.len()
        ));
    }

    env.flags = deduped;
    env.judged = judged;
    guard.task_finished(&env);
    Ok(env)
}

// ─── the driver ───────────────────────────────────────────────────────────

/// Run the full funnel: bundles → probe(k draws × seat) → dedup →
/// double-confirm judge → envelope. `chat` performs one single-shot
/// dispatch and returns its reply (the closure owns model/base-URL
/// resolution — tests script it; production wiring calls
/// `darkmux_crew::single_shot::single_shot_chat`). `cycler` loads/releases
/// models around the dispatches (production: [`LmsCycler`]; tests: a
/// recording mock).
///
/// Also starts the run's host telemetry sampler (#1247 doctrine surface) —
/// see [`FunnelBookendGuard`]/[`HostTelemetrySampler`] — at the production
/// cadence ([`FUNNEL_TELEMETRY_INTERVAL`]) with the real
/// `darkmux_crew::telemetry_sampler::sample_host`.
/// [`run_funnel_with_telemetry`] is the test-only seam for a faster
/// cadence and an injected sampling function.
pub fn run_funnel(
    inputs: &FunnelInputs,
    mut chat: impl FnMut(&ChatCall) -> Result<SingleShotReply>,
    cycler: &mut dyn ModelCycler,
    emitter: &mut dyn FunnelEmitter,
) -> Result<FunnelEnvelope> {
    run_funnel_impl(
        inputs,
        &mut chat,
        cycler,
        emitter,
        FUNNEL_TELEMETRY_INTERVAL,
        FUNNEL_TELEMETRY_POLL,
        sample_host,
    )
}

/// Test-only seam: identical pipeline to [`run_funnel`], but with a
/// caller-chosen telemetry cadence AND sampling function, so a scripted
/// test can observe deterministic host-telemetry samples without a
/// multi-second sleep and without shelling to the real macOS-only host
/// commands (hermetic — no subprocess timing to race on a CI runner).
/// No production caller uses this — `run_funnel` always fixes the cadence
/// at [`FUNNEL_TELEMETRY_INTERVAL`] and the sampler at the real
/// `sample_host`.
#[cfg(test)]
fn run_funnel_with_telemetry(
    inputs: &FunnelInputs,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
    cycler: &mut dyn ModelCycler,
    emitter: &mut dyn FunnelEmitter,
    telemetry_interval: Duration,
    telemetry_poll: Duration,
    sample_fn: fn() -> HostSample,
) -> Result<FunnelEnvelope> {
    run_funnel_impl(inputs, chat, cycler, emitter, telemetry_interval, telemetry_poll, sample_fn)
}

fn run_funnel_impl(
    inputs: &FunnelInputs,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
    cycler: &mut dyn ModelCycler,
    emitter: &mut dyn FunnelEmitter,
    telemetry_interval: Duration,
    telemetry_poll: Duration,
    sample_fn: fn() -> HostSample,
) -> Result<FunnelEnvelope> {
    let (probes, judge) = validate_funnel_crew(inputs.crew)?;
    let mode = resolve_mode(inputs.mode, probes, judge);

    let t_bundle = Instant::now();
    let bundles = resolve_bundles(inputs);
    let bundle_ms = t_bundle.elapsed().as_millis() as u64;

    let mut env = FunnelEnvelope {
        case_id: inputs.case_id.clone(),
        crew: inputs.crew.name.clone(),
        mode: mode_label(mode).to_string(),
        bundles: bundles.len(),
        // Stamped up front so DEGENERATE envelopes (zero bundles / zero
        // flags) carry the same comparability key as a full run — a
        // Null fingerprint on an early return would make the degenerate
        // record untraceable to its judge config.
        fingerprint: fingerprint(&swap::namespaced_identifier(&judge.pm), inputs.judge_system),
        // (#1247) The resolved staffing this run actually used, post any
        // caller-applied `--k` override — see `FunnelEnvelope::staffing`.
        staffing: Some(staffing_snapshot(probes, judge)),
        ..Default::default()
    };
    // `funnel.task` started (#1247 Part 1) — run started: case id, crew
    // name, exec mode, bundle count. From here every emission routes
    // through the bookend guard, which ARMS on this record: an early
    // `?`-return or panic below fires its Drop path (open steps closed with
    // `status: "error"`, then a terminal error task record) so no consumer
    // ever sees an orphaned `started`.
    let mut guard = FunnelBookendGuard::new_with_telemetry(
        emitter,
        &inputs.case_id,
        &inputs.crew.name,
        telemetry_interval,
        telemetry_poll,
        sample_fn,
    );
    guard.task_started(json!({
        "status": "started", "case_id": inputs.case_id, "crew": inputs.crew.name,
        "exec_mode": mode_label(mode), "bundles": bundles.len(),
    }));
    env.steps.push(StepRecord {
        step_id: "bundle".to_string(),
        kind: "procedural".to_string(),
        items_in: 1,
        items_out: bundles.len(),
        wall_ms: bundle_ms,
    });
    guard.step_finished(
        "bundle",
        json!({
            "step_id": "bundle", "kind": "procedural", "status": "finished",
            "items_in": 1, "items_out": bundles.len(), "wall_ms": bundle_ms,
        }),
    );
    if bundles.is_empty() {
        env.degenerate = Some("no bundles produced from the diff".to_string());
        guard.task_finished(&env);
        return Ok(env);
    }

    let t_probe = Instant::now();
    guard.step_started(
        "probe",
        "dispatch",
        json!({
            "step_id": "probe", "kind": "dispatch", "status": "started",
            "items_in": bundles.len(), "items_out": 0, "wall_ms": 0,
        }),
    );
    let raw_flags = probe_phase(&bundles, probes, inputs, chat, cycler, &mut env.members, mode, &mut guard)
        .context("review funnel: probe phase")?;
    let probe_ms = t_probe.elapsed().as_millis() as u64;
    env.steps.push(StepRecord {
        step_id: "probe".to_string(),
        kind: "dispatch".to_string(),
        items_in: bundles.len(),
        items_out: raw_flags.len(),
        wall_ms: probe_ms,
    });
    guard.step_finished(
        "probe",
        json!({
            "step_id": "probe", "kind": "dispatch", "status": "finished",
            "items_in": bundles.len(), "items_out": raw_flags.len(), "wall_ms": probe_ms,
        }),
    );
    if raw_flags.is_empty() {
        env.raw_flags = 0;
        env.degenerate = Some("zero flags from all probe draws — never a silent pass".to_string());
        guard.task_finished(&env);
        return Ok(env);
    }

    finish_funnel(env, raw_flags, &bundles, inputs, judge, chat, cycler, &mut guard)
}

/// Re-judge a previously-recorded flag list without re-running the probe
/// (the `--charges-file` entry point). Still dedups (a hand-edited or
/// concatenated charges file may carry raw, undeduped flags) and still
/// rebuilds bundles from `inputs.diff` — the judge needs the code each
/// flag's `bundle_id` refers to, and flags alone don't carry it.
pub fn run_judge_only(
    flags: Vec<ProbeFlag>,
    inputs: &FunnelInputs,
    mut chat: impl FnMut(&ChatCall) -> Result<SingleShotReply>,
    cycler: &mut dyn ModelCycler,
    emitter: &mut dyn FunnelEmitter,
) -> Result<FunnelEnvelope> {
    let (probes, judge) = validate_funnel_crew(inputs.crew)?;
    // Judge-only runs one model, so the mode is telemetry, not behavior —
    // but the envelope still records the CALLER's resolved mode rather
    // than a hardcoded label, so a judge-only re-run of a parallel funnel
    // doesn't misreport its provenance.
    let mode = resolve_mode(inputs.mode, probes, judge);

    let t_bundle = Instant::now();
    let bundles = resolve_bundles(inputs);
    let bundle_ms = t_bundle.elapsed().as_millis() as u64;

    let mut env = FunnelEnvelope {
        case_id: inputs.case_id.clone(),
        crew: inputs.crew.name.clone(),
        mode: mode_label(mode).to_string(),
        bundles: bundles.len(),
        // Same up-front stamp as `run_funnel` — degenerate (zero-flag)
        // envelopes carry the comparability key too.
        fingerprint: fingerprint(&swap::namespaced_identifier(&judge.pm), inputs.judge_system),
        // (#1247) The resolved staffing this run actually used, post any
        // caller-applied `--k` override — see `FunnelEnvelope::staffing`.
        staffing: Some(staffing_snapshot(probes, judge)),
        ..Default::default()
    };
    // Same guard discipline as `run_funnel` — see its comment at the
    // matching site.
    let mut guard = FunnelBookendGuard::new(emitter, &inputs.case_id, &inputs.crew.name);
    guard.task_started(json!({
        "status": "started", "case_id": inputs.case_id, "crew": inputs.crew.name,
        "exec_mode": mode_label(mode), "bundles": bundles.len(),
    }));
    env.steps.push(StepRecord {
        step_id: "bundle".to_string(),
        kind: "procedural".to_string(),
        items_in: 1,
        items_out: bundles.len(),
        wall_ms: bundle_ms,
    });
    guard.step_finished(
        "bundle",
        json!({
            "step_id": "bundle", "kind": "procedural", "status": "finished",
            "items_in": 1, "items_out": bundles.len(), "wall_ms": bundle_ms,
        }),
    );
    if flags.is_empty() {
        env.degenerate = Some("--charges-file carried zero flags".to_string());
        guard.task_finished(&env);
        return Ok(env);
    }

    finish_funnel(env, flags, &bundles, inputs, judge, &mut chat, cycler, &mut guard)
}

// ═══════════════════════════════════════════════════════════════════════
// tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    // ── fixtures ────────────────────────────────────────────────────

    const DIFF: &str = "--- a/billing.ts\n+++ b/billing.ts\n@@ -1,3 +1,4 @@\n context line\n+const end = start.plus(30)\n+const total = base * rate\n more context\n";

    fn pm(id: &str) -> ProfileModel {
        ProfileModel { id: id.to_string(), n_ctx: 32_000, ..Default::default() }
    }

    fn staffing(profile: &str, model: &str, k: u32) -> ResolvedSeatStaffing {
        ResolvedSeatStaffing {
            name: profile.to_string(),
            pm: pm(model),
            k,
            max_tokens: None,
            selector: None,
        }
    }

    fn crew_with(seats: Vec<(&str, Vec<ResolvedSeatStaffing>)>) -> ResolvedCrew {
        let mut m = BTreeMap::new();
        for (k, v) in seats {
            m.insert(k.to_string(), v);
        }
        ResolvedCrew { name: "test-crew".to_string(), seats: m }
    }

    fn valid_crew() -> ResolvedCrew {
        crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 2)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ])
    }

    fn flag(bundle_id: &str, member: &str, draw: u32, charge_text: &str) -> ProbeFlag {
        ProbeFlag {
            bundle_id: bundle_id.to_string(),
            fact_family: "unscoped".to_string(),
            member: member.to_string(),
            draw,
            charge_text: charge_text.to_string(),
            anchor: None,
        }
    }

    /// Recording [`ModelCycler`] mock: pushes `"load:<id>"` / `"release:<id>"`
    /// into a shared log so cycling ORDER is assertable.
    struct RecordingCycler {
        log: Vec<String>,
    }
    impl RecordingCycler {
        fn new() -> Self {
            Self { log: Vec::new() }
        }
    }
    impl ModelCycler for RecordingCycler {
        fn ensure_loaded(&mut self, pm: &ProfileModel) -> Result<()> {
            self.log.push(format!("load:{}", pm.id));
            Ok(())
        }
        fn release(&mut self, pm: &ProfileModel) -> Result<()> {
            self.log.push(format!("release:{}", pm.id));
            Ok(())
        }
    }

    fn reply(content: &str) -> SingleShotReply {
        SingleShotReply {
            content: content.to_string(),
            total_tokens: Some(10),
            model: None,
        }
    }

    // ── judge ruling parser ──────────────────────────────────────────

    #[test]
    fn parse_judge_ruling_last_fence_wins() {
        let text = "Weighing the flag: the code quotes\n```\nconst days = Math.min(raw, 30)\n```\nwhich looks relevant.\n\n```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": \"the clamp is bypassed\", \"note_for_author\": \"real bug\"}\n```\n";
        let (ruling, evidence, note) = parse_judge_ruling(text).expect("parses");
        assert_eq!(ruling, FunnelRuling::Confirmed);
        assert_eq!(evidence, "the clamp is bypassed");
        assert_eq!(note, "real bug");
    }

    #[test]
    fn parse_judge_ruling_prose_wrapped_still_parses() {
        let text = "Some long reasoning about the code goes here, spanning several\nsentences before the verdict.\n```json\n{\"ruling\": \"false_positive\", \"decisive_evidence\": \"input is clamped upstream\", \"note_for_author\": \"no action needed\"}\n```";
        let (ruling, ..) = parse_judge_ruling(text).expect("parses");
        assert_eq!(ruling, FunnelRuling::FalsePositive);
    }

    #[test]
    fn parse_judge_ruling_needs_check_and_case_insensitive() {
        let text = "```json\n{\"ruling\": \"NEEDS_CHECK\", \"decisive_evidence\": \"outside the bundle\", \"note_for_author\": \"verify manually\"}\n```";
        let (ruling, ..) = parse_judge_ruling(text).expect("parses");
        assert_eq!(ruling, FunnelRuling::NeedsCheck);
    }

    #[test]
    fn parse_judge_ruling_unparsed_on_garbage() {
        assert!(parse_judge_ruling("I could not determine a verdict.").is_none());
        assert!(parse_judge_ruling("").is_none());
        // Off-contract ruling value never matches — falls through to None.
        assert!(parse_judge_ruling("```json\n{\"ruling\": \"maybe\"}\n```").is_none());
    }

    // ── dedup ─────────────────────────────────────────────────────────

    #[test]
    fn dedup_same_anchor_and_family_collapses_across_members_and_draws() {
        let flags = vec![
            flag("b1", "member-a", 0, "The clamp at `const end = start.plus(30)` double counts."),
            flag("b1", "member-b", 1, "`const end = start.plus(30)` double-counts the boundary day."),
            flag("b1", "member-a", 2, "`const end = start.plus(30)` looks off by one."),
        ];
        let (deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(stats.raw, 3);
        assert_eq!(stats.deduped, 1);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].anchor.as_deref(), Some("const end = start.plus(30)"));
    }

    #[test]
    fn dedup_different_mechanism_family_survives() {
        let flags = vec![
            flag("b1", "member-a", 0, "`const end = start.plus(30)` double counts the boundary."),
            flag("b1", "member-b", 0, "`const end = start.plus(30)` — timezone handling is wrong here."),
        ];
        let (deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(stats.deduped, 2, "different mechanism family must survive dedup");
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn dedup_no_anchor_flags_dedup_by_family_only() {
        let flags = vec![
            flag("b1", "member-a", 0, "This is a null pointer risk on the branch."),
            flag("b1", "member-b", 0, "A NaN can reach this path unchecked."),
        ];
        let (deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(stats.deduped, 1, "no-anchor flags in the same family collapse");
        assert!(deduped[0].anchor.is_none());
    }

    #[test]
    fn dedup_no_anchor_different_bundle_survives() {
        let flags = vec![
            flag("b1", "member-a", 0, "This is a null pointer risk."),
            flag("b2", "member-a", 0, "This is also a null pointer risk."),
        ];
        let (_deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(stats.deduped, 2, "different bundle_id never collapses");
    }

    /// Frontier QA should-fix on this packet's PR: substring matching
    /// classified "tenant", "covenant", and "finance" as `null/nan` (all
    /// contain "nan"), so two DISTINCT unanchored charges on a billing
    /// corpus keyed identically and one real defect was silently dropped
    /// in dedup. Word-boundary matching must not fire on those words.
    #[test]
    fn mechanism_family_does_not_substring_match_inside_words() {
        assert_eq!(
            mechanism_family("The tenant covenant check is skipped for finance accounts."),
            "other",
            "'tenant'/'covenant'/'finance' must not classify as null/nan"
        );
        // The real keywords still classify as whole tokens.
        assert_eq!(mechanism_family("A null value reaches this branch."), "null/nan");
        assert_eq!(mechanism_family("NaN propagates into the total."), "null/nan");
        assert_eq!(mechanism_family("None is returned on the error path."), "null/nan");
        // Punctuation-adjacent tokens still match (tokenizer strips it).
        assert_eq!(mechanism_family("Uses `Date.now()` for the cutoff."), "timezone/ambient-time");
        // "nonexistent" must not token-match "none".
        assert_eq!(mechanism_family("References a nonexistent column."), "other");
    }

    /// Two unanchored flags on the SAME bundle whose charges describe
    /// genuinely different mechanisms must both survive dedup — the
    /// substring bug collapsed them (both misclassified `null/nan`) and
    /// silently dropped a real defect.
    #[test]
    fn dedup_distinct_mechanisms_same_bundle_both_survive() {
        let flags = vec![
            flag(
                "b1",
                "member-a",
                0,
                "The tenant covenant check is skipped when the finance flag is set.",
            ),
            flag("b1", "member-b", 0, "A null value reaches the accumulator unguarded."),
        ];
        let (deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(
            stats.deduped, 2,
            "genuinely different mechanisms in one bundle must both survive"
        );
        assert_eq!(deduped.len(), 2);
    }

    // ── double-confirm state machine ────────────────────────────────

    fn scripted_chat(
        script: RefCell<Vec<&'static str>>,
    ) -> impl FnMut(&ChatCall) -> Result<SingleShotReply> {
        move |_call: &ChatCall| {
            let mut s = script.borrow_mut();
            if s.is_empty() {
                return Ok(reply(""));
            }
            Ok(reply(s.remove(0)))
        }
    }

    const CONFIRM_JSON: &str = "```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```";
    const FP_JSON: &str = "```json\n{\"ruling\": \"false_positive\", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```";
    const NEEDS_CHECK_JSON: &str = "```json\n{\"ruling\": \"needs_check\", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```";

    #[test]
    fn double_confirm_confirm_then_confirm_is_confirmed_tier() {
        let mut chat = scripted_chat(RefCell::new(vec![CONFIRM_JSON, CONFIRM_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, &mut chat);
        assert_eq!(o.pass1.ruling, FunnelRuling::Confirmed);
        assert_eq!(o.pass2.unwrap().ruling, FunnelRuling::Confirmed);
        assert_eq!(o.tier, Tier::Confirmed);
        assert!(!o.demoted_by_pass2);
        assert_eq!(o.calls, 2, "one clean dispatch per pass");
    }

    #[test]
    fn double_confirm_confirm_then_false_positive_demotes_to_needs_check() {
        let mut chat = scripted_chat(RefCell::new(vec![CONFIRM_JSON, FP_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, &mut chat);
        assert_eq!(o.pass1.ruling, FunnelRuling::Confirmed);
        assert_eq!(o.pass2.unwrap().ruling, FunnelRuling::FalsePositive);
        assert_eq!(o.tier, Tier::NeedsCheck, "disagreement demotes, never ships as confirmed");
        assert!(o.demoted_by_pass2);
    }

    #[test]
    fn double_confirm_pass1_needs_check_skips_pass2() {
        let mut chat = scripted_chat(RefCell::new(vec![NEEDS_CHECK_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, &mut chat);
        assert_eq!(o.pass1.ruling, FunnelRuling::NeedsCheck);
        assert!(o.pass2.is_none());
        assert_eq!(o.tier, Tier::NeedsCheck);
        assert!(!o.demoted_by_pass2);
        assert_eq!(o.calls, 1);
        assert_eq!(o.pass2_ms, 0, "no pass-2 dispatch, no pass-2 wall time");
    }

    #[test]
    fn double_confirm_pass1_false_positive_archives_without_pass2() {
        let mut chat = scripted_chat(RefCell::new(vec![FP_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, &mut chat);
        assert_eq!(o.pass1.ruling, FunnelRuling::FalsePositive);
        assert!(o.pass2.is_none());
        assert_eq!(o.tier, Tier::Archived);
    }

    #[test]
    fn double_confirm_unparsed_retries_then_archives() {
        // Two garbage replies: pass-1 attempt, retry — still unparsed.
        let mut chat = scripted_chat(RefCell::new(vec!["no verdict here", "still nothing"]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, &mut chat);
        assert_eq!(o.pass1.ruling, FunnelRuling::Unparsed);
        assert!(o.pass2.is_none());
        assert_eq!(o.tier, Tier::Archived);
        assert!(!o.demoted_by_pass2);
        assert_eq!(o.calls, 2, "the unparsed retry is a real dispatch and is counted");
    }

    #[test]
    fn double_confirm_unparsed_retry_recovers() {
        // First attempt garbage, retry succeeds — the retry's ruling wins.
        let mut chat = scripted_chat(RefCell::new(vec!["garbage", CONFIRM_JSON, CONFIRM_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, &mut chat);
        assert_eq!(o.pass1.ruling, FunnelRuling::Confirmed, "the retry's clean ruling survives");
        assert_eq!(o.pass2.unwrap().ruling, FunnelRuling::Confirmed);
        assert_eq!(o.tier, Tier::Confirmed);
        assert_eq!(o.calls, 3, "pass-1 attempt + retry + pass-2 = three real dispatches");
    }

    // ── empty probe draw ─────────────────────────────────────────────

    #[test]
    fn probe_one_draw_empty_content_retries_once_then_skips() {
        let mut calls = 0u32;
        let mut chat = |_call: &ChatCall| {
            calls += 1;
            Ok(reply(""))
        };
        let out = probe_one_draw(&mut chat, "m", "sys", "user", 100).expect("no dispatch error");
        assert!(out.is_none(), "still empty after retry -> skipped, not a flag");
        assert_eq!(calls, 2, "exactly one retry (two total attempts)");
    }

    #[test]
    fn probe_one_draw_recovers_on_retry() {
        let mut calls = 0u32;
        let mut chat = |_call: &ChatCall| {
            calls += 1;
            if calls == 1 {
                Ok(reply(""))
            } else {
                Ok(reply("a real defect description"))
            }
        };
        let out = probe_one_draw(&mut chat, "m", "sys", "user", 100).unwrap();
        assert_eq!(out.unwrap().0, "a real defect description");
        assert_eq!(calls, 2);
    }

    #[test]
    fn probe_one_draw_propagates_dispatch_error() {
        let mut chat = |_call: &ChatCall| -> Result<SingleShotReply> { Err(anyhow!("network down")) };
        let err = probe_one_draw(&mut chat, "m", "sys", "user", 100).unwrap_err();
        assert!(err.to_string().contains("network down"));
    }

    // ── selector filtering ───────────────────────────────────────────

    #[test]
    fn selector_filters_by_fact_family() {
        let bundles = vec![
            BundleInput { id: "a".into(), fact_family: "auth".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
            BundleInput { id: "b".into(), fact_family: "billing".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
        ];
        let sel =
            BundleSelector { fact_families: vec!["auth".to_string()], ..Default::default() };
        let selected = select_bundles_for_staffing(&bundles, Some(&sel));
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, "a");
    }

    #[test]
    fn selector_no_selector_runs_every_bundle() {
        let bundles = vec![
            BundleInput { id: "a".into(), fact_family: "auth".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
            BundleInput { id: "b".into(), fact_family: "billing".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
        ];
        assert_eq!(select_bundles_for_staffing(&bundles, None).len(), 2);
    }

    #[test]
    fn selector_prioritizes_param_flow_and_respects_max_bundles() {
        let bundles = vec![
            BundleInput { id: "a".into(), fact_family: "other".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
            BundleInput { id: "b".into(), fact_family: "param-flow".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
            BundleInput { id: "c".into(), fact_family: "other".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
        ];
        let sel = BundleSelector { max_bundles: Some(2), ..Default::default() };
        let selected = select_bundles_for_staffing(&bundles, Some(&sel));
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].id, "b", "param-flow bundle is prioritized first");
    }

    // ── crew seat-requirement validation ────────────────────────────

    #[test]
    fn validate_funnel_crew_happy_path() {
        let crew = valid_crew();
        let (probes, judge) = validate_funnel_crew(&crew).expect("valid");
        assert_eq!(probes.len(), 1);
        assert_eq!(judge.pm.id, "judge-model");
    }

    #[test]
    fn validate_funnel_crew_missing_probe_seat_rejected() {
        let crew = crew_with(vec![("review-judge", vec![staffing("fast", "j", 1)])]);
        let err = validate_funnel_crew(&crew).unwrap_err();
        assert!(err.to_string().contains("review-probe"));
    }

    #[test]
    fn validate_funnel_crew_empty_probe_staffing_rejected() {
        let crew = crew_with(vec![
            ("review-probe", vec![]),
            ("review-judge", vec![staffing("fast", "j", 1)]),
        ]);
        let err = validate_funnel_crew(&crew).unwrap_err();
        assert!(err.to_string().contains("review-probe"));
    }

    #[test]
    fn validate_funnel_crew_missing_judge_seat_rejected() {
        let crew = crew_with(vec![("review-probe", vec![staffing("fast", "p", 1)])]);
        let err = validate_funnel_crew(&crew).unwrap_err();
        assert!(err.to_string().contains("review-judge"));
    }

    #[test]
    fn validate_funnel_crew_multiple_judge_staffings_rejected() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "p", 1)]),
            ("review-judge", vec![staffing("fast", "j1", 1), staffing("fast", "j2", 1)]),
        ]);
        let err = validate_funnel_crew(&crew).unwrap_err();
        assert!(err.to_string().contains("EXACTLY 1"));
    }

    // ── sequential cycling order ─────────────────────────────────────

    #[test]
    fn sequential_cycling_loads_and_releases_each_member_before_the_next_then_judge_last() {
        let crew = crew_with(vec![
            (
                "review-probe",
                vec![staffing("fast", "member-a", 1), staffing("fast", "member-b", 1)],
            ),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply("a real defect `const end = start.plus(30)`"));
        let env = run_funnel(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("funnel runs");
        assert!(env.confirmed + env.needs_check + env.archived > 0 || env.deduped_flags == 0);
        let log = &cycler.log;
        let a_load = log.iter().position(|s| s == "load:member-a").unwrap();
        let a_release = log.iter().position(|s| s == "release:member-a").unwrap();
        let b_load = log.iter().position(|s| s == "load:member-b").unwrap();
        let b_release = log.iter().position(|s| s == "release:member-b").unwrap();
        let judge_load = log.iter().position(|s| s == "load:judge-model").unwrap();
        assert!(a_load < a_release, "member A releases before member B loads");
        assert!(a_release < b_load, "member A fully cycled before member B starts");
        assert!(b_load < b_release);
        assert!(b_release < judge_load, "judge loads last, after every probe member");
    }

    // ── envelope counts + steps consistency ──────────────────────────

    #[test]
    fn envelope_counts_and_steps_are_internally_consistent() {
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut call_n = 0u32;
        let mut chat = |_call: &ChatCall| {
            call_n += 1;
            if call_n <= 2 {
                // two probe draws (k=2), both find the same defect
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                Ok(reply(CONFIRM_JSON))
            }
        };
        let env = run_funnel(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("funnel runs");
        assert!(env.degenerate.is_none());
        assert_eq!(env.bundles, 1, "one changed file in the fixture diff");
        assert_eq!(env.raw_flags, 2, "k=2 draws, both non-empty");
        assert_eq!(env.deduped_flags, 1, "identical anchor+family collapses to one");
        assert_eq!(env.flags.len(), env.deduped_flags);
        assert_eq!(env.judged.len(), env.deduped_flags);
        assert_eq!(
            env.confirmed + env.needs_check + env.archived,
            env.judged.len(),
            "every judged flag lands in exactly one tier"
        );
        let step_ids: Vec<&str> = env.steps.iter().map(|s| s.step_id.as_str()).collect();
        assert!(step_ids.contains(&"bundle"));
        assert!(step_ids.contains(&"probe"));
        assert!(step_ids.contains(&"dedup"));
        assert!(step_ids.contains(&"judge-pass1"));
        assert!(!env.members.is_empty());
        assert!(env.fingerprint.get("protocol").is_some());
    }

    // ── flow-record emission (#1247 Part 1) ───────────────────────────

    /// Recording [`FunnelEmitter`] mock — pushes every emitted record into
    /// a shared `Vec` so a test can assert the exact SEQUENCE (action +
    /// payload), same discipline as `RecordingCycler` above.
    struct RecordingEmitter {
        records: Vec<darkmux_flow::FlowRecord>,
    }
    impl RecordingEmitter {
        fn new() -> Self {
            Self { records: Vec::new() }
        }
    }
    impl FunnelEmitter for RecordingEmitter {
        fn emit(&mut self, record: darkmux_flow::FlowRecord) {
            self.records.push(record);
        }
    }

    #[test]
    fn flow_emission_records_the_expected_action_sequence_for_a_healthy_run() {
        // Same scripted scenario as `envelope_counts_and_steps_are_internally_consistent`:
        // one probe seat, k=2 draws both finding the same defect (dedup
        // collapses to 1 flag), a judge that confirms both passes.
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut emitter = RecordingEmitter::new();
        let mut call_n = 0u32;
        let mut chat = |_call: &ChatCall| {
            call_n += 1;
            if call_n <= 2 {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                Ok(reply(CONFIRM_JSON))
            }
        };
        let env = run_funnel(&inputs, &mut chat, &mut cycler, &mut emitter).expect("funnel runs");
        assert!(env.degenerate.is_none());

        let actions: Vec<&str> = emitter.records.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(
            actions.first(),
            Some(&"funnel.task"),
            "the run's first emitted record is the task-started bookend: {actions:?}"
        );
        assert_eq!(
            actions.last(),
            Some(&"funnel.task"),
            "the run's last emitted record is the task-finished bookend: {actions:?}"
        );
        assert_eq!(
            emitter.records.first().unwrap().payload.as_ref().unwrap()["status"],
            json!("started")
        );
        assert_eq!(
            emitter.records.last().unwrap().payload.as_ref().unwrap()["status"],
            json!("finished")
        );

        // Every step_id named in the envelope's own `steps` shows up as a
        // `funnel.step` record too (the live-run counterpart of the
        // end-of-run summary), plus one seat-level `probe:<name>` record.
        let step_records: Vec<&darkmux_flow::FlowRecord> =
            emitter.records.iter().filter(|r| r.action == "funnel.step").collect();
        let step_ids: Vec<String> = step_records
            .iter()
            .map(|r| r.payload.as_ref().unwrap()["step_id"].as_str().unwrap().to_string())
            .collect();
        assert!(step_ids.contains(&"bundle".to_string()));
        assert!(step_ids.contains(&"probe".to_string()));
        assert!(step_ids.iter().any(|s| s.starts_with("probe:")), "seat-level step: {step_ids:?}");
        assert!(step_ids.contains(&"dedup".to_string()));
        assert!(step_ids.contains(&"judge-pass1".to_string()));
        assert!(step_ids.contains(&"judge-pass2".to_string()), "both passes confirm in this scenario");

        // The per-ruling ticker: one flag, pass-1 AND pass-2 both confirm ->
        // exactly two ruling records.
        let rulings: Vec<&darkmux_flow::FlowRecord> =
            emitter.records.iter().filter(|r| r.action == "funnel.ruling").collect();
        assert_eq!(rulings.len(), 2, "one deduped flag, pass1 confirms so pass2 also runs");
        let passes: Vec<i64> =
            rulings.iter().map(|r| r.payload.as_ref().unwrap()["pass"].as_i64().unwrap()).collect();
        assert!(passes.contains(&1));
        assert!(passes.contains(&2));

        // Truthful ordering (the bug this test guards): `judge-pass2`'s
        // `started` step record must be emitted AT OR BEFORE the first
        // `pass:2` ruling — never after. A pass-2 ruling streaming to a live
        // observer while the `judge-pass2` step still reads "not started"
        // is exactly the contradiction fixed here.
        let pass2_started_idx = emitter
            .records
            .iter()
            .position(|r| {
                r.action == "funnel.step"
                    && r.payload.as_ref().unwrap()["step_id"] == json!("judge-pass2")
                    && r.payload.as_ref().unwrap()["status"] == json!("started")
            })
            .expect("judge-pass2 started record emitted");
        let first_pass2_ruling_idx = emitter
            .records
            .iter()
            .position(|r| r.action == "funnel.ruling" && r.payload.as_ref().unwrap()["pass"] == json!(2))
            .expect("a pass-2 ruling record emitted");
        assert!(
            pass2_started_idx < first_pass2_ruling_idx,
            "judge-pass2 started (record {pass2_started_idx}) must precede the first pass-2 ruling \
             (record {first_pass2_ruling_idx}) — a pass-2 ruling must never precede judge-pass2 started"
        );

        // Provenance: every record carries the case id as session_id and
        // the crew name as handle, matching `crew dispatch`'s own
        // handle=role_id / session_id=dispatch-identity convention.
        assert!(emitter.records.iter().all(|r| r.session_id.as_deref() == Some("c1")));
        assert!(emitter.records.iter().all(|r| r.handle == crew.name));
        assert!(emitter.records.iter().all(|r| r.source.as_deref() == Some("funnel")));
    }

    #[test]
    fn flow_emission_degenerate_zero_bundles_emits_only_task_and_bundle_step() {
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "",
            intent_body: "",
            diff: "",
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut emitter = RecordingEmitter::new();
        let mut chat = |_call: &ChatCall| Ok(reply("unused"));
        let env = run_funnel(&inputs, &mut chat, &mut cycler, &mut emitter).expect("funnel runs");
        assert!(env.degenerate.is_some());

        // Zero bundles short-circuits before any probe/judge work: task
        // started, bundle step finished, task finished — nothing else.
        let actions: Vec<&str> = emitter.records.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(actions, vec!["funnel.task", "funnel.step", "funnel.task"], "{actions:?}");
        let finished = emitter.records.last().unwrap();
        assert!(
            matches!(finished.level, darkmux_flow::Level::Warn),
            "a degenerate run's task-finished record is Warn, not Info"
        );
        assert_eq!(finished.payload.as_ref().unwrap()["degenerate"].as_str().unwrap(), env.degenerate.unwrap());
    }

    // ── bookend guard (#1247 review round) — no orphaned started records ──

    /// A probe dispatch error propagates out of `run_funnel` via `?` AFTER
    /// `funnel.task started`, `probe started`, and `probe:<seat> started`
    /// were emitted. Without the guard those three would dangle forever;
    /// with it, the Drop path must close each open step (innermost-first,
    /// `status: "error"`) and emit a terminal error task record — every
    /// `started` gets a matching terminal event even on the abort path.
    #[test]
    fn bookend_guard_probe_dispatch_error_closes_open_steps_and_emits_terminal_task_record() {
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut emitter = RecordingEmitter::new();
        let mut chat =
            |_call: &ChatCall| -> Result<SingleShotReply> { Err(anyhow!("network down")) };
        let err = run_funnel(&inputs, &mut chat, &mut cycler, &mut emitter).unwrap_err();
        assert!(err.to_string().contains("probe phase"), "the original error still propagates: {err:#}");

        // The record stream stays pair-consistent: the seat step closes
        // first (innermost), then the probe phase step, then the task.
        let tail: Vec<(String, String)> = emitter
            .records
            .iter()
            .rev()
            .take(3)
            .map(|r| {
                let p = r.payload.as_ref().unwrap();
                (
                    r.action.clone(),
                    p["step_id"].as_str().unwrap_or_else(|| p["status"].as_str().unwrap()).to_string(),
                )
            })
            .collect();
        assert_eq!(
            tail,
            vec![
                ("funnel.task".to_string(), "error".to_string()),
                ("funnel.step".to_string(), "probe".to_string()),
                ("funnel.step".to_string(), "probe:fast".to_string()),
            ],
            "reading backwards: terminal task record last, preceded by probe then probe:<seat> error-closes"
        );
        for r in emitter.records.iter().rev().take(3) {
            assert!(matches!(r.level, darkmux_flow::Level::Error), "abort-path records are Level::Error");
            let status = r.payload.as_ref().unwrap()["status"].as_str().unwrap();
            assert_eq!(status, "error");
        }
        // Exactly one terminal task record — the guard fired once, and the
        // clean-path task_finished never ran.
        let task_terminals = emitter
            .records
            .iter()
            .filter(|r| {
                r.action == "funnel.task"
                    && r.payload.as_ref().unwrap()["status"].as_str() != Some("started")
            })
            .count();
        assert_eq!(task_terminals, 1);
    }

    /// The reviewer-named scenario: a chat closure that errors mid-JUDGE-
    /// docket. Judge dispatch errors are deliberately swallowed per-flag
    /// (`FunnelRuling::Error` → `Tier::Archived` — one bad call must not
    /// abort the docket), so the run COMPLETES and the terminal task record
    /// is the ordinary `finished` one (degenerate-marked by the judge-dead
    /// honesty gate, since NO flag got a usable ruling). Either way the
    /// invariant under test holds: a terminal task record exists — no
    /// orphaned `started`.
    #[test]
    fn bookend_guard_chat_error_mid_judge_docket_still_yields_terminal_task_record() {
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut emitter = RecordingEmitter::new();
        let mut chat = |call: &ChatCall| -> Result<SingleShotReply> {
            if call.model.contains("probe-model") {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                // Every judge call errors — mid-docket dispatch failure.
                Err(anyhow!("judge endpoint down"))
            }
        };
        let env = run_funnel(&inputs, &mut chat, &mut cycler, &mut emitter)
            .expect("judge dispatch errors are swallowed per-flag, never abort the run");
        assert!(env.degenerate.is_some(), "the judge-dead honesty gate marks the envelope");

        let last = emitter.records.last().unwrap();
        assert_eq!(last.action, "funnel.task");
        assert_eq!(
            last.payload.as_ref().unwrap()["status"].as_str(),
            Some("finished"),
            "the run completed cleanly, so the terminal record is finished (degenerate), not the guard's error"
        );
        assert!(last.payload.as_ref().unwrap()["degenerate"].is_string());
        // The judge-pass1 step still closed normally.
        assert!(emitter.records.iter().any(|r| {
            r.action == "funnel.step"
                && r.payload.as_ref().unwrap()["step_id"].as_str() == Some("judge-pass1")
                && r.payload.as_ref().unwrap()["status"].as_str() == Some("finished")
        }));
    }

    /// The genuine mid-docket abort vector: the judge's `cycler.release`
    /// failing AFTER `judge-pass1 started` was emitted and the docket ran.
    /// This scenario's flag confirms on both passes, so `judge-pass2 started`
    /// also fired mid-loop (the fix under test) and is STILL open at the
    /// release failure (its `finished` only emits after `cycler.release`
    /// returns). The guard must close both, innermost-first — `judge-pass2`
    /// (opened last) before `judge-pass1` — with `status: "error"`, then
    /// emit the terminal error task record.
    #[test]
    fn bookend_guard_judge_release_failure_closes_judge_pass1_and_task() {
        /// Cycler whose `release` fails for one named model id.
        struct FailingReleaseCycler {
            fail_on: String,
        }
        impl ModelCycler for FailingReleaseCycler {
            fn ensure_loaded(&mut self, _pm: &ProfileModel) -> Result<()> {
                Ok(())
            }
            fn release(&mut self, pm: &ProfileModel) -> Result<()> {
                if pm.id == self.fail_on {
                    bail!("simulated release failure for {}", pm.id);
                }
                Ok(())
            }
        }
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = FailingReleaseCycler { fail_on: "judge-model".to_string() };
        let mut emitter = RecordingEmitter::new();
        let mut call_n = 0u32;
        let mut chat = |_call: &ChatCall| {
            call_n += 1;
            if call_n <= 2 {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                Ok(reply(CONFIRM_JSON))
            }
        };
        let err = run_funnel(&inputs, &mut chat, &mut cycler, &mut emitter).unwrap_err();
        assert!(err.to_string().contains("release failure"), "{err:#}");

        let last = emitter.records.last().unwrap();
        assert_eq!(last.action, "funnel.task");
        assert_eq!(last.payload.as_ref().unwrap()["status"].as_str(), Some("error"));
        // Innermost-first: `judge-pass2` (opened last, mid-loop, once its
        // first pass-2 ruling fired) closes before `judge-pass1` (opened
        // first, before the loop).
        let second_to_last = &emitter.records[emitter.records.len() - 2];
        assert_eq!(second_to_last.action, "funnel.step");
        assert_eq!(
            second_to_last.payload.as_ref().unwrap()["step_id"].as_str(),
            Some("judge-pass1"),
            "judge-pass1 was the outermost open step at the release failure"
        );
        assert_eq!(second_to_last.payload.as_ref().unwrap()["status"].as_str(), Some("error"));
        let third_to_last = &emitter.records[emitter.records.len() - 3];
        assert_eq!(third_to_last.action, "funnel.step");
        assert_eq!(
            third_to_last.payload.as_ref().unwrap()["step_id"].as_str(),
            Some("judge-pass2"),
            "judge-pass2 was ALSO open (its ruling already fired mid-loop) and closes first, innermost"
        );
        assert_eq!(third_to_last.payload.as_ref().unwrap()["status"].as_str(), Some("error"));
        // The rulings the docket DID produce before the abort are on the
        // stream — partial progress is preserved, not retconned.
        assert!(emitter.records.iter().any(|r| r.action == "funnel.ruling"));
    }

    // ── host telemetry sampler (#1247 doctrine surface) ─────────────────

    /// Deterministic fake sampler for the telemetry tests below — returns
    /// instantly with fixed values, so no test races real subprocess
    /// latency (`sample_host`'s `top -l 1` measured 600-900ms per call)
    /// against a scripted deadline on a shared CI runner. The REAL
    /// `sample_host` gets its own direct, macOS-gated coverage in
    /// `darkmux-crew`'s `telemetry_sampler` tests.
    fn fake_sample() -> HostSample {
        HostSample { cpu: Some(42), mem: Some(50), gpu: Some(7) }
    }

    /// `HostTelemetrySampler` on its own, outside any guard: `drop` alone
    /// must stop and join the background thread. The join itself runs on
    /// a SPAWNED thread (not the test thread) and the test asserts via
    /// `recv_timeout` — a regression that makes the sampler ignore its
    /// stop flag then fails LOUD with a bounded timeout instead of
    /// wedging the whole `cargo test` run.
    #[test]
    fn host_telemetry_sampler_stops_and_joins_promptly_on_drop() {
        let sampler = HostTelemetrySampler::start(
            "case".to_string(),
            "crew".to_string(),
            Duration::from_millis(5),
            Duration::from_millis(2),
            fake_sample,
        );
        // Let at least one interval tick elapse so the thread is inside
        // its live sample-or-sleep loop, not still spinning up.
        thread::sleep(Duration::from_millis(20));
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            drop(sampler); // `HostTelemetrySampler::drop` -> stop() -> join()
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("sampler thread did not stop within 5s — thread leak");
    }

    /// `FunnelBookendGuard` owns the sampler's whole-run lifecycle (see its
    /// doc). Clean finish: `task_started` -> `task_finished` -> the guard
    /// drops — the sampler thread must already be stopped by the time that
    /// drop returns. Same bounded-timeout discipline as the sampler-only
    /// test above (drop runs on a spawned thread; the test thread asserts
    /// via `recv_timeout` so a hang fails loud instead of wedging the run).
    #[test]
    fn bookend_guard_clean_finish_stops_telemetry_sampler_thread() {
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut emitter = RecordingEmitter::new();
            let mut guard = FunnelBookendGuard::new_with_telemetry(
                &mut emitter,
                "case-1",
                "crew-1",
                Duration::from_millis(5),
                Duration::from_millis(2),
                fake_sample,
            );
            guard.task_started(json!({"status": "started"}));
            let env = FunnelEnvelope {
                case_id: "case-1".to_string(),
                crew: "crew-1".to_string(),
                ..Default::default()
            };
            guard.task_finished(&env);
            drop(guard); // blocks until the sampler thread stops + joins
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("guard drop (clean finish) did not stop the telemetry sampler thread within 5s — thread leak");
    }

    /// The error-path mirror: `task_started` with no matching
    /// `task_finished` (an early `?`-return / panic unwind) — the guard's
    /// Drop path (still ARMED) closes open steps + emits a terminal error
    /// record AND must still stop the sampler thread, exactly like the
    /// clean-finish path above.
    #[test]
    fn bookend_guard_error_path_drop_stops_telemetry_sampler_thread() {
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut emitter = RecordingEmitter::new();
            {
                let mut guard = FunnelBookendGuard::new_with_telemetry(
                    &mut emitter,
                    "case-2",
                    "crew-2",
                    Duration::from_millis(5),
                    Duration::from_millis(2),
                    fake_sample,
                );
                guard.task_started(json!({"status": "started"}));
                // No `task_finished` call — the guard drops here still
                // ARMED, exercising the error path.
            }
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("guard drop (error path) did not stop the telemetry sampler thread within 5s — thread leak");
    }

    /// End-to-end: a scripted `run_funnel` (via the test-only
    /// `run_funnel_with_telemetry` seam) with a fast sampler cadence, an
    /// injected instant fake sampler (hermetic — no real subprocess
    /// timing), and a small sleep per scripted dispatch (so the run's own
    /// wall-clock comfortably exceeds several sample intervals) must show
    /// at least one `telemetry.process` record on the `RecordingEmitter`,
    /// with the same field shape `dispatch_internal`'s sampler already
    /// produces (`category=telemetry, source="process"`) plus this run's
    /// own identity (`session_id=case_id`, `handle=crew name`) — the
    /// convention `funnel_flow_record` already uses for the `funnel.*`
    /// action family, so a telemetry record groups with a run's other
    /// records under the same `session_id`.
    #[test]
    fn flow_emission_includes_host_telemetry_when_sampler_cadence_is_fast() {
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c-telemetry".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut emitter = RecordingEmitter::new();
        let mut call_n = 0u32;
        // Same scripted scenario as `flow_emission_records_the_expected_
        // action_sequence_for_a_healthy_run` (probe k=2 both find the same
        // defect, judge confirms both passes -> 4 dispatch calls total),
        // plus a 25ms sleep per call. With the injected `fake_sample`
        // returning instantly, the only timing in play is the 5ms sampler
        // interval vs the run's guaranteed >=100ms wall-clock (4 x 25ms)
        // — a 20x margin with no subprocess latency to race.
        let mut chat = |_call: &ChatCall| {
            thread::sleep(Duration::from_millis(25));
            call_n += 1;
            if call_n <= 2 {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                Ok(reply(CONFIRM_JSON))
            }
        };
        let env = run_funnel_with_telemetry(
            &inputs,
            &mut chat,
            &mut cycler,
            &mut emitter,
            Duration::from_millis(5),
            Duration::from_millis(2),
            fake_sample,
        )
        .expect("funnel runs");
        assert!(env.degenerate.is_none());

        let telemetry: Vec<&darkmux_flow::FlowRecord> =
            emitter.records.iter().filter(|r| r.action == "telemetry.process").collect();
        assert!(
            !telemetry.is_empty(),
            "expected at least one telemetry.process record with a fast sampler cadence"
        );
        for r in &telemetry {
            assert!(
                matches!(r.category, darkmux_flow::Category::Telemetry),
                "telemetry record must carry category=telemetry"
            );
            assert_eq!(r.source.as_deref(), Some("process"));
            assert_eq!(
                r.session_id.as_deref(),
                Some("c-telemetry"),
                "session_id must match the funnel's case_id — same convention funnel_flow_record uses"
            );
            assert_eq!(r.handle, crew.name);
            // The injected fake sampler returns fixed values, so the
            // payload assertion can be exact — proving the sample's
            // fields flow through the record-building path unmangled.
            let payload = r.payload.as_ref().expect("telemetry record carries a payload");
            assert_eq!(payload["cpu"], json!(42));
            assert_eq!(payload["mem"], json!(50));
            assert_eq!(payload["gpu"], json!(7));
        }
    }

    // ── staffing snapshot (#1247 lab-view addition) ────────────────────

    #[test]
    fn staffing_snapshot_round_trips_and_reflects_the_callers_resolved_k_not_a_registry_default() {
        // `k: 9` here stands in for a `--k` override the caller (review-bench
        // or `pr-review run`) already applied to the crew BEFORE building
        // `FunnelInputs` — `run_funnel` never re-reads a registry, so the
        // snapshot can only ever reflect what it was actually handed.
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 9)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply("a real defect `const end = start.plus(30)`"));
        let env = run_funnel(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("funnel runs");

        let snapshot = env.staffing.as_ref().expect("staffing snapshot present on a normal run");
        assert_eq!(snapshot.probes.len(), 1);
        assert_eq!(snapshot.probes[0].k, 9, "the OVERRIDDEN k the caller resolved onto the crew");
        assert_eq!(snapshot.probes[0].name, "fast");
        assert_eq!(snapshot.probes[0].model, "darkmux:probe-model", "same namespaced form MemberRecord.model uses");
        let judge = snapshot.judge.as_ref().expect("exactly one judge staffing");
        assert_eq!(judge.model, "darkmux:judge-model");
        assert_eq!(judge.k, 1);
        // Settings provenance (scope extension on #1256): the resolved
        // ProfileModel's declared context length, so "what context was this
        // model loaded at" is never a forensic question. `pm()` fixtures
        // n_ctx=32_000 for every model.
        assert_eq!(snapshot.probes[0].n_ctx, 32_000);
        assert_eq!(judge.n_ctx, 32_000);

        // The shape `funnels.json` persists — a JSON round trip must
        // preserve the snapshot exactly, same discipline as the envelope's
        // own full serde-round-trip test.
        let json = serde_json::to_string(&env).expect("envelope serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("envelope parses back");
        assert_eq!(value["staffing"]["probes"][0]["k"], json!(9));
        assert_eq!(value["staffing"]["probes"][0]["model"], json!("darkmux:probe-model"));
        assert_eq!(value["staffing"]["probes"][0]["n_ctx"], json!(32_000));
        assert_eq!(value["staffing"]["judge"]["model"], json!("darkmux:judge-model"));
        assert_eq!(value["staffing"]["judge"]["n_ctx"], json!(32_000));
    }

    #[test]
    fn staffing_snapshot_absent_field_on_an_older_envelope_deserializes_as_none() {
        // A pre-#1247 envelope has no `staffing` key at all — `default` +
        // `skip_serializing_if` must let it deserialize as `None`, never a
        // hard parse failure (the schema-lenience discipline every optional
        // envelope field in this module follows).
        let legacy = r#"{
            "case_id": "c1", "crew": "test-crew", "mode": "sequential",
            "members": [], "steps": [], "bundles": 1, "raw_flags": 0,
            "deduped_flags": 0, "flags": [], "judged": [],
            "confirmed": 0, "needs_check": 0, "archived": 0,
            "fingerprint": {}
        }"#;
        let env: FunnelEnvelope = serde_json::from_str(legacy).expect("legacy envelope without staffing parses");
        assert!(env.staffing.is_none());
    }

    #[test]
    fn degenerate_zero_bundles_never_silently_passes() {
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "",
            intent_body: "",
            diff: "",
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply("unused"));
        let env = run_funnel(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("funnel runs");
        assert!(env.degenerate.is_some());
        assert_eq!(env.bundles, 0);
        assert_eq!(env.confirmed, 0);
        assert_eq!(env.needs_check, 0);
        assert_eq!(env.archived, 0);
        assert!(
            env.fingerprint.get("protocol").is_some(),
            "a degenerate envelope still carries the comparability fingerprint"
        );
    }

    #[test]
    fn degenerate_zero_flags_never_silently_passes() {
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        // Every probe draw comes back empty — retried, then skipped.
        let mut chat = |_call: &ChatCall| Ok(reply(""));
        let env = run_funnel(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("funnel runs");
        assert!(env.degenerate.is_some());
        assert_eq!(env.raw_flags, 0);
        assert_eq!(env.judged.len(), 0);
        assert!(
            env.fingerprint.get("protocol").is_some(),
            "a zero-flag envelope still carries the comparability fingerprint"
        );
    }

    #[test]
    fn degenerate_all_unparsed_judge_never_renders_as_a_clean_pass() {
        // The judge-dead honesty gate (#1222 packet 5 review): per-flag
        // judge failures are swallowed to Unparsed/Error -> Archived, so a
        // dead or off-contract judge used to produce confirmed=0 /
        // needs_check=0 / degenerate=None — indistinguishable downstream
        // from a genuinely clean "none confirmed" run. Flags judged but
        // ZERO usable pass-1 rulings must mark the envelope degenerate.
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            if call.model.contains("probe-model") {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                // Every judge call (pass-1 AND its unparsed-retry) is
                // off-contract prose — no fenced JSON ruling.
                Ok(reply("I could not reach a verdict on this."))
            }
        };
        let env = run_funnel(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("funnel runs");
        assert_eq!(env.judged.len(), 1, "the flag WAS judged (archived), not dropped");
        assert_eq!(env.confirmed, 0);
        assert_eq!(env.needs_check, 0);
        assert_eq!(env.archived, 1);
        let note = env.degenerate.expect("all-unparsed judge must mark the envelope degenerate");
        assert!(note.contains("no usable ruling"), "{note}");
        assert!(note.contains("1 flags"), "names how many flags got nothing: {note}");
    }

    #[test]
    fn genuine_all_false_positive_docket_is_not_degenerate() {
        // The counterpart: a judge that RULED (false_positive) on every
        // flag produced real signal — zero confirms is then an honest
        // outcome, not a degenerate one.
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            if call.model.contains("probe-model") {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                Ok(reply(FP_JSON))
            }
        };
        let env = run_funnel(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("funnel runs");
        assert_eq!(env.confirmed, 0);
        assert_eq!(env.archived, 1);
        assert!(
            env.degenerate.is_none(),
            "a ruled-on docket is honest signal, never degenerate: {:?}",
            env.degenerate
        );
    }

    // ── run_judge_only ────────────────────────────────────────────────

    #[test]
    fn run_judge_only_skips_probe_and_judges_supplied_flags() {
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let flags = vec![flag("billing.ts", "member-a", 0, "`const end = start.plus(30)` double-counts")];
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply(CONFIRM_JSON));
        let env = run_judge_only(flags, &inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        assert_eq!(env.raw_flags, 1);
        assert_eq!(env.judged.len(), 1);
        assert!(!cycler.log.iter().any(|s| s.contains("probe-model")), "probe never dispatched");
        assert_eq!(
            env.mode, "sequential",
            "the envelope records the caller's resolved mode, not a hardcoded label"
        );
    }

    #[test]
    fn run_judge_only_records_the_callers_parallel_mode() {
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Parallel,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let flags = vec![flag("billing.ts", "member-a", 0, "`const end = start.plus(30)` off by one")];
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply(FP_JSON));
        let env = run_judge_only(flags, &inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        assert_eq!(env.mode, "parallel", "a judge-only re-run of a parallel funnel keeps its provenance");
    }

    // ── ExecMode auto-resolution ──────────────────────────────────────

    #[test]
    fn resolve_auto_stays_parallel_within_budget_and_falls_back_sequential_over() {
        let hw_small = darkmux_hardware::HardwareSpec {
            platform: darkmux_hardware::Platform::AppleSilicon,
            arch: "aarch64".into(),
            total_ram_gb: 16,
            physical_cores: 8,
            performance_cores: None,
            efficiency_cores: None,
            has_unified_memory: true,
        };
        assert_eq!(resolve_auto(1, &hw_small), ExecMode::Parallel);
        assert_eq!(resolve_auto(2, &hw_small), ExecMode::Sequential, "small tier budget is 1");
        let hw_xl = darkmux_hardware::HardwareSpec { total_ram_gb: 128, ..hw_small };
        assert_eq!(resolve_auto(3, &hw_xl), ExecMode::Parallel, "xl tier budget is 3");
        assert_eq!(resolve_auto(4, &hw_xl), ExecMode::Sequential);
    }

    // ── judge_prompt shape ─────────────────────────────────────────────

    #[test]
    fn judge_prompt_includes_all_sections_when_present() {
        let p = judge_prompt(
            "Add billing window",
            "extends the retention window",
            "const end = start.plus(30)",
            &["fact one".to_string()],
            "the boundary is double-counted",
        );
        assert!(p.contains("Add billing window"));
        assert!(p.contains("extends the retention window"));
        assert!(p.contains("const end = start.plus(30)"));
        assert!(p.contains("## Fact sheet given to the flagging reviewer"));
        assert!(p.contains("fact one"));
        assert!(p.contains("the boundary is double-counted"));
        assert!(p.contains("```json"));
        assert!(p.contains("\"ruling\""));
    }

    #[test]
    fn judge_prompt_omits_bare_sections() {
        let p = judge_prompt("", "", "code", &[], "charge");
        assert!(p.contains("(no description provided)"));
        assert!(!p.contains("## Fact sheet given to the flagging reviewer"));
    }

    /// Phase A parity (#1256): a title present but an ABSENT body defaults
    /// only the body line — the title still renders. A single combined
    /// `intent: &str` field couldn't distinguish this from "everything
    /// blank"; separate `intent_title`/`intent_body` params can (and do,
    /// matching `judge-runner.py`'s `judge_one` per-field defaulting).
    #[test]
    fn judge_prompt_title_present_body_absent_still_renders_the_title() {
        let p = judge_prompt("Add billing window", "", "code", &[], "charge");
        assert!(p.contains("Add billing window"));
        assert!(p.contains("(no description provided)"));
    }

    // ── Phase A prompt-parity golden harness (#1256) ───────────────────
    //
    // Provenance: every golden constant below was captured by RUNNING the
    // Phase A python reference (NOT hand-transcribed) against a synthetic,
    // non-corpus fixture during development of this PR:
    //   - probe-runner.py's own `build_prompt()` + `read_code_excerpt()`,
    //     both real and unmodified, over a synthetic worktree containing
    //     the two-function `src/example.ts` fixture — so the probe goldens
    //     carry Phase A's OWN probe code format (``### `path` (lines
    //     a-b)`` + a ```` ```typescript ```` fence per block), which
    //     `bundle::slice_code_probe` ports and `BundleInput::probe_code`
    //     carries (per-seat formats — the judge's `// path` raw format
    //     lives in `BundleInput::code`).
    //   - judge-runner.py's real `slice_code()` against the same synthetic
    //     worktree, then `judge_one`'s exact `user` f-string template
    //     (copy-pasted verbatim, not paraphrased) fed with synthetic
    //     probe/bundle/label dicts — `judge_one` itself fires a live
    //     LMStudio call and can't be invoked directly.
    // The generating scripts are NOT checked into this repo (scratch,
    // depend on the private `pr-review-corpus` fixture tree on the
    // maintainer's machine) — this comment plus the fixture text below is
    // the durable record of how each golden was produced.

    /// The JUDGE-format fixture code slice — what `bundle::slice_code`
    /// emits for a single-ref bundle (`// path (lines a-b)` header, raw
    /// source lines, no fence), matching judge-runner.py's own
    /// `slice_code`. Synthetic, non-corpus.
    const GOLDEN_CODE: &str = "// src/example.ts (lines 1-4)\nexport function clampRetryDelay(attempt: number, base: number): number {\n  const delay = base * Math.pow(2, attempt);\n  return Math.min(delay, 30000);\n}";

    /// The PROBE-format fixture code slice — `read_code_excerpt`'s output
    /// for the same ref, captured verbatim from running the python
    /// reference (``### `path` (lines a-b)`` + ```` ```typescript ````
    /// fence); what `bundle::slice_code_probe` emits into
    /// `BundleInput::probe_code`.
    const GOLDEN_PROBE_CODE: &str = "### `src/example.ts` (lines 1-4)\n```typescript\nexport function clampRetryDelay(attempt: number, base: number): number {\n  const delay = base * Math.pow(2, attempt);\n  return Math.min(delay, 30000);\n}\n```";

    /// `probe-runner.py`'s hardcoded `STRONG_PRIOR` constant, copied
    /// verbatim — used ONLY as this golden test's `prior` argument, to
    /// prove `probe_user_message`'s ASSEMBLY is byte-identical to
    /// `build_prompt`'s. Production wiring passes `review-probe.md`'s text
    /// instead (close in spirit, not necessarily byte-identical wording —
    /// a persona-CONTENT question handled at the measurement layer, out of
    /// this issue's scope).
    const GOLDEN_STRONG_PRIOR: &str = "A thorough code review established that this code contains at least one real defect. Identify the most serious defect in the code below. Name the mechanism precisely, quote the exact line(s) responsible using backticks, and give a concrete example scenario: a specific input and what the code actually does versus what it should do.";

    fn golden_bundle(facts: Vec<String>) -> BundleInput {
        BundleInput {
            id: "src/example.ts".to_string(),
            fact_family: "unscoped".to_string(),
            code: GOLDEN_CODE.to_string(),
            probe_code: GOLDEN_PROBE_CODE.to_string(),
            facts,
            manifest: vec![],
        }
    }

    #[test]
    fn probe_prompt_matches_phase_a_golden_bare() {
        // Captured from probe-runner.py's real build_prompt(worktree,
        // [{path: "src/example.ts", start: 1, end: 4}], []) — including
        // read_code_excerpt's own fenced block format.
        let golden = "A thorough code review established that this code contains at least one real defect. Identify the most serious defect in the code below. Name the mechanism precisely, quote the exact line(s) responsible using backticks, and give a concrete example scenario: a specific input and what the code actually does versus what it should do.\n\nCode:\n\n### `src/example.ts` (lines 1-4)\n```typescript\nexport function clampRetryDelay(attempt: number, base: number): number {\n  const delay = base * Math.pow(2, attempt);\n  return Math.min(delay, 30000);\n}\n```";
        let bundle = golden_bundle(vec![]);
        assert_eq!(probe_user_message(GOLDEN_STRONG_PRIOR, &bundle), golden);
    }

    #[test]
    fn probe_prompt_matches_phase_a_golden_with_facts() {
        // Same build_prompt run with the two facts supplied.
        let golden = "A thorough code review established that this code contains at least one real defect. Identify the most serious defect in the code below. Name the mechanism precisely, quote the exact line(s) responsible using backticks, and give a concrete example scenario: a specific input and what the code actually does versus what it should do.\n\nCode:\n\n### `src/example.ts` (lines 1-4)\n```typescript\nexport function clampRetryDelay(attempt: number, base: number): number {\n  const delay = base * Math.pow(2, attempt);\n  return Math.min(delay, 30000);\n}\n```\n\nComputed facts about this code (mechanically extracted, not interpreted):\n\n- `attempt` is caller-controlled and unbounded\n- `base` defaults to 1000 in all call sites";
        let bundle = golden_bundle(vec![
            "`attempt` is caller-controlled and unbounded".to_string(),
            "`base` defaults to 1000 in all call sites".to_string(),
        ]);
        assert_eq!(probe_user_message(GOLDEN_STRONG_PRIOR, &bundle), golden);
    }

    #[test]
    fn judge_prompt_matches_phase_a_golden_with_facts_and_intent() {
        let golden = "## The author's stated case (the pull request description)\nBound retry backoff to a sane ceiling\nCaps the exponential backoff delay so a large attempt count cannot stall retries indefinitely.\n\n## The code under review\n```typescript\n// src/example.ts (lines 1-4)\nexport function clampRetryDelay(attempt: number, base: number): number {\n  const delay = base * Math.pow(2, attempt);\n  return Math.min(delay, 30000);\n}\n```\n\n## Fact sheet given to the flagging reviewer\n`attempt` is caller-controlled and unbounded\n`base` defaults to 1000 in all call sites\n\n## The flagged item to investigate\nThe delay calculation in `clampRetryDelay` never verifies `attempt` is non-negative — a negative attempt shrinks the delay below the intended floor.\n\nInvestigate the flagged item against the code above. End your reply with exactly one fenced JSON block:\n```json\n{\"ruling\": \"confirmed\" | \"needs_check\" | \"false_positive\", \"decisive_evidence\": \"<the specific code line or checked claim that decided it>\", \"note_for_author\": \"<one or two sentences the author reads>\"}\n```";
        let p = judge_prompt(
            "Bound retry backoff to a sane ceiling",
            "Caps the exponential backoff delay so a large attempt count cannot stall retries indefinitely.",
            GOLDEN_CODE,
            &[
                "`attempt` is caller-controlled and unbounded".to_string(),
                "`base` defaults to 1000 in all call sites".to_string(),
            ],
            "The delay calculation in `clampRetryDelay` never verifies `attempt` is non-negative — a negative attempt shrinks the delay below the intended floor.",
        );
        assert_eq!(p, golden);
    }

    #[test]
    fn judge_prompt_matches_phase_a_golden_bare_no_facts_no_intent() {
        let golden = "## The author's stated case (the pull request description)\n\n(no description provided)\n\n## The code under review\n```typescript\n// src/example.ts (lines 1-4)\nexport function clampRetryDelay(attempt: number, base: number): number {\n  const delay = base * Math.pow(2, attempt);\n  return Math.min(delay, 30000);\n}\n```\n\n## The flagged item to investigate\nThe delay calculation in `clampRetryDelay` never verifies `attempt` is non-negative — a negative attempt shrinks the delay below the intended floor.\n\nInvestigate the flagged item against the code above. End your reply with exactly one fenced JSON block:\n```json\n{\"ruling\": \"confirmed\" | \"needs_check\" | \"false_positive\", \"decisive_evidence\": \"<the specific code line or checked claim that decided it>\", \"note_for_author\": \"<one or two sentences the author reads>\"}\n```";
        let p = judge_prompt(
            "",
            "",
            GOLDEN_CODE,
            &[],
            "The delay calculation in `clampRetryDelay` never verifies `attempt` is non-negative — a negative attempt shrinks the delay below the intended floor.",
        );
        assert_eq!(p, golden);
    }

    // ── bundles_from_diff (provisional bundler) ────────────────────────

    #[test]
    fn bundles_from_diff_one_bundle_per_changed_file() {
        let bundles = bundles_from_diff(DIFF);
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].id, "billing.ts");
        assert!(bundles[0].code.contains("const end = start.plus(30)"));
    }

    // ═══════════════════════════════════════════════════════════════
    // Phase B coverage packet (#1222) — protocol/dedup/telemetry edges
    // ═══════════════════════════════════════════════════════════════

    // ── judge ruling parser: multi-fence, extras, null values ─────────

    /// A judge reply can carry more than one fenced JSON block (e.g. a
    /// judge that reasons out loud, states a tentative verdict, then
    /// revises it). `judge_json_candidates` tries fences LAST-to-FIRST, so
    /// the LAST fenced block in the text must win — an earlier, superseded
    /// verdict must never leak through.
    #[test]
    fn parse_judge_ruling_multiple_valid_fences_last_wins() {
        let text = "```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": \"first pass\", \"note_for_author\": \"n1\"}\n```\nOn reflection, revising the verdict:\n```json\n{\"ruling\": \"false_positive\", \"decisive_evidence\": \"second pass\", \"note_for_author\": \"n2\"}\n```";
        let (ruling, evidence, note) = parse_judge_ruling(text).expect("parses");
        assert_eq!(ruling, FunnelRuling::FalsePositive, "the LAST fenced JSON wins, not the first");
        assert_eq!(evidence, "second pass", "the first fence's evidence must be ignored");
        assert_eq!(note, "n2");
    }

    /// `RawJudgeRuling` has no `deny_unknown_fields` — extra keys a judge
    /// bolts onto its ruling (confidence scores, nested detail) must not
    /// break parsing.
    #[test]
    fn parse_judge_ruling_tolerates_unknown_extra_fields() {
        let text = "```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\", \"confidence\": 0.87, \"extra\": {\"nested\": true}}\n```";
        let (ruling, evidence, note) = parse_judge_ruling(text).expect("unknown fields must not break parsing");
        assert_eq!(ruling, FunnelRuling::Confirmed);
        assert_eq!(evidence, "e");
        assert_eq!(note, "n");
    }

    /// `decisive_evidence`/`note_for_author` are `String`, not
    /// `Option<String>`, and `ruling` is a plain `String` matched against a
    /// closed set. A JSON `null` on any of these is a TYPE mismatch for
    /// serde (not a missing-field default), so every candidate in
    /// `judge_json_candidates` fails to deserialize and the whole reply
    /// falls through to `None` (Unparsed) rather than null silently
    /// standing in for an empty string or a bogus ruling.
    #[test]
    fn parse_judge_ruling_null_values_fail_to_parse_not_treated_as_empty() {
        let evidence_null = "```json\n{\"ruling\": \"confirmed\", \"decisive_evidence\": null, \"note_for_author\": \"n\"}\n```";
        assert!(
            parse_judge_ruling(evidence_null).is_none(),
            "null decisive_evidence must not silently parse as an empty string"
        );

        let ruling_null = "```json\n{\"ruling\": null, \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```";
        assert!(
            parse_judge_ruling(ruling_null).is_none(),
            "a null ruling value must not silently match a variant"
        );
    }

    // ── dedup: whitespace-only anchor variance ─────────────────────────

    /// `extract_new_side_anchor` NORMALIZES (marker-strip + whitespace
    /// collapse) only to decide whether a quoted span is a legitimate
    /// anchor — the stored/returned anchor is the model's VERBATIM quote.
    /// Two flags whose backtick-quoted anchors are semantically identical
    /// but differ in internal whitespace both validate against the diff
    /// (via the collapsed fallback), yet the raw strings differ, so the
    /// dedup key `(bundle_id, anchor, family)` differs and they do NOT
    /// collapse. Characterizes current behavior — not asserted as a bug,
    /// since `dedup_flags`'s doc makes no whitespace-insensitivity promise
    /// on the key itself.
    #[test]
    fn dedup_anchors_differing_only_by_internal_whitespace_do_not_collapse() {
        let flags = vec![
            flag("b1", "member-a", 0, "The `const end = start.plus(30)` double counts."),
            flag("b1", "member-b", 0, "The `const  end = start.plus(30)` double counts."),
        ];
        let (deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(
            stats.deduped, 2,
            "whitespace-differing anchors both validate against the diff but do not share a dedup key"
        );
        assert_eq!(deduped[0].anchor.as_deref(), Some("const end = start.plus(30)"));
        assert_eq!(
            deduped[1].anchor.as_deref(),
            Some("const  end = start.plus(30)"),
            "the stored anchor is the model's verbatim quote, not the normalized/collapsed form"
        );
    }

    // ── mechanism_family word-boundary regression suite (expanded) ─────

    /// Expands the substring-vs-token regression beyond the "tenant" case
    /// already covered: every table keyword must match as a whole token
    /// and must NOT fire on a longer/different word that merely contains
    /// it as a substring.
    #[test]
    fn mechanism_family_word_boundary_regression_suite() {
        // Real keywords match as standalone tokens.
        assert_eq!(mechanism_family("This has an async issue."), "async/await");
        assert_eq!(mechanism_family("Watch the dst transition."), "timezone/ambient-time");
        assert_eq!(mechanism_family("Provenance information is missing."), "provenance/sibling");
        assert_eq!(mechanism_family("Check the arg count."), "arity/param");

        // Longer/different words that merely CONTAIN a keyword as a
        // substring must not false-match — word-boundary, never substring.
        assert_eq!(
            mechanism_family("The function is asynchronous by design."),
            "other",
            "'asynchronous' must not token-match 'async'"
        );
        assert_eq!(
            mechanism_family("A windstorm knocked out power."),
            "other",
            "'windstorm' must not token-match 'dst'"
        );
        assert_eq!(
            mechanism_family("This proves the claim is unproven."),
            "other",
            "'proves'/'unproven' must not token-match 'provenance'"
        );
        assert_eq!(
            mechanism_family("The margarine recipe changed."),
            "other",
            "'margarine' must not token-match 'arg'"
        );
    }

    // ── double-confirm: pass-2 unparsed ─────────────────────────────────

    /// A `confirmed` pass-1 followed by a pass-2 that stays `Unparsed`
    /// (even after its own retry) is still ANY-other-than-confirmed —
    /// `judge_one_flag`'s doc is explicit this must demote, never silently
    /// promote to `Confirmed` on a garbled second call.
    #[test]
    fn double_confirm_confirm_then_pass2_unparsed_demotes_to_needs_check() {
        let mut chat = scripted_chat(RefCell::new(vec![CONFIRM_JSON, "no verdict here", "still nothing"]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, &mut chat);
        assert_eq!(o.pass1.ruling, FunnelRuling::Confirmed);
        assert_eq!(o.pass2.as_ref().unwrap().ruling, FunnelRuling::Unparsed);
        assert_eq!(o.tier, Tier::NeedsCheck, "an unparsed pass-2 must demote, never silently confirm");
        assert!(o.demoted_by_pass2);
        assert_eq!(o.calls, 3, "pass-1 (1 call) + pass-2 attempt + pass-2's own unparsed-retry (2 calls)");
    }

    // ── ModelCycler load-failure propagation ────────────────────────────

    /// Recording [`ModelCycler`] mock that fails `ensure_loaded` for one
    /// named model id, so cycling order AND the abort point are both
    /// assertable.
    struct FailingLoadCycler {
        fail_on: String,
        log: Vec<String>,
    }
    impl FailingLoadCycler {
        fn new(fail_on: &str) -> Self {
            Self { fail_on: fail_on.to_string(), log: Vec::new() }
        }
    }
    impl ModelCycler for FailingLoadCycler {
        fn ensure_loaded(&mut self, pm: &ProfileModel) -> Result<()> {
            self.log.push(format!("load:{}", pm.id));
            if pm.id == self.fail_on {
                bail!("simulated load failure for {}", pm.id);
            }
            Ok(())
        }
        fn release(&mut self, pm: &ProfileModel) -> Result<()> {
            self.log.push(format!("release:{}", pm.id));
            Ok(())
        }
    }

    /// Sequential mode loads/dispatches/releases one member fully before
    /// moving to the next. A load failure on the SECOND member aborts the
    /// whole probe phase via `?` — the first member's already-gathered
    /// flags are discarded (never surfaced, since `run_funnel` returns
    /// `Err` and the partially-built envelope is dropped), the failed
    /// member is never released, and the judge never loads at all.
    #[test]
    fn probe_phase_sequential_load_failure_aborts_remaining_members_and_drops_prior_flags() {
        let crew = crew_with(vec![
            (
                "review-probe",
                vec![staffing("fast", "member-a", 1), staffing("fast", "member-b", 1)],
            ),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = FailingLoadCycler::new("member-b");
        let mut chat = |_call: &ChatCall| Ok(reply("a real defect `const end = start.plus(30)`"));
        let err = run_funnel(&inputs, &mut chat, &mut cycler, &mut NullEmitter).unwrap_err();
        assert!(
            err.to_string().contains("probe phase"),
            "run_funnel wraps the propagated load error with phase context"
        );
        assert_eq!(
            cycler.log,
            vec!["load:member-a", "release:member-a", "load:member-b"],
            "member-a fully cycled before member-b's load failure aborts — no release for member-b, no judge load at all"
        );
    }

    /// Parallel mode loads EVERY member up front, before dispatching any of
    /// them. A load failure partway through that up-front loop aborts
    /// before a single dispatch happens — member-a's draw never runs even
    /// though its own load succeeded.
    #[test]
    fn probe_phase_parallel_load_failure_aborts_before_any_dispatch() {
        let crew = crew_with(vec![
            (
                "review-probe",
                vec![staffing("fast", "member-a", 1), staffing("fast", "member-b", 1)],
            ),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Parallel,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = FailingLoadCycler::new("member-b");
        let mut dispatch_count = 0u32;
        let mut chat = |_call: &ChatCall| {
            dispatch_count += 1;
            Ok(reply("a real defect `const end = start.plus(30)`"))
        };
        let err = run_funnel(&inputs, &mut chat, &mut cycler, &mut NullEmitter).unwrap_err();
        assert!(err.to_string().contains("probe phase"));
        assert_eq!(
            dispatch_count, 0,
            "parallel mode loads every member before dispatching any — the failure aborts before member-a's draw ever runs"
        );
        assert_eq!(cycler.log, vec!["load:member-a", "load:member-b"]);
    }

    // ── LmsCycler residency reconciliation (#1271) ──────────────────────

    /// Write an executable shell stub standing in for `lms`, dispatching on
    /// `$1` the same subcommands `LmsCycler` issues: `ps --json` echoes the
    /// canned resident list from `$STUB_LMS_PS_JSON`; anything else (`load`,
    /// `unload`) appends its FULL argv to `$STUB_LMS_LOG` so cycling ORDER
    /// is assertable. Mirrors the `write_stub_script` pattern already used
    /// for the external-bundler subprocess seam (`lab::bundle::external`).
    #[cfg(unix)]
    fn write_stub_lms(dir: &std::path::Path) -> std::path::PathBuf {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("lms-stub.sh");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "case \"$1\" in").unwrap();
        writeln!(f, "  ps) cat \"$STUB_LMS_PS_JSON\" ;;").unwrap();
        writeln!(f, "  *) echo \"$*\" >> \"$STUB_LMS_LOG\" ;;").unwrap();
        writeln!(f, "esac").unwrap();
        writeln!(f, "exit 0").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// Stands up the stub + points `DARKMUX_LMS_BIN` (and its two auxiliary
    /// env vars) at it for the lifetime of one test. Env mutation means
    /// every test using this needs `#[serial_test::serial]`; `Drop` cleans
    /// the vars back up so a later, non-serial test never inherits a stale
    /// `DARKMUX_LMS_BIN`.
    #[cfg(unix)]
    struct LmsStubEnv {
        _dir: tempfile::TempDir,
        log_path: std::path::PathBuf,
    }

    #[cfg(unix)]
    impl LmsStubEnv {
        fn new(residents_json: &str) -> Self {
            let dir = tempfile::TempDir::new().unwrap();
            let script = write_stub_lms(dir.path());
            let ps_json_path = dir.path().join("ps.json");
            std::fs::write(&ps_json_path, residents_json).unwrap();
            let log_path = dir.path().join("log.txt");
            std::fs::write(&log_path, "").unwrap();
            unsafe {
                std::env::set_var("DARKMUX_LMS_BIN", &script);
                std::env::set_var("STUB_LMS_PS_JSON", &ps_json_path);
                std::env::set_var("STUB_LMS_LOG", &log_path);
            }
            Self { _dir: dir, log_path }
        }

        fn log(&self) -> String {
            std::fs::read_to_string(&self.log_path).unwrap()
        }
    }

    #[cfg(unix)]
    impl Drop for LmsStubEnv {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var("DARKMUX_LMS_BIN");
                std::env::remove_var("STUB_LMS_PS_JSON");
                std::env::remove_var("STUB_LMS_LOG");
            }
        }
    }

    /// (a) darkmux-owned resident sharing the modelKey but at an
    /// INSUFFICIENT ctx — reconcile: unload the stale instance, then load
    /// fresh at the required ctx. This is the exact #1271 repro shape
    /// (a resident from a DIFFERENT profile/crew, same underlying model,
    /// smaller ctx than this seat needs) — the old identifier-only check
    /// missed the collision and attempted a doomed second `lms load`.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_darkmux_owned_wrong_ctx_reconciles_unload_then_reload() {
        let env = LmsStubEnv::new(
            r#"[{"identifier":"darkmux:devstral","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":20000}]"#,
        );
        let mut cycler = LmsCycler;
        let model = ProfileModel { id: "devstral".to_string(), n_ctx: 32768, ..Default::default() };
        cycler.ensure_loaded(&model).expect("reconcile succeeds");
        let log = env.log();
        assert!(log.contains("unload darkmux:devstral"), "unload runs: {log}");
        assert!(
            log.contains("load devstral --context-length 32768 --identifier darkmux:devstral"),
            "reload runs at the required ctx: {log}"
        );
        let unload_pos = log.find("unload darkmux:devstral").unwrap();
        let load_pos = log.find("load devstral").unwrap();
        assert!(unload_pos < load_pos, "unload must precede the reload: {log}");
    }

    /// (b) darkmux-owned resident sharing the modelKey, ALREADY at a
    /// sufficient ctx — reuse, no load or unload issued. The pre-#1271
    /// "current skip-if-loaded behavior" this preserves.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_darkmux_owned_right_ctx_skips_reload() {
        let env = LmsStubEnv::new(
            r#"[{"identifier":"darkmux:devstral","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":40960}]"#,
        );
        let mut cycler = LmsCycler;
        let model = ProfileModel { id: "devstral".to_string(), n_ctx: 32768, ..Default::default() };
        cycler.ensure_loaded(&model).expect("reuse succeeds");
        assert_eq!(env.log(), "", "sufficient ctx already resident — no load/unload issued");
    }

    /// (c) a resident sharing the modelKey that is NOT darkmux-owned (no
    /// `darkmux:` prefix) — operator state. The cycler must fail BEFORE
    /// attempting any load, naming the colliding resident instance and a
    /// fix command, never unload it itself (operator sovereignty).
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_user_owned_same_model_key_blocks_before_any_load() {
        let env = LmsStubEnv::new(
            r#"[{"identifier":"devstral-manual","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":40960}]"#,
        );
        let mut cycler = LmsCycler;
        let model = ProfileModel { id: "devstral".to_string(), n_ctx: 32768, ..Default::default() };
        let err = cycler.ensure_loaded(&model).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("devstral-manual"), "error names the resident instance: {msg}");
        assert!(
            msg.contains("darkmux model eject") && msg.contains("lms unload devstral-manual"),
            "error names both fix commands: {msg}"
        );
        assert_eq!(env.log(), "", "no load or unload attempted against user state");
    }

    /// (d) no resident shares the modelKey — plain load, unchanged.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_no_resident_loads_plain() {
        let env = LmsStubEnv::new("[]");
        let mut cycler = LmsCycler;
        let model = ProfileModel { id: "devstral".to_string(), n_ctx: 32768, ..Default::default() };
        cycler.ensure_loaded(&model).expect("plain load succeeds");
        let log = env.log();
        assert!(
            log.contains("load devstral --context-length 32768 --identifier darkmux:devstral"),
            "{log}"
        );
        assert!(!log.contains("unload"), "no unload without a resident: {log}");
    }

    /// (#1271 review round, REQUIRED fix) A resident under an EXPLICIT
    /// operator alias (`ProfileModel.identifier = Some(..)`, the documented
    /// namespace opt-out — `swap::namespaced_identifier` passes it through
    /// verbatim) is darkmux's OWN load for this profile and must classify as
    /// ours: sufficient ctx → Reuse, never Blocked.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_explicit_alias_resident_right_ctx_reuses_not_blocked() {
        let env = LmsStubEnv::new(
            r#"[{"identifier":"custom","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":32768}]"#,
        );
        let mut cycler = LmsCycler;
        let model = ProfileModel {
            id: "devstral".to_string(),
            n_ctx: 32768,
            identifier: Some("custom".to_string()),
            ..Default::default()
        };
        cycler.ensure_loaded(&model).expect("explicit-alias resident reuses, never Blocked");
        assert_eq!(env.log(), "", "no load or unload issued on reuse");
    }

    /// Explicit-alias resident at an INSUFFICIENT ctx — same reconcile path
    /// as the namespaced case: unload the alias instance, reload under the
    /// same alias at the required ctx.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_explicit_alias_resident_wrong_ctx_reconciles() {
        let env = LmsStubEnv::new(
            r#"[{"identifier":"custom","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":20000}]"#,
        );
        let mut cycler = LmsCycler;
        let model = ProfileModel {
            id: "devstral".to_string(),
            n_ctx: 32768,
            identifier: Some("custom".to_string()),
            ..Default::default()
        };
        cycler.ensure_loaded(&model).expect("explicit-alias reconcile succeeds");
        let log = env.log();
        assert!(log.contains("unload custom"), "stale alias instance unloads: {log}");
        assert!(
            log.contains("load devstral --context-length 32768 --identifier custom"),
            "reload keeps the operator's alias: {log}"
        );
        let unload_pos = log.find("unload custom").unwrap();
        let load_pos = log.find("load devstral").unwrap();
        assert!(unload_pos < load_pos, "unload precedes the reload: {log}");
    }

    /// (#1271 review round) Multi-resident, FIRST match decides: user-owned
    /// listed ahead of a darkmux-stale instance → Blocked, and neither
    /// instance is touched. Pins the `.find()` order-dependence as asserted
    /// behavior rather than an implicit implementation detail.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_multi_resident_user_owned_first_blocks() {
        let env = LmsStubEnv::new(
            r#"[
                {"identifier":"devstral-manual","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":40960},
                {"identifier":"darkmux:devstral","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":20000}
            ]"#,
        );
        let mut cycler = LmsCycler;
        let model = ProfileModel { id: "devstral".to_string(), n_ctx: 32768, ..Default::default() };
        let err = cycler.ensure_loaded(&model).unwrap_err();
        assert!(
            err.to_string().contains("devstral-manual"),
            "error names the first-match user-owned resident: {err}"
        );
        assert_eq!(env.log(), "", "no load or unload issued when a user-owned resident is first");
    }

    /// Multi-resident, mirror ordering: darkmux-stale listed ahead of a
    /// user-owned instance → Reconcile, touching ONLY the darkmux instance —
    /// the user-owned one is never unloaded.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_multi_resident_darkmux_stale_first_reconciles_only_darkmux_instance() {
        let env = LmsStubEnv::new(
            r#"[
                {"identifier":"darkmux:devstral","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":20000},
                {"identifier":"devstral-manual","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":40960}
            ]"#,
        );
        let mut cycler = LmsCycler;
        let model = ProfileModel { id: "devstral".to_string(), n_ctx: 32768, ..Default::default() };
        cycler.ensure_loaded(&model).expect("reconcile succeeds with a user-owned resident present");
        let log = env.log();
        assert!(log.contains("unload darkmux:devstral"), "darkmux instance reconciles: {log}");
        assert!(
            !log.contains("unload devstral-manual"),
            "user-owned instance is never touched: {log}"
        );
        assert!(
            log.contains("load devstral --context-length 32768 --identifier darkmux:devstral"),
            "{log}"
        );
    }

    // ── selector edge cases ──────────────────────────────────────────

    /// `max_bundles` is taken literally — `0` means the staffing gets ZERO
    /// bundles (a degenerate, silent no-op selection), not "unlimited".
    #[test]
    fn selector_max_bundles_zero_selects_nothing() {
        let bundles = vec![
            BundleInput { id: "a".into(), fact_family: "other".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
            BundleInput { id: "b".into(), fact_family: "param-flow".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
        ];
        let sel = BundleSelector { fact_families: vec![], max_bundles: Some(0), ..Default::default() };
        let selected = select_bundles_for_staffing(&bundles, Some(&sel));
        assert!(selected.is_empty(), "max_bundles: 0 must select nothing, not \"unlimited\"");
    }

    /// A `fact_families` restriction naming a family no bundle carries
    /// degrades to an empty selection (zero bundles for that staffing),
    /// never falls back to "no restriction matches everything."
    #[test]
    fn selector_fact_families_naming_unknown_family_selects_nothing() {
        let bundles = vec![
            BundleInput { id: "a".into(), fact_family: "auth".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
            BundleInput { id: "b".into(), fact_family: "billing".into(), code: String::new(), probe_code: String::new(), facts: vec![], manifest: vec![] },
        ];
        let sel = BundleSelector {
            fact_families: vec!["nonexistent-family".to_string()],
            max_bundles: None,
            ..Default::default()
        };
        let selected = select_bundles_for_staffing(&bundles, Some(&sel));
        assert!(
            selected.is_empty(),
            "an unmatched fact_families restriction must select zero bundles, not fall back to 'no restriction'"
        );
    }

    // ── step telemetry consistency ───────────────────────────────────

    /// The `probe` step's wall_ms wraps the ENTIRE `probe_phase` call
    /// (cycler load/release overhead + every member's dispatch time), so it
    /// must be >= the sum of the probe seats' own `MemberRecord.wall_ms`
    /// (which excludes cycler overhead). A small real sleep in the mocked
    /// `chat` makes the timing comparison meaningful instead of two zeros.
    #[test]
    fn step_telemetry_probe_wall_ms_encompasses_member_wall_ms() {
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut call_n = 0u32;
        let mut chat = |_call: &ChatCall| {
            call_n += 1;
            std::thread::sleep(std::time::Duration::from_millis(2));
            if call_n <= 2 {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                Ok(reply(CONFIRM_JSON))
            }
        };
        let env = run_funnel(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("funnel runs");
        let probe_step = env.steps.iter().find(|s| s.step_id == "probe").expect("probe step recorded");
        let probe_member_ms: u64 = env
            .members
            .iter()
            .filter(|m| m.seat == "review-probe")
            .map(|m| m.wall_ms)
            .sum();
        assert!(
            probe_step.wall_ms >= probe_member_ms,
            "probe step ({}) must wrap at least as much wall time as its members' dispatch time ({})",
            probe_step.wall_ms,
            probe_member_ms
        );
    }

    /// The judge's `MemberRecord.wall_ms` is set to EXACTLY `pass1_ms +
    /// pass2_ms` (`finish_funnel`), and the `judge-pass1`/`judge-pass2`
    /// step rows carry those same two values — so their sum must equal the
    /// judge member's wall_ms EXACTLY, not just approximately (both are
    /// derived from the same accumulator variables, so this holds
    /// regardless of real elapsed time).
    #[test]
    fn step_telemetry_judge_steps_sum_equals_judge_member_wall_ms() {
        let crew = valid_crew();
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut call_n = 0u32;
        let mut chat = |_call: &ChatCall| {
            call_n += 1;
            if call_n <= 2 {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            } else {
                // Both judge passes confirm, so both judge-pass1 and
                // judge-pass2 step rows get recorded.
                Ok(reply(CONFIRM_JSON))
            }
        };
        let env = run_funnel(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("funnel runs");
        let judge_member = env
            .members
            .iter()
            .find(|m| m.seat == "review-judge")
            .expect("judge member recorded");
        let step_sum: u64 = env
            .steps
            .iter()
            .filter(|s| s.step_id.starts_with("judge-"))
            .map(|s| s.wall_ms)
            .sum();
        assert_eq!(
            step_sum, judge_member.wall_ms,
            "judge-pass1 + judge-pass2 step wall_ms must sum EXACTLY to the judge MemberRecord's wall_ms"
        );
    }

    // ── envelope serde round trip through a file ─────────────────────

    /// `FunnelEnvelope` derives `Serialize` only (no `Deserialize`), so a
    /// literal `FunnelEnvelope -> FunnelEnvelope` round trip isn't
    /// expressible. This writes a fully-populated envelope (covering all
    /// three `Tier` variants) to a real file, reads it back, and checks
    /// value-level equality through `serde_json::Value` — the strongest
    /// round-trip check available against the current shape.
    #[test]
    fn envelope_serde_round_trips_through_a_file_with_all_tier_variants() {
        use std::io::Write;

        let flag_confirmed = flag("b1", "member-a", 0, "confirmed charge");
        let flag_needs_check = flag("b1", "member-a", 1, "needs-check charge");
        let flag_archived = flag("b1", "member-a", 2, "archived charge");

        let judged = vec![
            JudgedFlag {
                flag: flag_confirmed.clone(),
                pass1: JudgeRecord {
                    ruling: FunnelRuling::Confirmed,
                    decisive_evidence: "e1".into(),
                    note_for_author: "n1".into(),
                    pass: 1,
                    seconds: 0.5,
                },
                pass2: Some(JudgeRecord {
                    ruling: FunnelRuling::Confirmed,
                    decisive_evidence: "e1b".into(),
                    note_for_author: "n1b".into(),
                    pass: 2,
                    seconds: 0.4,
                }),
                tier: Tier::Confirmed,
                demoted_by_pass2: false,
            },
            JudgedFlag {
                flag: flag_needs_check.clone(),
                pass1: JudgeRecord {
                    ruling: FunnelRuling::Confirmed,
                    decisive_evidence: "e2".into(),
                    note_for_author: "n2".into(),
                    pass: 1,
                    seconds: 0.3,
                },
                pass2: Some(JudgeRecord {
                    ruling: FunnelRuling::FalsePositive,
                    decisive_evidence: "e2b".into(),
                    note_for_author: "n2b".into(),
                    pass: 2,
                    seconds: 0.2,
                }),
                tier: Tier::NeedsCheck,
                demoted_by_pass2: true,
            },
            JudgedFlag {
                flag: flag_archived.clone(),
                pass1: JudgeRecord {
                    ruling: FunnelRuling::FalsePositive,
                    decisive_evidence: "e3".into(),
                    note_for_author: "n3".into(),
                    pass: 1,
                    seconds: 0.1,
                },
                pass2: None,
                tier: Tier::Archived,
                demoted_by_pass2: false,
            },
        ];

        let env = FunnelEnvelope {
            case_id: "case-42".to_string(),
            crew: "test-crew".to_string(),
            mode: "sequential".to_string(),
            members: vec![
                MemberRecord {
                    model: "darkmux:probe-model".to_string(),
                    seat: "review-probe".to_string(),
                    draws: 3,
                    wall_ms: 1200,
                    total_tokens: 900,
                },
                MemberRecord {
                    model: "darkmux:judge-model".to_string(),
                    seat: "review-judge".to_string(),
                    draws: 5,
                    wall_ms: 800,
                    total_tokens: 600,
                },
            ],
            steps: vec![
                StepRecord { step_id: "bundle".to_string(), kind: "procedural".to_string(), items_in: 1, items_out: 1, wall_ms: 2 },
                StepRecord { step_id: "probe".to_string(), kind: "dispatch".to_string(), items_in: 1, items_out: 3, wall_ms: 1200 },
                StepRecord { step_id: "dedup".to_string(), kind: "procedural".to_string(), items_in: 3, items_out: 3, wall_ms: 1 },
                StepRecord { step_id: "judge-pass1".to_string(), kind: "dispatch".to_string(), items_in: 3, items_out: 3, wall_ms: 500 },
                StepRecord { step_id: "judge-pass2".to_string(), kind: "dispatch".to_string(), items_in: 2, items_out: 2, wall_ms: 300 },
            ],
            bundles: 1,
            raw_flags: 3,
            deduped_flags: 3,
            flags: vec![flag_confirmed, flag_needs_check, flag_archived],
            judged,
            confirmed: 1,
            needs_check: 1,
            archived: 1,
            degenerate: None,
            fingerprint: fingerprint("darkmux:judge-model", "judge sys"),
            staffing: None,
        };

        let json = serde_json::to_string_pretty(&env).expect("serialize");

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("envelope.json");
        {
            let mut f = std::fs::File::create(&path).expect("create");
            f.write_all(json.as_bytes()).expect("write");
        }
        let read_back = std::fs::read_to_string(&path).expect("read");
        let value: serde_json::Value = serde_json::from_str(&read_back).expect("valid json");

        assert_eq!(value["case_id"], "case-42");
        assert_eq!(value["crew"], "test-crew");
        assert_eq!(value["mode"], "sequential");
        assert_eq!(value["bundles"], 1);
        assert_eq!(value["raw_flags"], 3);
        assert_eq!(value["deduped_flags"], 3);
        assert_eq!(value["confirmed"], 1);
        assert_eq!(value["needs_check"], 1);
        assert_eq!(value["archived"], 1);
        assert!(value.get("degenerate").is_none(), "a None degenerate must be omitted, not written as null");
        assert_eq!(value["fingerprint"]["protocol"], "double-confirm-v1");

        let tiers: Vec<String> = value["judged"]
            .as_array()
            .expect("judged array")
            .iter()
            .map(|j| j["tier"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            tiers,
            vec!["confirmed", "needs_check", "archived"],
            "all three Tier variants must survive the file round trip verbatim"
        );

        assert_eq!(value["members"].as_array().unwrap().len(), 2);
        assert_eq!(value["steps"].as_array().unwrap().len(), 5);
        assert_eq!(value["judged"][1]["demoted_by_pass2"], true);
        assert!(value["judged"][2]["pass2"].is_null(), "no pass-2 dispatch serializes pass2 as null, not omitted");
    }

    // ── manifest is dropped from the judge prompt (#1256) ──────────────

    /// `judge-runner.py`'s `judge_one` has no MANIFEST section at all —
    /// `bundler.py`'s bundles carry no such field. The Rust funnel's
    /// `BundleInput.manifest` is a Rust-only addition; per the "match
    /// Phase A exactly" operator decision it's dropped from the judge
    /// prompt entirely (not silently threaded through) even though the
    /// field itself still exists on `BundleInput` for a future consumer.
    /// Regression-tested at the `run_judge_only` integration level, not a
    /// `judge_prompt` unit test — the function no longer TAKES a manifest
    /// param, so there's nothing left to unit-test at that level; what's
    /// worth guarding is that a populated `BundleInput.manifest` never
    /// leaks into the dispatched prompt.
    #[test]
    fn manifest_never_reaches_the_dispatched_judge_prompt() {
        let crew = valid_crew();
        let bundles = vec![BundleInput {
            id: "billing.ts".to_string(),
            fact_family: "unscoped".to_string(),
            code: "const end = start.plus(30)".to_string(),
            probe_code: "const end = start.plus(30)".to_string(),
            facts: vec![],
            manifest: vec!["helperFn".to_string()],
        }];
        let inputs = FunnelInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            bundles: Some(bundles),
        };
        let flags = vec![flag("billing.ts", "member-a", 0, "`const end = start.plus(30)` double-counts")];
        let mut cycler = RecordingCycler::new();
        let seen_prompts = RefCell::new(Vec::new());
        let mut chat = |call: &ChatCall| {
            seen_prompts.borrow_mut().push(call.user.to_string());
            Ok(reply(CONFIRM_JSON))
        };
        let env = run_judge_only(flags, &inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        assert_eq!(env.judged.len(), 1);
        let prompts = seen_prompts.borrow();
        // A `confirmed` pass-1 (CONFIRM_JSON) earns a pass-2 (double-confirm
        // judge, module doc) — TWO dispatches over the SAME prompt text, not
        // one. Assert every dispatched prompt, not just the first.
        assert_eq!(prompts.len(), 2, "pass-1 confirmed -> pass-2 also dispatches");
        assert!(
            prompts.iter().all(|p| !p.contains("helperFn")),
            "the bundle's manifest entry must never reach the dispatched judge prompt: {prompts:?}"
        );
        assert!(
            prompts.iter().all(|p| !p.to_lowercase().contains("manifest") && !p.contains("Symbols referenced")),
            "no manifest section header at all, matching judge-runner.py: {prompts:?}"
        );
    }
}
