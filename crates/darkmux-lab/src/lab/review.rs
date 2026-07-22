//! (#1222 Phase B packet 4; module renamed from "funnel" to "review" in
//! #1349 — the earlier name described a retired bespoke execution
//! mechanism this pipeline no longer needs) The validated PR-review
//! pipeline:
//! bundles → probe seats ×k draws → dedup → double-confirm judge → a
//! three-tier envelope.
//!
//! ```text
//! bundle → probe(k draws × seat, temp 0.2) → dedup → judge pass-1(every flag)
//!        → judge pass-2(pass-1 confirms only) → {confirmed, needs_check, archived}
//! ```
//!
//! This module is the DRIVER: given resolved roles (#1512, #1513 review —
//! `darkmux_crew::resourcing::resolve_review_roles`, the one generic
//! per-task resolver; no "crew" concept), a diff, and an intent, it runs
//! the whole pipeline and returns a [`ReviewEnvelope`]. Dispatch itself goes
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
//! **Reconciled in packet 5** (now `darkmux mission launch review`,
//! `src/mission_launch_review.rs` in the binary crate — retired from
//! `pr-review run` in #1284 Packet 4b): rather than editing `bundles_from_diff`'s body
//! in place, [`ReviewInputs::bundles`] is the injection seam — packet 5
//! builds real bundles via `build_bundles`/`external_bundles` + `slice_code`
//! and passes `Some(..)`; [`run_judge_only`] uses those directly and never
//! calls the provisional bundler. (`ReviewStepContext::bundles`, the graph
//! path's own analogous field, has no `Option`/fallback at all — its caller
//! always resolves real bundles before building the graph.) `bundles_from_diff`
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
//! (#1434 update: the sequential `--charges-file` re-judge driver
//! [`run_judge_only`]/`finish_review` now emits the SAME generic
//! `step result` vocabulary the graph path (`run_review_graph`) emits — via
//! [`emit_review_step_result`], through the run's [`ReviewObs`] helper. The
//! bespoke per-run task/step/ruling `review.*` action vocabulary and the
//! run-guard that emitted it were retired: exactly one record vocabulary now
//! exists across BOTH review paths. Run-level liveness is the caller's
//! `with_dispatch_bookends` `dispatch start`/`dispatch complete`/`dispatch
//! error` wrap (`src/mission_launch_review.rs`, brackets both paths) — never
//! a review-scoped task bookend from inside the driver, per contract 2 /
//! #1349.)
//!
//! The driver emits [`darkmux_flow::FlowRecord`]s through a caller-injected
//! [`ReviewEmitter`] — same injection discipline as `chat`/`cycler` above,
//! so a scripted test can assert the exact record SEQUENCE via a recording
//! mock. The driver is deliberately SINK-AGNOSTIC: it has no idea whether
//! the records land on the real engagement-scoped flow stream or a
//! per-run-local file — that choice belongs to the caller (`darkmux mission
//! launch review` wires the real stream via `FleetFlowEmitter`, per the
//! lab-vs-fleet scope boundary — a bench's hundreds of per-flag ruling
//! records must never spam an operator's engagement stream). One action
//! family — the generic `step result` record ([`emit_review_step_result`],
//! `action = "step result"`, `handle = step_id`, `session_id = case_id`),
//! with a `kind` field distinguishing which review step produced it, aligned
//! with #1230/#1240's Mission → Phase → Task → Step hierarchy so the records
//! forward-port to the generic mission-flow graph view unchanged:
//!
//! - `kind = "review.bundle"` — the bundle step's completion (`items_out` =
//!   the resolved bundle count).
//! - `kind = "review.dedup"` — dedup completion (`items_in`/`items_out`/
//!   `wall_ms`).
//! - `kind = "review.judge"`, `step_id = "review-ruling"` — the live judge
//!   ticker: one record per judge ruling (every pass-1, plus the decisive
//!   later pass when it ran) with `bundle_id`/`pass`/`ruling`/`seconds`.
//! - `kind = "review.judge"`, `step_id = "judge"` — the judge stage's single
//!   completion record (`items_in`/`items_out`/`wall_ms`, plus
//!   `pass1_wall_ms`/`pass2_wall_ms`/`model`/`tokens`/`calls`/
//!   `dispatch_errors`/`served_model`), matching the graph judge kind's shape.
//! - `kind = "review.verify"`, `step_id = "review-ruling"` — per-adjudication
//!   verify ticker (`bundle_id`/`stage`/`ruling`/`seconds`).
//! - `kind = "review.verify"`, `step_id = "verify"` — the verify stage's
//!   single completion record (`items_in`/`items_out`/`wall_ms`/`model`/
//!   `tokens`/`calls`/`remote`/`endpoint`/`served_model`).
//!
//! Emission happens ONLY in the driver — never inside the pure protocol
//! functions (`dedup_flags`, `mechanism_family`, `parse_judge_ruling`,
//! `judge_prompt`, etc.) or the per-flag dispatch helper `judge_one_flag`
//! (its [`JudgeOutcome`] is emitted from by the caller in `finish_review`'s
//! loop, after the call returns).
//!
//! ## Host telemetry sampling (#1247 doctrine surface — "No blind runs")
//!
//! `run_review_graph`/`run_judge_only` also start a background host cpu/ram/gpu
//! sampler for the run's whole lifetime — see [`ReviewObs`] and
//! [`HostTelemetrySampler`]. Samples emit as `telemetry.process` records
//! through the SAME injected [`ReviewEmitter`] the `step result` action family
//! above uses (so a bench run's samples stay per-run-local and a
//! `mission launch review`'s samples ride the fleet stream, same split), with the
//! identical field shape `darkmux_crew::dispatch_internal`'s always-on
//! sampler already produces — the run-monitor/viewer code that renders
//! `telemetry.process` today applies unchanged.

use anyhow::{anyhow, bail, Context, Result};
use darkmux_crew::single_shot::SingleShotReply;
use darkmux_crew::step_kinds::patterns::dedup::{dedup as pattern_dedup, DedupStrategy};
use darkmux_crew::step_kinds::patterns::multi_pass_confirm::{multi_pass_confirm, ConfirmTier, PassClass};
use darkmux_crew::telemetry_sampler::{sample_host, HostSample};
// (#1230 Packet 1) LmsCycler's residency mechanism now routes through
// gestalt's pure planner, executed via the real LmsHost/MacProbe port
// adapters (their first production call site) — see the "model cycling"
// section below.
use darkmux_gestalt::{AcquireOpts, AcquireScope, Action, CallerIntent, Facts, ModelHost, Placement, ResourceProbe, V1Estimator};
use darkmux_crew::resourcing::{ResolvedReviewRoles, ResolvedSeatStaffing};
use darkmux_profiles::gestalt_host::{resolved_load_deadline, LmsHost, MacProbe};
use darkmux_profiles::swap;
use darkmux_types::{BundleSelector, ModelEndpoint, ProfileModel};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

// ─── execution mode ───────────────────────────────────────────────────────

/// How probe/judge models are cycled through LMStudio across the review's
/// dispatches. `Auto` resolves once, up front, to `Sequential` or
/// `Parallel` (see [`resolve_mode`]) — the resolved choice is what
/// `ReviewEnvelope::mode` records, so an operator reading the envelope
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
    /// (#1299) Charge texts of same-site duplicate findings this flag
    /// ABSORBED during dedup — the "aggregate, never discard" contract. On
    /// collapse the survivor keeps its own `charge_text` and APPENDS each
    /// absorbed finding's framing here, so a renderer can show BOTH ("also
    /// flagged: …"). This is the safety net for the asymmetric objective: a
    /// residual false cut degrades to "one bullet, two framings shown,"
    /// never a vanished defect. Empty (and unserialized) when nothing was
    /// absorbed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_flagged: Vec<String>,
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
pub enum JudgeRuling {
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
    pub ruling: JudgeRuling,
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

/// (#1260/#1177) The verify (adjudication) seat's ruling vocabulary — the
/// optional fourth review stage, run once per double-confirmed finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyRuling {
    /// The finding's mechanism holds against the provided evidence — posted
    /// WITHOUT the manual-verification marker.
    Verified,
    /// A claim the finding depends on does not hold — demoted to
    /// [`Tier::Archived`] with the demotion recorded.
    Refuted,
    /// The deciding fact lies outside the provided evidence — stays
    /// confirmed WITH the existing marker.
    Uncertain,
    /// No recognizable fenced JSON ruling (after one retry).
    Unparsed,
    /// The dispatch itself failed (or the stage's remote token budget was
    /// exhausted — the note names which).
    Error,
}

/// (#1260) One verify-seat adjudication outcome for a confirmed finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyRecord {
    pub ruling: VerifyRuling,
    pub decisive_evidence: String,
    pub note_for_author: String,
    pub seconds: f64,
    /// The adjudicating model — rendered in the posted review's
    /// "verified by <model> adjudication" line.
    pub model: String,
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
    /// (#1260) The verify seat's adjudication — present iff the crew
    /// declares a `review-verify` seat AND this flag reached it (tier was
    /// `Confirmed` after the double-confirm judge). Absent (and never
    /// serialized) on crews without the seat, so their envelopes stay
    /// byte-identical to today's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<VerifyRecord>,
    /// (#1260) `true` iff the verify seat REFUTED this confirmed finding —
    /// the tier is then `Archived`, with this flag recording why.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub demoted_by_verify: bool,
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
    /// (#1260/#1186) `true` when this seat dispatched to a remote endpoint —
    /// its `total_tokens` are CLOUD tokens, which downstream savings
    /// surfaces must exclude (remote work is never "off the meter").
    /// Skipped when `false` so local-only envelopes serialize unchanged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remote: bool,
    /// (#1260) Endpoint HOST only (e.g. `myorg.cognitiveservices.azure.com`)
    /// — never credentials, never the full deployment path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// (#1300) The model the endpoint's response body actually reported it
    /// served (`SingleShotReply.model`, the OpenAI-compatible completion's
    /// top-level `model` field) — DISTINCT from `model` above, which is the
    /// requested/declared identifier (a deployment name can alias to a
    /// different underlying model; for local seats `lms ps` is ground truth
    /// and this is always `None`). `None` when the response omitted the
    /// field, not when they match — provenance surfaces compare the two and
    /// only call out a mismatch, never assume aliasing from absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served_model: Option<String>,
}

/// One pipeline step's in/out counts + wall time — the issue #1230 bridge:
/// a future flow-record consumer can render the review as a step timeline
/// without re-deriving it from the envelope's nested arrays. Realized by
/// the `step result` flow record (#1247 Part 1, see the module doc) — the
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

/// Sink for the review driver's run-observability records. The driver only
/// knows how to build [`darkmux_flow::FlowRecord`]s and hand them to
/// `emit` — it never decides where they land. See the module doc's
/// "Flow-record emission" section for the action/payload vocabulary and
/// why the driver stays sink-agnostic (lab-vs-fleet scope boundary).
pub trait ReviewEmitter {
    fn emit(&mut self, record: darkmux_flow::FlowRecord);
}

/// No-op emitter — the "at minimum a no-op-able sink" default for callers
/// (and this module's own tests that don't assert on flow records) that
/// don't want review observability output.
pub struct NullEmitter;

impl ReviewEmitter for NullEmitter {
    fn emit(&mut self, _record: darkmux_flow::FlowRecord) {}
}

// ─── host telemetry sampling (#1247 doctrine surface) ────────────────────

/// Production sample cadence — identical to `dispatch_internal`'s always-on
/// sampler (`TELEMETRY_SAMPLE_INTERVAL`/`SAMPLER_POLL_INTERVAL`). `interval`
/// is the time between samples; `poll` is how often the sampler thread
/// re-checks the stop flag while sleeping out `interval`, so teardown is
/// prompt (≤`poll`) instead of blocking for a full tick.
const REVIEW_TELEMETRY_INTERVAL: Duration = Duration::from_millis(2000);
const REVIEW_TELEMETRY_POLL: Duration = Duration::from_millis(500);

/// (#1247 doctrine surface — "No blind runs") Best-effort host cpu/ram/gpu
/// sampler for the review driver. The container dispatch path
/// (`darkmux_crew::dispatch_internal`) has always sampled host load at
/// ~2s cadence; the review path bypasses `dispatch_internal` entirely
/// (it dispatches through the container-free single-shot primitive) and
/// so, until now, produced zero host telemetry — a review envelope
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
/// [`emit_review_step_result`] stamps on the `step result` action family, so a
/// telemetry record for this run groups with its other records under the
/// same `session_id`.
///
/// The sampling FUNCTION is injected (`sample_fn`, a plain fn pointer
/// defaulting to `sample_host` at every production call site — see
/// [`ReviewObs::new`]) so tests can drive the sampler with an
/// instant fake instead of racing real `top -l 1` subprocess latency
/// (~600-900ms per call) against a scripted deadline on a shared CI
/// runner — the same injection discipline as `chat`/`cycler`/`emitter`.
/// The real `sample_host` gets its own direct coverage in
/// `darkmux-crew` (macOS-gated, since the commands it shells to are
/// macOS-only).
///
/// Samples land on an `mpsc` channel rather than being emitted directly
/// from the background thread: the review driver's [`ReviewEmitter`] is a
/// caller-injected `&mut dyn` trait object — not thread-safe, and
/// deliberately not wrapped in a `Mutex` (that would force every
/// `ReviewEmitter` impl and every existing emission call site in this file
/// through lock-guarded access for a feature this narrow). Instead,
/// [`ReviewObs`] (the sequential `run_judge_only` path) drains the channel
/// immediately before every `step result` record it emits, and
/// `run_review_graph` drains it inside the scheduler's own emit closure,
/// so telemetry interleaves with the
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
    /// the review run it's observing.
    fn start(
        case_id: String,
        crew: String,
        interval: Duration,
        poll: Duration,
        sample_fn: fn() -> HostSample,
        lms_fn: fn() -> anyhow::Result<Vec<darkmux_types::LoadedModel>>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let stop_thread = Arc::clone(&stop);
        // (#1247 follow-up) lms load/unload deltas — the diff-state twin of
        // dispatch_internal.rs's `run_telemetry_sampler`. `sample_host` was
        // already shared (the doctrine surface this module's own doc names),
        // but `lms_diff` wasn't wired up here at all, so a review run's
        // "model (lms)" viewer track always read "no telemetry yet" — the
        // run-detail view has host cpu/mem/gpu (from the sampling below) but
        // never which models were actually resident. `seeded` mirrors
        // dispatch_internal's baseline-emission: the FIRST successful sample
        // diffs against empty so the models already resident when the run
        // started show up immediately, not only on a later load/unload edge.
        // `lms_fn` is injected (same discipline as `sample_fn`, same reason)
        // — the real `darkmux_profiles::lms::list_loaded` shells out to the
        // `lms` CLI, and an un-injected real subprocess call in this loop
        // raced and broke the fast-cadence telemetry test's tight timing
        // margin (this file has 20+ sub-millisecond mocked review runs).
        let mut prev_loaded: Vec<darkmux_types::LoadedModel> = Vec::new();
        let mut seeded = false;
        let handle = thread::Builder::new()
            .name("review-telemetry".to_string())
            .spawn(move || loop {
                // Sleep out `interval` FIRST, THEN sample — deliberately
                // NOT sample-then-sleep. A review run's own dispatches
                // (bundling, probe draws, judge passes) are real LMStudio
                // round trips that take real wall-clock, so a run genuinely
                // running past one `interval` gets its first sample right
                // on schedule. The load-bearing side effect: at the
                // PRODUCTION cadence ([`REVIEW_TELEMETRY_INTERVAL`], 2s),
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
                // lms load/unload deltas. Only diff against a SUCCESSFUL
                // probe — a failed `list_loaded` is skipped (leaves
                // `prev_loaded` intact) so a transient lms hiccup doesn't
                // emit a flurry of spurious unloads (same guard as
                // dispatch_internal.rs's sampler).
                if let Ok(cur) = lms_fn() {
                    let diffs = if seeded {
                        darkmux_crew::telemetry_sampler::lms_diff(&prev_loaded, &cur)
                    } else {
                        seeded = true;
                        darkmux_crew::telemetry_sampler::lms_diff(&[], &cur)
                    };
                    for payload in diffs {
                        let record = darkmux_crew::dispatch::build_telemetry_record(
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
    /// RAII tie-in that guarantees no orphaned sampler thread outlives its
    /// owner ([`ReviewObs`] on the sequential path, `run_review_graph`'s own
    /// local sampler on the graph path), on every exit path (clean finish,
    /// early `?`-return, or panic).
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

/// (#1434) Run-observability helper for the sequential `run_judge_only`
/// path (the `--charges-file` re-judge). Emits the SAME generic
/// `step result` records `run_review_graph`'s step kinds emit (via
/// [`emit_review_step_result`]) — but through the injected [`ReviewEmitter`]
/// rather than the global `darkmux_flow::record`, so the sink-agnostic /
/// lab-vs-fleet-boundary discipline the driver has always kept is preserved
/// (a bench emitter suppresses; `FleetFlowEmitter` writes the real stream).
///
/// It also owns the run's [`HostTelemetrySampler`] (#1247 "no blind runs")
/// and interleaves its samples with the run's own records: [`Self::drain`]
/// fires before every `step result` emission (and once more in `Drop`), so
/// telemetry streams alongside the run rather than batching at the end. The
/// sampler thread is torn down by [`HostTelemetrySampler`]'s own `Drop`
/// (a field of this struct, run automatically after `ReviewObs::drop`
/// returns), so it never outlives the run on any exit path.
///
/// There is deliberately NO task-level liveness bookend here — the caller's
/// `with_dispatch_bookends` wrap (`src/mission_launch_review.rs`) opens/closes
/// the canonical `dispatch start`/`dispatch complete`/`dispatch error`
/// record around the whole `run_judge_only` call (contract 2 / #1349), the
/// same edge the graph path relies on. A second review-scoped bookend would
/// be exactly the competing-vocabulary duplication #1349 retired.
struct ReviewObs<'a> {
    case_id: String,
    emitter: &'a mut dyn ReviewEmitter,
    telemetry: HostTelemetrySampler,
}

impl<'a> ReviewObs<'a> {
    fn new(emitter: &'a mut dyn ReviewEmitter, case_id: &str, crew: &str) -> Self {
        Self::new_with_telemetry(
            emitter,
            case_id,
            crew,
            REVIEW_TELEMETRY_INTERVAL,
            REVIEW_TELEMETRY_POLL,
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
    /// [`REVIEW_TELEMETRY_INTERVAL`] and the samplers at the real
    /// `sample_host` / `darkmux_profiles::lms::list_loaded`.
    fn new_with_telemetry(
        emitter: &'a mut dyn ReviewEmitter,
        case_id: &str,
        crew: &str,
        telemetry_interval: Duration,
        telemetry_poll: Duration,
        sample_fn: fn() -> HostSample,
        lms_fn: fn() -> anyhow::Result<Vec<darkmux_types::LoadedModel>>,
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
            emitter,
        }
    }

    /// Drain every telemetry sample buffered since the last drain and emit
    /// each through the injected emitter — called immediately before every
    /// `step result` record so telemetry streams alongside the run rather
    /// than batching at the end.
    fn drain(&mut self) {
        let records: Vec<darkmux_flow::FlowRecord> = self.telemetry.rx.try_iter().collect();
        for record in records {
            self.emitter.emit(record);
        }
    }

    /// Drain pending telemetry, then emit one generic `step result` record —
    /// the SAME shape [`emit_review_step_result`] builds for the graph path
    /// (`action = "step result"`, `handle = step_id`, `session_id = case_id`,
    /// a `kind` field), just routed through the injected emitter.
    fn step_result(&mut self, kind: &str, step_id: &str, payload: serde_json::Value) {
        self.drain();
        self.emitter.emit(review_step_result_record(kind, step_id, &self.case_id, payload));
    }
}

impl Drop for ReviewObs<'_> {
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

// ─── the envelope ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReviewEnvelope {
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
    /// (#1260) Confirmed findings the verify seat ruled `verified` —
    /// posted WITHOUT the manual-verification marker. Zero (and never
    /// serialized) on crews without the seat.
    #[serde(default, skip_serializing_if = "usize_is_zero")]
    pub verified: usize,
    /// (#1260) Confirmed findings the verify seat REFUTED — demoted to the
    /// archived tier with the demotion recorded on the flag.
    #[serde(default, skip_serializing_if = "usize_is_zero")]
    pub refuted: usize,
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
    /// `ReviewEnvelope::crew` is only the crew's NAME; if the operator
    /// edits or renames that crew's staffing between runs, a series
    /// comparison keyed on the name alone silently corrupts. This snapshot
    /// makes the run's knob config self-contained in its own artifact — an
    /// experiment-series lab view can diff two runs' `staffing` fields
    /// directly, never re-reading a registry that may have since changed.
    /// `Option` (not a bare `Default`) so pre-#1247 envelopes deserialize
    /// as `None` rather than a misleadingly-empty snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staffing: Option<StaffingSnapshot>,
    /// (#1260) Non-fatal run findings the operator should read — e.g. a
    /// remote probe seat failing after bounded retries (reduced coverage)
    /// or the probe stage's remote token budget exhausting. Empty on a
    /// clean run (and then not serialized — older envelopes are unchanged).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// (#1260/#1177 — operator decision) Per-stage remote token-bucket
    /// accounting: one record per pipeline stage that made (or skipped) at
    /// least one REMOTE call. Empty (and unserialized) on local-only runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_budgets: Vec<RemoteBudgetRecord>,
    /// (#1299) The `needs_check` tier clustered by `(file, mechanism-family)`
    /// when it exceeded [`NEEDS_CHECK_CLUSTER_THRESHOLD`] — a renderer emits
    /// one "N related concerns" bullet per cluster instead of N raw ones, so
    /// a duplicative tier can't wall-of-text. NEVER a drop: the clusters'
    /// counts sum to `needs_check`. Empty (and unserialized) when the tier
    /// was at or below the threshold — small sets render raw.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs_check_clusters: Vec<NeedsCheckCluster>,
}

/// Serde helper for the skip-if-zero count fields — keeps envelopes from
/// crews without the verify seat byte-identical to pre-#1260 ones.
fn usize_is_zero(n: &usize) -> bool {
    *n == 0
}

/// (#1260) One pipeline stage's remote token-bucket outcome — see
/// [`ReviewEnvelope::remote_budgets`]. An "execution" is one stage (the
/// probe pass, each judge pass, the verify pass), each drawing from its own
/// `remote.max_tokens_per_execution` allowance so a runaway stage is caught
/// at the cap without starving later stages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteBudgetRecord {
    /// `probe` | `judge-pass1` | `judge-pass2` | `verify`.
    pub stage: String,
    pub max_tokens: u64,
    pub used_tokens: u64,
    pub exhausted: bool,
    /// Remote calls NOT made because the bucket had already exhausted.
    pub skipped_calls: u32,
}

/// (#1260) In-flight bucket state for one stage's remote calls. Local calls
/// never touch it. `record()` yields `None` when the stage made no remote
/// calls at all, so local-only envelopes carry no budget rows.
pub(crate) struct RemoteBucket {
    stage: &'static str,
    budget: u64,
    used: u64,
    calls: u32,
    skipped: u32,
}

impl RemoteBucket {
    fn new(stage: &'static str, budget: u64) -> Self {
        Self { stage, budget, used: 0, calls: 0, skipped: 0 }
    }

    fn exhausted(&self) -> bool {
        self.used >= self.budget
    }

    /// Gate one remote call: `false` ⇒ the bucket is exhausted and the call
    /// must not fire (counted as skipped, for the envelope's named reason).
    fn admit(&mut self) -> bool {
        if self.exhausted() {
            self.skipped += 1;
            false
        } else {
            true
        }
    }

    fn spend(&mut self, tokens: u64, calls: u32) {
        self.used += tokens;
        self.calls += calls;
    }

    fn record(&self) -> Option<RemoteBudgetRecord> {
        if self.calls == 0 && self.skipped == 0 {
            return None;
        }
        Some(RemoteBudgetRecord {
            stage: self.stage.to_string(),
            max_tokens: self.budget,
            used_tokens: self.used,
            exhausted: self.exhausted(),
            skipped_calls: self.skipped,
        })
    }
}

/// (#1260) The dispatch identity for one seat. LOCAL seats use the
/// darkmux-namespaced LMStudio identifier (`swap::namespaced_identifier`);
/// REMOTE seats keep the profile's bare model id — nothing is loaded into
/// LMStudio, so no `darkmux:` namespace entry is ever minted for them (the
/// namespace marks darkmux-owned LOCAL residency, and a remote seat has
/// none).
pub fn seat_identifier(pm: &ProfileModel) -> String {
    if pm.is_remote() {
        pm.id.clone()
    } else {
        swap::namespaced_identifier(pm)
    }
}

/// (#1260) The remote endpoint HOST for provenance records — host only,
/// NEVER credentials and never the full deployment path (an Azure
/// deployment URL embeds the deployment name; the host is the boundary
/// operators reason about). `None` for local seats.
fn seat_endpoint_host(pm: &ProfileModel) -> Option<String> {
    let ep = pm.endpoint.as_ref().filter(|e| e.is_remote())?;
    let url = ep.base_url();
    Some(
        url.split("://")
            .nth(1)
            .and_then(|s| s.split('/').next())
            .unwrap_or("remote")
            .to_string(),
    )
}

/// (#1260) The endpoint a seat's chat calls should route through — `Some`
/// only when the staffing's resolved model declares a remote endpoint.
fn seat_endpoint(pm: &ProfileModel) -> Option<&ModelEndpoint> {
    pm.endpoint.as_ref().filter(|e| e.is_remote())
}

/// One seat staffing's resolved config, snapshotted as ACTUALLY used —
/// see [`ReviewEnvelope::staffing`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatStaffingSnapshot {
    pub name: String,
    /// (#1475 packet 2) The review ROLE this seat was staffed for
    /// (`review-probe-high`/`-mid`/`-low`, `review-judge`, `review-verify`)
    /// when the role→profile flip produced it — so the envelope names WHICH
    /// role bound this seat's profile+model, the truthful record of the
    /// task→role→profile→model rollup (operator sovereignty #44). `Option` +
    /// `#[serde(default)]`: absent on the legacy roster-scoring staffings and
    /// on pre-#1475 snapshots, the module's standard schema-lenience.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_id: Option<String>,
    /// The darkmux-namespaced LMStudio identifier for a LOCAL seat; the
    /// profile's bare model id for a REMOTE one — the same form
    /// [`MemberRecord::model`] records, so the two line up at a glance.
    pub model: String,
    /// (#1260) `true` when the staffing's model declares a remote endpoint.
    /// Skipped when `false` so pre-#1260 snapshots round-trip unchanged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remote: bool,
    /// (#1260) Endpoint HOST only — never credentials, never the full
    /// deployment path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub k: u32,
    /// (#1266) The judge seat's resolved consensus depth (`passes` — 1
    /// single / 2 double-confirm / N unanimous), snapshotted so every run is
    /// self-describing. Present on every seat's snapshot the same way `k` is
    /// (the judge is the consumer; other seats carry it inertly). Defaults to
    /// 2 on read so a pre-1.3 snapshot (this field didn't exist) deserializes
    /// as today's double-confirm rather than a hard parse failure — the
    /// module's standard schema-lenience.
    #[serde(default = "default_snapshot_passes")]
    pub passes: u32,
    /// The resolved `ProfileModel`'s DECLARED context length — settings
    /// provenance per run, so "what context was this model loaded at" is
    /// never a forensic question (a sibling concern to the config-vs-
    /// measured-context mismatch class of bug #1135 shipped). `Option` +
    /// `#[serde(default)]` so a pre-#1256 snapshot (staffing existed, this
    /// field didn't) deserializes as `None` rather than a hard parse failure
    /// — the same schema-lenience discipline every field in this module
    /// follows. (#1282: `n_ctx` is optional on `ProfileModel` itself now —
    /// resolution requires it for LOCAL seats, so their snapshots always
    /// carry a value; a REMOTE seat, #1260, has no local context and stays
    /// `None` here.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n_ctx: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<BundleSelector>,
    /// (#1426 ship-2 / #44) HOW this seat's model was chosen — `scored`
    /// (capability scoring against the roster profile, with what it scored
    /// against) or `pinned` (which launch param pinned it). The snapshot
    /// already recorded WHAT resolved; this records WHY, so the operator
    /// never wonders where the staffing decision came from. `Option` +
    /// `#[serde(default)]` — a pre-ship-2 snapshot (field absent)
    /// deserializes as `None`, the module's standard schema-lenience.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<darkmux_crew::resourcing::StaffingProvenance>,
}

/// Per-seat resolved staffing snapshot — `review-probe` (one or more
/// staffings) + `review-judge` (exactly one) + the optional `review-verify`
/// seat (#1260). See [`ReviewEnvelope::staffing`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StaffingSnapshot {
    pub probes: Vec<SeatStaffingSnapshot>,
    pub judge: Option<SeatStaffingSnapshot>,
    /// (#1260) Present iff the crew declares the `review-verify` seat —
    /// absent (and never serialized) otherwise, so pre-#1260 snapshots
    /// round-trip unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<SeatStaffingSnapshot>,
    /// (#1302) The crew's resolved `request_changes` flag — snapshotted so the
    /// render path reads the run's own blocking-vs-advisory choice from its
    /// self-describing artifact, and a serialized envelope re-rendered later
    /// picks the same review event. Defaults to `false` on read (skipped when
    /// `false`) so pre-#1302 snapshots round-trip unchanged as the non-blocking
    /// default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub request_changes: bool,
}

/// (#1266) Snapshot default for `SeatStaffingSnapshot::passes` — 2 (double-
/// confirm), so a pre-1.3 envelope missing the field reads as today's judge.
fn default_snapshot_passes() -> u32 {
    2
}

pub fn staffing_snapshot(
    probes: &[ResolvedSeatStaffing],
    judge: &ResolvedSeatStaffing,
    verify: Option<&ResolvedSeatStaffing>,
    request_changes: bool,
) -> StaffingSnapshot {
    fn one(s: &ResolvedSeatStaffing) -> SeatStaffingSnapshot {
        SeatStaffingSnapshot {
            name: s.name.clone(),
            // (#1475 packet 2) Carry the seat's bound role verbatim so the
            // envelope records role → profile → model, not just profile+model.
            role_id: s.role_id.clone(),
            model: seat_identifier(&s.pm),
            remote: s.pm.is_remote(),
            endpoint: seat_endpoint_host(&s.pm),
            k: s.k,
            passes: s.passes,
            n_ctx: s.pm.n_ctx,
            max_tokens: s.max_tokens,
            selector: s.selector.clone(),
            // (#44) Scored-vs-pinned, carried verbatim from the resolver.
            provenance: s.provenance.clone(),
        }
    }
    StaffingSnapshot {
        probes: probes.iter().map(one).collect(),
        judge: Some(one(judge)),
        verify: verify.map(one),
        // (#1302) The run's blocking-vs-advisory choice, snapshotted for the
        // render path (see `StaffingSnapshot::request_changes`).
        request_changes,
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

/// Production [`ModelCycler`] (#1230 Packet 1 cutover): every residency
/// decision now routes through `darkmux_gestalt::plan_acquire`/
/// `plan_release` — the pure planner the dispatch preflight routes
/// through — executed via the real `LmsHost`/`MacProbe`
/// port adapters (`darkmux_profiles::gestalt_host`). Those adapters existed
/// fully built and unit-tested but had ZERO production callers before this
/// cutover; this is their first one.
///
/// This retires the review's own private `ResidencyDecision`/
/// `decide_residency` (the pre-cutover duplicate `tests/gestalt_parity.rs`
/// existed only to keep the two from silently forking) and the
/// `resolve_auto` hardware-tier table (see `resolve_mode` below) in favor
/// of ONE canonical arbiter.
///
/// Namespaced under `darkmux:` and context-sufficiency aware exactly as
/// before — that logic now lives in `darkmux_gestalt::decide_residency`
/// rather than being re-derived here. One deliberate behavior divergence,
/// named in `darkmux_gestalt::planner`'s "Cutover behavior changes" module
/// doc: a foreign (non-darkmux) resident sharing the model key no longer
/// hard-blocks the seat. The planner loads darkmux's own namespaced copy
/// ALONGSIDE it when the facts show room (absolute namespace ownership,
/// operator decision 2026-07-10, #1274) — still never reusing or unloading
/// user state, just no longer refusing to proceed around it.
pub struct LmsCycler;

/// Per-call [`Facts`] for [`LmsCycler`] (#1230 Packet 1): observed residents
/// from a real `LmsHost::list_resident()`, and pool facts from a real
/// `MacProbe::pools()` — both port adapters constructed HERE, their first
/// production call site.
///
/// `catalog: None` — the review has never run the #1276 existence
/// fast-fail (an unknown model key fails at the real `lms load` call the
/// same way it always has), and wiring `list_catalog()` here would cost
/// every `ensure_loaded` an extra `lms ls --json` round-trip for a check
/// this call site doesn't use. `budget: None` — the #1243 AI-RAM-budget
/// config knob (`runtime.max_model_ram_gb`) isn't plumbed anywhere in the
/// codebase yet; inventing it as a side effect of this cutover is out of
/// scope. A `MacProbe` failure (including its documented non-macOS v1
/// scope) degrades to empty pools — "no known constraint," the same
/// leniency the planner's budget/pool arms already document.
fn gather_facts(host: &mut LmsHost) -> Result<Facts> {
    let residents = host
        .list_resident()
        .map_err(|e| anyhow!("darkmux: could not read LMStudio residents (`lms ps`): {e}"))?;
    let pools = MacProbe.pools().unwrap_or_default();
    Ok(Facts { residents, pools, ..Default::default() })
}

/// Non-load-bearing placeholder: `Facts.catalog = None` in `gather_facts`
/// means every `V1Estimator::estimate_bytes` call returns `None` (unknown)
/// regardless of `kv_bytes_per_ctx_token` — this cutover doesn't wire
/// catalog sizing, so the estimator is structurally inert here today. A
/// concrete estimator is still required because `plan_acquire`/`plan_waves`
/// take one by signature; `0` documents "not yet meaningful," not a tuned
/// value.
fn inert_estimator() -> V1Estimator {
    V1Estimator { kv_bytes_per_ctx_token: 0 }
}

impl ModelCycler for LmsCycler {
    fn ensure_loaded(&mut self, pm: &ProfileModel) -> Result<()> {
        let n_ctx = pm.require_n_ctx()?;
        let identifier = darkmux_gestalt::namespaced_identifier(&pm.id, pm.identifier.as_deref());
        let mut host = LmsHost::new();
        let facts = gather_facts(&mut host)?;
        let placement =
            Placement { model_key: pm.id.clone(), identifier, min_ctx: n_ctx, seat: "review".to_string() };
        let opts = AcquireOpts::new(CallerIntent::Auto, AcquireScope::Additive);
        let plan =
            darkmux_gestalt::plan_acquire(std::slice::from_ref(&placement), &facts, opts, &inert_estimator());
        let deadline = resolved_load_deadline();
        for planned in &plan.actions {
            match &planned.action {
                Action::Reuse { identifier, resident_ctx, min_ctx } => {
                    if *resident_ctx > u64::from(*min_ctx) {
                        // (#1271 review round) Declared-vs-actual ctx
                        // divergence can happen ACROSS profiles (a bigger
                        // load from another profile satisfies this seat's
                        // minimum) — leave a trace until #1257's full
                        // load-config provenance lands.
                        println!(
                            "cycler: reusing {identifier} at ctx={resident_ctx} (declared {min_ctx})"
                        );
                    }
                }
                Action::Unload { target } => {
                    // (#1271) Reconcile rather than attempt a doomed second
                    // load: the stale instance's free-phase unload always
                    // precedes its reload in `plan.actions` (the planner's
                    // free-then-load ordering contract), logged in the same
                    // unload-then-load style.
                    println!("cycler: unload {} — reconciling for {}", target.identifier(), pm.id);
                    host.unload(target, deadline).map_err(|e| {
                        anyhow!("darkmux: unload failed for \"{}\": {e}", target.identifier())
                    })?;
                }
                Action::Load { model_key, identifier, min_ctx } => {
                    host.load(model_key, identifier, *min_ctx, deadline).map_err(|e| {
                        anyhow!("darkmux: load failed for \"{model_key}\" (\"{identifier}\"): {e}")
                    })?;
                }
                Action::Block { model_key, .. } => {
                    bail!("darkmux: cannot load \"{model_key}\" for the review — {}", planned.reason)
                }
            }
        }
        Ok(())
    }

    fn release(&mut self, pm: &ProfileModel) -> Result<()> {
        let identifier = darkmux_gestalt::namespaced_identifier(&pm.id, pm.identifier.as_deref());
        let mut host = LmsHost::new();
        let facts = gather_facts(&mut host)?;
        let placement = Placement {
            model_key: pm.id.clone(),
            identifier,
            min_ctx: pm.n_ctx.unwrap_or(0),
            seat: "review".to_string(),
        };
        let plan = darkmux_gestalt::plan_release(std::slice::from_ref(&placement), &[], &facts);
        let deadline = resolved_load_deadline();
        for planned in &plan.actions {
            if let Action::Unload { target } = &planned.action {
                host.unload(target, deadline)
                    .map_err(|e| anyhow!("darkmux: unload failed for \"{}\": {e}", target.identifier()))?;
            }
        }
        Ok(())
    }
}

// ─── constants ────────────────────────────────────────────────────────────

const PROBE_TEMPERATURE: f32 = 0.2;
const JUDGE_TEMPERATURE: f32 = 0.2;
const DEFAULT_PROBE_MAX_TOKENS: u32 = 4_000;
const DEFAULT_JUDGE_MAX_TOKENS: u32 = 20_000;
/// (#1260) Reasoning-aware completion FLOOR for REMOTE seats. Local-tuned
/// defaults (probe's 4000 especially) are the reasoning-guillotine class on
/// hosted reasoning models — reasoning tokens bill inside
/// `max_completion_tokens`, so a low cap gets consumed by invisible thinking
/// and the seat returns empty content (the exact lesson `dispatch_internal`
/// already learned: its single-shot default rises to 16384 when a hosted
/// endpoint declares `reasoning_effort`). A remote seat with NO explicit
/// staffing `max_tokens` therefore never dips below this floor; an explicit
/// staffing `max_tokens` always wins verbatim (operator sovereignty — the
/// operator may know their task is short). Local seats are unaffected.
const REMOTE_REASONING_MAX_TOKENS_FLOOR: u32 = 16_384;
const REVIEW_PROTOCOL: &str = "double-confirm-v1";

/// (#1260) Resolve one seat's completion cap: an explicit staffing
/// `max_tokens` always wins verbatim; otherwise a REMOTE seat floors at
/// [`REMOTE_REASONING_MAX_TOKENS_FLOOR`] (never lowering an already-higher
/// local default — a floor, not a clamp), while a LOCAL seat keeps its
/// local-tuned default. Applies uniformly to probe, judge, and verify seats.
fn resolve_seat_max_tokens(s: &ResolvedSeatStaffing, local_default: u32) -> u32 {
    match s.max_tokens {
        Some(explicit) => explicit,
        None if s.pm.is_remote() => local_default.max(REMOTE_REASONING_MAX_TOKENS_FLOOR),
        None => local_default,
    }
}

/// (#1230 Packet 1 cutover) Auto-resolution: `Parallel` iff gestalt's
/// co-residency wave scheduler ([`darkmux_gestalt::plan_waves`],
/// `WaveMode::Auto`) packs every distinct LOCAL model (probe seats + judge,
/// deduped) into ONE wave — i.e. the same arithmetic `plan_acquire`'s
/// budget/pool-headroom arms use judges them safe to hold resident
/// together, against REAL facts (a live `MacProbe` pool snapshot, and the
/// #1243 AI-RAM budget when an operator has configured one). More than one
/// wave means they don't fit — the same shape `Sequential` always meant,
/// now DERIVED from live facts instead of a static hardware-tier lookup
/// table.
///
/// Replaces the deleted `resolve_auto` tier table. `darkmux_gestalt::waves`'s
/// own module doc already claimed the hardware-tier-threshold concept "was
/// removed end-to-end in #602/#604/#605" — that claim was aspirational until
/// this function, the review's last holdout, stopped re-deriving one.
fn wave_schedule_to_exec_mode(schedule: &darkmux_gestalt::WaveSchedule) -> ExecMode {
    if schedule.waves.len() <= 1 {
        ExecMode::Parallel
    } else {
        ExecMode::Sequential
    }
}

/// Gathers real facts and asks gestalt's wave scheduler whether `placements`
/// fit one wave. Separated from [`wave_schedule_to_exec_mode`] (the pure
/// projection, directly unit-testable against a scripted `WaveSchedule`) so
/// the I/O — `LmsHost::list_resident` + `MacProbe::pools`, the SAME adapters
/// [`LmsCycler`] wires — lives in exactly one place. A residency-read
/// failure degrades to `Sequential` (never guess `Parallel` without knowing
/// what's already resident) with a loud stderr line, never a silent
/// downgrade to a riskier mode.
fn resolve_auto_via_waves(placements: &[Placement]) -> ExecMode {
    if placements.is_empty() {
        return ExecMode::Parallel;
    }
    let mut host = LmsHost::new();
    let facts = match gather_facts(&mut host) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "darkmux: could not resolve auto exec mode from live LMStudio state, \
                 defaulting to sequential: {e}"
            );
            return ExecMode::Sequential;
        }
    };
    match darkmux_gestalt::plan_waves(placements, &facts, &inert_estimator(), darkmux_gestalt::WaveMode::Auto)
    {
        Ok(schedule) => wave_schedule_to_exec_mode(&schedule),
        // `Auto` mode never refuses (only `ForceParallel` can, per
        // `plan_waves`'s own doc) — kept for exhaustiveness rather than an
        // unwrap on a real dispatch path.
        Err(_) => ExecMode::Sequential,
    }
}

fn resolve_mode(mode: ExecMode, probes: &[ResolvedSeatStaffing], judge: &ResolvedSeatStaffing) -> ExecMode {
    match mode {
        ExecMode::Auto => {
            // (#1260) Only LOCAL models count toward the residency budget —
            // a remote seat is a zero-footprint placement (nothing loaded,
            // no pool bytes), so it never forces Sequential.
            let mut placements: Vec<Placement> = Vec::new();
            let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for s in probes.iter().chain(std::iter::once(judge)).filter(|s| !s.pm.is_remote()) {
                let identifier = darkmux_gestalt::namespaced_identifier(&s.pm.id, s.pm.identifier.as_deref());
                if !seen.insert(identifier.clone()) {
                    continue; // dedup — a repeated model needs one placement, not one per seat
                }
                placements.push(Placement {
                    model_key: s.pm.id.clone(),
                    identifier,
                    min_ctx: s.pm.n_ctx.unwrap_or(0),
                    seat: "review-auto".to_string(),
                });
            }
            resolve_auto_via_waves(&placements)
        }
        other => other,
    }
}

// ─── (#1512, #1513 review) role validation dissolved ─────────────────────
//
// `ReviewSeats`/`validate_review_crew` used to re-check a `ResolvedCrew`'s
// `seats` map carried >= 1 probe staffing, EXACTLY 1 judge, and an optional
// verify. That check is now REDUNDANT by construction: `resolve_review_roles`
// (`darkmux_crew::resourcing`) already enforces every one of those shape
// rules as part of resolving them (a config with zero probe-role tasks, or
// no judge task, is a resolution ERROR, never a value that reaches a
// caller) — so a `ResolvedReviewRoles` is valid the moment it exists.
// There is nothing left to separately validate, and nothing to extract:
// `.probes`/`.judge`/`.verify` are direct fields.

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
/// `null/bounds` (all contain "nan"), so two DISTINCT unanchored charges on
/// a billing corpus collapsed in dedup and a real defect was silently
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
            // (#1299) Provenance / field-name-mismatch — the family for a
            // value recorded under the WRONG field, read from the WRONG
            // source, or a derived value that drops its source-of-record.
            //
            // Ordered BEFORE `null/bounds` DELIBERATELY (#1299 MUST_FIX): a
            // provenance defect co-located with a bounds defect (same line,
            // same symbol, same anchor) whose prose mentions `index`/`array`
            // must land HERE, not in bounds — otherwise the two collapse and
            // the provenance bug is lost. Specific families are checked
            // before the coarse `null/bounds` catch-all for exactly this
            // reason. This is one of the two guards (the other is symbol
            // overlap) that keeps a provenance bug from merging into a bounds
            // bug — e.g. the #396 `incorporatedDate` (wrong field) vs
            // `docFileEntry` (out of bounds) in the same file.
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
                &["field", "name"],
                &["wrong", "field"],
                &["wrong", "source"],
                &["field", "mismatch"],
                &["recorded", "under"],
                &["source", "field"],
                &["source", "mapping"],
                &["source", "of", "record"],
            ],
        ),
        (
            // (#1299) The coarse null-safety/bounds family, checked LAST so
            // every more-specific family above wins first. A frontier judge
            // words the SAME undefined/out-of-bounds defect many ways, and
            // the old table split those synonyms across `null/nan` and
            // `other`, so a bug stated five ways never shared a dedup key.
            //
            // Keywords are ANCHORED PHRASES, never BARE GENERIC TOKENS
            // (#1299 MUST_FIX): `index`/`array`/`bounds` alone co-occur
            // across unrelated defect classes (a provenance bug can read the
            // "wrong source at this index"), so classifying on them merged
            // distinct bugs. Only `undefined`/`null`/`nan` and the multi-word
            // `out of bounds`/`out of range` — signals that actually name a
            // null-safety/bounds defect — count. This deliberately collapses
            // FEWER restatements (a bare-`index` restatement lands in
            // `other`); that's the right trade (duplicates beat false cuts).
            // Safe against over-collapse anyway: the dedup predicate ALSO
            // demands a shared symbol AND a shared location, never family
            // alone.
            "null/bounds",
            &[
                &["null"],
                &["undefined"],
                &["nan"],
                &["none"],
                &["nil"],
                &["out", "of", "bounds"],
                &["out", "of", "range"],
                &["index", "out", "of"],
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

// ─── referenced-symbol extraction (a dedup-predicate signal) ─────────────

/// The set of code identifiers a charge NAMES — the function/field/variable
/// it points at (`docFileEntry`, `writeDocumentInstance`, `isInThousands`).
/// Pure, deterministic string work — no dispatch, no similarity model
/// (#1299). A maximal `[A-Za-z0-9_]` run counts as a SYMBOL only when it
/// reads like code rather than prose:
///
///  * camelCase / PascalCase — an internal case change (`docFileEntry`,
///    `FinancialStatement`), OR
///  * snake_case — an interior `_` between alphanumerics (`doc_file_entry`),
///    OR
///  * a call site — the run is immediately followed by `(` (`record(`).
///
/// Plain lowercase English words are EXCLUDED even inside backticks: making
/// `record` / `value` / `data` a symbol would let two unrelated bugs that
/// both mention a common word false-collapse — the exact over-cut #1299's
/// asymmetric objective ("a leaked duplicate beats a false cut") forbids. A
/// missed specific symbol only costs a leaked duplicate; a spurious generic
/// one risks merging two real bugs. Comparison is lowercased so
/// `DocFileEntry` and `docFileEntry` overlap.
fn referenced_symbols(charge_text: &str) -> std::collections::BTreeSet<String> {
    let chars: Vec<char> = charge_text.chars().collect();
    let mut out = std::collections::BTreeSet::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_alphanumeric() || chars[i] == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let run: String = chars[start..i].iter().collect();
            // An identifier starts with a letter or `_`, never a bare number.
            let first = run.chars().next().unwrap();
            let starts_ok = first.is_alphabetic() || first == '_';
            // A call site: the run is IMMEDIATELY followed by `(` (no space)
            // — catches lowercase method/function names the case rules miss.
            let followed_by_call = i < chars.len() && chars[i] == '(';
            if starts_ok && (is_code_identifier(&run) || followed_by_call) {
                out.insert(run.to_lowercase());
            }
        } else {
            i += 1;
        }
    }
    out
}

/// True when `run` has an internal case change (camelCase / PascalCase) or
/// an interior underscore (snake_case) — the "this is an identifier, not an
/// English word" test. See [`referenced_symbols`].
fn is_code_identifier(run: &str) -> bool {
    let cs: Vec<char> = run.chars().collect();
    // snake_case: an underscore flanked by alphanumerics on BOTH sides.
    let snake = cs.iter().enumerate().any(|(k, &c)| {
        c == '_' && k > 0 && k + 1 < cs.len() && cs[k - 1].is_alphanumeric() && cs[k + 1].is_alphanumeric()
    });
    // camelCase / PascalCase: a lowercase-or-digit immediately followed by
    // an uppercase (`docFileEntry` → `cF`, `NaN` → `aN`).
    let camel = cs
        .windows(2)
        .any(|w| (w[0].is_lowercase() || w[0].is_ascii_digit()) && w[1].is_uppercase());
    snake || camel
}

// ─── dedup ────────────────────────────────────────────────────────────────

/// Dedup raw probe flags (#1299). Two flags collapse ONLY when ALL FOUR
/// signals agree — the predicate is an AND, never an OR, and ANY missing or
/// diverging signal keeps the two findings SEPARATE:
///
///  1. same `bundle_id` (same file), AND
///  2. same [`mechanism_family`], AND
///  3. an overlapping referenced SYMBOL ([`referenced_symbols`] — an empty
///     set overlaps nothing, so a charge that names no identifier collapses
///     with nothing), AND
///  4. an overlapping LOCATION — both flags anchored, to the SAME diff site
///     ([`extract_new_side_anchor`]). A missing anchor (the #1299 frontier
///     case — 0/9 anchored) or two DIFFERENT anchors → separate.
///
/// This encodes the operator's asymmetric objective: a leaked duplicate is
/// acceptable; a FALSE CUT (two distinct bugs merged into one) is not. So a
/// frontier judge that words ONE defect many ways AT ONE SITE collapses,
/// while the SAME symbol at DIFFERENT sites (`docFileEntry` across five
/// branches) stays as separate findings — different sites can be different
/// bugs, and every site keeps its own finding. When nothing anchors, the
/// honest result is "fewer collapses, more duplicates," never an over-merge;
/// the `needs_check` volume is tamed downstream by [`cluster_needs_check`].
///
/// Collapsing AGGREGATES, never discards: a survivor folds in each absorbed
/// same-site finding's symbols, so a later restatement overlapping EITHER of
/// them still collapses (transitive same-site duplicates). Because collapse
/// requires an IDENTICAL location, no distinct site is ever hidden.
///
/// Anchor extraction happens HERE, populating `ProbeFlag::anchor` on the
/// surviving flags — `diff` is why this function needs it.
///
/// (#1352) The survivor-scan PROCEDURE around this predicate — first-match
/// in input order, aggregate-on-collapse, never silently drop — is now the
/// generic `darkmux_crew::step_kinds::patterns::dedup` Tier 2 pattern; the
/// four-signal mechanism-family-keying predicate above stays here as a
/// [`DedupStrategy`] impl ([`MechanismFamilyDedup`]) because — per #1352's
/// own framing — the MATCHING ALGORITHM is legitimately bespoke review
/// domain logic, while the scan procedure around it had no review-specific
/// knowledge at all. Pure control-flow extraction: every `dedup_*` unit
/// test below pins the exact same outcomes as the pre-#1352 hand-written
/// loop.
pub fn dedup_flags(flags: Vec<ProbeFlag>, diff: &str) -> (Vec<ProbeFlag>, DedupStats) {
    let strategy = MechanismFamilyDedup { diff };
    let outcome = pattern_dedup(
        flags,
        &strategy,
        // New survivor: stamp the strategy's computed anchor onto the flag
        // itself (`ProbeFlag::anchor` starts `None` at construction — see
        // its own doc; this is where it gets populated for a real
        // survivor).
        |flag, key| flag.anchor = key.anchor.clone(),
        // Collapse: AGGREGATE, never discard (#1299 MUST_FIX) — fold the
        // absorbed finding's framing into the survivor so a rendered
        // finding shows BOTH. The safety net — even a residual false cut
        // degrades to "one bullet, two framings," never a vanished defect.
        |survivor, candidate| {
            survivor.also_flagged.push(candidate.charge_text);
            survivor.also_flagged.extend(candidate.also_flagged);
        },
    );
    (outcome.items, DedupStats { raw: outcome.raw, deduped: outcome.deduped })
}

/// [`dedup_flags`]'s per-survivor key material (#1352) — the four dedup
/// signals ([`mechanism_family`], the diff anchor, the referenced-symbol
/// set, plus the bundle id) computed once per flag.
struct MechanismFamilyDedupKey {
    bundle_id: String,
    family: &'static str,
    anchor: Option<String>,
    symbols: std::collections::BTreeSet<String>,
}

/// [`dedup_flags`]'s [`DedupStrategy`] plug-in (#1352) — the review
/// pipeline's mechanism-family-keying algorithm, unchanged from its
/// pre-extraction form: two flags collapse only when ALL FOUR signals agree
/// (same bundle, same mechanism family, an overlapping referenced symbol,
/// an overlapping diff anchor — see [`dedup_flags`]'s own doc for the full
/// asymmetric-objective reasoning).
struct MechanismFamilyDedup<'a> {
    diff: &'a str,
}

impl DedupStrategy<ProbeFlag> for MechanismFamilyDedup<'_> {
    type Key = MechanismFamilyDedupKey;

    fn key(&self, item: &ProbeFlag) -> Self::Key {
        MechanismFamilyDedupKey {
            bundle_id: item.bundle_id.clone(),
            family: mechanism_family(&item.charge_text),
            anchor: extract_new_side_anchor(&item.charge_text, self.diff),
            symbols: referenced_symbols(&item.charge_text),
        }
    }

    fn matches(&self, survivor: &Self::Key, candidate: &Self::Key) -> bool {
        survivor.bundle_id == candidate.bundle_id
            && survivor.family == candidate.family
            && candidate.anchor.is_some()
            && survivor.anchor == candidate.anchor
            && !candidate.symbols.is_empty()
            && !survivor.symbols.is_disjoint(&candidate.symbols)
    }

    fn merge_key(&self, survivor: &mut Self::Key, candidate: Self::Key) {
        survivor.symbols.extend(candidate.symbols);
    }
}

// ─── needs_check clustering (tier-volume cap) ────────────────────────────

/// Above this many `needs_check` findings, [`cluster_needs_check`] groups
/// them by `(file, mechanism-family)` so the tier can't wall-of-text a
/// review (#1299 — the #396 review carried ~25 heavily-duplicative
/// `needs_check` items). At or below it, the raw findings render as-is.
/// Named, not a magic literal, so the operator can see the knob.
pub const NEEDS_CHECK_CLUSTER_THRESHOLD: usize = 8;

/// One `(file, mechanism-family)` cluster of `needs_check` findings — a
/// count, never a drop (#1299). Rendered as a single "N related concerns"
/// bullet ([`NeedsCheckCluster::bullet`]) in place of N raw ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeedsCheckCluster {
    /// The bundle id (file path) the clustered findings share.
    pub file: String,
    /// The [`mechanism_family`] the clustered findings share.
    pub mechanism: String,
    /// How many `needs_check` findings this cluster stands in for. The sum
    /// of every cluster's `count` EQUALS the total `needs_check` count —
    /// clustering conserves concerns, it never hides one.
    pub count: usize,
}

impl NeedsCheckCluster {
    /// The single review bullet this cluster renders as — names the count,
    /// the file, and the mechanism, so nothing is hidden behind the cap.
    pub fn bullet(&self) -> String {
        format!(
            "{} related concern{} in {} around {}",
            self.count,
            if self.count == 1 { "" } else { "s" },
            self.file,
            self.mechanism,
        )
    }
}

/// Cluster the `needs_check` tier when it exceeds
/// [`NEEDS_CHECK_CLUSTER_THRESHOLD`] (#1299). Groups the `needs_check`
/// findings by `(bundle_id, mechanism-family)` and returns one
/// [`NeedsCheckCluster`] per group; the sum of the clusters' counts always
/// equals the input `needs_check` count (nothing is ever dropped — clustered
/// findings are counted, not hidden). Returns an EMPTY vec when the tier is
/// at or below the threshold, so small `needs_check` sets render raw. Pure
/// and deterministic: groups are emitted sorted by `(file, mechanism)`, so
/// the same input yields byte-identical output every run.
pub fn cluster_needs_check(judged: &[JudgedFlag]) -> Vec<NeedsCheckCluster> {
    let needs_check: Vec<&JudgedFlag> =
        judged.iter().filter(|j| j.tier == Tier::NeedsCheck).collect();
    if needs_check.len() <= NEEDS_CHECK_CLUSTER_THRESHOLD {
        return Vec::new();
    }
    // BTreeMap keyed on (file, mechanism) → deterministic, already sorted.
    let mut groups: std::collections::BTreeMap<(String, &'static str), usize> =
        std::collections::BTreeMap::new();
    for j in &needs_check {
        let family = mechanism_family(&j.flag.charge_text);
        *groups.entry((j.flag.bundle_id.clone(), family)).or_insert(0) += 1;
    }
    groups
        .into_iter()
        .map(|((file, mechanism), count)| NeedsCheckCluster {
            file,
            mechanism: mechanism.to_string(),
            count,
        })
        .collect()
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
/// field and `judge_one` never renders one) — the Rust review's `manifest`
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
    review_prompt_with_tail(intent_title, intent_body, code, facts, charge, JUDGE_TAIL_INSTRUCTION)
}

/// (#1260) The frozen fenced-JSON instruction tail for the VERIFY seat —
/// identical structure to [`JUDGE_TAIL_INSTRUCTION`], with the adjudication
/// ruling vocabulary ({verified, refuted, uncertain}). Byte-locked by
/// `verify_prompt_matches_frozen_golden` (contract 6).
const VERIFY_TAIL_INSTRUCTION: &str = "Adjudicate the confirmed finding against the code above. End your reply with exactly one fenced JSON block:\n```json\n{\"ruling\": \"verified\" | \"refuted\" | \"uncertain\", \"decisive_evidence\": \"<the specific code line or checked claim that decided it>\", \"note_for_author\": \"<one or two sentences the author reads>\"}\n```";

/// (#1260) Build the verify seat's prompt — the SAME evidence assembly the
/// judge sees (`review_prompt_with_tail`; the adjudication is scoped to the
/// same record), with the verify tail instruction. One shared assembly, two
/// frozen tails — the two prompts structurally cannot drift apart.
pub fn verify_prompt(intent_title: &str, intent_body: &str, code: &str, facts: &[String], charge: &str) -> String {
    review_prompt_with_tail(intent_title, intent_body, code, facts, charge, VERIFY_TAIL_INSTRUCTION)
}

/// The shared judge/verify evidence assembly (see [`judge_prompt`]'s doc
/// for the Phase A provenance of every section) — extracted for the verify
/// seat (#1260) WITHOUT changing a byte of the judge's output: only the
/// tail differs per seat, and the judge's Phase A goldens pin that this
/// refactor is assembly-neutral.
fn review_prompt_with_tail(
    intent_title: &str,
    intent_body: &str,
    code: &str,
    facts: &[String],
    charge: &str,
    tail: &str,
) -> String {
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
    out.push_str(tail);
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
/// caller treats that as [`JudgeRuling::Unparsed`].
pub fn parse_judge_ruling(text: &str) -> Option<(JudgeRuling, String, String)> {
    for cand in judge_json_candidates(text) {
        if let Ok(raw) = serde_json::from_str::<RawJudgeRuling>(&cand) {
            let ruling = match raw.ruling.trim().to_ascii_lowercase().as_str() {
                "confirmed" => JudgeRuling::Confirmed,
                "needs_check" => JudgeRuling::NeedsCheck,
                "false_positive" => JudgeRuling::FalsePositive,
                _ => continue,
            };
            return Some((ruling, raw.decisive_evidence, raw.note_for_author));
        }
    }
    None
}

/// (#1260) Parse a verify-seat reply into `(ruling, decisive_evidence,
/// note_for_author)` — same fence-aware candidate discipline as
/// [`parse_judge_ruling`], matched against the adjudication vocabulary.
/// `None` when no candidate carries a recognized ruling — the caller
/// treats that as [`VerifyRuling::Unparsed`].
pub fn parse_verify_ruling(text: &str) -> Option<(VerifyRuling, String, String)> {
    for cand in judge_json_candidates(text) {
        if let Ok(raw) = serde_json::from_str::<RawJudgeRuling>(&cand) {
            let ruling = match raw.ruling.trim().to_ascii_lowercase().as_str() {
                "verified" => VerifyRuling::Verified,
                "refuted" => VerifyRuling::Refuted,
                "uncertain" => VerifyRuling::Uncertain,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
/// (this module's own pre-packet-3 tests only — see [`ReviewInputs::bundles`]).
fn resolve_bundles(inputs: &ReviewInputs) -> Vec<BundleInput> {
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

/// One single-shot chat call the review wants dispatched. Test closures
/// assert on these fields directly; production wiring turns this into a
/// `darkmux_crew::single_shot::SingleShotRequest` (the caller resolves
/// `base_url`) — or, when `endpoint` is `Some` (#1260), a
/// `darkmux_crew::single_shot::HostedSingleShotRequest` through the hosted
/// dialect. The `system`/`user` TEXTS are identical either way (contract 6
/// — only the transport dialect differs; `temperature` is a local-dialect
/// parameter the hosted body deliberately omits).
pub struct ChatCall<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub user: &'a str,
    pub temperature: f32,
    pub max_tokens: u32,
    /// (#1260) `Some` ⇒ this seat is remote: route through the hosted
    /// dialect, host-side. `None` ⇒ local LMStudio.
    pub endpoint: Option<&'a ModelEndpoint>,
}

// ─── review inputs ────────────────────────────────────────────────────────

/// Everything [`run_judge_only`] needs beyond the injected
/// `chat`/`cycler`. Role-prompt resolution (`review-probe.md` /
/// `review-judge.md`) is the caller's job — `darkmux-lab` already depends
/// on `darkmux-crew`, but pulling role-manifest resolution INTO this
/// module would couple the pure pipeline to `darkmux_crew::loader`'s
/// filesystem/embedded-role search order for no benefit the caller
/// couldn't provide more simply.
pub struct ReviewInputs<'a> {
    pub case_id: String,
    /// (#1512, #1513 review) Every role this run resolved — however many
    /// probe roles, the judge, the optional verify — via the ONE generic
    /// per-task resolver (`darkmux_crew::resourcing::resolve_review_roles`).
    /// Not a "crew": no family grouping, just the three fields
    /// [`run_judge_only`] needs.
    pub roles: &'a ResolvedReviewRoles,
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
    /// no system message at all, and [`ReviewProbeStepKind::run`] (the only
    /// probe dispatcher left — `run_judge_only` never probes) sends an
    /// empty `ChatCall::system` for probe calls to match (which
    /// `darkmux_crew::single_shot::local_chat_body` then omits from the
    /// wire entirely).
    pub probe_system: &'a str,
    /// The judge seat's PERSONA — still sent as a genuine system-role
    /// message (`judge-runner.py`'s `call_judge` does the same).
    pub judge_system: &'a str,
    /// (#1260) The verify seat's PERSONA (`review-verify.md`), sent as a
    /// system-role message like the judge's. Read only when the crew
    /// declares a `review-verify` seat — callers without one may pass the
    /// embedded text anyway (it is simply never dispatched).
    pub verify_system: &'a str,
    /// (#1222 Phase B packet 5 reconciliation) Caller-supplied bundles from
    /// the REAL bundler (`darkmux_lab::lab::bundle::build_bundles`/
    /// `external_bundles`, packet 3), already mapped `Bundle` ->
    /// [`BundleInput`] (via `slice_code` for the code text). `None` falls
    /// back to the provisional [`bundles_from_diff`] — kept ONLY so this
    /// module's own tests (written before packet 3 landed) keep working
    /// unchanged. Production callers (`darkmux mission launch review`,
    /// packet 5's `pr-review run` until #1284 Packet 4b retired it)
    /// always pass `Some` and never invoke the provisional bundler.
    pub bundles: Option<Vec<BundleInput>>,
    /// (#1260/#1177 — operator decision) The per-EXECUTION remote token
    /// allowance, where an execution is one pipeline stage (the probe pass,
    /// each judge pass, the verify pass). Only REMOTE seats draw from it.
    /// Callers resolve it through `darkmux_types::config_access::
    /// remote_max_tokens_per_execution()` (`env > config.remote.
    /// max_tokens_per_execution > 500000`) — injected here, not read in the
    /// driver, so the pipeline stays config-free and unit-testable.
    pub remote_max_tokens_per_execution: u64,
}

pub fn fingerprint(judge_identifier: &str, judge_system: &str) -> serde_json::Value {
    serde_json::json!({
        "judge_model": judge_identifier,
        "judge_temperature": JUDGE_TEMPERATURE,
        "judge_persona_blake3": blake3::hash(judge_system.as_bytes()).to_hex().to_string(),
        "protocol": REVIEW_PROTOCOL,
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
/// `build_prompt` never saw one; `ReviewInputs::intent_title`/
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

// (#1442 ship-2b) `probe_one_draw` retired: the probe stage's per-draw
// dispatch + retry-on-empty loop now lives in the generic `dispatch.map`
// block (`darkmux-crew`'s `map_local_item`/`map_hosted_item`, with
// `retry_on_empty: 1` carrying the historical single retry). Its unit
// coverage's successors are the `dispatch_map_retry_on_empty_*` suite in
// `darkmux-crew::step_kinds::builtins`.

// ─── judge phase (double-confirm) ─────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn run_judge_pass(
    pass: u8,
    model: &str,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    endpoint: Option<&ModelEndpoint>,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
) -> (JudgeRecord, u64, Option<String>) {
    let t0 = Instant::now();
    let call = ChatCall {
        model,
        system,
        user: prompt,
        temperature: JUDGE_TEMPERATURE,
        max_tokens,
        endpoint,
    };
    match chat(&call) {
        Ok(reply) => {
            let seconds = t0.elapsed().as_secs_f64();
            let tokens = reply.total_tokens.unwrap_or(0);
            // (#1300) Captured regardless of parse outcome — an unparsed
            // reply still came from a real served model, and the caller
            // needs that provenance too. Gated on `endpoint.is_some()`:
            // LMStudio's response is ALSO OpenAI-compatible and carries a
            // `model` field, so a local judge must not pick it up — `lms ps`
            // is the only ground truth for local dispatch.
            let served = if endpoint.is_some() { reply.model.clone() } else { None };
            match parse_judge_ruling(&reply.content) {
                Some((ruling, decisive_evidence, note_for_author)) => (
                    JudgeRecord { ruling, decisive_evidence, note_for_author, pass, seconds },
                    tokens,
                    served,
                ),
                None => (
                    JudgeRecord {
                        ruling: JudgeRuling::Unparsed,
                        decisive_evidence: String::new(),
                        note_for_author: String::new(),
                        pass,
                        seconds,
                    },
                    tokens,
                    served,
                ),
            }
        }
        // A dispatch-level failure is recorded as `Error`, not propagated —
        // one bad judge call must not abort the whole docket (the review's
        // job is to be loud PER-FLAG, not to be fragile). No reply body, so
        // no served model to report.
        Err(_) => (
            JudgeRecord {
                ruling: JudgeRuling::Error,
                decisive_evidence: String::new(),
                note_for_author: String::new(),
                pass,
                seconds: t0.elapsed().as_secs_f64(),
            },
            0,
            None,
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
    /// (#1260) `true` iff this pass's surviving record came from a
    /// dispatch-level `Err` (a chat failure surviving the transport's bounded
    /// retries), NOT from a parse failure or a budget denial. A REMOTE judge
    /// with any such failure marks the run degraded (honest-fail — the
    /// affected flag carries no real adjudication); see `finish_review`.
    dispatch_error: bool,
    /// (#1300) The served model reported by this pass's response, if any
    /// (`None` on a dispatch error or a budget-denied call — no response
    /// body to report).
    served_model: Option<String>,
}

/// One judge pass, retried ONCE if the reply was [`JudgeRuling::Unparsed`]
/// (the retry keeps the same `pass` number — a retried pass-1 is still
/// pass-1, just a second attempt at it). Still unparsed after the retry:
/// the retry's record survives (the first attempt's record is discarded,
/// not hidden — it added no information a clean retry didn't already
/// supersede). Tokens/wall/calls account for BOTH attempts.
#[allow(clippy::too_many_arguments)]
fn judge_pass_with_retry(
    pass: u8,
    model: &str,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    endpoint: Option<&ModelEndpoint>,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
) -> PassOutcome {
    let t0 = Instant::now();
    let (r1, t1, served1) = run_judge_pass(pass, model, system, prompt, max_tokens, endpoint, chat);
    if r1.ruling == JudgeRuling::Unparsed {
        let (r2, t2, served2) = run_judge_pass(pass, model, system, prompt, max_tokens, endpoint, chat);
        // `run_judge_pass` only ever yields `JudgeRuling::Error` from its
        // dispatch-`Err` arm (a parse miss is `Unparsed`, and the budget-denied
        // record is built by the caller, never here) — so the surviving
        // ruling being `Error` is exactly the dispatch-failure signal (#1260).
        let dispatch_error = r2.ruling == JudgeRuling::Error;
        PassOutcome {
            record: r2,
            tokens: t1 + t2,
            wall_ms: t0.elapsed().as_millis() as u64,
            calls: 2,
            dispatch_error,
            served_model: served2.or(served1),
        }
    } else {
        let dispatch_error = r1.ruling == JudgeRuling::Error;
        PassOutcome {
            record: r1,
            tokens: t1,
            wall_ms: t0.elapsed().as_millis() as u64,
            calls: 1,
            dispatch_error,
            served_model: served1,
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
    /// (#1260) `true` iff either pass hit a dispatch-level `Err` (see
    /// [`PassOutcome::dispatch_error`]) — a REMOTE judge's honest-fail signal.
    dispatch_error: bool,
    /// (#1300) The served model, taken from pass-1 (falling back to a later
    /// pass if pass-1 had none) — one seat means one served identity for the
    /// whole flag; pass-1 always runs, so it's the representative source.
    served_model: Option<String>,
}

/// (#1260) The judge phase's two remote token buckets — pass-1 and pass-2
/// are separate EXECUTIONS per the operator decision (each judge pass draws
/// from its own allowance). `None` for a local judge, whose calls never
/// touch a bucket.
struct JudgeBudgets {
    pass1: RemoteBucket,
    pass2: RemoteBucket,
}

/// (#1260) The named-reason record for a judge call the remote bucket
/// refused — ruled `Error` (never silently `confirmed`), with the reason in
/// `note_for_author` so the envelope carries it per-flag; the run itself
/// then goes DEGRADED (the judge is a load-bearing stage), see
/// `finish_review`.
fn budget_exhausted_record(pass: u8) -> JudgeRecord {
    JudgeRecord {
        ruling: JudgeRuling::Error,
        decisive_evidence: String::new(),
        note_for_author: "remote token budget exhausted for this stage — call skipped".to_string(),
        pass,
        seconds: 0.0,
    }
}

/// (#1300) The bucket-denial `PassOutcome` — no dispatch happened, so no
/// served model.
fn budget_exhausted_outcome(pass: u8) -> PassOutcome {
    PassOutcome {
        record: budget_exhausted_record(pass),
        tokens: 0,
        wall_ms: 0,
        calls: 0,
        // A budget denial is NOT a dispatch failure — it's metered
        // separately (the judge-budget degeneracy in `finish_review`).
        dispatch_error: false,
        served_model: None,
    }
}

/// One judge pass with the [`JudgeBudgets`] gate applied (#1260): a REMOTE
/// judge's `bucket` is consulted first — a denied `admit()` skips the
/// dispatch entirely and yields a named `budget_exhausted_record` (Error, so
/// it never counts as agreement); an admitted call runs (with the
/// unparsed-retry) and `spend()`s its tokens/calls back. A LOCAL judge
/// (`bucket == None`) always dispatches, untouched by any bucket.
#[allow(clippy::too_many_arguments)]
fn run_budgeted_pass(
    pass: u8,
    bucket: Option<&mut RemoteBucket>,
    model: &str,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    endpoint: Option<&ModelEndpoint>,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
) -> PassOutcome {
    match bucket {
        Some(b) => {
            if !b.admit() {
                return budget_exhausted_outcome(pass);
            }
            let o = judge_pass_with_retry(pass, model, system, prompt, max_tokens, endpoint, chat);
            b.spend(o.tokens, o.calls);
            o
        }
        None => judge_pass_with_retry(pass, model, system, prompt, max_tokens, endpoint, chat),
    }
}

/// (#1266) The judge state machine for one flag, generalized over `passes`
/// (the judge seat's consensus depth — replaces the historical hardcoded
/// double-confirm). Pass-1 (with the unparsed-retry) ALWAYS runs; a
/// non-confirmed pass-1 needs no further pass REGARDLESS of `passes`
/// (`needs_check` stays [`Tier::NeedsCheck`]; `false_positive`/`unparsed`/
/// `error` archive — the specific ruling is still preserved on the record,
/// just tiered out of the author-facing report). What a `confirmed` pass-1
/// does next depends on `passes`:
///
/// - `passes == 1` — SINGLE pass: pass-1's confirm IS [`Tier::Confirmed`]
///   directly; no confirmation pass runs (the frontier cost lever).
/// - `passes == 2` — today's double-confirm (DEFAULT): one confirmation pass;
///   agreement → `Confirmed`, ANY other outcome (needs_check, false_positive,
///   unparsed, error) demotes to `NeedsCheck`, never silently to `confirmed`.
/// - `passes == N > 2` — UNANIMOUS consensus: confirmation passes `2..=N` run
///   in sequence and EVERY one must confirm for the flag to stay `Confirmed`;
///   the FIRST non-confirm demotes it to `NeedsCheck` and EARLY-EXITS (so N
///   passes never costs N× — later passes run only on still-confirmed
///   survivors, the same bounded shape the double-confirm already used).
///
/// The `pass2` slot holds the LAST confirmation pass that ran — for
/// `passes == 2` that is literally pass-2 (byte-identical to the historical
/// double-confirm); for `N > 2` it is the DECISIVE later pass (the one that
/// demoted, or the final confirm). Intermediate confirmation records fold
/// into the token/wall/call totals but are not individually retained on the
/// flag; full per-pass retention arrives with the sharding build (#1266).
///
/// (#1260) A REMOTE judge's calls gate on the per-pass buckets in `budgets`:
/// pass-1 draws from the pass-1 bucket, every confirmation pass from the
/// pass-2 bucket. An exhausted pass-1 bucket skips the flag's whole ruling
/// (Error → Archived, reason named); an exhausted confirmation bucket demotes
/// a pass-1 confirm to NeedsCheck (Error is not agreement) — in both cases the
/// run goes degraded downstream, never a silent pass.
///
/// (#1352) The outer control flow (pass 1, conditional confirmation passes,
/// demote on the first disagreement — described in full above) is now the
/// generic `darkmux_crew::step_kinds::patterns::multi_pass_confirm` Tier 2
/// pattern; this function supplies the review-specific PARTS the pattern
/// plugs in: which token bucket a pass draws from (pass 1 → `budgets.pass1`,
/// every confirmation pass → `budgets.pass2`, via `run_budgeted_pass`'s own
/// dispatch/retry/budget mechanics — unchanged), and how a [`JudgeRuling`]
/// classifies against the confirm/demote decision
/// ([`JudgeRuling::Confirmed`] → `Confirm`, [`JudgeRuling::NeedsCheck`] →
/// `NeedsCheck`, everything else → `Reject`). Resource accounting
/// (tokens/calls/wall-time/dispatch-error/served-model) is folded from the
/// pattern's returned per-pass results below — the pattern itself has zero
/// opinion on what a pass costs. This is a pure control-flow extraction: the
/// `double_confirm_*`/`passes_*` unit tests pin the exact same outcomes as
/// the pre-#1352 hand-written loop.
#[allow(clippy::too_many_arguments)]
fn judge_one_flag_with_passes(
    passes: u32,
    prompt: &str,
    model: &str,
    system: &str,
    max_tokens: u32,
    endpoint: Option<&ModelEndpoint>,
    mut budgets: Option<&mut JudgeBudgets>,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
) -> JudgeOutcome {
    let result = multi_pass_confirm(
        passes,
        |pass_no| {
            let bucket = budgets
                .as_deref_mut()
                .map(|b| if pass_no == 1 { &mut b.pass1 } else { &mut b.pass2 });
            run_budgeted_pass(pass_no as u8, bucket, model, system, prompt, max_tokens, endpoint, chat)
        },
        |p: &PassOutcome| match p.record.ruling {
            JudgeRuling::Confirmed => PassClass::Confirm,
            JudgeRuling::NeedsCheck => PassClass::NeedsCheck,
            // false_positive | unparsed | error
            _ => PassClass::Reject,
        },
    );

    // Fold per-pass resource accounting across pass-1 + every confirmation
    // pass that ran (#1260 accounting stays honest — the SAME fold the
    // hand-written loop did, just driven off the pattern's returned Vec
    // instead of accumulating inline).
    let mut tokens = result.pass1.tokens;
    let mut calls = result.pass1.calls;
    let mut dispatch_error = result.pass1.dispatch_error;
    // (#1300) Falls back to a later pass's served model when pass-1 had
    // none — one seat means one served identity for the whole flag.
    let mut served_model = result.pass1.served_model.clone();
    let pass1_ms = result.pass1.wall_ms;
    let mut pass2_ms = 0u64;
    for p in &result.confirmation_passes {
        tokens += p.tokens;
        calls += p.calls;
        dispatch_error |= p.dispatch_error;
        if served_model.is_none() {
            served_model = p.served_model.clone();
        }
        pass2_ms += p.wall_ms;
    }

    let tier = match result.tier {
        ConfirmTier::Confirmed => Tier::Confirmed,
        ConfirmTier::NeedsCheck => Tier::NeedsCheck,
        ConfirmTier::Rejected => Tier::Archived,
    };
    // The `pass2` slot holds the LAST confirmation pass that ran (see this
    // function's doc) — `confirmation_passes`' final entry, carrying its
    // real pass number.
    let pass2 = result.confirmation_passes.into_iter().last().map(|p| p.record);

    JudgeOutcome {
        tier,
        demoted_by_pass2: result.demoted_by_later_pass,
        tokens,
        pass1_ms,
        pass2_ms,
        calls,
        dispatch_error,
        served_model,
        pass1: result.pass1.record,
        pass2,
    }
}

/// (#1266) The historical double-confirm entry point (`passes: 2`) — retained
/// for the `double_confirm_*` unit tests, which pin today's exact behavior.
/// Production dispatch calls [`judge_one_flag_with_passes`] with the judge
/// seat's resolved `passes`.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn judge_one_flag(
    prompt: &str,
    model: &str,
    system: &str,
    max_tokens: u32,
    endpoint: Option<&ModelEndpoint>,
    budgets: Option<&mut JudgeBudgets>,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
) -> JudgeOutcome {
    judge_one_flag_with_passes(2, prompt, model, system, max_tokens, endpoint, budgets, chat)
}

// ─── verify stage (#1260/#1177) — optional adjudication of confirms ──────

/// One verify-seat dispatch — mirrors [`run_judge_pass`]'s shape: a chat
/// failure is recorded as [`VerifyRuling::Error`] with the reason in the
/// note, never propagated (one bad adjudication must not abort the run;
/// the flag then keeps its manual-verification marker downstream).
/// The returned `bool` is `content_empty` — whether the reply's trimmed
/// content came back empty (the ONLY condition [`verify_pass_with_retry`]
/// re-dispatches on, matching the graph path's `dispatch.map`
/// `retry_on_empty`). A dispatch `Err` reports `content_empty = false`: an
/// infra failure is isolated, never retried (the same policy `map_local_item`
/// applies to a dispatch `Err`).
fn run_verify_pass(
    model: &str,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    endpoint: Option<&ModelEndpoint>,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
) -> (VerifyRecord, u64, Option<String>, bool) {
    let t0 = Instant::now();
    let call = ChatCall {
        model,
        system,
        user: prompt,
        temperature: JUDGE_TEMPERATURE,
        max_tokens,
        endpoint,
    };
    match chat(&call) {
        Ok(reply) => {
            let seconds = t0.elapsed().as_secs_f64();
            let tokens = reply.total_tokens.unwrap_or(0);
            let content_empty = reply.content.trim().is_empty();
            // (#1300 QA follow-up) Gated on `endpoint.is_some()` — see
            // `run_judge_pass`'s identical comment; LMStudio's response is
            // also OpenAI-compatible and carries a `model` field.
            let served = if endpoint.is_some() { reply.model.clone() } else { None };
            match parse_verify_ruling(&reply.content) {
                Some((ruling, decisive_evidence, note_for_author)) => (
                    VerifyRecord { ruling, decisive_evidence, note_for_author, seconds, model: model.to_string() },
                    tokens,
                    served,
                    content_empty,
                ),
                None => (
                    VerifyRecord {
                        ruling: VerifyRuling::Unparsed,
                        decisive_evidence: String::new(),
                        note_for_author: String::new(),
                        seconds,
                        model: model.to_string(),
                    },
                    tokens,
                    served,
                    content_empty,
                ),
            }
        }
        Err(e) => (
            VerifyRecord {
                ruling: VerifyRuling::Error,
                decisive_evidence: String::new(),
                note_for_author: format!("verify dispatch failed: {e}"),
                seconds: t0.elapsed().as_secs_f64(),
                model: model.to_string(),
            },
            0,
            None,
            false,
        ),
    }
}

/// One verify adjudication, retried ONCE on an EMPTY-content reply — the SAME
/// retry semantics the graph path's `dispatch.map` applies (`retry_on_empty:
/// 1`, set in `build_review_graph`). Returns the surviving record plus
/// token/call accounting for BOTH attempts, plus (#1300) the served model
/// reported by whichever attempt survives.
///
/// (#1442) The historical unparsed-RETRY retired here: a non-empty but
/// UNPARSEABLE reply is now recorded as [`VerifyRuling::Unparsed`] on the
/// FIRST attempt (no re-dispatch), and its finding stays `Confirmed` with the
/// manual-verification marker downstream. That aligns the sequential
/// `--charges-file` path (`run_verify_stage` → here) with the graph path,
/// which — since the probe/verify stages retired onto the generic
/// `dispatch.map` block — only ever re-dispatches an EMPTY reply, never an
/// unparseable non-empty one. Two verify paths that diverged on this is the
/// #1373-class drift the shared-semantics discipline exists to prevent (an
/// operator-decided alignment, operator-veto-flagged).
fn verify_pass_with_retry(
    model: &str,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    endpoint: Option<&ModelEndpoint>,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
) -> (VerifyRecord, u64, u32, Option<String>) {
    let (r1, t1, served1, empty1) = run_verify_pass(model, system, prompt, max_tokens, endpoint, chat);
    if empty1 {
        // Empty reply — re-dispatch ONCE (retry_on_empty: 1 parity). The
        // second attempt's record is kept regardless of what it returns
        // (a second empty stays the honest inconclusive result), and tokens
        // are billed across BOTH attempts.
        let (r2, t2, served2, _empty2) = run_verify_pass(model, system, prompt, max_tokens, endpoint, chat);
        (r2, t1 + t2, 2, served2.or(served1))
    } else {
        (r1, t1, 1, served1)
    }
}

/// (#1260) The optional verify stage: ONE adjudication call per
/// double-confirmed flag, after pass-2. State machine per the settled
/// design:
///
/// - `verified` — tier stays `Confirmed`; the posted review drops the
///   manual-verification marker for a "verified by <model> adjudication"
///   line (rendering lives in `synthesize_review`).
/// - `refuted` — demoted to [`Tier::Archived`], `demoted_by_verify` set,
///   the refutation recorded on the flag.
/// - `uncertain` (and `unparsed`/`error` — an inconclusive adjudication
///   never promotes) — tier stays `Confirmed` WITH the existing marker.
///
/// A crew without the seat never reaches here — byte-identical behavior
/// to today. Zero confirms ⇒ no stage at all (no dispatch, no records).
/// The stage is its own EXECUTION for the remote token bucket; exhausting
/// it is load-bearing (degraded run — see the caller in `finish_review`).
/// Emits its own `step result` records (the graph verify kind's shape —
/// `kind = "review.verify"`, per-adjudication `step_id = "review-ruling"`
/// records plus one completion `step_id = "verify"`) through the run's
/// [`ReviewObs`]. Run-level liveness is the caller's `with_dispatch_bookends`
/// wrap (contract 2 — the stage runs inside the run's existing dispatch
/// envelope), not a review-scoped bookend here.
/// (#1373 gates a/c, verify half) The verify stage's remote-budget
/// exhaustion warning + budget row — the SAME decision `run_verify_stage`
/// (`finish_review`'s path, via `run_judge_only`) has always applied,
/// extracted so `ReviewVerifyStepKind` (the graph path) can apply it too
/// without the two callers drifting (CLAUDE.md's #1352 tiering: "shared
/// logic that both `run_judge_only` and the graph path use should live
/// once"). `bucket.record()` returns `None` when the stage made no remote
/// calls at all (a local verify seat, or zero confirmed docket before this
/// is even reached) — both fields come back empty in that case.
struct VerifyBudgetOutcome {
    warning: Option<String>,
    remote_budget_row: Option<RemoteBudgetRecord>,
}

fn verify_budget_outcome(bucket: &RemoteBucket, docket: usize) -> VerifyBudgetOutcome {
    let rec = bucket.record();
    let warning = rec.as_ref().filter(|r| r.skipped_calls > 0).map(|r| {
        // (#1260, ruling applied) Verify-bucket exhaustion degrades the
        // STAGE, not the run: findings already adjudicated `verified` still
        // post as frontier-verified, and each flag whose adjudication was
        // SKIPPED keeps its `Confirmed` tier WITH the manual-verification
        // marker. The posted review + envelope carry a loud warning naming
        // the exhaustion — never a silent pass.
        let adjudicated = docket.saturating_sub(r.skipped_calls as usize);
        format!(
            "verify budget exhausted after {adjudicated} of {docket} adjudications — the \
             remaining {} confirmed finding(s) keep the manual-verification marker (the \
             per-execution allowance of {} tokens ran out)",
            r.skipped_calls, r.max_tokens
        )
    });
    VerifyBudgetOutcome { warning, remote_budget_row: rec }
}

#[allow(clippy::too_many_arguments)]
fn run_verify_stage(
    env: &mut ReviewEnvelope,
    judged: &mut [JudgedFlag],
    bundles: &[BundleInput],
    inputs: &ReviewInputs,
    vstaff: &ResolvedSeatStaffing,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
    cycler: &mut dyn ModelCycler,
    obs: &mut ReviewObs<'_>,
) -> Result<()> {
    let docket = judged.iter().filter(|j| j.tier == Tier::Confirmed).count();
    if docket == 0 {
        return Ok(());
    }
    let identifier = seat_identifier(&vstaff.pm);
    let endpoint = seat_endpoint(&vstaff.pm);
    let endpoint_host = seat_endpoint_host(&vstaff.pm);
    let max_tokens = resolve_seat_max_tokens(vstaff, DEFAULT_JUDGE_MAX_TOKENS);
    let mut bucket = RemoteBucket::new("verify", inputs.remote_max_tokens_per_execution);

    if !vstaff.pm.is_remote() {
        cycler.ensure_loaded(&vstaff.pm)?;
    }

    let t0 = Instant::now();
    let mut calls = 0u32;
    let mut tokens = 0u64;
    // (#1300) First-seen served model across the stage's adjudications.
    let mut served_model: Option<String> = None;
    for j in judged.iter_mut().filter(|j| j.tier == Tier::Confirmed) {
        // Remote gate BEFORE dispatch — a skipped adjudication is recorded
        // per-flag (ruling Error, reason named); the whole run then goes
        // degraded below (verify is load-bearing, operator decision).
        let (record, spent, made, served) = if endpoint.is_some() && !bucket.admit() {
            (
                VerifyRecord {
                    ruling: VerifyRuling::Error,
                    decisive_evidence: String::new(),
                    note_for_author:
                        "remote token budget exhausted for this stage — call skipped".to_string(),
                    seconds: 0.0,
                    model: identifier.clone(),
                },
                0u64,
                0u32,
                None,
            )
        } else {
            let bundle = bundles.iter().find(|b| b.id == j.flag.bundle_id);
            let code = bundle.map(|b| b.code.as_str()).unwrap_or_default();
            let facts: &[String] = bundle.map(|b| b.facts.as_slice()).unwrap_or_default();
            let prompt =
                verify_prompt(inputs.intent_title, inputs.intent_body, code, facts, &j.flag.charge_text);
            let out = verify_pass_with_retry(
                &identifier,
                inputs.verify_system,
                &prompt,
                max_tokens,
                endpoint,
                chat,
            );
            if endpoint.is_some() {
                bucket.spend(out.1, out.2);
            }
            out
        };
        tokens += spent;
        calls += made;
        if served_model.is_none() {
            served_model = served;
        }
        obs.step_result(
            "review.verify",
            "review-ruling",
            json!({
                "bundle_id": j.flag.bundle_id, "stage": "verify",
                "ruling": record.ruling, "seconds": record.seconds,
            }),
        );
        if record.ruling == VerifyRuling::Refuted {
            j.tier = Tier::Archived;
            j.demoted_by_verify = true;
        }
        j.verify = Some(record);
    }
    let wall_ms = t0.elapsed().as_millis() as u64;
    if !vstaff.pm.is_remote() {
        cycler.release(&vstaff.pm)?;
    }

    env.members.push(MemberRecord {
        model: identifier.clone(),
        seat: "review-verify".to_string(),
        draws: calls,
        wall_ms,
        total_tokens: tokens,
        remote: endpoint.is_some(),
        endpoint: endpoint_host.clone(),
        served_model: served_model.clone(),
    });
    env.steps.push(StepRecord {
        step_id: "verify".to_string(),
        kind: "dispatch".to_string(),
        items_in: docket,
        items_out: docket,
        wall_ms,
    });
    // The verify stage's single completion record — the SAME shape the graph
    // path's `ReviewVerifyStepKind` emits (#1434).
    obs.step_result(
        "review.verify",
        "verify",
        json!({
            "items_in": docket, "items_out": docket, "wall_ms": wall_ms,
            "model": identifier, "tokens": tokens, "calls": calls,
            "remote": endpoint.is_some(), "endpoint": endpoint_host, "served_model": served_model,
        }),
    );

    // (#1373 gates a/c) Shared with the graph path's `ReviewVerifyStepKind`
    // — see `verify_budget_outcome`'s own doc. NEVER sets run-level
    // `degenerate` — routing the whole run to "degraded" would discard
    // findings already verified and read as "produced no signal", which is
    // factually false.
    let outcome = verify_budget_outcome(&bucket, docket);
    if let Some(w) = outcome.warning {
        env.warnings.push(w);
    }
    if let Some(rec) = outcome.remote_budget_row {
        env.remote_budgets.push(rec);
    }
    Ok(())
}

/// (#1373 gates a/b/c + the reason-specificity fix) One judge stage's
/// honesty-gate decision — the SAME budget-exhaustion / dispatch-error /
/// no-usable-ruling logic `finish_review` has always applied, extracted so
/// `ReviewJudgeStepKind` (the graph path) can apply it too without the two
/// callers drifting again (CLAUDE.md's #1352 tiering: "shared logic that
/// both `run_judge_only` and the graph path use should live once").
///
/// At most ONE `degenerate_reason` ever comes back — budget exhaustion
/// wins over the "no usable ruling" gate, mirroring the original
/// `degen_reasons.is_empty()` short-circuit this was extracted from (never
/// a "combine every reason" accumulator, #1329). `dispatch_error_warning`
/// is independent and UNCONDITIONAL (#1329's loud-beats-quiet half) —
/// present whenever a remote judge had ANY per-flag dispatch failure,
/// whether or not the run also degenerates.
struct JudgeGateOutcome {
    remote_budget_rows: Vec<RemoteBudgetRecord>,
    dispatch_error_warning: Option<String>,
    degenerate_reason: Option<String>,
}

fn judge_gate_outcome(
    is_remote: bool,
    judged_len: usize,
    usable: usize,
    dispatch_errors: usize,
    budgets: Option<&JudgeBudgets>,
    remote_max_tokens_per_execution: u64,
) -> JudgeGateOutcome {
    let mut degen_reasons: Vec<String> = Vec::new();
    let mut remote_budget_rows = Vec::new();

    // (#1329 fix) A REMOTE judge dispatch failure on a MINORITY of flags is
    // already handled honestly at the per-flag level (archive/demote, never
    // silently confirmed) — but the "loud beats quiet" doctrine still wants
    // it NAMED even on an otherwise-healthy run, so this warning fires
    // unconditionally whenever a remote judge saw ANY dispatch error,
    // independent of whether a `degenerate_reason` below also fires.
    let dispatch_error_warning = if is_remote && dispatch_errors > 0 {
        Some(format!(
            "remote judge dispatch failed on {dispatch_errors} of {judged_len} flag(s) after bounded \
             retries — each affected flag was conservatively archived (if its own pass-1 failed) \
             or demoted to needs-check (if pass-1 confirmed but a later pass failed), never \
             silently confirmed"
        ))
    } else {
        None
    };

    // Gate 1: a REMOTE judge whose per-pass token bucket EXHAUSTED (a
    // load-bearing stage — operator decision, DARKMUX_REMOTE_MAX_TOKENS_
    // PER_EXECUTION). Any exhaustion degrades the run regardless of scale.
    if let Some(b) = budgets {
        if let Some(rec) = b.pass1.record() {
            remote_budget_rows.push(rec);
        }
        if let Some(rec) = b.pass2.record() {
            remote_budget_rows.push(rec);
        }
        let skipped = b.pass1.skipped + b.pass2.skipped;
        if skipped > 0 {
            degen_reasons.push(format!(
                "remote judge token budget exhausted — {skipped} judge call(s) skipped after the \
                 per-execution allowance ({remote_max_tokens_per_execution} tokens per stage) ran out; \
                 degenerate run, never a silent pass"
            ));
        }
    }

    // Gate 2: the judge-dead honesty gate — NO flag produced a usable
    // pass-1 ruling, so the whole judge phase produced no signal worth
    // rendering. Names the specific "remote dispatch failed on N of M"
    // shape when that's the cause, rather than the generic wording, so the
    // operator sees WHY the judge went dead, not just THAT it did.
    if degen_reasons.is_empty() && judged_len > 0 && usable == 0 {
        if is_remote && dispatch_errors > 0 {
            degen_reasons.push(format!(
                "remote judge dispatch failed on {dispatch_errors} of {judged_len} flag(s) after \
                 bounded retries — degraded run, the affected flag(s) carry no adjudication"
            ));
        } else {
            degen_reasons.push(format!(
                "judge produced no usable ruling on any of {judged_len} flags (all errored/unparsed)"
            ));
        }
    }

    JudgeGateOutcome {
        remote_budget_rows,
        dispatch_error_warning,
        degenerate_reason: if degen_reasons.is_empty() { None } else { Some(degen_reasons.join("; ")) },
    }
}

// ─── shared finish (probe→dedup→judge→envelope), reused by run_judge_only ─

#[allow(clippy::too_many_arguments)]
fn finish_review(
    mut env: ReviewEnvelope,
    raw_flags: Vec<ProbeFlag>,
    bundles: &[BundleInput],
    inputs: &ReviewInputs,
    judge: &ResolvedSeatStaffing,
    verify: Option<&ResolvedSeatStaffing>,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
    cycler: &mut dyn ModelCycler,
    obs: &mut ReviewObs<'_>,
) -> Result<ReviewEnvelope> {
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
    obs.step_result(
        "review.dedup",
        "dedup",
        json!({ "items_in": env.raw_flags, "items_out": deduped.len(), "wall_ms": dedup_ms }),
    );
    env.deduped_flags = deduped.len();

    let judge_identifier = seat_identifier(&judge.pm);
    let judge_endpoint = seat_endpoint(&judge.pm);
    let judge_max_tokens = resolve_seat_max_tokens(judge, DEFAULT_JUDGE_MAX_TOKENS);
    // (#1260) A remote judge draws from its own per-pass token buckets
    // (pass-1 and pass-2 are separate executions — operator decision) and
    // skips the cycler entirely (nothing to load off-box).
    let mut judge_budgets = judge_endpoint.map(|_| JudgeBudgets {
        pass1: RemoteBucket::new("judge-pass1", inputs.remote_max_tokens_per_execution),
        pass2: RemoteBucket::new("judge-pass2", inputs.remote_max_tokens_per_execution),
    });

    if !judge.pm.is_remote() {
        cycler.ensure_loaded(&judge.pm)?;
    }
    let mut judged = Vec::with_capacity(deduped.len());
    let mut pass1_ms = 0u64;
    let mut pass2_ms = 0u64;
    let mut pass2_flags = 0usize;
    let mut judge_calls = 0u32;
    let mut judge_tokens = 0u64;
    // (#1260) Flags whose ruling came from a dispatch-level failure (chat
    // `Err` surviving bounded retries) — the honest-fail count a REMOTE
    // judge degrades the run on.
    let mut judge_dispatch_errors = 0usize;
    // (#1300) First-seen served model across every flag's judge outcome —
    // one judge seat, one served identity for the whole run.
    let mut judge_served_model: Option<String> = None;
    for flag in &deduped {
        let bundle = bundles.iter().find(|b| b.id == flag.bundle_id);
        let code = bundle.map(|b| b.code.as_str()).unwrap_or_default();
        let facts: &[String] = bundle.map(|b| b.facts.as_slice()).unwrap_or_default();
        let prompt = judge_prompt(inputs.intent_title, inputs.intent_body, code, facts, &flag.charge_text);
        let outcome = judge_one_flag_with_passes(
            judge.passes,
            &prompt,
            &judge_identifier,
            inputs.judge_system,
            judge_max_tokens,
            judge_endpoint,
            judge_budgets.as_mut(),
            chat,
        );
        judge_tokens += outcome.tokens;
        judge_calls += outcome.calls;
        if outcome.dispatch_error {
            judge_dispatch_errors += 1;
        }
        if judge_served_model.is_none() {
            judge_served_model = outcome.served_model.clone();
        }
        pass1_ms += outcome.pass1_ms;
        pass2_ms += outcome.pass2_ms;
        // The per-ruling ticker (#1247 Part 1) — one `step result` record per
        // judge dispatch outcome (the graph judge kind's `step_id =
        // "review-ruling"` shape, #1434), emitted BEFORE `outcome`'s fields
        // move into the `JudgedFlag` below.
        obs.step_result(
            "review.judge",
            "review-ruling",
            json!({
                "bundle_id": flag.bundle_id, "pass": 1,
                "ruling": outcome.pass1.ruling, "seconds": outcome.pass1.seconds,
            }),
        );
        if let Some(p2) = &outcome.pass2 {
            pass2_flags += 1;
            // (#1266) The decisive later pass's REAL pass number — 2 under
            // the default double-confirm (byte-identical to before), or the
            // demoting/final pass under an N-pass consensus judge.
            obs.step_result(
                "review.judge",
                "review-ruling",
                json!({
                    "bundle_id": flag.bundle_id, "pass": p2.pass,
                    "ruling": p2.ruling, "seconds": p2.seconds,
                }),
            );
        }
        judged.push(JudgedFlag {
            flag: flag.clone(),
            pass1: outcome.pass1,
            pass2: outcome.pass2,
            tier: outcome.tier,
            demoted_by_pass2: outcome.demoted_by_pass2,
            verify: None,
            demoted_by_verify: false,
        });
    }
    if !judge.pm.is_remote() {
        cycler.release(&judge.pm)?;
    }

    env.members.push(MemberRecord {
        model: judge_identifier.clone(),
        seat: "review-judge".to_string(),
        // Actual dispatches, unparsed retries included — never fewer calls
        // than the operator paid for.
        draws: judge_calls,
        wall_ms: pass1_ms + pass2_ms,
        total_tokens: judge_tokens,
        remote: judge.pm.is_remote(),
        endpoint: seat_endpoint_host(&judge.pm),
        served_model: judge_served_model.clone(),
    });
    env.steps.push(StepRecord {
        step_id: "judge-pass1".to_string(),
        kind: "dispatch".to_string(),
        items_in: deduped.len(),
        items_out: deduped.len(),
        wall_ms: pass1_ms,
    });
    if pass2_flags > 0 {
        env.steps.push(StepRecord {
            step_id: "judge-pass2".to_string(),
            kind: "dispatch".to_string(),
            items_in: pass2_flags,
            items_out: pass2_flags,
            wall_ms: pass2_ms,
        });
    }
    // The judge stage's single completion record — the SAME shape the graph
    // path's `ReviewJudgeStepKind` emits (#1434). This sequential driver runs
    // one flag at a time, so `concurrency` is always 1.
    obs.step_result(
        "review.judge",
        "judge",
        json!({
            "items_in": deduped.len(), "items_out": judged.len(), "wall_ms": pass1_ms + pass2_ms,
            "pass1_wall_ms": pass1_ms, "pass2_wall_ms": pass2_ms,
            "model": judge_identifier, "tokens": judge_tokens, "calls": judge_calls,
            "dispatch_errors": judge_dispatch_errors, "concurrency": 1,
            "served_model": judge_served_model,
        }),
    );

    // (#1260, revised #1329, extracted #1373) Judge-stage degeneracy is
    // decided BEFORE the optional verify stage so a run the judge already
    // doomed never spends frontier money on verify (CONSIDER g — see the
    // `env.degenerate.is_none()` gate below). `judge_gate_outcome` is the
    // SAME decision `ReviewJudgeStepKind` (the graph path) applies — see
    // its own doc for the two-gate/one-warning shape.
    let usable = judged
        .iter()
        .filter(|j| {
            matches!(
                j.pass1.ruling,
                JudgeRuling::Confirmed | JudgeRuling::NeedsCheck | JudgeRuling::FalsePositive
            )
        })
        .count();
    let gate = judge_gate_outcome(
        judge.pm.is_remote(),
        judged.len(),
        usable,
        judge_dispatch_errors,
        judge_budgets.as_ref(),
        inputs.remote_max_tokens_per_execution,
    );
    if let Some(w) = gate.dispatch_error_warning {
        env.warnings.push(w);
    }
    env.remote_budgets.extend(gate.remote_budget_rows);
    // Guarded assign (#1373 frontier review): an unconditional
    // `env.degenerate = gate.degenerate_reason` would clobber a pre-set
    // Some with None. Safe today only because run_judge_only's zero-flags
    // case early-returns before reaching here; the graph twin uses this
    // same guarded form, keep them matched.
    if gate.degenerate_reason.is_some() {
        env.degenerate = gate.degenerate_reason;
    }

    // (#1260) The optional verify stage — one adjudication per confirmed
    // flag, AFTER the double-confirm judge and BEFORE the tier counts so a
    // refutation's demotion lands in the totals. Crews without the seat skip
    // this entirely (byte-identical behavior to today); a run the judge
    // already marked degenerate skips it too (CONSIDER g — no frontier spend
    // on a doomed run).
    if let Some(vstaff) = verify {
        if env.degenerate.is_none() {
            run_verify_stage(&mut env, &mut judged, bundles, inputs, vstaff, chat, cycler, obs)?;
        }
    }

    env.confirmed = judged.iter().filter(|j| j.tier == Tier::Confirmed).count();
    env.needs_check = judged.iter().filter(|j| j.tier == Tier::NeedsCheck).count();
    env.archived = judged.iter().filter(|j| j.tier == Tier::Archived).count();
    // (#1299) Cluster the `needs_check` tier when it exceeds the threshold —
    // a count-preserving cap, never a drop (see [`cluster_needs_check`]).
    env.needs_check_clusters = cluster_needs_check(&judged);
    env.verified = judged
        .iter()
        .filter(|j| matches!(&j.verify, Some(v) if v.ruling == VerifyRuling::Verified))
        .count();
    env.refuted = judged.iter().filter(|j| j.demoted_by_verify).count();

    env.flags = deduped;
    env.judged = judged;
    Ok(env)
}

/// Re-judge a previously-recorded flag list without re-running the probe
/// (the `--charges-file` entry point). Still dedups (a hand-edited or
/// concatenated charges file may carry raw, undeduped flags) and still
/// rebuilds bundles from `inputs.diff` — the judge needs the code each
/// flag's `bundle_id` refers to, and flags alone don't carry it.
pub fn run_judge_only(
    flags: Vec<ProbeFlag>,
    inputs: &ReviewInputs,
    mut chat: impl FnMut(&ChatCall) -> Result<SingleShotReply>,
    cycler: &mut dyn ModelCycler,
    emitter: &mut dyn ReviewEmitter,
) -> Result<ReviewEnvelope> {
    // (#1512, #1513 review) `inputs.roles` is already the validated,
    // resolved shape — no separate crew-validation step. Probes/judge are
    // required by construction; verify is optionally present.
    let probes = &inputs.roles.probes;
    let judge = &inputs.roles.judge;
    let verify = inputs.roles.verify.as_ref();
    let crew_name = inputs.roles.distinct_profile_names();
    // Judge-only runs one model, so the mode is telemetry, not behavior —
    // but the envelope still records the CALLER's resolved mode rather
    // than a hardcoded label, so a judge-only re-run of a parallel review
    // doesn't misreport its provenance.
    let mode = resolve_mode(inputs.mode, probes, judge);

    let t_bundle = Instant::now();
    let bundles = resolve_bundles(inputs);
    let bundle_ms = t_bundle.elapsed().as_millis() as u64;

    let mut env = ReviewEnvelope {
        case_id: inputs.case_id.clone(),
        crew: crew_name.clone(),
        mode: mode_label(mode).to_string(),
        bundles: bundles.len(),
        // Same up-front stamp as `run_review` — degenerate (zero-flag)
        // envelopes carry the comparability key too.
        fingerprint: fingerprint(&seat_identifier(&judge.pm), inputs.judge_system),
        // (#1247) The resolved staffing this run actually used, post any
        // caller-applied `--k` override — see `ReviewEnvelope::staffing`.
        staffing: Some(staffing_snapshot(probes, judge, verify, inputs.roles.request_changes)),
        ..Default::default()
    };
    // (#1434) Run observability rides the injected emitter via `ReviewObs`,
    // which also owns the host-telemetry sampler for the run's lifetime. No
    // task-level bookend here — the caller's `with_dispatch_bookends` wrap
    // owns run liveness (contract 2). `obs` drops at function end (early
    // `?`-return or clean), tearing down its sampler thread.
    let mut obs = ReviewObs::new(emitter, &inputs.case_id, &crew_name);
    env.steps.push(StepRecord {
        step_id: "bundle".to_string(),
        kind: "procedural".to_string(),
        items_in: 1,
        items_out: bundles.len(),
        wall_ms: bundle_ms,
    });
    obs.step_result("review.bundle", "bundle", json!({ "items_out": bundles.len() }));
    if flags.is_empty() {
        env.degenerate = Some("--charges-file carried zero flags".to_string());
        return Ok(env);
    }

    finish_review(env, flags, &bundles, inputs, judge, verify, &mut chat, cycler, &mut obs)
}

// ═══════════════════════════════════════════════════════════════════════
// Task/Step graph orchestration — ONE upfront-declared graph
//
// Redesign per the DRY-with-teeth mandate: instead of `run_review_impl`'s
// hand-written sequential driver (bundle → probe_phase → dedup_flags →
// judge loop → run_verify_stage → finish_review, six ad-hoc calls), the
// review's structure — which stages exist, in what order — is declared
// as a real `Task`/`Step` graph BEFORE any dispatch happens, and executed
// through ONE `darkmux_crew::scheduler::run_step_graph` call (mirrors
// `coder_phase.rs`'s own migration, #1230 Packet 3). What's NOT knowable
// upfront — how many deduped flags exist — is handled entirely INSIDE the
// judge/verify steps' own internal bounded-concurrency for-each loops,
// never as graph shape.
//
// Grouped into three Phases (an operator/coordinator decision, not an
// execution mechanism — Phase boundaries are exactly as statically known
// as everything else here; they're a labeling/observability layer over
// the same flat Step graph, not a second scheduler):
//
//   investigate: bundle → probe×N seats → dedup   (ends with deduped flags)
//   adjudicate:  judge (one step, internal pass1/pass2 loop)
//   report:      verify → synthesis                (ends with tier counts)
//
// `depends_on` edges cross Phase boundaries exactly like they cross Task
// boundaries within one Phase — `adjudicate`'s `judge` step `depends_on`
// `investigate`'s `dedup` step; no special cross-phase mechanism.
//
// **Crate-boundary note**: this module (`darkmux-lab`) builds and runs the
// graph and returns the final `ReviewEnvelope` — it does NOT create the
// Mission/Phase/Task records on disk (that needs `darkmux_crew::lifecycle`
// plus a `mission_id`/case-scoped identity, which is the CALLER's concern:
// `darkmux mission launch review` creates a real persisted Mission; a lab bench run
// stays per-run-local per the lab-vs-fleet boundary doctrine — same
// caller-decides pattern `ReviewEmitter` already uses for flow-record
// destination). It also does NOT render the posted-comment markdown
// (`Rendered`) — that type and its `synthesize_review` builder live in the
// binary crate's `src/pr_review.rs`, which `darkmux-lab` cannot depend on
// without a reverse dependency; `pr_review.rs` calls `synthesize_review` on
// the `ReviewEnvelope` this module returns, exactly as it does today.
//
// **The double-confirm judge protocol, dedup key, judge/verify prompts,
// and tier synthesis are UNCHANGED** — every step kind below calls the
// SAME preserved functions (`dedup_flags`, `judge_one_flag_with_passes`,
// `parse_judge_ruling`, `parse_verify_ruling`,
// `cluster_needs_check`, `mechanism_family`, `judge_prompt`,
// `verify_prompt`) verbatim — only the ORCHESTRATION shape (six sequential
// calls → one declared graph) and the telemetry plumbing (the sequential
// path's `ReviewObs` can't cross a `run_bounded` worker-thread
// boundary — see `darkmux_crew::step_kinds::StepOutcome`'s doc — so
// per-step telemetry now rides `StepOutcome.flow_records` / direct
// `darkmux_flow::record()` calls instead) changed.

use darkmux_crew::scheduler::run_step_graph;
use darkmux_crew::single_shot::{single_shot_chat, single_shot_chat_hosted, HostedSingleShotRequest, SingleShotRequest};
use darkmux_crew::step_kinds::{
    MapDispatchOverride, MapItemResult, OverrideDispatchCall, StepKind, StepKindRegistry,
    StepOutcome, MAP_BUDGET_SKIP_ERROR,
};
use darkmux_crew::types::{Step, Task};
use std::sync::Mutex as StdMutex;

/// Everything a review Step kind needs, OWNED (not borrowed) and
/// `Send + Sync` so it can cross the `run_bounded` worker-thread boundary —
/// `ReviewInputs<'a>`'s borrows can't. Built ONCE by the orchestrator
/// (`build_review_graph`) before the graph starts; every step kind holds an
/// `Arc` clone. Mirrors `ReviewInputs` field-for-field, minus the injected
/// `chat`/`cycler`: dispatch routes through `dispatch_chat` (below), and
/// model residency is the scheduler's job — `run_step_graph`'s
/// `host_factory` + each step kind's `residency()` placement, via gestalt's
/// wave planner — so no step kind constructs a cycler of its own (there is
/// no `ModelCycler` anywhere in the graph's dispatch path; `LmsCycler`
/// survives only for `run_judge_only`'s sequential path).
pub struct ReviewStepContext {
    pub case_id: String,
    /// (#1512, #1513 review) Every role this run resolved, via the ONE
    /// generic per-task resolver — not a "crew". Carried here purely for
    /// test-fixture convenience (`step_ctx`'s callers build it once
    /// alongside the rest of the context); the graph's own step kinds never
    /// read it — `build_review_graph` stamps each task's resolved model
    /// directly into that task's `Step.config` before the graph runs, and
    /// `run_review_graph` takes the crew-display-name/staffing-snapshot it
    /// needs as its own explicit parameters (see that function's doc).
    pub roles: ResolvedReviewRoles,
    pub intent_title: String,
    pub intent_body: String,
    pub diff: String,
    pub probe_system: String,
    pub judge_system: String,
    pub verify_system: String,
    pub bundles: Vec<BundleInput>,
    pub remote_max_tokens_per_execution: u64,
    pub timeout_seconds: u32,
    /// (#1355 follow-up) Test-only dispatch seam for [`dispatch_chat`] —
    /// `None` at every production call site (`src/pr_review.rs`,
    /// `review_bench.rs`), which always falls through to the real
    /// `single_shot_chat`/`_hosted` routing below. When `Some`, the graph's
    /// step kinds (`ReviewProbeStepKind`/`ReviewJudgeStepKind`/
    /// `ReviewVerifyStepKind`, all of which hold `Arc<ReviewStepContext>`
    /// and run across `run_bounded`'s worker-thread boundary — hence
    /// `Arc<dyn Fn... + Send + Sync>`, not `&mut dyn FnMut`) dispatch through
    /// the injected mock instead. This is the SAME injection discipline the
    /// module doc already names for `HostTelemetrySampler`'s `sample_fn`/
    /// `lms_fn` (a plain-fn/closure seam defaulting to the real primitive at
    /// every production site) — added here because #1355 found that the
    /// module doc's original "no seam for this call" decision (see
    /// `dispatch_chat`'s own doc below) traded away real dispatch-level test
    /// coverage for `run_review_graph`, and two real bugs (dropped member
    /// attribution, a missing degenerate gate) shipped through the resulting
    /// blind spot. Test fixtures also set `n_ctx: None` on every seat's
    /// `ProfileModel` so `StepKind::residency()` reports `Residency::Remote`
    /// (see `graph_pm`/`graph_staffing` below) — `run_bounded`'s Remote
    /// track never touches `host_factory` (the real `lms` CLI) at all, so a
    /// mocked graph test stays fully hermetic without needing to inject the
    /// scheduler's own `host_factory` parameter too.
    #[allow(clippy::type_complexity)]
    pub chat_override: Option<Arc<dyn for<'a> Fn(&ChatCall<'a>) -> Result<SingleShotReply> + Send + Sync>>,
}

/// The production dispatch primitive every review step kind below calls —
/// routes on `call.endpoint` exactly like `pr_review.rs::run_dispatch`'s
/// own `chat` closure (contract 1: a consumer routes on what the profile
/// declares, never re-derives its own local/remote judgment). `coder_phase.rs`'s
/// `MissionCoderStepKind`/`MissionWorktreeStepKind` still call their real
/// primitive directly with no seam at all; this call gets one
/// (`ReviewStepContext::chat_override`) because #1355 found the "no seam"
/// trade genuinely cost real dispatch-level coverage for the step kinds
/// below — see that field's doc for the full reasoning. The PRESERVED
/// algorithm functions this dispatches into (`judge_one_flag_with_passes`,
/// `verify_pass_with_retry`, `probe_one_draw`) remain independently
/// mock-testable via their own existing `chat: &mut dyn FnMut` parameter —
/// this seam is specifically for exercising the GRAPH GLUE (the step kinds
/// themselves) that those functions are called from.
fn dispatch_chat(ctx: &ReviewStepContext, call: &ChatCall) -> Result<SingleShotReply> {
    if let Some(mock) = &ctx.chat_override {
        let reply = mock(call)?;
        emit_review_token_telemetry(&ctx.case_id, call.model, &reply);
        return Ok(reply);
    }
    let reply = match call.endpoint {
        Some(endpoint) => single_shot_chat_hosted(&HostedSingleShotRequest {
            endpoint,
            model: call.model,
            system: call.system,
            user: call.user,
            max_tokens: call.max_tokens,
            timeout_seconds: ctx.timeout_seconds,
        }),
        None => single_shot_chat(&SingleShotRequest {
            base_url: None,
            model: call.model,
            system: call.system,
            user: call.user,
            temperature: call.temperature,
            max_tokens: call.max_tokens,
            timeout_seconds: ctx.timeout_seconds,
        }),
    }?;
    emit_review_token_telemetry(&ctx.case_id, call.model, &reply);
    Ok(reply)
}

/// (#1361) Emit a `telemetry.tokens` record for one review dispatch call —
/// the shape `dispatch_internal.rs`'s per-turn tailer emits for the
/// internal-runtime container path. The review pipeline's
/// `single_shot_chat`/`_hosted` calls never go through that tailer at all
/// (it's not an agentic loop), so without this the fleet dashboard's
/// `tokensOffMeter()` — which sums ONLY `category:telemetry, source:tokens`
/// records — is structurally blind to every review/funnel dispatch's real
/// token usage. No `turn_seq`: each review call is an independent
/// single-shot request, not a growing agentic-loop context, so the
/// viewer's fresh/re-read decomposition correctly buckets these as
/// unclassified rather than fabricating a sequential-turn overlap.
/// Silently skipped when the response carried no `usage.total_tokens` at
/// all (nothing to report — matches `turn_tokens_payload`'s same skip).
fn emit_review_token_telemetry(case_id: &str, model: &str, reply: &SingleShotReply) {
    let Some(payload) = review_token_telemetry_payload(reply) else {
        return;
    };
    let _ = darkmux_flow::record(darkmux_crew::dispatch::build_telemetry_record(
        darkmux_flow::Level::Info,
        "telemetry.tokens",
        "tokens",
        "review",
        case_id,
        Some(model),
        None,
        None,
        payload,
    ));
}

/// Pure: map a review dispatch's [`SingleShotReply`] to the
/// `{prompt_tokens, completion_tokens, total_tokens}` `telemetry.tokens`
/// payload — the sibling of `dispatch_internal.rs`'s `turn_tokens_payload`
/// for the review pipeline's single-shot calls. No I/O, so unit-testable
/// in isolation from `emit_review_token_telemetry`'s flow-record emission
/// (same split as `turn_tokens_payload` / `handle_event`).
///
/// `None` when the reply carried no `total_tokens` at all (the OpenAI-compat
/// response omitted `usage` entirely) — nothing to report, mirrors
/// `turn_tokens_payload` skipping turns with no `usage`. A `total_tokens`
/// with no `prompt_tokens` breakdown defaults prompt to 0 and completion to
/// the full total (defensive; real LMStudio/hosted responses always send
/// both alongside `total_tokens`).
fn review_token_telemetry_payload(reply: &SingleShotReply) -> Option<serde_json::Value> {
    let total_tokens = reply.total_tokens?;
    let prompt_tokens = reply.prompt_tokens.unwrap_or(0);
    let completion_tokens = reply
        .completion_tokens
        .unwrap_or_else(|| total_tokens.saturating_sub(prompt_tokens));
    Some(serde_json::json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": total_tokens,
    }))
}

/// Build a "step result" companion flow record — the review's own
/// equivalent of `coder_phase.rs`'s `emit_step_result` (#1230 Packet 4
/// sibling convention): one generic action, `kind` distinguishing which
/// review step produced it, free-form `payload` for the rest. Split from the
/// emit so BOTH dispatch paths can share the exact record shape: the graph
/// step kinds emit it globally via [`emit_review_step_result`] (they run in
/// worker threads with no injected emitter to hold), and the sequential
/// `run_judge_only` path emits it through its injected [`ReviewEmitter`] via
/// [`ReviewObs::step_result`] (#1434 — one vocabulary across both paths).
fn review_step_result_record(
    kind: &str,
    step_id: &str,
    case_id: &str,
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
        source: Some("review".to_string()),
        model: None,
        reasoning: None,
        mission_id: None,
        machine_id: None,
        machine_uid: None,
        orchestrator: None,
        prev_hash: None,
        hash: None,
        payload: Some(full),
        work_id: None,
        attempt: None,
    }
}

/// Emit a [`review_step_result_record`] to the GLOBAL flow sink
/// (`darkmux_flow::record`). Used by the graph step kinds, which run inside
/// the scheduler's `run_bounded` worker threads and so can't hold the
/// caller-injected emitter (`ReviewObs` covers the sequential path instead).
fn emit_review_step_result(kind: &str, step_id: &str, case_id: &str, payload: serde_json::Value) {
    let _ = darkmux_flow::record(review_step_result_record(kind, step_id, case_id, payload));
}

// ─── investigate: bundle ────────────────────────────────────────────────

/// Phase "investigate", step 1: hands back the already-resolved bundle list
/// (`ReviewStepContext::bundles` — resolved once by the orchestrator before
/// the graph starts, since bundling needs `&ReviewInputs<'a>`'s borrow,
/// which can't cross into a `'static` step kind). Procedural — no dispatch.
///
/// **Tier 3 (#1352), on purpose.** Diff-parsing/bundle-resolution is
/// genuinely specific to the review pipeline — no second consumer is
/// visible today, and its whole job is unwrapping THIS module's own
/// `ReviewStepContext`. Stays physically co-located here, not moved to
/// `darkmux-crew`'s `step_kinds` — see that crate's `step_kinds::patterns`
/// module doc for the three-tier picture this classification follows.
pub struct ReviewBundleStepKind {
    pub ctx: Arc<ReviewStepContext>,
}

impl StepKind for ReviewBundleStepKind {
    fn id(&self) -> &'static str {
        "review.bundle"
    }

    fn display_name(&self) -> &'static str {
        "Bundle"
    }

    fn run(&self, step: &Step, _task: &Task, _input: &std::collections::BTreeMap<String, String>) -> Result<StepOutcome> {
        let output = serde_json::to_string(&self.ctx.bundles).context("serializing bundles")?;
        emit_review_step_result(
            "review.bundle",
            &step.id,
            &self.ctx.case_id,
            json!({ "items_out": self.ctx.bundles.len() }),
        );
        Ok(StepOutcome { output, flow_records: Vec::new() })
    }
}

// ─── investigate: probe reconstruction (seats x k dispatch.map fan-out) ─

/// (#1442 ship-2b, #1512) One probe SEAT's mint-time spec — the key the
/// dedup boundary uses to reconstruct the review's domain results (raw
/// [`ProbeFlag`]s, per-seat [`MemberRecord`] accounting, reduced-coverage
/// warnings, the probe stage's remote budget row) from the generic
/// `dispatch.map` fan-out's per-item results. The probe stage is one
/// EXPLICIT one-role `dispatch.map` task per probe role, declared
/// statically in review.json (#1512 — no `expand` template, no crew-level
/// probe-role enumeration; `build_review_graph` claims each resolved seat
/// against its declared task by `role_id`), sharing one `bucket_group:
/// "probe"` remote allowance. `draw_task_ids` is a single-entry `Vec` today
/// (one role, one task, one dispatch) — kept plural for the dedup step's
/// existing per-draw iteration shape, not because more than one entry is
/// ever produced.
#[derive(Debug, Clone)]
pub(crate) struct ProbeSeatSpec {
    pub(crate) name: String,
    /// The seat's dispatch identity (`seat_identifier` — namespaced for a
    /// local seat, the bare profile id for a remote one). Also the wire
    /// `model` the seat's map steps dispatch.
    pub(crate) identifier: String,
    pub(crate) remote: bool,
    pub(crate) endpoint_host: Option<String>,
    /// This seat's claimed probe task id, wrapped in a `Vec` for the dedup
    /// step's `input`-key iteration (`gather_inputs` keys a first-step
    /// input by dependency TASK id, #1341) — always exactly one entry as of
    /// #1512 (one role, one task).
    pub(crate) draw_task_ids: Vec<String>,
    /// The selected bundles, in the exact order the pre-rendered prompt
    /// collection was minted: `(bundle_id, fact_family)` per item index.
    pub(crate) bundles: Vec<(String, String)>,
}

/// Everything the dedup boundary reconstructs from the probe fan-out's raw
/// per-item results, before dedup itself runs. Pure output of
/// [`reconstruct_probe_stage`] — unit-testable without a graph.
pub(crate) struct ProbeReconstruction {
    /// Raw flags in the HISTORICAL probe order (seat → bundle → draw), so
    /// dedup's first-survivor-wins semantics match the retired per-seat
    /// probe loop exactly.
    pub(crate) flags: Vec<ProbeFlag>,
    pub(crate) members: Vec<MemberRecord>,
    pub(crate) warnings: Vec<String>,
    pub(crate) budget_row: Option<RemoteBudgetRecord>,
    /// `Some(reason)` when at least one draw fired and EVERY fired draw was
    /// a dispatch error — the all-draws-failed honesty gate. (Previously a
    /// LOCAL seat's dispatch error was a hard step `Err` that aborted the
    /// graph; `dispatch.map`'s per-item isolation carries the stage
    /// through instead, so the gate lands here as a NAMED degenerate
    /// reason — loud, never a silent zero-flag "clean pass".)
    pub(crate) all_draws_failed: Option<String>,
}

/// (#1442 ship-2b) Rebuild the probe stage's domain results from the
/// `seats x k` map steps' serialized [`MapItemResult`] arrays.
///
/// Accounting semantics preserved from the retired kind:
/// - a **draw** = an item whose call actually FIRED (a first-attempt
///   remote-budget skip — recognized by [`MAP_BUDGET_SKIP_ERROR`] — is a
///   skip, never a draw);
/// - `MemberRecord` per seat, summed across its k sibling steps
///   (`draws`/`total_tokens`/`wall_ms`; `served_model` = the first
///   endpoint-reported model, which stays `None` by construction on local
///   seats — the [`MapItemResult`] contract);
///   - (#1442) `wall_ms` SEMANTICS SHIFTED at the `dispatch.map` cutover:
///     the retired bespoke probe kind recorded the seat's whole-step
///     ELAPSED wall (`t0.elapsed()` around the seat's inner loop); this
///     reconstruction SUMS each item's own per-dispatch wall
///     (`item.wall_ms`). The new figure is more honest as a COST metric
///     (it excludes per-step scheduling/idle overhead the old elapsed
///     folded in), but it is NOT a timeline — under concurrent draws the
///     per-item walls overlap in real time, so the sum can exceed the seat's
///     wall-clock. Series comparisons ACROSS the cutover should read the
///     probe `wall_ms` accordingly.
/// - a seat with zero fired draws (empty selector match, or every attempt
///   budget-skipped) records NO member — `member_summary()` must not
///   credit work that never happened;
/// - the probe stage's ONE remote budget row (`stage: "probe"`) and the
///   exhaustion warning reconstruct from the same items.
pub(crate) fn reconstruct_probe_stage(
    specs: &[ProbeSeatSpec],
    input: &std::collections::BTreeMap<String, String>,
    budget: u64,
) -> Result<ProbeReconstruction> {
    let mut flags = Vec::new();
    let mut members = Vec::new();
    let mut warnings = Vec::new();
    let mut remote_used = 0u64;
    let mut remote_calls = 0u32;
    let mut remote_skips = 0u32;
    let mut any_remote_seat = false;
    let mut total_fired = 0u32;
    let mut total_errors = 0u32;
    let mut first_error: Option<String> = None;

    for spec in specs {
        let mut per_draw: Vec<Vec<MapItemResult>> = Vec::with_capacity(spec.draw_task_ids.len());
        for task_id in &spec.draw_task_ids {
            let raw = input.get(task_id).map(String::as_str).unwrap_or("[]");
            let results: Vec<MapItemResult> = serde_json::from_str(raw).with_context(|| {
                format!(
                    "deserializing probe map results from task `{task_id}` (seat `{}`)",
                    spec.name
                )
            })?;
            per_draw.push(results);
        }

        // Flags in the historical seat → bundle → draw order.
        for (b_idx, (bundle_id, fact_family)) in spec.bundles.iter().enumerate() {
            for (draw, results) in per_draw.iter().enumerate() {
                let Some(item) = results.get(b_idx) else { continue };
                if item.ok && !item.content.trim().is_empty() {
                    flags.push(ProbeFlag {
                        bundle_id: bundle_id.clone(),
                        fact_family: fact_family.clone(),
                        member: spec.identifier.clone(),
                        draw: draw as u32,
                        charge_text: item.content.trim().to_string(),
                        anchor: None,
                        also_flagged: Vec::new(),
                    });
                }
            }
        }

        // Per-seat accounting, summed across the seat's k sibling steps.
        let mut draws = 0u32;
        let mut skips = 0u32;
        let mut errors = 0u32;
        let mut seat_first_error: Option<String> = None;
        let mut tokens = 0u64;
        let mut wall_ms = 0u64;
        let mut served_model: Option<String> = None;
        for results in &per_draw {
            for item in results {
                if item.error.as_deref() == Some(MAP_BUDGET_SKIP_ERROR) {
                    skips += 1;
                    continue;
                }
                draws += 1;
                tokens += item.total_tokens.unwrap_or(0);
                wall_ms += item.wall_ms;
                if served_model.is_none() {
                    served_model = item.served_model.clone();
                }
                if !item.ok {
                    errors += 1;
                    if seat_first_error.is_none() {
                        seat_first_error = item.error.clone();
                    }
                }
            }
        }
        total_fired += draws;
        total_errors += errors;
        if first_error.is_none() {
            first_error = seat_first_error.clone();
        }
        if spec.remote {
            any_remote_seat = true;
            remote_used += tokens;
            remote_calls += draws;
            remote_skips += skips;
        }
        if errors > 0 {
            // The retired kind aborted a remote seat's remaining draws on
            // the first failure; `dispatch.map` isolates per item and keeps
            // going, so the warning names the per-draw failure count.
            let scope = if spec.remote { "remote probe seat" } else { "probe seat" };
            warnings.push(format!(
                "{scope} \"{}\" ({}) dispatch failed on {errors} draw(s) — each failure \
                 isolated per draw (reduced coverage): {}",
                spec.name,
                spec.identifier,
                seat_first_error.unwrap_or_default()
            ));
        }
        if draws > 0 {
            members.push(MemberRecord {
                model: spec.identifier.clone(),
                seat: "review-probe".to_string(),
                draws,
                wall_ms,
                total_tokens: tokens,
                remote: spec.remote,
                endpoint: spec.endpoint_host.clone(),
                served_model,
            });
        }
    }

    let budget_row =
        (any_remote_seat && (remote_calls > 0 || remote_skips > 0)).then(|| RemoteBudgetRecord {
            stage: "probe".to_string(),
            max_tokens: budget,
            used_tokens: remote_used,
            // (#1442 gate CONSIDER) `remote_used` SUMS the endpoint-REPORTED
            // tokens, but the live `MapRemoteBucket` meters CONSERVATIVELY
            // (it settles a usage-omitting reply at its granted cap). So a
            // usage-omitting endpoint can exhaust the bucket — producing
            // `remote_skips > 0` — while the summed reported total stays
            // BELOW `budget`. `skipped_calls > 0` is itself proof the bucket
            // exhausted (that is the only reason a draw is skipped), so it
            // makes `exhausted` truthful regardless of what the endpoint
            // reported.
            exhausted: remote_skips > 0 || remote_used >= budget,
            skipped_calls: remote_skips,
        });
    if remote_skips > 0 {
        warnings.push(format!(
            "remote probe token budget exhausted — {remote_skips} draw(s) skipped after the \
             per-execution allowance ({budget} tokens) ran out; reduced coverage"
        ));
    }
    let all_draws_failed = (total_fired > 0 && total_errors == total_fired).then(|| {
        format!(
            "every probe draw errored — {total_errors} of {total_fired} dispatch(es) failed, \
             zero probe signal (first error: {})",
            first_error.unwrap_or_default()
        )
    });
    Ok(ProbeReconstruction { flags, members, warnings, budget_row, all_draws_failed })
}

// ─── investigate: dedup (terminal step of the phase) ────────────────────

/// Phase "investigate", terminal step: `depends_on` every probe step, reads
/// back each one's flags, concatenates, and calls `dedup_flags` VERBATIM
/// (the mechanism-family keying + anchor-based matching — explicitly
/// preserved, unchanged). Its OWN `StepOutcome.output` IS the phase's
/// observable artifact: "what's the review forming to be."
///
/// **Tier classification (#1352).** This `StepKind` is Tier 3 — it's
/// graph wiring specific to this pipeline (which upstream steps it
/// `depends_on`, this pipeline's flow-record vocabulary). The dedup
/// ALGORITHM it calls (`dedup_flags`) is a thin Tier 3 plug-in
/// (`MechanismFamilyDedup`) over the generic Tier 2
/// `darkmux_crew::step_kinds::patterns::dedup` procedure — see
/// `dedup_flags`'s own doc.
pub struct ReviewDedupStepKind {
    pub ctx: Arc<ReviewStepContext>,
    /// (#1373 gate e) Shared cross-cutting envelope this step writes the
    /// TRUE pre-dedup flag count into (`env.raw_flags`) the moment it's
    /// known. `ReviewSynthesisStepKind` only ever sees THIS step's OWN
    /// `StepOutcome.output` (the already-deduped list, since that's the
    /// data judge/verify need to consume), so without this write it has no
    /// way to recover the true raw count — the field silently read the
    /// deduped count instead (`raw_flags == deduped_flags` always).
    /// (#1442 ship-2b) This step ALSO pushes the probe stage's
    /// reconstructed remote-budget row + all-draws-failed degenerate reason
    /// here — the reconstruction boundary (see `probe_specs` below).
    pub env: SharedReviewEnvelope,
    /// (#1442 ship-2b, #1512) The mint-time per-seat specs `build_review_graph`
    /// computed while claiming each staffing against its declared probe
    /// task — this step's `input`
    /// map (keyed by probe TASK id) is raw `MapItemResult` arrays from the
    /// generic `dispatch.map` fan-out, and [`reconstruct_probe_stage`]
    /// aligns them back into domain results against these specs before the
    /// (unchanged) dedup algorithm runs.
    pub(crate) probe_specs: Vec<ProbeSeatSpec>,
    /// The run-wide member/warning accumulators (the same handles the
    /// judge/verify contributors use), merged into `shared_env` by
    /// `run_review_graph` once `run_step_graph` returns.
    pub(crate) members: Arc<StdMutex<Vec<MemberRecord>>>,
    pub(crate) warnings: Arc<StdMutex<Vec<String>>>,
    /// The probe stage's per-execution remote allowance
    /// (`ReviewStepContext::remote_max_tokens_per_execution`) — the SAME
    /// value stamped into every probe step's `bucket_budget` config, used
    /// here to reconstruct the stage's budget row.
    pub(crate) remote_budget: u64,
}

impl StepKind for ReviewDedupStepKind {
    fn id(&self) -> &'static str {
        "review.dedup"
    }

    fn display_name(&self) -> &'static str {
        "Dedup"
    }

    fn run(&self, step: &Step, _task: &Task, input: &std::collections::BTreeMap<String, String>) -> Result<StepOutcome> {
        let t0 = Instant::now();
        // (#1442 ship-2b) Reconstruction boundary: raw flags + per-seat
        // member accounting + warnings + the probe budget row, rebuilt from
        // the seats x k map steps' per-item results.
        let recon = reconstruct_probe_stage(&self.probe_specs, input, self.remote_budget)?;
        self.members
            .lock()
            .expect("probe members mutex poisoned")
            .extend(recon.members);
        self.warnings
            .lock()
            .expect("probe warnings mutex poisoned")
            .extend(recon.warnings);
        let raw = recon.flags;
        let raw_count = raw.len();
        {
            let mut env = self.env.lock().expect("shared review envelope mutex poisoned");
            env.raw_flags = env.raw_flags.max(raw_count);
            if let Some(row) = recon.budget_row {
                env.remote_budgets.push(row);
            }
            if env.degenerate.is_none() {
                if let Some(reason) = recon.all_draws_failed {
                    env.degenerate = Some(reason);
                }
            }
        }
        let (deduped, _stats) = dedup_flags(raw, &self.ctx.diff);
        let wall_ms = t0.elapsed().as_millis() as u64;
        emit_review_step_result(
            "review.dedup",
            &step.id,
            &self.ctx.case_id,
            json!({ "items_in": raw_count, "items_out": deduped.len(), "wall_ms": wall_ms }),
        );
        let output = serde_json::to_string(&deduped).context("serializing deduped flags")?;
        Ok(StepOutcome { output, flow_records: Vec::new() })
    }
}

// ─── adjudicate: judge (the whole Phase, one Step) ──────────────────────

/// Phase "adjudicate", its ONLY step: internally loops over however many
/// deduped flags `dedup` produced — a bounded-concurrency for-each over a
/// runtime-determined quantity (dispatch pass-1, then pass-2 if confirmed,
/// for each flag, bounded by `concurrency` — no capacity-constrained
/// grouping decision, just iterate with a concurrency limit; NOT the
/// RAM-budget bin-packing `darkmux_gestalt::planner::plan_waves` does for
/// probe's model-loading concern, a genuinely different mechanism this step
/// does not use), mirroring probe's own internal k-draw loop pattern rather
/// than needing one graph node per flag. Reuses
/// `judge_prompt`/`judge_one_flag_with_passes` VERBATIM (the double-confirm
/// protocol — pass-1 judges every flag, only pass-1 confirms get pass-2,
/// disagreement demotes — is explicitly UNCHANGED).
///
/// **Concurrency**: `concurrency` (from `Step.config.concurrency`, default
/// 1 — see `build_review_graph`) bounds how many flags this step judges AT
/// ONCE via a chunked `std::thread::scope`. LMStudio's real per-model
/// concurrent-prediction ceiling is genuinely unresolved (operator
/// observation: ~4 in practice, sometimes 1) — judge is typically ONE model
/// processing N flags (not N different models like probe), so graph-level
/// fan-out buys little while adding real complexity; a small, OPERATOR-SET
/// bound here is the honest answer until an empirical ceiling exists.
/// `concurrency: 1` (the default) is byte-identical in dispatch ORDER to
/// the historical sequential loop.
///
/// **Tier classification (#1352).** This `StepKind` is Tier 3 — the
/// concurrency/chunking loop, budget wiring, and review-specific telemetry
/// below are graph wiring specific to this pipeline. The double-confirm
/// control flow it dispatches per flag (`judge_one_flag_with_passes`) is a
/// thin Tier 3 wrapper around the generic Tier 2
/// `darkmux_crew::step_kinds::patterns::multi_pass_confirm` pattern — see
/// that function's own doc.
pub struct ReviewJudgeStepKind {
    pub ctx: Arc<ReviewStepContext>,
    pub judge: ResolvedSeatStaffing,
    /// (#1354 follow-up) The same shared accumulator `ReviewProbeStepKind`
    /// writes its `MemberRecord`s to — one collector for every dispatching
    /// step kind, merged into `shared_env` once `run_step_graph` returns.
    pub members: Arc<StdMutex<Vec<MemberRecord>>>,
    /// (#1373 gates a/b/c) Shared cross-cutting envelope this step writes
    /// its remote-budget rows, the #1329 dispatch-error warning, and (when
    /// this stage is itself doomed) the run's `degenerate` reason into —
    /// the SAME handle `ReviewSynthesisStepKind` reads at the end and
    /// `ReviewVerifyStepKind` reads BEFORE dispatching (gate d — no
    /// frontier spend on a run this stage already doomed, CONSIDER g).
    pub env: SharedReviewEnvelope,
}

/// One deduped flag's judged outcome, in dispatch order — the shared
/// scratch `ReviewJudgeStepKind::run` collects chunk-by-chunk (see its doc)
/// before serializing into the step's output.
struct JudgeChunkResult {
    index: usize,
    judged: JudgedFlag,
    tokens: u64,
    calls: u32,
    pass1_ms: u64,
    pass2_ms: u64,
    dispatch_error: bool,
    served_model: Option<String>,
}

impl StepKind for ReviewJudgeStepKind {
    fn id(&self) -> &'static str {
        "review.judge"
    }

    fn display_name(&self) -> &'static str {
        "Judge"
    }

    fn run(&self, step: &Step, _task: &Task, input: &std::collections::BTreeMap<String, String>) -> Result<StepOutcome> {
        let dedup_output = input.values().next().cloned().unwrap_or_default();
        let deduped: Vec<ProbeFlag> = if dedup_output.is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&dedup_output).context("deserializing deduped flags")?
        };

        let concurrency = step
            .config
            .get("concurrency")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1) as usize;

        let judge = &self.judge;
        let judge_identifier = seat_identifier(&judge.pm);
        // A `&String` (`Copy`) so the `move` closure below can capture ITS
        // OWN copy of the reference on every loop iteration without moving
        // the owned `judge_identifier` String out from under a later one.
        let judge_identifier_ref: &str = &judge_identifier;
        let judge_endpoint = seat_endpoint(&judge.pm);
        let judge_max_tokens = resolve_seat_max_tokens(judge, DEFAULT_JUDGE_MAX_TOKENS);
        let judge_system = self.ctx.judge_system.as_str();
        let judge_budgets = judge_endpoint.map(|_| {
            StdMutex::new(JudgeBudgets {
                pass1: RemoteBucket::new("judge-pass1", self.ctx.remote_max_tokens_per_execution),
                pass2: RemoteBucket::new("judge-pass2", self.ctx.remote_max_tokens_per_execution),
            })
        });

        let t0 = Instant::now();
        let results: StdMutex<Vec<JudgeChunkResult>> = StdMutex::new(Vec::with_capacity(deduped.len()));

        // (#1374) The deterministic global index of a flag is its position in
        // `deduped`: the chunk's start offset (running count of flags in
        // already-scheduled chunks) plus its offset WITHIN the chunk. The old
        // form read `results.lock().len()` — the COMPLETED count, which for
        // chunks after the first collides across offsets whenever earlier
        // threads in the chunk haven't finished at spawn time, making
        // `env.judged` completion-order rather than deduped-docket order. Plain
        // arithmetic in the main loop is both correct and lock-free.
        let mut chunk_start = 0usize;
        for chunk in deduped.chunks(concurrency) {
            std::thread::scope(|scope| {
                for (offset, flag) in chunk.iter().enumerate() {
                    let bundle = self.ctx.bundles.iter().find(|b| b.id == flag.bundle_id);
                    let code = bundle.map(|b| b.code.as_str()).unwrap_or_default();
                    let facts: &[String] = bundle.map(|b| b.facts.as_slice()).unwrap_or_default();
                    let prompt = judge_prompt(&self.ctx.intent_title, &self.ctx.intent_body, code, facts, &flag.charge_text);
                    // (#1374) `chunk_start + offset` = this flag's stable index
                    // in `deduped`, independent of thread completion order.
                    let index = chunk_start + offset;
                    let ctx = &self.ctx;
                    let judge_budgets = judge_budgets.as_ref();
                    let results = &results;
                    scope.spawn(move || {
                        let mut chat = |call: &ChatCall| dispatch_chat(ctx, call);
                        let mut guard = judge_budgets.map(|b| b.lock().expect("judge budgets mutex poisoned"));
                        let outcome = judge_one_flag_with_passes(
                            judge.passes,
                            &prompt,
                            judge_identifier_ref,
                            judge_system,
                            judge_max_tokens,
                            judge_endpoint,
                            guard.as_deref_mut(),
                            &mut chat,
                        );
                        emit_review_step_result(
                            "review.judge",
                            "review-ruling",
                            &ctx.case_id,
                            json!({
                                "bundle_id": flag.bundle_id, "pass": 1,
                                "ruling": outcome.pass1.ruling, "seconds": outcome.pass1.seconds,
                            }),
                        );
                        if let Some(p2) = &outcome.pass2 {
                            emit_review_step_result(
                                "review.judge",
                                "review-ruling",
                                &ctx.case_id,
                                json!({
                                    "bundle_id": flag.bundle_id, "pass": p2.pass,
                                    "ruling": p2.ruling, "seconds": p2.seconds,
                                }),
                            );
                        }
                        results.lock().expect("judge results mutex poisoned").push(JudgeChunkResult {
                            index,
                            tokens: outcome.tokens,
                            calls: outcome.calls,
                            pass1_ms: outcome.pass1_ms,
                            pass2_ms: outcome.pass2_ms,
                            dispatch_error: outcome.dispatch_error,
                            served_model: outcome.served_model.clone(),
                            judged: JudgedFlag {
                                flag: flag.clone(),
                                pass1: outcome.pass1,
                                pass2: outcome.pass2,
                                tier: outcome.tier,
                                demoted_by_pass2: outcome.demoted_by_pass2,
                                verify: None,
                                demoted_by_verify: false,
                            },
                        });
                    });
                }
            });
            // (#1374) Advance the running start AFTER the chunk's threads join,
            // so the next chunk's flags index from the correct base.
            chunk_start += chunk.len();
        }

        let mut results = results.into_inner().expect("judge results mutex poisoned");
        results.sort_by_key(|r| r.index);

        let wall_ms = t0.elapsed().as_millis() as u64;
        let judge_tokens: u64 = results.iter().map(|r| r.tokens).sum();
        let judge_calls: u32 = results.iter().map(|r| r.calls).sum();
        let judge_dispatch_errors = results.iter().filter(|r| r.dispatch_error).count();
        let judge_served_model = results.iter().find_map(|r| r.served_model.clone());
        // Per-pass wall-time breakdown (summed across every flag's own
        // dispatches — real elapsed if run sequentially; with `concurrency
        // > 1` these overlap in wall-clock, so the sum is a COST metric,
        // not a timeline).
        let pass1_wall_ms: u64 = results.iter().map(|r| r.pass1_ms).sum();
        let pass2_wall_ms: u64 = results.iter().map(|r| r.pass2_ms).sum();

        let judged: Vec<JudgedFlag> = results.into_iter().map(|r| r.judged).collect();

        // (#1373 gates a/b/c) The SAME honesty-gate decision `finish_review`
        // applies, via the shared `judge_gate_outcome` helper — see its own
        // doc. `judge_budgets`'s scope (the `std::thread::scope` above) has
        // already joined, so `into_inner()` is safe here on the main thread.
        let usable = judged
            .iter()
            .filter(|j| {
                matches!(
                    j.pass1.ruling,
                    JudgeRuling::Confirmed | JudgeRuling::NeedsCheck | JudgeRuling::FalsePositive
                )
            })
            .count();
        let budgets_final = judge_budgets.map(|m| m.into_inner().expect("judge budgets mutex poisoned"));
        let gate = judge_gate_outcome(
            judge_endpoint.is_some(),
            judged.len(),
            usable,
            judge_dispatch_errors,
            budgets_final.as_ref(),
            self.ctx.remote_max_tokens_per_execution,
        );
        {
            let mut env = self.env.lock().expect("shared review envelope mutex poisoned");
            env.remote_budgets.extend(gate.remote_budget_rows);
            if let Some(w) = gate.dispatch_error_warning {
                env.warnings.push(w);
            }
            if gate.degenerate_reason.is_some() {
                env.degenerate = gate.degenerate_reason;
            }
        }

        emit_review_step_result(
            "review.judge",
            &step.id,
            &self.ctx.case_id,
            json!({
                "items_in": deduped.len(), "items_out": judged.len(), "wall_ms": wall_ms,
                "pass1_wall_ms": pass1_wall_ms, "pass2_wall_ms": pass2_wall_ms,
                "model": judge_identifier.clone(), "tokens": judge_tokens, "calls": judge_calls,
                "dispatch_errors": judge_dispatch_errors, "concurrency": concurrency,
                "served_model": judge_served_model.clone(),
            }),
        );

        // (#1354 follow-up) Unlike `ReviewProbeStepKind`, this step never
        // recorded a `MemberRecord` at all — the judge's real dispatch cost
        // (tokens/calls/wall-time/model identity) was computed above and
        // emitted into the flow-record stream but never landed in the
        // envelope, so `member_summary()`'s "judged by ..." attribution
        // fell back to "unknown" on every run. Same shared accumulator
        // `ReviewProbeStepKind` writes to, merged into `shared_env` once
        // `run_step_graph` returns.
        // (#1355 follow-up) Only record a member when the judge actually
        // dispatched — zero deduped flags means an empty `deduped` slice and
        // the loop above never ran, so there's nothing to credit "judged
        // by" with.
        if judge_calls > 0 {
            self.members.lock().expect("members mutex poisoned").push(MemberRecord {
                model: judge_identifier,
                seat: "review-judge".to_string(),
                draws: judge_calls,
                wall_ms: pass1_wall_ms + pass2_wall_ms,
                total_tokens: judge_tokens,
                remote: judge_endpoint.is_some(),
                endpoint: seat_endpoint_host(&self.judge.pm),
                served_model: judge_served_model,
            });
        }

        let output = serde_json::to_string(&judged).context("serializing judged flags")?;
        Ok(StepOutcome { output, flow_records: Vec::new() })
    }

    fn residency(
        &self,
        _step: &Step,
        _task: &Task,
        _input: &std::collections::BTreeMap<String, String>,
    ) -> Option<darkmux_gestalt::Placement> {
        if self.judge.pm.is_remote() {
            return None;
        }
        // (#1360 follow-up) Unlike probe, judge can't know upfront whether
        // dedup will hand it any flags — that's genuinely data-dependent on
        // an earlier step's real output, not knowable at graph-build time.
        // But a TRULY empty bundle set is a safe, conservative exception:
        // every probe seat's selector operates on `ctx.bundles`, so if that
        // set is empty, dedup's output is guaranteed empty too, transitively
        // — no seat's selector matters. Skips loading a model this step is
        // certain not to use.
        if self.ctx.bundles.is_empty() {
            return None;
        }
        let n_ctx = self.judge.pm.n_ctx?;
        let identifier = darkmux_gestalt::namespaced_identifier(&self.judge.pm.id, self.judge.pm.identifier.as_deref());
        Some(darkmux_gestalt::Placement {
            model_key: self.judge.pm.id.clone(),
            identifier,
            min_ctx: n_ctx,
            seat: "review-judge".to_string(),
        })
    }
}

// ─── report: verify (prompt render → generic dispatch.map → apply) ──────

/// (#1442 ship-2b, the operator-recorded render-step decision on PR #1455)
/// Phase "report", step 1 of the verify TASK: render one frozen
/// [`verify_prompt`] per judge-CONFIRMED flag into a JSON string array —
/// the collection the SAME task's second step (a generic `dispatch.map`)
/// maps over with `user_template: "{item}"` (byte parity by construction —
/// `{item}` substitutes verbatim). Procedural, zero dispatch.
///
/// The verify stage's three dispatch gates all collapse into "the rendered
/// collection is EMPTY" (which makes the map step a completed no-op with
/// `residency() == None` — zero model loads, the #1438 property, now held
/// by the generic block):
/// - no verify seat staffed (byte-identical passthrough to today);
/// - the run is already degenerate (#1373 gate d — the judge task always
///   completes before this one, so `env.degenerate` is authoritative here;
///   no frontier spend on a doomed run);
/// - zero confirmed findings (the empty-docket short-circuit).
///
/// **Tier 3 (#1352), on purpose.** Prompt rendering against this
/// pipeline's own `JudgedFlag`/`BundleInput` types is genuinely
/// review-specific; the judge stays on the generic `multi_pass_confirm`
/// pattern and gains no domain rendering.
pub struct ReviewVerifyRenderStepKind {
    pub ctx: Arc<ReviewStepContext>,
    pub verify: Option<ResolvedSeatStaffing>,
    pub env: SharedReviewEnvelope,
}

impl StepKind for ReviewVerifyRenderStepKind {
    fn id(&self) -> &'static str {
        "review.verify-render"
    }

    fn display_name(&self) -> &'static str {
        "Verify prompts"
    }

    fn run(&self, step: &Step, _task: &Task, input: &std::collections::BTreeMap<String, String>) -> Result<StepOutcome> {
        let judge_output = input.values().next().cloned().unwrap_or_default();
        let judged: Vec<JudgedFlag> = if judge_output.is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&judge_output).context("deserializing judged flags")?
        };

        let confirmed: Vec<&JudgedFlag> = judged.iter().filter(|j| j.tier == Tier::Confirmed).collect();
        let skip_reason: Option<&str> = if self.verify.is_none() {
            Some("no verify seat staffed — judged flags pass through unchanged")
        } else if self
            .env
            .lock()
            .expect("shared review envelope mutex poisoned")
            .degenerate
            .is_some()
        {
            Some("run already degenerate — no verify dispatch on a doomed run")
        } else if confirmed.is_empty() {
            Some("zero confirmed findings — verify skipped before any model load")
        } else {
            None
        };

        let prompts: Vec<String> = if skip_reason.is_some() {
            Vec::new()
        } else {
            confirmed
                .iter()
                .map(|j| {
                    let bundle = self.ctx.bundles.iter().find(|b| b.id == j.flag.bundle_id);
                    let code = bundle.map(|b| b.code.as_str()).unwrap_or_default();
                    let facts: &[String] = bundle.map(|b| b.facts.as_slice()).unwrap_or_default();
                    verify_prompt(&self.ctx.intent_title, &self.ctx.intent_body, code, facts, &j.flag.charge_text)
                })
                .collect()
        };

        let mut payload = json!({ "items_in": confirmed.len(), "items_out": prompts.len() });
        if let Some(reason) = skip_reason {
            payload["short_circuit"] = json!(reason);
        }
        emit_review_step_result("review.verify-render", &step.id, &self.ctx.case_id, payload);

        let output = serde_json::to_string(&prompts).context("serializing verify prompts")?;
        Ok(StepOutcome { output, flow_records: Vec::new() })
    }
}

/// (#1442 ship-2b) What the verify-apply boundary contributes to the
/// envelope beyond the in-place `judged` mutation: the seat's member row,
/// the stage's exhaustion warning, and its remote budget row.
pub(crate) struct VerifyApplyOutcome {
    pub(crate) member: Option<MemberRecord>,
    pub(crate) warning: Option<String>,
    pub(crate) budget_row: Option<RemoteBudgetRecord>,
}

/// (#1442 ship-2b) Apply the verify map step's per-item results back onto
/// the judged docket — the domain half of the retired `ReviewVerifyStepKind`
/// loop, now running at the synthesis boundary. Item index i corresponds to
/// the i-th CONFIRMED flag (the render step minted the collection in
/// exactly that order — index alignment by construction).
///
/// State machine preserved verbatim: `verified` keeps `Confirmed` (marker
/// dropped downstream), `refuted` demotes to `Archived` +
/// `demoted_by_verify`, everything inconclusive (`uncertain`/`unparsed`/
/// `error`/budget-skip) keeps `Confirmed` WITH the manual-verification
/// marker. Verify-stage exhaustion degrades the STAGE, never the run.
pub(crate) fn apply_verify_results(
    judged: &mut [JudgedFlag],
    results: &[MapItemResult],
    vstaff: &ResolvedSeatStaffing,
    budget: u64,
    case_id: &str,
) -> VerifyApplyOutcome {
    let identifier = seat_identifier(&vstaff.pm);
    let remote = vstaff.pm.is_remote();
    let endpoint_host = seat_endpoint_host(&vstaff.pm);
    let docket = judged.iter().filter(|j| j.tier == Tier::Confirmed).count();

    let mut calls = 0u32;
    let mut skipped = 0u32;
    let mut tokens = 0u64;
    let mut wall_ms = 0u64;
    let mut served_model: Option<String> = None;

    for (j, item) in judged.iter_mut().filter(|j| j.tier == Tier::Confirmed).zip(results.iter()) {
        let record = if item.error.as_deref() == Some(MAP_BUDGET_SKIP_ERROR) {
            skipped += 1;
            VerifyRecord {
                ruling: VerifyRuling::Error,
                decisive_evidence: String::new(),
                note_for_author:
                    "remote token budget exhausted for this stage — call skipped".to_string(),
                seconds: 0.0,
                model: identifier.clone(),
            }
        } else {
            calls += 1;
            tokens += item.total_tokens.unwrap_or(0);
            wall_ms += item.wall_ms;
            if served_model.is_none() {
                served_model = item.served_model.clone();
            }
            let seconds = item.wall_ms as f64 / 1000.0;
            if !item.ok {
                VerifyRecord {
                    ruling: VerifyRuling::Error,
                    decisive_evidence: String::new(),
                    note_for_author: format!(
                        "verify dispatch failed: {}",
                        item.error.as_deref().unwrap_or_default()
                    ),
                    seconds,
                    model: identifier.clone(),
                }
            } else {
                match parse_verify_ruling(&item.content) {
                    Some((ruling, decisive_evidence, note_for_author)) => VerifyRecord {
                        ruling,
                        decisive_evidence,
                        note_for_author,
                        seconds,
                        model: identifier.clone(),
                    },
                    None => VerifyRecord {
                        ruling: VerifyRuling::Unparsed,
                        decisive_evidence: String::new(),
                        note_for_author: String::new(),
                        seconds,
                        model: identifier.clone(),
                    },
                }
            }
        };
        emit_review_step_result(
            "review.verify",
            "review-ruling",
            case_id,
            json!({ "bundle_id": j.flag.bundle_id, "stage": "verify", "ruling": record.ruling, "seconds": record.seconds }),
        );
        if record.ruling == VerifyRuling::Refuted {
            j.tier = Tier::Archived;
            j.demoted_by_verify = true;
        }
        j.verify = Some(record);
    }

    let member = (calls > 0).then(|| MemberRecord {
        model: identifier,
        seat: "review-verify".to_string(),
        draws: calls,
        wall_ms,
        total_tokens: tokens,
        remote,
        endpoint: endpoint_host,
        served_model,
    });
    // Same wording as `verify_budget_outcome` (the sequential path's
    // helper) — stage-degrading, loud, never run-degrading.
    let warning = (skipped > 0).then(|| {
        let adjudicated = docket.saturating_sub(skipped as usize);
        format!(
            "verify budget exhausted after {adjudicated} of {docket} adjudications — the \
             remaining {skipped} confirmed finding(s) keep the manual-verification marker (the \
             per-execution allowance of {budget} tokens ran out)"
        )
    });
    let budget_row = (remote && (calls > 0 || skipped > 0)).then(|| RemoteBudgetRecord {
        stage: "verify".to_string(),
        max_tokens: budget,
        used_tokens: tokens,
        // (#1442 gate CONSIDER) `tokens` sums the endpoint-REPORTED usage
        // while the live `MapRemoteBucket` meters conservatively — a
        // usage-omitting endpoint can skip calls (`skipped > 0`) with the
        // summed total still below `budget`. A skip is itself proof the
        // bucket exhausted, so it keeps `exhausted` truthful. (Same corner as
        // the probe reconstruction's budget row.)
        exhausted: skipped > 0 || tokens >= budget,
        skipped_calls: skipped,
    });
    VerifyApplyOutcome { member, warning, budget_row }
}

// ─── report: synthesis (terminal step) ──────────────────────────────────

/// Phase "report", terminal step: `depends_on` BOTH `dedup` (for
/// `ReviewEnvelope::flags`, the deduped list) and `verify` (for the final,
/// verify-adjusted `ReviewEnvelope::judged`) — graph-native data flow
/// rather than a bespoke side channel. Recomputes tier counts +
/// `cluster_needs_check` (VERBATIM, explicitly preserved) directly from the
/// final judged list — correct by construction, no incremental-accumulator
/// double-counting risk. Procedural — no dispatch. Produces the FINAL
/// `ReviewEnvelope` (not the posted-comment `Rendered` markdown — that
/// stays `pr_review.rs::synthesize_review`'s job; see the module doc's
/// crate-boundary note).
///
/// **Tier 3 (#1352), on purpose.** Final-envelope assembly (tier-count
/// recomputation, the degenerate-run honesty gates, GitHub-comment-shaped
/// output) is genuinely specific to this pipeline's own `ReviewEnvelope`
/// type — no second consumer is visible today. Stays physically co-located
/// here.
pub struct ReviewSynthesisStepKind {
    pub ctx: Arc<ReviewStepContext>,
    pub env: SharedReviewEnvelope,
    /// (#1341) `gather_inputs` now keys a Step's `input` map by the
    /// DEPENDENCY TASK's id (dependency/data-flow lives at Task level) —
    /// so this step reads its upstream contributions by TASK id, not
    /// step id.
    pub dedup_task_id: String,
    /// (#1442 ship-2b) The judge task's id — the judged docket arrives
    /// directly from the judge now: the verify task's own output is the
    /// generic `dispatch.map`'s per-item result array, not a judged list.
    pub judge_task_id: String,
    pub verify_task_id: String,
    /// (#1442 ship-2b) The verify seat's staffing (None = no seat) — this
    /// step owns the verify-APPLY boundary: parsing the map results,
    /// running the preserved verified/refuted/uncertain state machine over
    /// the confirmed docket, and contributing the seat's member row +
    /// budget row + exhaustion warning (see [`apply_verify_results`]).
    pub(crate) verify: Option<ResolvedSeatStaffing>,
    /// Same run-wide accumulator every dispatching contributor uses,
    /// merged into `shared_env` by `run_review_graph` post-run.
    pub(crate) members: Arc<StdMutex<Vec<MemberRecord>>>,
    pub(crate) remote_budget: u64,
}

impl StepKind for ReviewSynthesisStepKind {
    fn id(&self) -> &'static str {
        "review.synthesis"
    }

    fn display_name(&self) -> &'static str {
        "Synthesis"
    }

    fn run(&self, step: &Step, _task: &Task, input: &std::collections::BTreeMap<String, String>) -> Result<StepOutcome> {
        let t0 = Instant::now();
        let dedup_output = input.get(&self.dedup_task_id).cloned().unwrap_or_default();
        let judge_output = input.get(&self.judge_task_id).cloned().unwrap_or_default();
        let verify_output = input.get(&self.verify_task_id).cloned().unwrap_or_default();
        let flags: Vec<ProbeFlag> = if dedup_output.is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&dedup_output).context("deserializing deduped flags")?
        };
        let mut judged: Vec<JudgedFlag> = if judge_output.is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&judge_output).context("deserializing judged flags")?
        };
        let verify_results: Vec<MapItemResult> = if verify_output.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&verify_output).context("deserializing verify map results")?
        };

        // (#1442 ship-2b) Verify-APPLY boundary: fold the generic map's
        // per-item results back onto the confirmed docket. An empty result
        // set covers every no-dispatch path in one shape (no seat staffed /
        // doomed run / zero confirmed — the render step emitted an empty
        // collection, the map short-circuited): the docket passes through
        // untouched, byte-identical to a crew with no verify seat.
        if let Some(vstaff) = &self.verify {
            if !verify_results.is_empty() {
                let outcome = apply_verify_results(
                    &mut judged,
                    &verify_results,
                    vstaff,
                    self.remote_budget,
                    &self.ctx.case_id,
                );
                if let Some(member) = outcome.member {
                    self.members.lock().expect("members mutex poisoned").push(member);
                }
                if outcome.warning.is_some() || outcome.budget_row.is_some() {
                    let mut env = self.env.lock().expect("shared review envelope mutex poisoned");
                    if let Some(w) = outcome.warning {
                        env.warnings.push(w);
                    }
                    if let Some(rec) = outcome.budget_row {
                        env.remote_budgets.push(rec);
                    }
                }
            }
        }

        let mut env = self.env.lock().expect("shared review envelope mutex poisoned").clone();
        env.raw_flags = env.raw_flags.max(flags.len());
        env.deduped_flags = flags.len();
        env.confirmed = judged.iter().filter(|j| j.tier == Tier::Confirmed).count();
        env.needs_check = judged.iter().filter(|j| j.tier == Tier::NeedsCheck).count();
        env.archived = judged.iter().filter(|j| j.tier == Tier::Archived).count();
        env.needs_check_clusters = cluster_needs_check(&judged);
        env.verified = judged
            .iter()
            .filter(|j| matches!(&j.verify, Some(v) if v.ruling == VerifyRuling::Verified))
            .count();
        env.refuted = judged.iter().filter(|j| j.demoted_by_verify).count();
        env.flags = flags;
        env.judged = judged;

        // (#1355 follow-up) The two most fundamental "no signal" gates from
        // the old `run_review_impl` driver (`bundles.is_empty()` / early
        // `raw_flags.is_empty()`) were never ported when the graph engine
        // replaced it — the graph never early-returns; every step just runs
        // on whatever (possibly empty) data it's handed and synthesis is the
        // only place with full visibility to catch this. Without these, a
        // diff that produces zero bundles (or zero probe draws) silently
        // renders as a clean pass instead of the LOUD degenerate outcome
        // `ReviewEnvelope::degenerate`'s own doc comment promises ("never a
        // silent pass") — confirmed as a real, live regression via the
        // review-bench migration's degenerate-fixture test.
        // (#1418) This step runs INSIDE `run_step_graph`, before
        // `run_review_graph`'s post-run merge populates `env.members` from
        // the probe accumulators (still empty here, see that merge's own
        // doc), so synthesis can catch THAT draws were zero
        // (`deduped_flags == 0`) but not WHY. `run_review_graph` replaces
        // this generic reason with a more specific "no seat matched any
        // bundle" one, once `env.members` is accurate, when that's the
        // actual cause; see the doc there.
        if env.degenerate.is_none() {
            if env.bundles == 0 {
                env.degenerate = Some("no bundles produced from the diff".to_string());
            } else if env.deduped_flags == 0 {
                env.degenerate = Some("zero flags from all probe draws — never a silent pass".to_string());
            }
        }

        // Judge-dead honesty gate (unchanged reasoning from `finish_review`):
        // no flag produced a usable pass-1 ruling means the judge phase
        // produced no signal worth rendering — a degenerate run, named.
        if env.degenerate.is_none() && !env.judged.is_empty() {
            let usable = env
                .judged
                .iter()
                .filter(|j| {
                    matches!(
                        j.pass1.ruling,
                        JudgeRuling::Confirmed | JudgeRuling::NeedsCheck | JudgeRuling::FalsePositive
                    )
                })
                .count();
            if usable == 0 {
                env.degenerate = Some(format!(
                    "judge produced no usable ruling on any of {} flags (all errored/unparsed)",
                    env.judged.len()
                ));
            }
        }

        *self.env.lock().expect("shared review envelope mutex poisoned") = env.clone();

        let wall_ms = t0.elapsed().as_millis() as u64;
        emit_review_step_result(
            "review.synthesis",
            &step.id,
            &self.ctx.case_id,
            json!({
                "confirmed": env.confirmed, "needs_check": env.needs_check, "archived": env.archived,
                "verified": env.verified, "refuted": env.refuted, "wall_ms": wall_ms,
            }),
        );

        let output = serde_json::to_string(&env).context("serializing final envelope")?;
        Ok(StepOutcome { output, flow_records: Vec::new() })
    }
}

/// The shared, mutex-guarded `ReviewEnvelope` every review step kind
/// contributes cross-cutting metrics to (member records, warnings, remote
/// budgets — fields with no single "owning" step) — the review's own
/// equivalent of `coder_phase.rs`'s `Arc<Mutex<Option<T>>>` result-slot
/// pattern for rich results that don't fit `StepOutcome.output: String`.
/// The FLAG DATA itself (`env.flags`/`env.judged`) flows graph-natively
/// through `Step.output`/`gather_inputs` instead (dedup → judge → verify →
/// synthesis) — this handle is deliberately NOT where that lives.
pub type SharedReviewEnvelope = Arc<StdMutex<ReviewEnvelope>>;

/// Everything [`build_review_graph`] hands back: the `Task`/`Step` shape
/// (for the caller to persist via `darkmux_crew::lifecycle::save_task`/
/// `save_step` under real Phase ids it creates — this module has no
/// `mission_id`/`lifecycle` dependency of its own, see the module doc's
/// crate-boundary note), the resolved [`StepKindRegistry`], the shared
/// cross-cutting envelope state, and a `step_id -> Task.phase_id` map (so
/// the caller can persist each Step under the SAME Phase its owning Task
/// belongs to without re-deriving the lookup).
pub struct BuiltReviewGraph {
    pub tasks: Vec<Task>,
    pub steps: std::collections::BTreeMap<String, Step>,
    pub registry: StepKindRegistry,
    pub shared_env: SharedReviewEnvelope,
    pub synthesis_step_id: String,
    pub phase_id_of_step: std::collections::BTreeMap<String, String>,
    /// The probe stage's accumulated `MemberRecord`s + reduced-coverage
    /// warnings + shared remote-token bucket — still EMPTY/unspent at
    /// return time (every probe step is registered, not yet run);
    /// [`run_review_graph`] reads them back through these SAME `Arc` handles
    /// AFTER `run_step_graph` completes, when every probe step kind has
    /// actually written into them.
    probe_members: Arc<StdMutex<Vec<MemberRecord>>>,
    probe_warnings: Arc<StdMutex<Vec<String>>>,
}

/// (#1402) Pure kind-id → display-name lookup for review's six Tier 3
/// kinds, usable WITHOUT constructing a live `StepKind` instance (which
/// needs a `ReviewStepContext`/staffing that only exist during a real
/// dispatch). `darkmux-serve`'s `mission_graph` module — a pure read path
/// over persisted JSON, never a live dispatch — calls this directly (the
/// crate already depends on `darkmux-lab`, so no new cross-crate edge).
///
/// Prefix-matches `"review.probe:<seat-name>"` (the only per-instance-
/// suffixed kind here — see `ReviewProbeStepKind::id`'s doc) to the SAME
/// base label its own `display_name()` returns; every other kind matches
/// exactly. `review_step_kind_display_names_match_the_live_impls` (below)
/// pins this literal table against the real `StepKind::display_name()`
/// implementations so the two can't silently drift apart.
pub fn review_step_kind_display_name(kind: &str) -> Option<&'static str> {
    if kind == "review.bundle" {
        return Some("Bundle");
    }
    // (#1442 ship-2b) `review.probe:<seat>` / `review.verify` kinds no
    // longer mint (the probe/verify stages ride the generic `dispatch.map`
    // block, whose display name resolves through the builtin registry) —
    // these entries remain so PERSISTED steps from pre-rewiring missions
    // still label correctly in the viewer's read path.
    if kind == "review.probe" || kind.starts_with("review.probe:") {
        return Some("Probe");
    }
    if kind == "review.dedup" {
        return Some("Dedup");
    }
    if kind == "review.verify-render" {
        return Some("Verify prompts");
    }
    if kind == "review.judge" {
        return Some("Judge");
    }
    if kind == "review.verify" {
        return Some("Verify");
    }
    if kind == "review.synthesis" {
        return Some("Synthesis");
    }
    None
}

/// Build the review's complete Task/Step graph across three Phases
/// (investigate / adjudicate / report — see the module doc) PLUS the
/// registry every step kind resolves through — see [`BuiltReviewGraph`].
/// Caller persists `tasks`/`steps`, then runs the graph via
/// [`run_review_graph`].
///
/// (#1284 Packet 3, #1512) A THIN LAUNCHER: loads the built-in "review"
/// mission config (`darkmux_crew::mission_config::load`), resolves every
/// genuinely per-launch value THIS FUNCTION's own parameters carry — the
/// three real phase ids and the resolved judge concurrency — into
/// `mission_config::interpret::LaunchParams`, then calls
/// `mission_config::interpret` to materialize the real `Vec<Task>` +
/// `BTreeMap<String, Step>`. `interpret` does NOT construct `StepKind`
/// instances (#1284 Packet 3's own scope, #1352's Tier 3 rule) — this
/// function still owns registering every Tier 3 kind this pipeline needs.
///
/// **No `expand` template (#1512).** review.json declares its probe tasks
/// EXPLICITLY — one task per probe role, statically, each carrying its own
/// `role_id` and depending only on `review-bundle-task`. `interpret` needs
/// no expansion collection at all; the probe COUNT is whatever
/// `review-dedup-task.depends_on` names. This function claims each
/// resolved `probes` staffing against the interpreted task whose `role_id`
/// matches (falling back to positional claiming for a hand-built staffing
/// with no `role_id` — test/back-compat fixtures), then PRUNES any declared
/// probe task nobody claimed. Pruning is what makes "fewer probe seats than
/// the document declares" a valid graph — it's how a hermetic test can
/// stand up a graph with 1-2 probes against the real 3-probe embedded
/// document without a second copy of review.json, and it's the same
/// mechanism an operator gets for free by editing the document itself (the
/// #1512 payoff: a genuinely 1-probe REVIEW.JSON has nothing to prune).
///
/// **Ids are FIXED, not case-id-seeded** (fixing a pre-Packet-3 doc-drift
/// finding): review.json's task/step ids are literal strings
/// (`review-bundle-task`, `review-judge-step`, …), never derived from
/// `ctx.case_id`. A single Mission running multiple PR reviews would
/// collide on these Task/Step ids — what actually prevents that collision
/// is `build_mission_for_review` (`src/pr_review.rs`) minting a
/// CASE-ID-DERIVED Mission/Phase per review, so two reviews' identical
/// Task/Step ids persist under different Phase directories, never the
/// literal ids themselves varying by case.
#[allow(clippy::too_many_arguments)]
pub fn build_review_graph(
    ctx: Arc<ReviewStepContext>,
    judge: ResolvedSeatStaffing,
    verify: Option<ResolvedSeatStaffing>,
    probes: &[ResolvedSeatStaffing],
    investigate_phase_id: &str,
    adjudicate_phase_id: &str,
    report_phase_id: &str,
    judge_concurrency: u32,
) -> Result<BuiltReviewGraph> {
    // (#1284 review round 2, consider 7) `load` resolves user →
    // on-disk → embedded, so a failure here is NOT necessarily the
    // embedded built-in's fault — a malformed USER-tier
    // `~/.darkmux/mission-configs/review.json` lands on this exact path.
    // Graceful error (never a panic), and the loader's own context names
    // the failing file's path, which identifies the tier.
    let loaded = darkmux_crew::mission_config::load("review").context(
        "loading mission config \"review\" — note: a user-tier copy \
         (~/.darkmux/mission-configs/review.json) or an on-disk template \
         overrides the embedded built-in; the failing file is named below",
    )?;
    build_review_graph_from_config(
        &loaded.config,
        &format!("resolved from the {} tier at {}", loaded.source, loaded.manifest_path.display()),
        ctx,
        judge,
        verify,
        probes,
        investigate_phase_id,
        adjudicate_phase_id,
        report_phase_id,
        judge_concurrency,
    )
}

/// [`build_review_graph`]'s pure core — everything AFTER loading the
/// document. Split out (#1512) so a test can build a graph from a
/// HAND-BUILT `MissionConfig` (e.g. a genuinely one-probe document) without
/// mutating the process-wide `DARKMUX_CREW_DIR` env var that
/// `mission_config::load`'s user tier reads — env mutation would race every
/// OTHER concurrently-running test in this crate that also calls
/// `build_review_graph` (cargo test's default parallelism), where a purely
/// in-memory `MissionConfig` races nothing. `source_detail` is folded into
/// the `interpret` error context (mirrors what `loaded.source`/
/// `loaded.manifest_path` gave the caller before this split).
#[allow(clippy::too_many_arguments)]
fn build_review_graph_from_config(
    config: &darkmux_crew::mission_config::MissionConfig,
    source_detail: &str,
    ctx: Arc<ReviewStepContext>,
    judge: ResolvedSeatStaffing,
    verify: Option<ResolvedSeatStaffing>,
    probes: &[ResolvedSeatStaffing],
    investigate_phase_id: &str,
    adjudicate_phase_id: &str,
    report_phase_id: &str,
    judge_concurrency: u32,
) -> Result<BuiltReviewGraph> {
    use darkmux_crew::mission_config::{interpret, LaunchParams};

    let mut phase_ids = std::collections::BTreeMap::new();
    phase_ids.insert("investigate".to_string(), investigate_phase_id.to_string());
    phase_ids.insert("adjudicate".to_string(), adjudicate_phase_id.to_string());
    phase_ids.insert("report".to_string(), report_phase_id.to_string());

    // (#1284 Packet 3 worklist) `judge_concurrency` is ALWAYS an override,
    // never read back out of review.json's own static
    // `config.concurrency`. The caller (`src/pr_review.rs`,
    // `review_bench.rs`) already resolves it via
    // `darkmux_types::config_access::review_judge_concurrency()` (env >
    // config.review.judge_concurrency > 1) before calling this function —
    // the JSON's static value is a documented DEFAULT for a human reading
    // the file, not a load-bearing fallback the launcher trusts.
    let mut step_config_overrides = std::collections::BTreeMap::new();
    step_config_overrides.insert(
        "review-judge-step".to_string(),
        json!({ "concurrency": judge_concurrency }),
    );

    // (#1512) No expansion collection — the probe stage is static tasks in
    // the document now, not a template.
    let params = LaunchParams {
        phase_ids,
        task_overrides: std::collections::BTreeMap::new(),
        step_config_overrides,
        expansions: std::collections::BTreeMap::new(),
    };

    let (mut tasks, mut steps, mut interpret_warnings) = interpret(config, &params)
        .with_context(|| format!("interpreting mission config \"review\" ({source_detail})"))?;

    // (#1512) Claim each resolved probe staffing against the DECLARED
    // task-id it dispatches through: by `role_id` when the staffing carries
    // one (the production/config-driven path — `role_id` is always some
    // review.json-declared probe role), else POSITIONALLY (a hand-built
    // staffing with no `role_id` — hermetic tests, mainly — claims the
    // first still-unclaimed declared probe task, in
    // `review-dedup-task.depends_on` order). `claims` is a task id per
    // `probes` entry, in the SAME order — never an index, so it survives
    // the pruning step below untouched.
    let dedup_task = tasks.iter().find(|t| t.id == "review-dedup-task").ok_or_else(|| {
        anyhow!("darkmux: interpreted \"review\" graph has no \"review-dedup-task\" task")
    })?;
    let declared_probe_task_ids: Vec<String> = dedup_task.depends_on.clone();

    // (#1513 review C1) A task named in `review-dedup-task.depends_on` with
    // no `role_id` used to be silently invisible: `resolve_review_roles`
    // never classified it (it isn't a probe role — there's no role to
    // resolve), and the claim/prune step below would then prune it with no
    // signal — a declared task quietly loses its dispatch. This is the
    // "Studio hand-edits review.json and forgets a role_id" failure mode.
    // Bail loudly rather than let a reduced-coverage run pass as healthy.
    for id in &declared_probe_task_ids {
        let Some(t) = tasks.iter().find(|t| &t.id == id) else { continue };
        if t.role_id.is_none() {
            bail!(
                "darkmux: \"review\" mission config's \"review-dedup-task\" depends on task \
                 \"{id}\", but that task declares no role_id — a probe task with no role_id has \
                 no staffing to resolve and would be silently pruned (zero dispatch, reduced \
                 coverage, no signal). Give \"{id}\" a role_id, or remove it from \
                 review-dedup-task's depends_on (#1512, #1513 review C1)"
            );
        }
    }

    let mut claims: Vec<String> = Vec::with_capacity(probes.len());
    for staffing in probes {
        let claimed_id = if let Some(role_id) = staffing.role_id.as_deref() {
            tasks
                .iter()
                .find(|t| {
                    // (#1513 review C2) Defense in depth alongside
                    // resolve_review_roles's own duplicate-role_id bail:
                    // skip a declared task this loop already claimed, so
                    // two probe tasks that (somehow) share a role_id can't
                    // both match the same declared task here.
                    t.role_id.as_deref() == Some(role_id)
                        && declared_probe_task_ids.iter().any(|id| id == &t.id)
                        && !claims.contains(&t.id)
                })
                .map(|t| t.id.clone())
                .ok_or_else(|| {
                    anyhow!(
                        "darkmux: interpreted \"review\" graph has no probe task for role \
                         \"{role_id}\" — review.json must declare a task with role_id \
                         \"{role_id}\" that \"review-dedup-task\" depends on (#1512)"
                    )
                })?
        } else {
            // Positional fallback: the first declared probe task no
            // earlier staffing already claimed.
            declared_probe_task_ids
                .iter()
                .find(|id| !claims.contains(id))
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "darkmux: more probe staffings resolved ({}) than \"review\" declares \
                         probe tasks for ({}) — bind each staffing's role_id, or add more probe \
                         tasks to review.json (#1512)",
                        probes.len(),
                        declared_probe_task_ids.len()
                    )
                })?
        };
        claims.push(claimed_id);
    }

    // Prune any declared probe task nobody claimed — lets a caller staff
    // fewer probe roles than review.json declares (the review's own
    // hermetic test suite relies on this; it's also how a genuinely
    // 1-probe review.json needs zero pruning at all, since every declared
    // task gets claimed).
    let pruned_ids: Vec<String> =
        declared_probe_task_ids.iter().filter(|id| !claims.contains(*id)).cloned().collect();
    if !pruned_ids.is_empty() {
        // (#1513 review C1) Loud, not silent: the production path pruning a
        // DECLARED task is exactly the reduced-coverage scenario a Studio
        // hand-edit (fewer staffed roles than review.json declares tasks
        // for) can trigger with zero other signal.
        interpret_warnings.push(format!(
            "\"review\" mission config declares probe task(s) {} that no resolved probe \
             staffing claimed — pruned from the graph (fewer roles resolved than review.json \
             declares probe tasks for) (#1512, #1513 review C1)",
            pruned_ids.join(", ")
        ));
        for id in &pruned_ids {
            if let Some(t) = tasks.iter().find(|t| &t.id == id) {
                for sid in &t.step_ids {
                    steps.remove(sid);
                }
            }
        }
        tasks.retain(|t| !pruned_ids.contains(&t.id));
        if let Some(d) = tasks.iter_mut().find(|t| t.id == "review-dedup-task") {
            d.depends_on.retain(|id| !pruned_ids.contains(id));
        }
    }

    // `step_id -> Task.phase_id`, derived once from `tasks` (each Task
    // already carries both) rather than threaded through every push site
    // above.
    let mut phase_id_of_step = std::collections::BTreeMap::new();
    for task in &tasks {
        for step_id in &task.step_ids {
            phase_id_of_step.insert(step_id.clone(), task.phase_id.clone());
        }
    }

    // (#1373) Built EARLY (moved up from its former place right before
    // `ReviewSynthesisStepKind`'s construction) so `ReviewDedupStepKind`/
    // `ReviewJudgeStepKind`/`ReviewVerifyStepKind` can ALSO hold a clone —
    // see each kind's own doc for what it reads/writes here (gates a-e).
    let shared_env: SharedReviewEnvelope = Arc::new(StdMutex::new(ReviewEnvelope {
        case_id: ctx.case_id.clone(),
        bundles: ctx.bundles.len(),
        warnings: interpret_warnings,
        ..Default::default()
    }));

    // (#1442 ship-2b) The probe/verify stages ride the GENERIC
    // `dispatch.map` builtin, so the registry starts from the Tier-1
    // builtin set instead of empty.
    let registry = StepKindRegistry::with_builtins();

    let bundle_kind = Arc::new(ReviewBundleStepKind { ctx: ctx.clone() });
    registry.register(bundle_kind.clone()).expect("review.bundle registered once");
    // (#1349) Legacy alias — a `Step.kind` persisted before the funnel->review
    // rename must still resolve if anything ever re-reads it back through a
    // fresh registry (see `StepKindRegistry::register_alias`'s doc).
    // (#1442 ship-2b) The probe/verify legacy aliases (`funnel.probe:<seat>`,
    // `funnel.verify`) retired WITH their kinds — there is no live
    // implementation left to alias to (pre-1.0, no compat baggage); the
    // read-path labeling of persisted historical steps lives in
    // `review_step_kind_display_name` instead.
    registry
        .register_alias("funnel.bundle", bundle_kind)
        .expect("funnel.bundle legacy alias registered once");

    // (#1354 follow-up) One accumulator for every dispatching contributor
    // (probe reconstruction at dedup, judge, verify apply at synthesis),
    // merged into `shared_env` once `run_step_graph` returns.
    let probe_members = Arc::new(StdMutex::new(Vec::new()));
    let probe_warnings = Arc::new(StdMutex::new(Vec::new()));

    // (#1512) Stamp each CLAIMED probe task's single step with its FULL
    // dispatch config — the pre-rendered `probe_user_message` collection
    // (byte parity by construction: `user_template: "{item}"` substitutes
    // each rendered prompt verbatim), the seat's dispatch identity, the
    // shared `bucket_group: "probe"` allowance (+ its resolved budget, so
    // the artifact is self-describing), `retry_on_empty: 1` (the
    // historical single empty-content retry), and the residency hints for
    // local seats. NOTE: a hosted seat's `endpoint` block carries only the
    // URL / auth MECHANICS (Keychain item name / env-var NAME — never a
    // secret value; see `EndpointAuth`), the same material `profiles.json`
    // already persists on disk.
    //
    // One role, one task, one dispatch (#1512) — the old (seat, draw) fan-
    // out (`ResolvedSeatStaffing::k` multiplying a seat into several sibling
    // tasks) retired with the `expand` template it depended on. `k` still
    // exists on the staffing (snapshotted into the envelope verbatim for
    // back-compat/bench reporting) but is no longer read here — probe
    // recall breadth is a config edit (add another probe role/task to
    // review.json), never a per-run draw multiplier.
    let remote_budget = ctx.remote_max_tokens_per_execution;
    let mut probe_specs: Vec<ProbeSeatSpec> = Vec::new();
    for (staffing, task_id) in probes.iter().zip(claims.iter()) {
        let identifier = seat_identifier(&staffing.pm);
        let endpoint = seat_endpoint(&staffing.pm);
        let endpoint_host = seat_endpoint_host(&staffing.pm);
        let max_tokens = resolve_seat_max_tokens(staffing, DEFAULT_PROBE_MAX_TOKENS);
        let selected = select_bundles_for_staffing(&ctx.bundles, staffing.selector.as_ref());
        let collection: Vec<String> =
            selected.iter().map(|b| probe_user_message(&ctx.probe_system, b)).collect();
        let bundles: Vec<(String, String)> =
            selected.iter().map(|b| (b.id.clone(), b.fact_family.clone())).collect();

        let task = tasks.iter().find(|t| &t.id == task_id).unwrap_or_else(|| {
            panic!("the claimed probe task `{task_id}` must survive pruning")
        });
        let step_id = task.step_ids.first().cloned().unwrap_or_else(|| {
            panic!("claimed probe task `{task_id}` must have exactly one step")
        });
        let step = steps.get_mut(&step_id).unwrap_or_else(|| {
            // (#1284 review round 2, consider 3) Hard assert posture
            // preserved: a release build must not silently mint a spec no
            // interpreted step backs.
            panic!("the interpreted graph must have a step `{step_id}` for probe task `{task_id}`")
        });
        let mut config = json!({
            "model": identifier,
            "system": "",
            "user_template": "{item}",
            "collection": collection,
            "temperature": PROBE_TEMPERATURE,
            "max_tokens": max_tokens,
            "timeout_seconds": ctx.timeout_seconds,
            "retry_on_empty": 1,
            "bucket_group": "probe",
            "bucket_budget": remote_budget,
        });
        if let Some(ep) = endpoint {
            config["endpoint"] = serde_json::to_value(ep).context("serializing probe seat endpoint")?;
        } else {
            // Residency hints: the wire `model` is the NAMESPACED
            // identifier; the loadable key is the bare profile id.
            config["model_key"] = json!(staffing.pm.id);
            config["identifier"] = json!(identifier);
            if let Some(n_ctx) = staffing.pm.n_ctx {
                config["n_ctx"] = json!(n_ctx);
            }
        }
        step.config = config;

        probe_specs.push(ProbeSeatSpec {
            name: staffing.name.clone(),
            identifier,
            remote: endpoint.is_some(),
            endpoint_host,
            draw_task_ids: vec![task_id.clone()],
            bundles,
        });
    }

    // (#1442 ship-2b) The verify map step's config — its COLLECTION arrives
    // at runtime from the render step (the task's single upstream input),
    // so only the dispatch parameters are stamped here. No verify seat ⇒
    // config stays null: the render step emits an empty collection and the
    // map short-circuits before any config key is required.
    if let Some(vstaff) = &verify {
        let identifier = seat_identifier(&vstaff.pm);
        let endpoint = seat_endpoint(&vstaff.pm);
        let max_tokens = resolve_seat_max_tokens(vstaff, DEFAULT_JUDGE_MAX_TOKENS);
        let step = steps
            .get_mut("review-verify-step")
            .expect("interpreted \"review\" graph must have a review-verify-step");
        let mut config = json!({
            "model": identifier,
            "system": ctx.verify_system,
            "user_template": "{item}",
            "temperature": JUDGE_TEMPERATURE,
            "max_tokens": max_tokens,
            "timeout_seconds": ctx.timeout_seconds,
            "retry_on_empty": 1,
            "bucket_budget": remote_budget,
        });
        if let Some(ep) = endpoint {
            config["endpoint"] =
                serde_json::to_value(ep).context("serializing verify seat endpoint")?;
        } else {
            config["model_key"] = json!(vstaff.pm.id);
            config["identifier"] = json!(identifier);
            if let Some(n_ctx) = vstaff.pm.n_ctx {
                config["n_ctx"] = json!(n_ctx);
            }
        }
        step.config = config;
    }

    let dedup_kind = Arc::new(ReviewDedupStepKind {
        ctx: ctx.clone(),
        env: shared_env.clone(),
        probe_specs,
        members: probe_members.clone(),
        warnings: probe_warnings.clone(),
        remote_budget,
    });
    registry.register(dedup_kind.clone()).expect("review.dedup registered once");
    // (#1349) Legacy alias — see the bundle step's registration above.
    registry
        .register_alias("funnel.dedup", dedup_kind)
        .expect("funnel.dedup legacy alias registered once");

    let judge_kind = Arc::new(ReviewJudgeStepKind {
        ctx: ctx.clone(),
        judge,
        members: probe_members.clone(),
        env: shared_env.clone(),
    });
    registry.register(judge_kind.clone()).expect("review.judge registered once");
    // (#1349) Legacy alias — see the bundle step's registration above.
    registry
        .register_alias("funnel.judge", judge_kind)
        .expect("funnel.judge legacy alias registered once");

    // (#1442 ship-2b) The render step — the Tier-3 half of the verify
    // stage that stayed bespoke (frozen `verify_prompt` assembly against
    // this pipeline's own types); the dispatch half is the generic map.
    let verify_render_kind = Arc::new(ReviewVerifyRenderStepKind {
        ctx: ctx.clone(),
        verify: verify.clone(),
        env: shared_env.clone(),
    });
    registry
        .register(verify_render_kind)
        .expect("review.verify-render registered once");

    // The interpreted graph's fixed ids for the upstream tasks
    // `ReviewSynthesisStepKind` reads from — derived from the ACTUAL
    // interpreted `steps` map (never hardcoded) so a document/interpreter
    // drift surfaces as a clear panic here, not a silent mismatch.
    let dedup_task_id = steps
        .values()
        .find(|s| s.kind == "review.dedup")
        .map(|s| s.task_id.clone())
        .expect("interpreted \"review\" graph must have a review.dedup step");
    let judge_task_id = steps
        .values()
        .find(|s| s.kind == "review.judge")
        .map(|s| s.task_id.clone())
        .expect("interpreted \"review\" graph must have a review.judge step");
    // The verify map step's kind is the generic `dispatch.map`, so its task
    // resolves by the document's FIXED step id (same fixed-ids contract as
    // the kind-keyed lookups above).
    let verify_task_id = steps
        .get("review-verify-step")
        .map(|s| s.task_id.clone())
        .expect("interpreted \"review\" graph must have a review-verify-step");
    let synthesis_step_id = steps
        .values()
        .find(|s| s.kind == "review.synthesis")
        .map(|s| s.id.clone())
        .expect("interpreted \"review\" graph must have a review.synthesis step");

    let synthesis_kind = Arc::new(ReviewSynthesisStepKind {
        ctx: ctx.clone(),
        env: shared_env.clone(),
        dedup_task_id,
        judge_task_id,
        verify_task_id,
        verify,
        members: probe_members.clone(),
        remote_budget,
    });
    registry.register(synthesis_kind.clone()).expect("review.synthesis registered once");
    // (#1349) Legacy alias — see the bundle step's registration above.
    registry
        .register_alias("funnel.synthesis", synthesis_kind)
        .expect("funnel.synthesis legacy alias registered once");

    Ok(BuiltReviewGraph {
        tasks,
        steps,
        registry,
        shared_env,
        synthesis_step_id,
        phase_id_of_step,
        probe_members,
        probe_warnings,
    })
}

/// Run the review's complete Task/Step graph via ONE `run_step_graph` call
/// (the module's whole point — see its doc). Runs the host telemetry
/// sampler `run_judge_only`'s driver (`finish_review`) also starts, but —
/// as of #1349 — does NOT
/// wrap the call in its own task-level liveness bookend. Every production
/// caller of this function (`src/pr_review.rs`'s `run_dispatch`) already
/// invokes it from INSIDE `with_dispatch_bookends`, which opens/closes the
/// canonical `dispatch start`/`dispatch complete`/`dispatch error` record
/// (`darkmux_flow::bookend::BookendGuard`, #1230 Packet 0) around the whole
/// call — the SAME liveness edge #1272 fixed the viewer's running-dispatch
/// surfaces to key on. A second, review-scoped task-level bookend here
/// was pure duplication of that outer wrap, not an independent liveness
/// fix, and its competing vocabulary is exactly the "bespoke top-level
/// record instead of the generic mechanism" bug #1349 retires — see
/// `with_dispatch_bookends`'s payload construction for where this function's
/// former task-bookend payload fields (exec mode, bundle count,
/// confirmed/needs_check/archived, degenerate reason) now ride instead, so
/// no data is lost, only the redundant vocabulary. (#1434 extended the same
/// retirement to the sequential `run_judge_only` path, so BOTH review paths
/// now emit only the generic `step result` companion vocabulary.)
/// Assembles the final [`ReviewEnvelope`] from the synthesis step's output
/// merged with the shared cross-cutting state, and returns the COMPLETED
/// `steps` map (status/output/timestamps all reflect the real run) so the
/// caller can persist the final Step records — `darkmux mission status`/the
/// graph lens must show what actually happened, never the pre-run
/// `Planned` snapshot `build_review_graph` produced.
/// `persist` (#1397 — "the review pipeline may not run through the crew
/// scheduler; check how `run_review_graph` executes its steps" — it DOES,
/// via the same `run_step_graph` call `coder_phase.rs`/`mission_launch.rs`
/// use, so it gets the identical transition-time persistence hook rather
/// than a bespoke one) fires at every step's OWN status flip — `Running`
/// at dispatch, `Complete`/`Error` at completion — mirroring
/// `run_step_graph`'s own `persist` doc exactly, since this function is a
/// thin pass-through to that call. This module deliberately has no
/// `mission_id`/`darkmux_crew::lifecycle` dependency of its own (see the
/// module doc's crate-boundary note) — `persist` is how the CALLER (
/// `mission_launch_review::run_dispatch`, which owns the minted
/// `mission_id`) gets durable per-transition Step saves without this
/// driver knowing what a Mission is. A no-op closure (`&mut |_| {}`) is a
/// valid `persist` for callers with no durable Step storage (every test in
/// this module, and `darkmux lab review-bench`'s per-run-local bench path,
/// which mints no real Mission — lab-vs-fleet boundary).
/// (#1442 ship-2b) Adapt `ReviewStepContext::chat_override` (the review
/// module's own test seam) into the generic scheduler-level
/// [`MapDispatchOverride`] the probe/verify `dispatch.map` steps consult —
/// `None` whenever the ctx carries no mock (every production call site),
/// so production dispatch.map transport is untouched. The two call shapes
/// are field-parallel by construction ([`ChatCall`] ↔
/// [`OverrideDispatchCall`]).
pub(crate) fn review_dispatch_override(ctx: &ReviewStepContext) -> Option<MapDispatchOverride> {
    let chat = ctx.chat_override.clone()?;
    Some(Arc::new(move |call: &OverrideDispatchCall| {
        chat(&ChatCall {
            model: call.model,
            system: call.system,
            user: call.user,
            temperature: call.temperature,
            max_tokens: call.max_tokens,
            endpoint: call.endpoint,
        })
    }))
}

/// (#1486) Build the LOUD, SPECIFIC degenerate reason for a run whose
/// scheduler reported errored steps — surfacing each errored step's OWN
/// failure message, never just the bare step ids.
///
/// A step that terminates in error carries its failure reason in its
/// `output`: the scheduler's `apply_step_terminal` stores the message there
/// (`Err(format!("{e:#}"))`, the per-job `Err` `run_bounded` synthesizes for
/// a residency block or a wave-load failure). A wholesale probe-stage failure
/// — every seat's model failing to load, e.g. a 120B model that never fit the
/// RAM budget — used to finalize `flags=0 members=0` with a reason that named
/// only the step IDS, swallowing that "could not load … for this wave"
/// message. This surfaces the reason itself, keyed by step id, so the
/// operator/orchestrator sees WHY nothing ran — the dispatch-liveness
/// contract's converse (#857/#1272): blocked/failed work is as visible, and
/// as reasoned, as running work.
fn errored_steps_degenerate_reason(
    errored: &[String],
    steps: &std::collections::BTreeMap<String, Step>,
) -> String {
    let reasons: Vec<String> = errored
        .iter()
        .map(|id| {
            let msg = steps
                .get(id)
                .and_then(|s| s.output.as_deref())
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .unwrap_or("(no failure reason recorded)");
            format!("{id}: {msg}")
        })
        .collect();
    format!(
        "review graph: {} step(s) errored, zero usable signal — {}",
        errored.len(),
        reasons.join("; ")
    )
}

#[allow(clippy::too_many_arguments)]
pub fn run_review_graph(
    ctx: &ReviewStepContext,
    crew_name: &str,
    mode: ExecMode,
    fingerprint_val: serde_json::Value,
    staffing: StaffingSnapshot,
    graph: BuiltReviewGraph,
    emitter: &mut dyn ReviewEmitter,
    persist: &mut dyn FnMut(&Step),
) -> Result<(ReviewEnvelope, std::collections::BTreeMap<String, Step>)> {
    let BuiltReviewGraph {
        tasks,
        mut steps,
        registry,
        shared_env,
        synthesis_step_id,
        probe_members,
        probe_warnings,
        ..
    } = graph;
    let tasks_by_id: std::collections::BTreeMap<String, Task> =
        tasks.into_iter().map(|t| (t.id.clone(), t)).collect();

    {
        let mut env = shared_env.lock().expect("shared review envelope mutex poisoned");
        env.case_id = ctx.case_id.clone();
        env.crew = crew_name.to_string();
        env.mode = mode_label(mode).to_string();
        env.fingerprint = fingerprint_val;
        env.staffing = Some(staffing);
    }

    // (#1349) Host telemetry only — no bookend struct. The caller already
    // owns the run's liveness bookend (see this function's doc); this
    // sampler's samples are drained and forwarded to `emitter` alongside
    // `run_step_graph`'s own step-lifecycle records, same interleaving
    // discipline `HostTelemetrySampler`'s doc describes.
    let telemetry = HostTelemetrySampler::start(
        ctx.case_id.clone(),
        crew_name.to_string(),
        REVIEW_TELEMETRY_INTERVAL,
        REVIEW_TELEMETRY_POLL,
        sample_host,
        darkmux_profiles::lms::list_loaded,
    );

    let facts = {
        let mut host = LmsHost::new();
        gather_facts(&mut host).unwrap_or_default()
    };
    let est = inert_estimator();

    // `run_step_graph`'s own emit closure runs entirely on the MAIN thread
    // (the scheduler drains each wave's `run_bounded` results before
    // calling `emit` — see `scheduler::run_step_graph`'s loop), never
    // inside a worker thread, so capturing `&mut telemetry`/`emitter` here
    // is safe. This routes the scheduler's generic step-lifecycle bookends
    // through the SAME injected `ReviewEmitter` every other record in this
    // driver uses — the driver stays sink-agnostic (module doc), never
    // calling `darkmux_flow::record` directly itself.
    let report = run_step_graph(
        &mut steps,
        &tasks_by_id,
        &registry,
        &facts,
        &est,
        8,
        &darkmux_crew::concurrent_dispatch::lms_host_factory,
        &mut |record| {
            for sample in telemetry.rx.try_iter().collect::<Vec<_>>() {
                emitter.emit(sample);
            }
            emitter.emit(record);
        },
        persist,
        // (#1442 ship-2b) The ctx-mock adapter — `None` in production; a
        // mocked test's probe/verify `dispatch.map` items dispatch through
        // the same `chat_override` every bespoke review kind uses.
        review_dispatch_override(ctx),
    );

    // Merge the probe stage's NOW-populated accumulators (every probe step
    // has run by the time `run_step_graph` returns, whether it errored or
    // not) into the shared envelope — this can only happen AFTER the run,
    // not at `build_review_graph` time when they were still empty.
    // (#1442 ship-2b) The probe stage's budget row + exhaustion warning
    // now reconstruct at the DEDUP boundary (`reconstruct_probe_stage`) and
    // land in `shared_env` during the run; only the member/warning
    // accumulators still merge here.
    {
        let mut env = shared_env.lock().expect("shared review envelope mutex poisoned");
        env.members
            .extend(probe_members.lock().expect("probe members mutex poisoned").iter().cloned());
        env.warnings
            .extend(probe_warnings.lock().expect("probe warnings mutex poisoned").iter().cloned());
    }

    let report = match report {
        Ok(r) => r,
        Err(e) => {
            let mut env = shared_env.lock().expect("shared review envelope mutex poisoned").clone();
            env.degenerate = Some(format!("review graph scheduling failed: {e:#}"));
            for sample in telemetry.rx.try_iter().collect::<Vec<_>>() {
                emitter.emit(sample);
            }
            return Ok((env, steps));
        }
    };

    let env = if report.errored.is_empty() {
        let mut env = match steps.get(&synthesis_step_id).and_then(|s| s.output.as_deref()) {
            Some(out) => serde_json::from_str::<ReviewEnvelope>(out)
                .unwrap_or_else(|_| shared_env.lock().expect("shared review envelope mutex poisoned").clone()),
            None => shared_env.lock().expect("shared review envelope mutex poisoned").clone(),
        };
        // The synthesis step's own serialized `output` was captured DURING
        // the graph run — before the post-run merge above populated
        // `shared_env`'s members/warnings/remote_budgets from the probe
        // dispatch accumulators, which only land in `shared_env` after
        // `run_step_graph` returns. Pulling from the synthesis step's
        // snapshot alone silently drops real dispatch-provenance data (the
        // posted review's "probed by ...; judged by ..." attribution and
        // remote-budget warnings) even on a clean, fully-successful run.
        let shared = shared_env.lock().expect("shared review envelope mutex poisoned");
        env.members = shared.members.clone();
        env.warnings = shared.warnings.clone();
        env.remote_budgets = shared.remote_budgets.clone();
        drop(shared);

        // (#1418) `ReviewSynthesisStepKind::run` already catches a
        // `deduped_flags == 0` run via its own "zero flags from all probe
        // draws" gate, but synthesis runs INSIDE `run_step_graph`, before
        // `env.members` is merged in (just above), so it can't tell WHY
        // draws were zero. Now that `env.members` is accurate, name the
        // SPECIFIC "no seat matched any bundle" cause when that's what
        // actually happened (a selector/config problem, distinct from a
        // probe that genuinely dispatched and came back with nothing),
        // replacing synthesis's generic reason with a more actionable one.
        // Two routes land here: every probe seat's selector matching zero
        // of the diff's bundles, and a silently-zero-expanded probe
        // template (`mission_config::interpret`'s absent-`expand.over`-key
        // case, which also surfaces its own `env.warnings` entry). Either
        // way, `env.bundles > 0` (the diff produced real bundles) but not
        // one seat ever placed a call: a review that examined nothing
        // must never read as Clean.
        let total_draws: u32 = env.members.iter().map(|m| m.draws).sum();
        if env.bundles > 0 && total_draws == 0 {
            env.degenerate = Some(
                "no probe seat matched any bundle: zero draws across every staffed seat \
                 (check each seat's selector against the diff's bundles, and that the \
                 crew's probe expansion actually staffed a seat); a review that examined \
                 nothing is never a clean pass"
                    .to_string(),
            );
        }
        env
    } else {
        let mut env = shared_env.lock().expect("shared review envelope mutex poisoned").clone();
        // (#1486) Surface each errored step's OWN failure message — the
        // residency block / model-load error / synthesized dispatch error
        // that `run_bounded` handed back and the scheduler stored in the
        // step's `output` — never just the bare step ids. A wholesale
        // probe-stage failure (every seat's model failed to load, e.g. a
        // 120B model that never fit) previously finalized `flags=0
        // members=0` with a reason naming only the step IDS, swallowing the
        // "could not load … for this wave" message that pinpoints the cause.
        // That is the dispatch-liveness contract's converse (#857/#1272):
        // blocked/failed work must be as visible — and as REASONED — as
        // running work, never a silent Clean.
        if env.degenerate.is_none() {
            env.degenerate = Some(errored_steps_degenerate_reason(&report.errored, &steps));
        }
        env
    };

    // Final drain before `telemetry` drops (its own `Drop` then stops the
    // sampler thread) — same "known, accepted loss window" the retired
    // bookend guard documented: at most one final-tick sample can land in
    // the brief window between this drain and the thread join completing.
    for sample in telemetry.rx.try_iter().collect::<Vec<_>>() {
        emitter.emit(sample);
    }
    Ok((env, steps))
}

// ═══════════════════════════════════════════════════════════════════════
// tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
#[path = "review_tests.rs"]
mod tests;
