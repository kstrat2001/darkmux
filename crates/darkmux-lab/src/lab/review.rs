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
//! This module is the DRIVER: given a resolved crew (packet 1's
//! `darkmux_profiles::crews::resolve_crew`), a diff, and an intent, it runs
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
//! **Reconciled in packet 5** (`darkmux pr-review run`, `src/pr_review.rs`
//! in the binary crate): rather than editing `bundles_from_diff`'s body
//! in place, [`ReviewInputs::bundles`] is the injection seam — packet 5
//! builds real bundles via `build_bundles`/`external_bundles` + `slice_code`
//! and passes `Some(..)`; [`run_review`]/[`run_judge_only`] use those
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
//! The driver (`run_review`/`run_judge_only`/`finish_review`/`probe_phase`/
//! `dispatch_probe_staffing`) emits [`darkmux_flow::FlowRecord`]s through a
//! caller-injected [`ReviewEmitter`] — same injection discipline as `chat`/
//! `cycler` above, so a scripted test can assert the exact record SEQUENCE
//! via a recording mock. The driver is deliberately SINK-AGNOSTIC: it never
//! calls `darkmux_flow::record` itself and has no idea whether the records
//! land on the real engagement-scoped flow stream or a per-run-local file —
//! that choice belongs to the caller (`darkmux pr-review run` wires the real
//! stream; `darkmux lab review-bench --review` wires a per-run-local JSONL
//! file, per the lab-vs-fleet scope boundary — a bench's hundreds of
//! per-flag ruling records must never spam an operator's engagement
//! stream). Three action families, vocabulary aligned with #1230/#1240's
//! Mission → Phase → Task → Step hierarchy so the records forward-port to
//! the generic mission-flow graph view unchanged:
//!
//! - `review.task` — one review RUN's bookends (`payload.status` = `started`
//!   | `finished` | `error`): case id, crew, exec mode, bundle count on
//!   start; confirmed/needs_check/archived counts + `degenerate` reason
//!   (when set) on finish. `error` is the [`ReviewRunGuard`]'s Drop-path
//!   terminal record — emitted when the driver `?`-returns or panics after
//!   `started`, so no consumer ever sees an orphaned, perpetually-in-flight
//!   run (the same guarantee `darkmux-crew`'s `DispatchBookendGuard`, #717,
//!   gives `dispatch.start`).
//! - `review.step` — a step transition, payload shape matching #1230's
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
//!   observer see `review.ruling{pass:2}` records stream in while the
//!   `judge-pass2` step still read "not started" — a real contradiction
//!   caught live in the lab lens. `finished` closes once the loop
//!   completes, carrying the real final docket size and elapsed `wall_ms`.
//! - `review.ruling` — the live ticker: one record per judge ruling (every
//!   pass-1, plus pass-2 when it ran) with `bundle_id`/`pass`/`ruling`/
//!   `seconds`.
//!
//! Emission happens ONLY in the driver — never inside the pure protocol
//! functions (`dedup_flags`, `mechanism_family`, `parse_judge_ruling`,
//! `judge_prompt`, etc.) or the per-flag dispatch helper `judge_one_flag`
//! (its [`JudgeOutcome`] is emitted from by the caller in `finish_review`'s
//! loop, after the call returns).
//!
//! ## Host telemetry sampling (#1247 doctrine surface — "No blind runs")
//!
//! `run_review`/`run_judge_only` also start a background host cpu/ram/gpu
//! sampler for the run's whole lifetime — see [`ReviewRunGuard`] and
//! [`HostTelemetrySampler`]. Samples emit as `telemetry.process` records
//! through the SAME injected [`ReviewEmitter`] the `review.*` action family
//! above uses (so a bench run's samples stay per-run-local and a
//! `pr-review run`'s samples ride the fleet stream, same split), with the
//! identical field shape `darkmux_crew::dispatch_internal`'s always-on
//! sampler already produces — the run-monitor/viewer code that renders
//! `telemetry.process` today applies unchanged.

use anyhow::{anyhow, bail, Context, Result};
use darkmux_crew::single_shot::SingleShotReply;
use darkmux_crew::telemetry_sampler::{sample_host, HostSample};
// (#1230 Packet 1) LmsCycler's residency mechanism now routes through
// gestalt's pure planner, executed via the real LmsHost/MacProbe port
// adapters (their first production call site) — see the "model cycling"
// section below.
use darkmux_gestalt::{AcquireOpts, AcquireScope, Action, CallerIntent, Facts, ModelHost, Placement, ResourceProbe, V1Estimator};
use darkmux_profiles::crews::{ResolvedCrew, ResolvedSeatStaffing};
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
/// the `review.step` flow record (#1247 Part 1, see the module doc) — the
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

const REVIEW_TASK_ACTION: &str = "review.task";
const REVIEW_STEP_ACTION: &str = "review.step";
const REVIEW_RULING_ACTION: &str = "review.ruling";

/// Build one review observability record. `handle` = the crew name (this
/// review's addressable identity, the role `handle` plays for `crew
/// dispatch`'s per-role records); `session_id` = the case id (one review
/// RUN's identity, the role `session_id` plays for a single dispatch).
/// `source = "review"` distinguishes these from `crew_dispatch`/
/// `phase_review` records that may share the same sink. `category = Work`
/// / `tier = Local` / `stage = Dispatch` mirror `crew dispatch`'s own
/// per-turn records (`dispatch.tool`, `dispatch.turn`) — the review is,
/// mechanically, a multi-dispatch alternative shape of the same "produce a
/// local review" job.
fn review_flow_record(
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
        payload: Some(payload),
        work_id: None,
        attempt: None,
    }
}

/// The `review.task` "finished" record's payload + level, shared by every
/// return point (`run_review`'s two early degenerate returns,
/// `run_judge_only`'s one, and `finish_review`'s normal end) so the shape
/// can't drift between call sites. `Level::Warn` when `env.degenerate` is
/// set — a degenerate run is a loud, scoreable outcome, never quietly
/// `Info`.
fn task_finished_record(env: &ReviewEnvelope) -> darkmux_flow::FlowRecord {
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
    // (#1260/#1186) Remote-seat tokens, carried separately so downstream
    // savings surfaces can EXCLUDE them — cloud tokens are never counted
    // "off the meter". Present iff any seat dispatched remotely.
    if env.members.iter().any(|m| m.remote) {
        let remote_tokens: u64 = env.members.iter().filter(|m| m.remote).map(|m| m.total_tokens).sum();
        payload["remote_tokens"] = remote_tokens.into();
    }
    if !env.warnings.is_empty() {
        payload["warnings"] = serde_json::Value::Array(
            env.warnings.iter().map(|w| serde_json::Value::String(w.clone())).collect(),
        );
    }
    let level = if env.degenerate.is_some() { darkmux_flow::Level::Warn } else { darkmux_flow::Level::Info };
    review_flow_record(&env.case_id, &env.crew, REVIEW_TASK_ACTION, level, payload)
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
/// `review_flow_record` stamps on the `review.*` action family, so a
/// telemetry record for this run groups with its other records under the
/// same `session_id`.
///
/// The sampling FUNCTION is injected (`sample_fn`, a plain fn pointer
/// defaulting to `sample_host` at every production call site — see
/// [`ReviewRunGuard::new`]) so tests can drive the sampler with an
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
/// [`ReviewRunGuard`] drains the channel immediately before every
/// record it already emits (`review.task`/`review.step`/`review.ruling`)
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
    /// the review run it's observing.
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
    /// [`ReviewRunGuard`], on every exit path (clean finish, early
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

/// Adapts a [`ReviewEmitter`] to `darkmux_flow`'s generic
/// [`darkmux_flow::BookendSink`] (#1230 Packet 0) so [`ReviewRunGuard`]
/// can wrap `darkmux_flow::BookendGuard` without `darkmux-flow` (a
/// dependency LEAF) knowing this crate's own emitter trait exists. A local
/// type implementing a foreign trait — no orphan-rule friction.
struct EmitterSink<'a>(&'a mut dyn ReviewEmitter);

impl darkmux_flow::BookendSink for EmitterSink<'_> {
    fn emit(&mut self, record: darkmux_flow::FlowRecord) {
        self.0.emit(record);
    }
}

/// (#1247 review round, unified onto `darkmux_flow::BookendGuard` in #1230
/// Packet 0) Bookend guard for the review's flow-record lifecycle — same
/// class of problem `darkmux-crew`'s `DispatchBookendGuard` (#717) solves
/// for `dispatch.start`: once `review.task started` is emitted, the driver
/// can still `?`-return before the clean `finished` bookend (a probe
/// dispatch error, a cycler load/release failure) — or panic. Without a
/// terminal record that leaves an orphaned task (rendering as perpetually
/// in-flight to any consumer) plus whatever step-`started` records were
/// open at the abort point.
///
/// All driver emission routes THROUGH this guard, which delegates the
/// task/step open-stack bookkeeping to the shared `darkmux_flow::BookendGuard`
/// (`inner`) — the task itself is pushed as one open unit (`kind == "task"`)
/// and each step nests inside it as another (`kind` = the step's own kind,
/// `"dispatch"`/`"procedural"`), so `inner`'s Drop pops every still-open
/// unit innermost-first (steps before the task) and builds the right
/// `review.step`- or `review.task`-shaped abort record via its `on_abort`
/// closure — every `started` gets a matching terminal event, on every path.
/// The clean finish (`task_finished`, which every success/degenerate return
/// point calls) closes the task unit, disarming `inner` once the stack
/// empties — a run that reached its own terminal record is never
/// double-counted.
///
/// Emission on `Drop` is best-effort by construction — [`ReviewEmitter`]
/// impls are already infallible (`emit` returns nothing), so a sink problem
/// can't mask the original error propagating out.
///
/// Also owns the run's [`HostTelemetrySampler`] (#1247 doctrine surface):
/// started in [`Self::new`]/[`Self::new_with_telemetry`] and
/// stopped by its own `Drop` — which Rust runs automatically as a field of
/// this struct, right after `ReviewRunGuard`'s own `Drop::drop` body
/// returns, so the sampler thread never outlives the guard on any exit
/// path. This telemetry-draining half is genuinely review-specific (it
/// doesn't belong in `darkmux-flow`) — only the open/close stack moved.
struct ReviewRunGuard<'a> {
    case_id: String,
    crew: String,
    inner: darkmux_flow::BookendGuard<'a, EmitterSink<'a>>,
    telemetry: HostTelemetrySampler,
}

/// The fixed unit id/kind [`ReviewRunGuard::task_started`]/
/// [`ReviewRunGuard::task_finished`] open/close under — the review has
/// exactly one task per run, so this is a constant rather than something
/// derived per-call. `inner`'s `on_abort` closure matches on `kind == this`
/// to decide whether a still-open unit at Drop time needs a `review.task`-
/// or `review.step`-shaped abort record.
const REVIEW_TASK_UNIT_KIND: &str = "task";

impl<'a> ReviewRunGuard<'a> {
    fn new(sink: &'a mut EmitterSink<'a>, case_id: &str, crew: &str) -> Self {
        Self::new_with_telemetry(
            sink,
            case_id,
            crew,
            REVIEW_TELEMETRY_INTERVAL,
            REVIEW_TELEMETRY_POLL,
            sample_host,
        )
    }

    /// Same as [`Self::new`] but with a caller-chosen telemetry cadence
    /// AND sampling function — the test-only seam a scripted run uses to
    /// observe deterministic samples without a multi-second sleep and
    /// without shelling to the real (macOS-only, ~600-900ms-per-call)
    /// `top`/`vm_stat`/`ioreg` commands. Production always goes through
    /// `new`, which fixes the cadence at [`REVIEW_TELEMETRY_INTERVAL`]
    /// and the sampler at the real `sample_host`.
    fn new_with_telemetry(
        sink: &'a mut EmitterSink<'a>,
        case_id: &str,
        crew: &str,
        telemetry_interval: Duration,
        telemetry_poll: Duration,
        sample_fn: fn() -> HostSample,
    ) -> Self {
        let case_id_owned = case_id.to_string();
        let crew_owned = crew.to_string();
        let on_abort = move |id: &str, kind: &str| -> darkmux_flow::FlowRecord {
            if kind == REVIEW_TASK_UNIT_KIND {
                review_flow_record(
                    &case_id_owned,
                    &crew_owned,
                    REVIEW_TASK_ACTION,
                    darkmux_flow::Level::Error,
                    json!({
                        "status": "error",
                        "case_id": case_id_owned,
                        "crew": crew_owned,
                        "error": "review terminated before completion (early return or panic)",
                    }),
                )
            } else {
                review_flow_record(
                    &case_id_owned,
                    &crew_owned,
                    REVIEW_STEP_ACTION,
                    darkmux_flow::Level::Error,
                    json!({ "step_id": id, "kind": kind, "status": "error" }),
                )
            }
        };
        Self {
            telemetry: HostTelemetrySampler::start(
                case_id.to_string(),
                crew.to_string(),
                telemetry_interval,
                telemetry_poll,
                sample_fn,
            ),
            case_id: case_id.to_string(),
            crew: crew.to_string(),
            inner: darkmux_flow::BookendGuard::new(sink, on_abort),
        }
    }

    /// Drain every telemetry sample buffered since the last drain and emit
    /// each through the same sink the driver's own records go through —
    /// called immediately before every record this guard emits (see
    /// [`Self::emit_now`]) so telemetry streams alongside the run rather
    /// than batching at the end.
    fn drain_telemetry(&mut self) {
        let records: Vec<darkmux_flow::FlowRecord> = self.telemetry.rx.try_iter().collect();
        for record in records {
            self.inner.emit_now(record);
        }
    }

    /// Drain pending telemetry, then emit `record` with no open/close
    /// bookend of its own. Every direct emission in this guard routes
    /// through here so telemetry ordering stays close to wall-clock
    /// without needing the sampler thread to touch the sink itself.
    fn emit_now(&mut self, record: darkmux_flow::FlowRecord) {
        self.drain_telemetry();
        self.inner.emit_now(record);
    }

    /// Emit the `review.task started` bookend and ARM the guard — from here
    /// until `task_finished`, an early return or panic fires the Drop path.
    fn task_started(&mut self, payload: serde_json::Value) {
        self.drain_telemetry();
        let started = review_flow_record(
            &self.case_id,
            &self.crew,
            REVIEW_TASK_ACTION,
            darkmux_flow::Level::Info,
            payload,
        );
        self.inner.open(REVIEW_TASK_UNIT_KIND, REVIEW_TASK_UNIT_KIND, started);
    }

    /// Emit a `review.step` `status: "started"` record and track the step
    /// as open until [`Self::step_finished`] closes it.
    fn step_started(&mut self, step_id: &str, kind: &str, payload: serde_json::Value) {
        self.drain_telemetry();
        let started = review_flow_record(
            &self.case_id,
            &self.crew,
            REVIEW_STEP_ACTION,
            darkmux_flow::Level::Info,
            payload,
        );
        self.inner.open(step_id, kind, started);
    }

    /// Emit a `review.step` `status: "finished"` record and close the step.
    /// Also the entry point for one-shot steps that emit `finished` with no
    /// prior `started` (`bundle`, `dedup` — instantaneous procedural steps);
    /// the close is then a no-op on the open-step stack.
    fn step_finished(&mut self, step_id: &str, payload: serde_json::Value) {
        self.drain_telemetry();
        let finished = review_flow_record(
            &self.case_id,
            &self.crew,
            REVIEW_STEP_ACTION,
            darkmux_flow::Level::Info,
            payload,
        );
        self.inner.close(step_id, finished);
    }

    /// Emit a `review.ruling` ticker record (no open/close semantics).
    fn ruling(&mut self, payload: serde_json::Value) {
        self.emit_now(review_flow_record(
            &self.case_id,
            &self.crew,
            REVIEW_RULING_ACTION,
            darkmux_flow::Level::Info,
            payload,
        ));
    }

    /// Emit the terminal `review.task` record for `env` (finished, or
    /// degenerate-finished — see [`task_finished_record`]) and close the
    /// task unit: this run reached its own terminal record, so `inner`
    /// disarms once the stack empties (every real call site already closed
    /// its own steps before calling this, so the stack is just `[task]`).
    fn task_finished(&mut self, env: &ReviewEnvelope) {
        self.drain_telemetry();
        self.inner.close(REVIEW_TASK_UNIT_KIND, task_finished_record(env));
    }
}

impl Drop for ReviewRunGuard<'_> {
    fn drop(&mut self) {
        // Flush any sample the sampler produced since the last drain — even
        // on the clean-finish path (`task_finished` already closed `inner`'s
        // task unit), a sample can land in the brief window between that
        // drain and this `Drop` running. `inner`'s own Drop (a field of this
        // struct, run automatically right after this function returns) then
        // emits abort records for any still-open units through the same
        // sink if it's still armed. The sampler thread itself stops right
        // after, via `HostTelemetrySampler`'s own `Drop` (also a field,
        // torn down last), so it never outlives the guard on any exit path.
        //
        // Known, accepted loss window: a sample the sampler thread sends
        // AFTER this final drain but BEFORE the join in the sampler's
        // `Drop` completes is dropped with the channel — at most one
        // final-tick sample, consistent with the sampler's best-effort
        // framing (telemetry never blocks or extends teardown to chase
        // one more data point).
        self.drain_telemetry();
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
    pub staffing: Option<CrewStaffingSnapshot>,
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
pub struct StaffingSnapshot {
    pub name: String,
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
}

/// Per-seat resolved staffing snapshot — `review-probe` (one or more
/// staffings) + `review-judge` (exactly one) + the optional `review-verify`
/// seat (#1260). See [`ReviewEnvelope::staffing`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrewStaffingSnapshot {
    pub probes: Vec<StaffingSnapshot>,
    pub judge: Option<StaffingSnapshot>,
    /// (#1260) Present iff the crew declares the `review-verify` seat —
    /// absent (and never serialized) otherwise, so pre-#1260 snapshots
    /// round-trip unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<StaffingSnapshot>,
    /// (#1302) The crew's resolved `request_changes` flag — snapshotted so the
    /// render path reads the run's own blocking-vs-advisory choice from its
    /// self-describing artifact, and a serialized envelope re-rendered later
    /// picks the same review event. Defaults to `false` on read (skipped when
    /// `false`) so pre-#1302 snapshots round-trip unchanged as the non-blocking
    /// default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub request_changes: bool,
}

/// (#1266) Snapshot default for `StaffingSnapshot::passes` — 2 (double-
/// confirm), so a pre-1.3 envelope missing the field reads as today's judge.
fn default_snapshot_passes() -> u32 {
    2
}

pub fn staffing_snapshot(
    probes: &[ResolvedSeatStaffing],
    judge: &ResolvedSeatStaffing,
    verify: Option<&ResolvedSeatStaffing>,
    request_changes: bool,
) -> CrewStaffingSnapshot {
    fn one(s: &ResolvedSeatStaffing) -> StaffingSnapshot {
        StaffingSnapshot {
            name: s.name.clone(),
            model: seat_identifier(&s.pm),
            remote: s.pm.is_remote(),
            endpoint: seat_endpoint_host(&s.pm),
            k: s.k,
            passes: s.passes,
            n_ctx: s.pm.n_ctx,
            max_tokens: s.max_tokens,
            selector: s.selector.clone(),
        }
    }
    CrewStaffingSnapshot {
        probes: probes.iter().map(one).collect(),
        judge: Some(one(judge)),
        verify: verify.map(one),
        // (#1302) The run's blocking-vs-advisory choice, snapshotted for the
        // render path (see `CrewStaffingSnapshot::request_changes`).
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
/// `plan_release` — the pure planner `darkmux swap` and the crew dispatch
/// preflight are converging on — executed via the real `LmsHost`/`MacProbe`
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
        let opts = AcquireOpts { intent: CallerIntent::Auto, scope: AcquireScope::Additive };
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
                    // free-then-load ordering contract), matching the style
                    // of `swap::swap`'s own unload-then-load logging.
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

// ─── crew validation (review-owned seat requirements) ───────────────────

/// The review's validated seat set — what [`validate_review_crew`] hands
/// back. `verify` is the OPTIONAL fourth seat (#1260): when a crew declares
/// `review-verify` (exactly one staffing, like the judge), every
/// double-confirmed finding gets one adjudication call after pass-2; a crew
/// without it behaves byte-identically to today.
#[derive(Debug)]
pub struct ReviewSeats<'a> {
    pub probes: &'a Vec<ResolvedSeatStaffing>,
    pub judge: &'a ResolvedSeatStaffing,
    pub verify: Option<&'a ResolvedSeatStaffing>,
}

/// Validate `crew` carries what the review needs: seat `"review-probe"`
/// with >= 1 staffing, seat `"review-judge"` with EXACTLY 1 staffing, and
/// — when declared — seat `"review-verify"` with EXACTLY 1 staffing
/// (#1260; the seat is optional, its shape is not).
/// `resolve_crew` (packet 1) validates the crew schema is well-formed and
/// every model resolvable; it deliberately does NOT know about
/// pipeline-specific seat requirements — that's this function's job, and
/// it runs at review start so a misconfigured crew fails loud before any
/// dispatch spends a token.
///
/// `pub` (not private) since #1222 Phase B packet 7 review round: the
/// `review-bench --review` preflight (`darkmux_lab::lab::review_bench::
/// resolve_review_ctx`) calls this directly, ahead of `run_review`'s own
/// internal call, so a misconfigured crew fails at bench START (before the
/// per-case loop even begins) rather than at the first case's dispatch.
pub fn validate_review_crew(crew: &ResolvedCrew) -> Result<ReviewSeats<'_>> {
    let probes = crew
        .seats
        .get("review-probe")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "darkmux: crew \"{}\" is missing seat \"review-probe\" (the review \
                 review needs >= 1 staffing) — add one under crews.\"{}\".seats.\"review-probe\"",
                crew.name,
                crew.name
            )
        })?;
    let judges = crew.seats.get("review-judge").ok_or_else(|| {
        anyhow!(
            "darkmux: crew \"{}\" is missing seat \"review-judge\" (the review \
             review needs exactly 1 staffing)",
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
    let verify = match crew.seats.get("review-verify") {
        None => None,
        Some(v) if v.len() == 1 => Some(&v[0]),
        Some(v) => bail!(
            "darkmux: crew \"{}\" seat \"review-verify\" must have EXACTLY 1 staffing \
             when declared (got {}) — the adjudication seat is single, like \"review-judge\" \
             (#1260)",
            crew.name,
            v.len()
        ),
    };
    Ok(ReviewSeats { probes, judge: &judges[0], verify })
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
pub fn dedup_flags(flags: Vec<ProbeFlag>, diff: &str) -> (Vec<ProbeFlag>, DedupStats) {
    let raw = flags.len();
    struct Survivor {
        bundle_id: String,
        family: &'static str,
        anchor: Option<String>,
        symbols: std::collections::BTreeSet<String>,
    }
    let mut survivors: Vec<Survivor> = Vec::new();
    let mut out: Vec<ProbeFlag> = Vec::new();
    for mut f in flags {
        let anchor = extract_new_side_anchor(&f.charge_text, diff);
        let family = mechanism_family(&f.charge_text);
        let symbols = referenced_symbols(&f.charge_text);
        // First survivor (input order) satisfying the full AND-predicate.
        let target = survivors.iter().position(|s| {
            s.bundle_id == f.bundle_id
                && s.family == family
                && anchor.is_some()
                && s.anchor == anchor
                && !symbols.is_empty()
                && !s.symbols.is_disjoint(&symbols)
        });
        // `survivors` and `out` are pushed together, so index `i` addresses
        // the same finding in both.
        match target {
            Some(i) => {
                survivors[i].symbols.extend(symbols);
                // AGGREGATE, never discard (#1299 MUST_FIX): fold the
                // absorbed finding's framing into the survivor so a rendered
                // finding shows BOTH. The safety net — even a residual false
                // cut degrades to "one bullet, two framings," never a
                // vanished defect.
                out[i].also_flagged.push(f.charge_text);
                out[i].also_flagged.append(&mut f.also_flagged);
            }
            None => {
                f.anchor = anchor.clone();
                survivors.push(Survivor {
                    bundle_id: f.bundle_id.clone(),
                    family,
                    anchor,
                    symbols,
                });
                out.push(f);
            }
        }
    }
    let deduped = out.len();
    (out, DedupStats { raw, deduped })
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

/// Everything [`run_review`]/[`run_judge_only`] need beyond the injected
/// `chat`/`cycler`. Role-prompt resolution (`review-probe.md` /
/// `review-judge.md`) is the caller's job — `darkmux-lab` already depends
/// on `darkmux-crew`, but pulling role-manifest resolution INTO this
/// module would couple the pure pipeline to `darkmux_crew::loader`'s
/// filesystem/embedded-role search order for no benefit the caller
/// couldn't provide more simply.
pub struct ReviewInputs<'a> {
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
    /// unchanged. Production callers (`darkmux pr-review run`, packet 5)
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

/// One probe draw, retried once on empty content, then skipped (empty
/// `content` in the return) — never recorded as a flag. A dispatch-level
/// `Err` propagates immediately (the shared single-shot primitive already
/// carries its own backoff/retry — a second-guessing retry here would be
/// redundant AND would hide a real infra problem behind a "skipped" label).
///
/// (#1260) The `u64` is the tokens billed across EVERY attempt this draw
/// made, returned regardless of whether the content came back empty. Hosted
/// reasoning models legitimately burn the full completion budget thinking
/// and return empty content (see `dispatch_internal`'s reasoning-guillotine
/// lesson) — that spend is REAL and billed, so the caller must bill it into
/// the remote bucket + member accounting even on the empty (`None`) outcome,
/// exactly as the judge/verify retry paths bill both attempts (`t1 + t2`).
fn probe_one_draw(
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
    endpoint: Option<&ModelEndpoint>,
) -> Result<(Option<String>, u64, Option<String>)> {
    let mut tokens = 0u64;
    let mut served: Option<String> = None;
    for _ in 0..2 {
        let call = ChatCall {
            model,
            system,
            user,
            temperature: PROBE_TEMPERATURE,
            max_tokens,
            endpoint,
        };
        let reply = chat(&call)?;
        tokens += reply.total_tokens.unwrap_or(0);
        // (#1300 QA follow-up) LMStudio's response ALSO carries a `model`
        // field (it's OpenAI-compatible) — gate on `endpoint.is_some()` so a
        // local seat's `served_model` stays `None` by construction, never by
        // coincidence of what LMStudio happens to echo back. `lms ps` is the
        // only ground truth for local dispatch; the response body is not.
        served = if endpoint.is_some() { reply.model.clone() } else { None };
        let trimmed = reply.content.trim();
        if !trimmed.is_empty() {
            return Ok((Some(trimmed.to_string()), tokens, served));
        }
    }
    Ok((None, tokens, served))
}

/// One probe seat's dispatch — a `review.step` pair (`step_id =
/// "probe:<staffing-name>"`, #1247 Part 1) brackets the seat's whole
/// draw loop so a live ticker sees per-seat progress inside a multi-seat
/// probe phase, not just the phase-level aggregate `probe_phase` records.
///
/// (#1260) A REMOTE staffing differs in exactly three ways:
/// - its calls carry the endpoint (routed through the hosted dialect by
///   the caller's `chat` closure) and draw from the probe stage's shared
///   remote token `bucket` — an exhausted bucket skips the remaining
///   remote draws (counted, named in the envelope);
/// - a dispatch-level failure (after the transport's bounded retries) is
///   a WARNING + reduced coverage, not a run abort — the remaining seats
///   and the judge still run (`warnings` carries the named reason). Local
///   seats keep the propagate-and-abort behavior unchanged;
/// - its `review.step` records carry `remote: true` + the endpoint host.
#[allow(clippy::too_many_arguments)]
fn dispatch_probe_staffing(
    s: &ResolvedSeatStaffing,
    bundles: &[BundleInput],
    inputs: &ReviewInputs,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
    flags: &mut Vec<ProbeFlag>,
    guard: &mut ReviewRunGuard<'_>,
    bucket: &mut RemoteBucket,
    warnings: &mut Vec<String>,
) -> Result<MemberRecord> {
    let identifier = seat_identifier(&s.pm);
    let endpoint = seat_endpoint(&s.pm);
    let endpoint_host = seat_endpoint_host(&s.pm);
    let max_tokens = resolve_seat_max_tokens(s, DEFAULT_PROBE_MAX_TOKENS);
    let selected = select_bundles_for_staffing(bundles, s.selector.as_ref());
    let step_id = format!("probe:{}", s.name);
    let draws_total = selected.len() as u32 * s.k;
    let mut started = json!({
        "step_id": step_id, "kind": "dispatch", "status": "started",
        "items_in": selected.len(), "items_out": 0, "wall_ms": 0,
        "model": identifier, "draws_done": 0, "draws_total": draws_total,
    });
    if let Some(host) = &endpoint_host {
        started["remote"] = true.into();
        started["endpoint"] = host.clone().into();
    }
    guard.step_started(&step_id, "dispatch", started);
    let t0 = Instant::now();
    let mut draws = 0u32;
    let mut tokens = 0u64;
    // (#1300) The FIRST served model reported by any draw's response —
    // remote-only (a local seat's replies never carry `model`); one
    // deployment aliasing to one underlying model for the whole run is the
    // expected shape, so first-seen is representative without needing to
    // detect drift across draws.
    let mut served_model: Option<String> = None;
    let flags_before = flags.len();
    'staffing: for bundle in &selected {
        let user = probe_user_message(inputs.probe_system, bundle);
        for draw in 0..s.k {
            // (#1260) Remote draws gate on the probe stage's shared bucket
            // BEFORE dispatch; a skipped draw is counted, never billed.
            if endpoint.is_some() && !bucket.admit() {
                continue;
            }
            draws += 1;
            // Empty system — Phase A parity (#1256): probe-runner.py's
            // `call_model` sends ONE user-role message, no system message
            // at all. `single_shot::local_chat_body` omits the system
            // entry entirely when it's empty, so this is the wire-level
            // no-system-message behavior, not a system message with blank
            // content.
            match probe_one_draw(chat, &identifier, "", &user, max_tokens, endpoint) {
                // (#1260) EVERY attempt's tokens are billed — including a draw
                // that came back empty after the retry. Hosted reasoning burns
                // the full budget thinking and returns empty; that spend is
                // real, so it lands in the member total AND the remote bucket
                // whether or not a flag was produced. Only a non-empty content
                // becomes a flag.
                Ok((content, tok, served)) => {
                    tokens += tok;
                    if served_model.is_none() {
                        served_model = served;
                    }
                    if endpoint.is_some() {
                        bucket.spend(tok, 1);
                    }
                    if let Some(text) = content {
                        flags.push(ProbeFlag {
                            bundle_id: bundle.id.clone(),
                            fact_family: bundle.fact_family.clone(),
                            member: identifier.clone(),
                            draw,
                            charge_text: text,
                            anchor: None,
                            also_flagged: Vec::new(),
                        });
                    }
                }
                // (#1260) A remote seat's dispatch failure — AFTER the
                // shared transport's bounded 429 retries — degrades to a
                // named warning + reduced coverage; each failed call has
                // already burned the full backoff ladder, so the seat's
                // remaining draws are abandoned rather than retried into
                // the same wall. Local seats keep the abort (an LMStudio
                // failure is a harness problem the operator must see).
                Err(e) if endpoint.is_some() => {
                    warnings.push(format!(
                        "remote probe seat \"{}\" ({identifier}) failed after bounded retries — \
                         remaining draws skipped (reduced coverage): {e}",
                        s.name
                    ));
                    break 'staffing;
                }
                Err(e) => return Err(e),
            }
        }
    }
    let wall_ms = t0.elapsed().as_millis() as u64;
    let flags_produced = flags.len() - flags_before;
    let mut finished = json!({
        "step_id": step_id, "kind": "dispatch", "status": "finished",
        "items_in": selected.len(), "items_out": flags_produced, "wall_ms": wall_ms,
        "model": identifier, "draws_done": draws, "draws_total": draws_total, "tokens": tokens,
    });
    if let Some(host) = &endpoint_host {
        finished["remote"] = true.into();
        finished["endpoint"] = host.clone().into();
    }
    guard.step_finished(&step_id, finished);
    Ok(MemberRecord {
        model: identifier,
        seat: "review-probe".to_string(),
        draws,
        wall_ms,
        total_tokens: tokens,
        remote: endpoint.is_some(),
        endpoint: endpoint_host,
        served_model,
    })
}

/// (#1260) Remote staffings skip the cycler entirely on every path below —
/// there is nothing to load or unload for a seat whose model computes
/// off-box, and minting a residency operation for one would be the #1135
/// class of phantom state.
#[allow(clippy::too_many_arguments)]
fn probe_phase(
    bundles: &[BundleInput],
    probes: &[ResolvedSeatStaffing],
    inputs: &ReviewInputs,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
    cycler: &mut dyn ModelCycler,
    members: &mut Vec<MemberRecord>,
    mode: ExecMode,
    guard: &mut ReviewRunGuard<'_>,
    bucket: &mut RemoteBucket,
    warnings: &mut Vec<String>,
) -> Result<Vec<ProbeFlag>> {
    let mut flags = Vec::new();
    if mode == ExecMode::Parallel {
        for s in probes.iter().filter(|s| !s.pm.is_remote()) {
            cycler.ensure_loaded(&s.pm)?;
        }
        for s in probes {
            members.push(dispatch_probe_staffing(
                s, bundles, inputs, chat, &mut flags, guard, bucket, warnings,
            )?);
        }
        for s in probes.iter().filter(|s| !s.pm.is_remote()) {
            cycler.release(&s.pm)?;
        }
    } else {
        // Sequential (the only other resolved mode by the time this runs —
        // `resolve_mode` never leaves `Auto` unresolved): load member → all
        // its draws → release → next.
        for s in probes {
            if !s.pm.is_remote() {
                cycler.ensure_loaded(&s.pm)?;
            }
            members.push(dispatch_probe_staffing(
                s, bundles, inputs, chat, &mut flags, guard, bucket, warnings,
            )?);
            if !s.pm.is_remote() {
                cycler.release(&s.pm)?;
            }
        }
    }
    Ok(flags)
}

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
    // `passes >= 1` is validated at crew resolution (`resolve_staffing`);
    // clamp defensively so a hand-constructed 0 can never skip pass-1.
    let passes = passes.max(1);

    // Pass-1 — the breadth pass over every alive flag; draws from the pass-1
    // bucket. Always runs.
    let p1 = run_budgeted_pass(
        1,
        budgets.as_deref_mut().map(|b| &mut b.pass1),
        model,
        system,
        prompt,
        max_tokens,
        endpoint,
        chat,
    );
    // (#1300) Cloned rather than moved out of `p1` here — every return site
    // below still needs `p1.record`/`p1.tokens`/etc, and the consensus-loop
    // path may still overwrite this with a later pass's value.
    let mut served_model = p1.served_model.clone();

    // A non-confirmed pass-1 short-circuits identically for EVERY `passes`.
    if p1.record.ruling != JudgeRuling::Confirmed {
        let tier = match p1.record.ruling {
            JudgeRuling::NeedsCheck => Tier::NeedsCheck,
            // false_positive | unparsed | error
            _ => Tier::Archived,
        };
        return JudgeOutcome {
            tier,
            demoted_by_pass2: false,
            tokens: p1.tokens,
            pass1_ms: p1.wall_ms,
            pass2_ms: 0,
            calls: p1.calls,
            dispatch_error: p1.dispatch_error,
            served_model,
            pass1: p1.record,
            pass2: None,
        };
    }

    // `passes: 1` — the confirm stands alone; no confirmation pass.
    if passes == 1 {
        return JudgeOutcome {
            tier: Tier::Confirmed,
            demoted_by_pass2: false,
            tokens: p1.tokens,
            pass1_ms: p1.wall_ms,
            pass2_ms: 0,
            calls: p1.calls,
            dispatch_error: p1.dispatch_error,
            served_model,
            pass1: p1.record,
            pass2: None,
        };
    }

    // Unanimous consensus over confirmation passes `2..=passes`, early-exiting
    // on the first non-confirm. Every confirmation pass draws from the pass-2
    // bucket; totals span them all (#1260 accounting stays honest).
    let mut tokens = p1.tokens;
    let mut calls = p1.calls;
    let mut later_ms = 0u64;
    let mut dispatch_error = p1.dispatch_error;
    let mut last: Option<JudgeRecord> = None;
    let mut demoted = false;
    for pass_no in 2..=passes {
        let pn = run_budgeted_pass(
            pass_no as u8,
            budgets.as_deref_mut().map(|b| &mut b.pass2),
            model,
            system,
            prompt,
            max_tokens,
            endpoint,
            chat,
        );
        tokens += pn.tokens;
        calls += pn.calls;
        later_ms += pn.wall_ms;
        dispatch_error |= pn.dispatch_error;
        if served_model.is_none() {
            served_model = pn.served_model.clone();
        }
        let confirmed = pn.record.ruling == JudgeRuling::Confirmed;
        last = Some(pn.record);
        if !confirmed {
            // One disagreement breaks unanimity — demote and stop (the same
            // early-exit the double-confirm already used at N == 2).
            demoted = true;
            break;
        }
    }
    let tier = if demoted { Tier::NeedsCheck } else { Tier::Confirmed };
    JudgeOutcome {
        served_model,
        pass1: p1.record,
        pass2: last,
        tier,
        demoted_by_pass2: demoted,
        tokens,
        pass1_ms: p1.wall_ms,
        pass2_ms: later_ms,
        calls,
        dispatch_error,
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
fn run_verify_pass(
    model: &str,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    endpoint: Option<&ModelEndpoint>,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
) -> (VerifyRecord, u64, Option<String>) {
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
            // (#1300 QA follow-up) Gated on `endpoint.is_some()` — see
            // `run_judge_pass`'s identical comment; LMStudio's response is
            // also OpenAI-compatible and carries a `model` field.
            let served = if endpoint.is_some() { reply.model.clone() } else { None };
            match parse_verify_ruling(&reply.content) {
                Some((ruling, decisive_evidence, note_for_author)) => (
                    VerifyRecord { ruling, decisive_evidence, note_for_author, seconds, model: model.to_string() },
                    tokens,
                    served,
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
        ),
    }
}

/// One verify adjudication, retried ONCE on [`VerifyRuling::Unparsed`] —
/// the same retry discipline as [`judge_pass_with_retry`]. Returns the
/// surviving record plus token/call accounting for BOTH attempts, plus
/// (#1300) the served model reported by whichever attempt survives.
fn verify_pass_with_retry(
    model: &str,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    endpoint: Option<&ModelEndpoint>,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
) -> (VerifyRecord, u64, u32, Option<String>) {
    let (r1, t1, served1) = run_verify_pass(model, system, prompt, max_tokens, endpoint, chat);
    if r1.ruling == VerifyRuling::Unparsed {
        let (r2, t2, served2) = run_verify_pass(model, system, prompt, max_tokens, endpoint, chat);
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
/// Emits its own `review.step`/`review.ruling` records under `step_id =
/// "verify"` through the same bookend guard (contract 2 — the stage runs
/// inside the run's existing liveness envelope).
#[allow(clippy::too_many_arguments)]
fn run_verify_stage(
    env: &mut ReviewEnvelope,
    judged: &mut [JudgedFlag],
    bundles: &[BundleInput],
    inputs: &ReviewInputs,
    vstaff: &ResolvedSeatStaffing,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
    cycler: &mut dyn ModelCycler,
    guard: &mut ReviewRunGuard<'_>,
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
    let mut started = json!({
        "step_id": "verify", "kind": "dispatch", "status": "started",
        "items_in": docket, "items_out": 0, "wall_ms": 0, "model": identifier,
    });
    if let Some(host) = &endpoint_host {
        started["remote"] = true.into();
        started["endpoint"] = host.clone().into();
    }
    guard.step_started("verify", "dispatch", started);

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
        guard.ruling(json!({
            "bundle_id": j.flag.bundle_id, "stage": "verify",
            "ruling": record.ruling, "seconds": record.seconds,
        }));
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
        served_model,
    });
    env.steps.push(StepRecord {
        step_id: "verify".to_string(),
        kind: "dispatch".to_string(),
        items_in: docket,
        items_out: docket,
        wall_ms,
    });
    let mut finished = json!({
        "step_id": "verify", "kind": "dispatch", "status": "finished",
        "items_in": docket, "items_out": docket, "wall_ms": wall_ms,
        "model": identifier, "tokens": tokens,
    });
    if let Some(host) = &endpoint_host {
        finished["remote"] = true.into();
        finished["endpoint"] = host.clone().into();
    }
    guard.step_finished("verify", finished);

    if let Some(rec) = bucket.record() {
        if rec.skipped_calls > 0 {
            // (#1260, ruling applied) Verify-bucket exhaustion degrades the
            // STAGE, not the run: findings already adjudicated `verified`
            // still post as frontier-verified, and each flag whose
            // adjudication was SKIPPED keeps its `Confirmed` tier WITH the
            // manual-verification marker (recorded per-flag as `Error` in the
            // loop above, honored by `synthesize_review`). The posted review +
            // envelope carry a loud warning naming the exhaustion. It NEVER
            // sets run-level `degenerate` — routing the whole run to
            // "degraded" would discard findings already verified and read as
            // "produced no signal", which is factually false. This matches the
            // sibling verify-dispatch-error path (an inconclusive adjudication
            // keeps the marker, never a silent pass).
            let adjudicated = docket.saturating_sub(rec.skipped_calls as usize);
            env.warnings.push(format!(
                "verify budget exhausted after {adjudicated} of {docket} adjudications — the \
                 remaining {} confirmed finding(s) keep the manual-verification marker (the \
                 per-execution allowance of {} tokens ran out)",
                rec.skipped_calls, rec.max_tokens
            ));
        }
        env.remote_budgets.push(rec);
    }
    Ok(())
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
    guard: &mut ReviewRunGuard<'_>,
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
    guard.step_finished(
        "dedup",
        json!({
            "step_id": "dedup", "kind": "procedural", "status": "finished",
            "items_in": env.raw_flags, "items_out": deduped.len(), "wall_ms": dedup_ms,
        }),
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
                // (the prior behavior) let `review.ruling{pass:2}` records
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
            // (#1266) The decisive later pass's REAL pass number — 2 under
            // the default double-confirm (byte-identical to before), or the
            // demoting/final pass under an N-pass consensus judge.
            guard.ruling(json!({
                "bundle_id": flag.bundle_id, "pass": p2.pass,
                "ruling": p2.ruling, "seconds": p2.seconds,
            }));
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
        model: judge_identifier,
        seat: "review-judge".to_string(),
        // Actual dispatches, unparsed retries included — never fewer calls
        // than the operator paid for.
        draws: judge_calls,
        wall_ms: pass1_ms + pass2_ms,
        total_tokens: judge_tokens,
        remote: judge.pm.is_remote(),
        endpoint: seat_endpoint_host(&judge.pm),
        served_model: judge_served_model,
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

    // (#1260, revised #1329) Judge-stage degeneracy is decided BEFORE the
    // optional verify stage so a run the judge already doomed never spends
    // frontier money on verify (CONSIDER g). Two writers can push a
    // `degen_reasons` entry; `degen_reasons.is_empty()` gates the second on
    // the first, so AT MOST ONE reason string ends up in `env.degenerate` —
    // this is no longer a "combine every reason" accumulator (that was the
    // pre-#1329 shape). The per-flag dispatch-error WARNING below is the
    // channel that stays complete regardless of which (if either) gate
    // fired, so provenance is never silently dropped even when a reason
    // string is superseded:
    //
    //  1. a REMOTE judge whose per-pass token bucket EXHAUSTED (a
    //     load-bearing stage — operator decision, documented in
    //     DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION). Any exhaustion degrades
    //     the run regardless of scale — this IS the deliberate policy.
    //  2. the judge-dead honesty gate: `usable == 0` — NO flag produced a
    //     usable pass-1 ruling (Confirmed/NeedsCheck/FalsePositive), so the
    //     whole judge phase produced no signal worth rendering. This is the
    //     ONE run-level gate for per-flag adjudication failures, and it
    //     covers three causes uniformly: unparsed judge output surviving its
    //     retry, a dead LOCAL judge, and (#1329) a REMOTE judge whose
    //     dispatch failed after bounded retries.
    //
    // (#1329 fix) A REMOTE judge dispatch failure on a MINORITY of flags is
    // already handled honestly at the per-flag level: a pass-1 failure
    // archives just that flag (invisible in the report, present in the
    // envelope); a pass-2 failure demotes just that flag to NeedsCheck
    // (visible, flagged "(no note from the judge)") — never a silent fake
    // confirm either way. The prior code ALSO forced the entire run
    // degenerate on ANY dispatch_errors > 0, which is the asymmetry that
    // produced the bug: a `Confirmed`-then-`Unparsed` pass-2 outcome (same
    // "transient technical failure" class) was already exempt from this
    // check (`judge_dispatch_errors` only counts `Error`, never `Unparsed`)
    // and rendered fine; only the `Error` variant nuked the whole docket. A
    // single flag's timeout among 37 discarded 9 confirmed + 9 needs-check
    // real findings and false-alarmed CI on a fully successful run
    // (darkmux#1329). Fix: dispatch errors are counted and folded into
    // `usable` like every other per-flag outcome — the run only goes
    // degenerate via gate 2 when NO usable signal survived, exactly as
    // `Unparsed` already worked. This is a consistency fix, not new policy.
    //
    // But swinging from "always nukes the run" to "always silent when the
    // run stays green" would trade one honesty gap for another (this repo's
    // doctrine: "no blind runs," loud beats quiet) — the PROBE stage already
    // sets this precedent (a remote probe seat's bounded-retry failure pushes
    // a named `env.warnings` entry: "reduced coverage", never silent). The
    // judge side gets the same treatment: any remote dispatch error is named
    // in `env.warnings` UNCONDITIONALLY, whether or not it also ends up
    // being (or contributing to) the run-level `degenerate` reason.
    let mut degen_reasons: Vec<String> = Vec::new();

    if judge.pm.is_remote() && judge_dispatch_errors > 0 {
        env.warnings.push(format!(
            "remote judge dispatch failed on {judge_dispatch_errors} of {} flag(s) after bounded \
             retries — each affected flag was conservatively archived (if its own pass-1 failed) \
             or demoted to needs-check (if pass-1 confirmed but a later pass failed), never \
             silently confirmed",
            judged.len()
        ));
    }

    if let Some(b) = &judge_budgets {
        if let Some(rec) = b.pass1.record() {
            env.remote_budgets.push(rec);
        }
        if let Some(rec) = b.pass2.record() {
            env.remote_budgets.push(rec);
        }
        let skipped = b.pass1.skipped + b.pass2.skipped;
        if skipped > 0 {
            degen_reasons.push(format!(
                "remote judge token budget exhausted — {skipped} judge call(s) skipped after the \
                 per-execution allowance ({} tokens per stage) ran out; degraded run, never a \
                 silent pass",
                inputs.remote_max_tokens_per_execution
            ));
        }
    }

    let usable = judged
        .iter()
        .filter(|j| {
            matches!(
                j.pass1.ruling,
                JudgeRuling::Confirmed | JudgeRuling::NeedsCheck | JudgeRuling::FalsePositive
            )
        })
        .count();
    if degen_reasons.is_empty() && !judged.is_empty() && usable == 0 {
        if judge.pm.is_remote() && judge_dispatch_errors > 0 {
            degen_reasons.push(format!(
                "remote judge dispatch failed on {judge_dispatch_errors} of {} flag(s) after \
                 bounded retries — degraded run, the affected flag(s) carry no adjudication",
                judged.len()
            ));
        } else {
            degen_reasons.push(format!(
                "judge produced no usable ruling on any of {} flags (all errored/unparsed)",
                judged.len()
            ));
        }
    }

    if !degen_reasons.is_empty() {
        env.degenerate = Some(degen_reasons.join("; "));
    }

    // (#1260) The optional verify stage — one adjudication per confirmed
    // flag, AFTER the double-confirm judge and BEFORE the tier counts so a
    // refutation's demotion lands in the totals. Crews without the seat skip
    // this entirely (byte-identical behavior to today); a run the judge
    // already marked degenerate skips it too (CONSIDER g — no frontier spend
    // on a doomed run).
    if let Some(vstaff) = verify {
        if env.degenerate.is_none() {
            run_verify_stage(&mut env, &mut judged, bundles, inputs, vstaff, chat, cycler, guard)?;
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
    guard.task_finished(&env);
    Ok(env)
}

// ─── the driver ───────────────────────────────────────────────────────────

/// Run the full review: bundles → probe(k draws × seat) → dedup →
/// double-confirm judge → envelope. `chat` performs one single-shot
/// dispatch and returns its reply (the closure owns model/base-URL
/// resolution — tests script it; production wiring calls
/// `darkmux_crew::single_shot::single_shot_chat`). `cycler` loads/releases
/// models around the dispatches (production: [`LmsCycler`]; tests: a
/// recording mock).
///
/// Also starts the run's host telemetry sampler (#1247 doctrine surface) —
/// see [`ReviewRunGuard`]/[`HostTelemetrySampler`] — at the production
/// cadence ([`REVIEW_TELEMETRY_INTERVAL`]) with the real
/// `darkmux_crew::telemetry_sampler::sample_host`.
/// [`run_review_with_telemetry`] is the test-only seam for a faster
/// cadence and an injected sampling function.
pub fn run_review(
    inputs: &ReviewInputs,
    mut chat: impl FnMut(&ChatCall) -> Result<SingleShotReply>,
    cycler: &mut dyn ModelCycler,
    emitter: &mut dyn ReviewEmitter,
) -> Result<ReviewEnvelope> {
    run_review_impl(
        inputs,
        &mut chat,
        cycler,
        emitter,
        REVIEW_TELEMETRY_INTERVAL,
        REVIEW_TELEMETRY_POLL,
        sample_host,
    )
}

/// Test-only seam: identical pipeline to [`run_review`], but with a
/// caller-chosen telemetry cadence AND sampling function, so a scripted
/// test can observe deterministic host-telemetry samples without a
/// multi-second sleep and without shelling to the real macOS-only host
/// commands (hermetic — no subprocess timing to race on a CI runner).
/// No production caller uses this — `run_review` always fixes the cadence
/// at [`REVIEW_TELEMETRY_INTERVAL`] and the sampler at the real
/// `sample_host`.
#[cfg(test)]
fn run_review_with_telemetry(
    inputs: &ReviewInputs,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
    cycler: &mut dyn ModelCycler,
    emitter: &mut dyn ReviewEmitter,
    telemetry_interval: Duration,
    telemetry_poll: Duration,
    sample_fn: fn() -> HostSample,
) -> Result<ReviewEnvelope> {
    run_review_impl(inputs, chat, cycler, emitter, telemetry_interval, telemetry_poll, sample_fn)
}

fn run_review_impl(
    inputs: &ReviewInputs,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
    cycler: &mut dyn ModelCycler,
    emitter: &mut dyn ReviewEmitter,
    telemetry_interval: Duration,
    telemetry_poll: Duration,
    sample_fn: fn() -> HostSample,
) -> Result<ReviewEnvelope> {
    let ReviewSeats { probes, judge, verify } = validate_review_crew(inputs.crew)?;
    let mode = resolve_mode(inputs.mode, probes, judge);

    let t_bundle = Instant::now();
    let bundles = resolve_bundles(inputs);
    let bundle_ms = t_bundle.elapsed().as_millis() as u64;

    let mut env = ReviewEnvelope {
        case_id: inputs.case_id.clone(),
        crew: inputs.crew.name.clone(),
        mode: mode_label(mode).to_string(),
        bundles: bundles.len(),
        // Stamped up front so DEGENERATE envelopes (zero bundles / zero
        // flags) carry the same comparability key as a full run — a
        // Null fingerprint on an early return would make the degenerate
        // record untraceable to its judge config.
        fingerprint: fingerprint(&seat_identifier(&judge.pm), inputs.judge_system),
        // (#1247) The resolved staffing this run actually used, post any
        // caller-applied `--k` override — see `ReviewEnvelope::staffing`.
        staffing: Some(staffing_snapshot(probes, judge, verify, inputs.crew.request_changes)),
        ..Default::default()
    };
    // `review.task` started (#1247 Part 1) — run started: case id, crew
    // name, exec mode, bundle count. From here every emission routes
    // through the bookend guard, which ARMS on this record: an early
    // `?`-return or panic below fires its Drop path (open steps closed with
    // `status: "error"`, then a terminal error task record) so no consumer
    // ever sees an orphaned `started`.
    let mut sink = EmitterSink(emitter);
    let mut guard = ReviewRunGuard::new_with_telemetry(
        &mut sink,
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
    // (#1260) The probe stage's remote token bucket — ONE execution shared
    // by every remote probe staffing (the prosecution pass is one stage).
    // Local staffings never touch it.
    let mut probe_bucket = RemoteBucket::new("probe", inputs.remote_max_tokens_per_execution);
    let mut probe_warnings: Vec<String> = Vec::new();
    let raw_flags = probe_phase(
        &bundles,
        probes,
        inputs,
        chat,
        cycler,
        &mut env.members,
        mode,
        &mut guard,
        &mut probe_bucket,
        &mut probe_warnings,
    )
    .context("review: probe phase")?;
    env.warnings.append(&mut probe_warnings);
    if let Some(rec) = probe_bucket.record() {
        if rec.skipped_calls > 0 {
            // Probe exhaustion is REDUCED COVERAGE, not a degraded run —
            // whatever flags landed before the cap still go to the judge
            // (operator decision on #1260).
            env.warnings.push(format!(
                "remote probe token budget exhausted — {} draw(s) skipped after the \
                 per-execution allowance ({} tokens) ran out; reduced coverage",
                rec.skipped_calls, rec.max_tokens
            ));
        }
        env.remote_budgets.push(rec);
    }
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

    finish_review(env, raw_flags, &bundles, inputs, judge, verify, chat, cycler, &mut guard)
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
    let ReviewSeats { probes, judge, verify } = validate_review_crew(inputs.crew)?;
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
        crew: inputs.crew.name.clone(),
        mode: mode_label(mode).to_string(),
        bundles: bundles.len(),
        // Same up-front stamp as `run_review` — degenerate (zero-flag)
        // envelopes carry the comparability key too.
        fingerprint: fingerprint(&seat_identifier(&judge.pm), inputs.judge_system),
        // (#1247) The resolved staffing this run actually used, post any
        // caller-applied `--k` override — see `ReviewEnvelope::staffing`.
        staffing: Some(staffing_snapshot(probes, judge, verify, inputs.crew.request_changes)),
        ..Default::default()
    };
    // Same guard discipline as `run_review` — see its comment at the
    // matching site.
    let mut sink = EmitterSink(emitter);
    let mut guard = ReviewRunGuard::new(&mut sink, &inputs.case_id, &inputs.crew.name);
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

    finish_review(env, flags, &bundles, inputs, judge, verify, &mut chat, cycler, &mut guard)
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
// `mission_run.rs`'s own migration, #1230 Packet 3). What's NOT knowable
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
// `darkmux pr-review run` creates a real persisted Mission; a lab bench run
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
// `verify_pass_with_retry`, `parse_judge_ruling`, `parse_verify_ruling`,
// `cluster_needs_check`, `mechanism_family`, `judge_prompt`,
// `verify_prompt`) verbatim — only the ORCHESTRATION shape (six sequential
// calls → one declared graph) and the telemetry plumbing (the guard-
// coupled `ReviewRunGuard` can't cross a `run_bounded` worker-thread
// boundary — see `darkmux_crew::step_kinds::StepOutcome`'s doc — so
// per-step telemetry now rides `StepOutcome.flow_records` / direct
// `darkmux_flow::record()` calls instead) changed.

use darkmux_crew::scheduler::run_step_graph;
use darkmux_crew::single_shot::{single_shot_chat, single_shot_chat_hosted, HostedSingleShotRequest, SingleShotRequest};
use darkmux_crew::step_kinds::{StepKind, StepKindRegistry, StepOutcome};
use darkmux_crew::types::{NodeStatus, Step, Task};
use std::sync::Mutex as StdMutex;

/// Everything a review Step kind needs, OWNED (not borrowed) and
/// `Send + Sync` so it can cross the `run_bounded` worker-thread boundary —
/// `ReviewInputs<'a>`'s borrows can't. Built ONCE by the orchestrator
/// (`build_review_graph`) before the graph starts; every step kind holds an
/// `Arc` clone. Mirrors `ReviewInputs` field-for-field, minus the injected
/// `chat`/`cycler` (each step now resolves its own — see `dispatch_chat`
/// and the direct `LmsCycler` construction in each dispatch-shaped step).
pub struct ReviewStepContext {
    pub case_id: String,
    pub crew: ResolvedCrew,
    pub intent_title: String,
    pub intent_body: String,
    pub diff: String,
    pub probe_system: String,
    pub judge_system: String,
    pub verify_system: String,
    pub bundles: Vec<BundleInput>,
    pub remote_max_tokens_per_execution: u64,
    pub timeout_seconds: u32,
}

/// The production dispatch primitive every review step kind below calls —
/// routes on `call.endpoint` exactly like `pr_review.rs::run_dispatch`'s
/// own `chat` closure (contract 1: a consumer routes on what the profile
/// declares, never re-derives its own local/remote judgment). No test-mock
/// injection seam for this specific call (see the module doc above) —
/// matches `mission_run.rs`'s `MissionCoderStepKind`/`MissionWorktreeStepKind`
/// precedent, which likewise call their real primitive (`dispatch::dispatch`/
/// `add_worktree`) directly rather than through an injected closure; the
/// PRESERVED algorithm functions this dispatches into
/// (`judge_one_flag_with_passes`, `verify_pass_with_retry`, `probe_one_draw`)
/// remain fully mock-testable via their own existing `chat: &mut dyn FnMut`
/// parameter — only the NEW graph glue trades that seam for parity with the
/// rest of the Task/Step step-kind family.
fn dispatch_chat(ctx: &ReviewStepContext, call: &ChatCall) -> Result<SingleShotReply> {
    match call.endpoint {
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
    }
}

/// A "step result" companion flow record — the review's own equivalent of
/// `mission_run.rs`'s `emit_step_result` (#1230 Packet 4 sibling
/// convention): one generic action, `kind` distinguishing which review step
/// produced it, free-form `payload` for the rest. Called directly (not via
/// `StepOutcome.flow_records`) so it's usable from inside a step's own
/// internal concurrent loop (judge's bounded worker pool) — a plain
/// `darkmux_flow::record()` call has no non-`Send` state, unlike
/// `ReviewRunGuard` (see the module doc).
fn emit_review_step_result(kind: &str, step_id: &str, case_id: &str, payload: serde_json::Value) {
    let mut full = serde_json::json!({ "step_id": step_id, "kind": kind });
    if let (serde_json::Value::Object(extra), serde_json::Value::Object(base)) = (payload, &mut full) {
        base.extend(extra);
    }
    let _ = darkmux_flow::record(darkmux_flow::FlowRecord {
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
    });
}

// ─── investigate: bundle ────────────────────────────────────────────────

/// Phase "investigate", step 1: hands back the already-resolved bundle list
/// (`ReviewStepContext::bundles` — resolved once by the orchestrator before
/// the graph starts, since bundling needs `&ReviewInputs<'a>`'s borrow,
/// which can't cross into a `'static` step kind). Procedural — no dispatch.
pub struct ReviewBundleStepKind {
    pub ctx: Arc<ReviewStepContext>,
}

impl StepKind for ReviewBundleStepKind {
    fn id(&self) -> &'static str {
        "review.bundle"
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

// ─── investigate: probe (N steps, one per staffed seat) ────────────────

/// Phase "investigate", step 2 of N: ONE probe seat's whole draw loop
/// (bundle × k draws) — genuinely valuable graph-level concurrency (the
/// ONE stage where per-item fan-out is justified, per the redesign brief):
/// different seats plausibly run different models, and gestalt's real wave
/// planner (via `StepKind::residency`) decides which seats can co-reside.
/// Reuses `probe_one_draw`/`probe_user_message`/`select_bundles_for_staffing`/
/// `resolve_seat_max_tokens` VERBATIM — only the surrounding loop shape and
/// telemetry are new.
pub struct ReviewProbeStepKind {
    ctx: Arc<ReviewStepContext>,
    staffing: ResolvedSeatStaffing,
    /// Shared across every probe step in the run — the probe stage's remote
    /// token bucket is ONE execution shared by every remote probe staffing
    /// (unchanged semantics from `probe_phase`'s pre-graph design).
    bucket: Arc<StdMutex<RemoteBucket>>,
    /// Collects every probe seat's `MemberRecord` + any reduced-coverage
    /// warning — read back by the dedup step (whose `depends_on` includes
    /// every probe step, so it runs only after all seats finish) via this
    /// SAME shared handle, avoiding a separate side-channel.
    members: Arc<StdMutex<Vec<MemberRecord>>>,
    warnings: Arc<StdMutex<Vec<String>>>,
    /// `"review.probe:<staffing-name>"` — a distinct registered kind id per
    /// probe seat (the registry maps one kind id to one `StepKind`
    /// instance, and each seat has its own model/selector). `StepKind::id`
    /// must return `&'static str`; this is leaked EXACTLY ONCE at
    /// construction (`ReviewProbeStepKind::new`), never inside `id()`
    /// itself, so a bounded, one-time-per-seat-per-process leak in a
    /// short-lived CLI invocation — never a per-call leak. Fields are
    /// private (construct via `new()`) precisely so `RemoteBucket` (a
    /// crate-private type — see its own doc) never has to be re-exported
    /// just to name this struct's shape.
    kind_id: &'static str,
}

impl ReviewProbeStepKind {
    pub(crate) fn new(
        ctx: Arc<ReviewStepContext>,
        staffing: ResolvedSeatStaffing,
        bucket: Arc<StdMutex<RemoteBucket>>,
        members: Arc<StdMutex<Vec<MemberRecord>>>,
        warnings: Arc<StdMutex<Vec<String>>>,
    ) -> Self {
        let kind_id: &'static str =
            Box::leak(format!("review.probe:{}", staffing.name).into_boxed_str());
        Self { ctx, staffing, bucket, members, warnings, kind_id }
    }
}

impl StepKind for ReviewProbeStepKind {
    fn id(&self) -> &'static str {
        self.kind_id
    }

    fn run(&self, step: &Step, _task: &Task, _input: &std::collections::BTreeMap<String, String>) -> Result<StepOutcome> {
        let s = &self.staffing;
        let identifier = seat_identifier(&s.pm);
        let endpoint = seat_endpoint(&s.pm);
        let endpoint_host = seat_endpoint_host(&s.pm);
        let max_tokens = resolve_seat_max_tokens(s, DEFAULT_PROBE_MAX_TOKENS);
        let selected = select_bundles_for_staffing(&self.ctx.bundles, s.selector.as_ref());

        let t0 = Instant::now();
        let mut flags: Vec<ProbeFlag> = Vec::new();
        let mut draws = 0u32;
        let mut tokens = 0u64;
        let mut served_model: Option<String> = None;
        let mut chat = |call: &ChatCall| dispatch_chat(&self.ctx, call);

        'staffing: for bundle in &selected {
            let user = probe_user_message(&self.ctx.probe_system, bundle);
            for draw in 0..s.k {
                if endpoint.is_some() {
                    let mut bucket = self.bucket.lock().expect("probe bucket mutex poisoned");
                    if !bucket.admit() {
                        continue;
                    }
                }
                draws += 1;
                match probe_one_draw(&mut chat, &identifier, "", &user, max_tokens, endpoint) {
                    Ok((content, tok, served)) => {
                        tokens += tok;
                        if served_model.is_none() {
                            served_model = served;
                        }
                        if endpoint.is_some() {
                            self.bucket.lock().expect("probe bucket mutex poisoned").spend(tok, 1);
                        }
                        if let Some(text) = content {
                            flags.push(ProbeFlag {
                                bundle_id: bundle.id.clone(),
                                fact_family: bundle.fact_family.clone(),
                                member: identifier.clone(),
                                draw,
                                charge_text: text,
                                anchor: None,
                                also_flagged: Vec::new(),
                            });
                        }
                    }
                    Err(e) if endpoint.is_some() => {
                        self.warnings.lock().expect("probe warnings mutex poisoned").push(format!(
                            "remote probe seat \"{}\" ({identifier}) failed after bounded retries — \
                             remaining draws skipped (reduced coverage): {e}",
                            s.name
                        ));
                        break 'staffing;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        let wall_ms = t0.elapsed().as_millis() as u64;

        // (#1355 follow-up) Only record a member when this seat actually
        // dispatched at least once — a seat with zero selected bundles
        // (e.g. a zero-bundle degenerate run) never called out, and
        // `member_summary()`'s "probed by ..." attribution would otherwise
        // credit it with work it didn't do.
        if draws > 0 {
            self.members.lock().expect("probe members mutex poisoned").push(MemberRecord {
                model: identifier.clone(),
                seat: "review-probe".to_string(),
                draws,
                wall_ms,
                total_tokens: tokens,
                remote: endpoint.is_some(),
                endpoint: endpoint_host.clone(),
                served_model,
            });
        }
        emit_review_step_result(
            "review.probe",
            &step.id,
            &self.ctx.case_id,
            json!({
                "staffing": s.name, "model": identifier, "items_in": selected.len(),
                "items_out": flags.len(), "draws": draws, "wall_ms": wall_ms, "tokens": tokens,
                "remote": endpoint.is_some(), "endpoint": endpoint_host,
            }),
        );

        let output = serde_json::to_string(&flags).context("serializing probe flags")?;
        Ok(StepOutcome { output, flow_records: Vec::new() })
    }

    fn residency(&self, _step: &Step, _task: &Task) -> Option<darkmux_gestalt::Placement> {
        if self.staffing.pm.is_remote() {
            return None;
        }
        // (#1360 follow-up) A seat whose selector matches zero of the
        // review's bundles never dispatches at all (`run()`'s own
        // `select_bundles_for_staffing` call would come back empty too) —
        // declaring a residency need here would make `ensure_wave_loaded`
        // load (or fail loud trying to load) a model this step will never
        // actually use. Both inputs are already fully known before any step
        // runs (`ctx.bundles` is fixed at graph-build time), so this mirrors
        // `run()`'s own check rather than guessing.
        if select_bundles_for_staffing(&self.ctx.bundles, self.staffing.selector.as_ref()).is_empty() {
            return None;
        }
        let n_ctx = self.staffing.pm.n_ctx?;
        let identifier = darkmux_gestalt::namespaced_identifier(&self.staffing.pm.id, self.staffing.pm.identifier.as_deref());
        Some(darkmux_gestalt::Placement {
            model_key: self.staffing.pm.id.clone(),
            identifier,
            min_ctx: n_ctx,
            seat: format!("review-probe:{}", self.staffing.name),
        })
    }
}

// ─── investigate: dedup (terminal step of the phase) ────────────────────

/// Phase "investigate", terminal step: `depends_on` every probe step, reads
/// back each one's flags, concatenates, and calls `dedup_flags` VERBATIM
/// (the mechanism-family keying + anchor-based matching — explicitly
/// preserved, unchanged). Its OWN `StepOutcome.output` IS the phase's
/// observable artifact: "what's the review forming to be."
pub struct ReviewDedupStepKind {
    pub ctx: Arc<ReviewStepContext>,
}

impl StepKind for ReviewDedupStepKind {
    fn id(&self) -> &'static str {
        "review.dedup"
    }

    fn run(&self, step: &Step, _task: &Task, input: &std::collections::BTreeMap<String, String>) -> Result<StepOutcome> {
        let t0 = Instant::now();
        let mut raw: Vec<ProbeFlag> = Vec::new();
        for text in input.values() {
            let flags: Vec<ProbeFlag> =
                serde_json::from_str(text).context("deserializing a probe step's flags")?;
            raw.extend(flags);
        }
        let raw_count = raw.len();
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
pub struct ReviewJudgeStepKind {
    pub ctx: Arc<ReviewStepContext>,
    pub judge: ResolvedSeatStaffing,
    /// (#1354 follow-up) The same shared accumulator `ReviewProbeStepKind`
    /// writes its `MemberRecord`s to — one collector for every dispatching
    /// step kind, merged into `shared_env` once `run_step_graph` returns.
    pub members: Arc<StdMutex<Vec<MemberRecord>>>,
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

        for chunk in deduped.chunks(concurrency) {
            std::thread::scope(|scope| {
                for (offset, flag) in chunk.iter().enumerate() {
                    let bundle = self.ctx.bundles.iter().find(|b| b.id == flag.bundle_id);
                    let code = bundle.map(|b| b.code.as_str()).unwrap_or_default();
                    let facts: &[String] = bundle.map(|b| b.facts.as_slice()).unwrap_or_default();
                    let prompt = judge_prompt(&self.ctx.intent_title, &self.ctx.intent_body, code, facts, &flag.charge_text);
                    let index = {
                        // deterministic global index for this flag — the
                        // running count of already-scheduled flags plus this
                        // chunk's offset, so output order matches `deduped`
                        // order regardless of thread completion order.
                        let done = results.lock().expect("judge results mutex poisoned").len();
                        done - done.min(offset) + offset
                    };
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

    fn residency(&self, _step: &Step, _task: &Task) -> Option<darkmux_gestalt::Placement> {
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

// ─── report: verify ──────────────────────────────────────────────────────

/// Phase "report", step 1: internally loops over judge-confirmed flags only
/// — an empty confirmed set (judge came back degenerate, or every flag was
/// needs-check/archived) means this loop runs zero times, a normal outcome
/// of a for-each loop given an empty input, never a structural special case.
/// This makes the historical "verify only runs when judge isn't degenerate"
/// bug (a shared judge/verify graph previously let verify fire on
/// judge-doomed runs) structurally impossible: verify's OWN `depends_on` is
/// `judge`, and it only ever iterates `judge`'s CONFIRMED output — there is
/// no separate "is the run degenerate" gate to forget. Reuses
/// `verify_prompt`/`verify_pass_with_retry` VERBATIM.
pub struct ReviewVerifyStepKind {
    pub ctx: Arc<ReviewStepContext>,
    pub verify: Option<ResolvedSeatStaffing>,
    /// (#1354 follow-up) Same shared accumulator as `ReviewJudgeStepKind`'s
    /// — see its doc.
    pub members: Arc<StdMutex<Vec<MemberRecord>>>,
}

impl StepKind for ReviewVerifyStepKind {
    fn id(&self) -> &'static str {
        "review.verify"
    }

    fn run(&self, step: &Step, _task: &Task, input: &std::collections::BTreeMap<String, String>) -> Result<StepOutcome> {
        let judge_output = input.values().next().cloned().unwrap_or_default();
        let mut judged: Vec<JudgedFlag> = if judge_output.is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&judge_output).context("deserializing judged flags")?
        };

        let Some(vstaff) = &self.verify else {
            // Crew declares no `review-verify` seat — byte-identical to
            // today: no dispatch, no records, judged flags pass through.
            let output = serde_json::to_string(&judged).context("serializing judged flags")?;
            return Ok(StepOutcome { output, flow_records: Vec::new() });
        };

        let docket_count = judged.iter().filter(|j| j.tier == Tier::Confirmed).count();
        if docket_count == 0 {
            let output = serde_json::to_string(&judged).context("serializing judged flags")?;
            return Ok(StepOutcome { output, flow_records: Vec::new() });
        }

        let identifier = seat_identifier(&vstaff.pm);
        let endpoint = seat_endpoint(&vstaff.pm);
        let endpoint_host = seat_endpoint_host(&vstaff.pm);
        let max_tokens = resolve_seat_max_tokens(vstaff, DEFAULT_JUDGE_MAX_TOKENS);
        let mut bucket = RemoteBucket::new("verify", self.ctx.remote_max_tokens_per_execution);
        let mut chat = |call: &ChatCall| dispatch_chat(&self.ctx, call);

        let t0 = Instant::now();
        let mut calls = 0u32;
        let mut tokens = 0u64;
        let mut served_model: Option<String> = None;
        for j in judged.iter_mut().filter(|j| j.tier == Tier::Confirmed) {
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
                let bundle = self.ctx.bundles.iter().find(|b| b.id == j.flag.bundle_id);
                let code = bundle.map(|b| b.code.as_str()).unwrap_or_default();
                let facts: &[String] = bundle.map(|b| b.facts.as_slice()).unwrap_or_default();
                let prompt =
                    verify_prompt(&self.ctx.intent_title, &self.ctx.intent_body, code, facts, &j.flag.charge_text);
                let out = verify_pass_with_retry(
                    &identifier,
                    &self.ctx.verify_system,
                    &prompt,
                    max_tokens,
                    endpoint,
                    &mut chat,
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
            emit_review_step_result(
                "review.verify",
                "review-ruling",
                &self.ctx.case_id,
                json!({ "bundle_id": j.flag.bundle_id, "stage": "verify", "ruling": record.ruling, "seconds": record.seconds }),
            );
            if record.ruling == VerifyRuling::Refuted {
                j.tier = Tier::Archived;
                j.demoted_by_verify = true;
            }
            j.verify = Some(record);
        }
        let wall_ms = t0.elapsed().as_millis() as u64;

        emit_review_step_result(
            "review.verify",
            &step.id,
            &self.ctx.case_id,
            json!({
                "items_in": docket_count, "items_out": docket_count, "wall_ms": wall_ms,
                "model": identifier.clone(), "tokens": tokens, "calls": calls,
                "remote": endpoint.is_some(), "endpoint": endpoint_host.clone(), "served_model": served_model.clone(),
            }),
        );

        // (#1354 follow-up) Same gap as `ReviewJudgeStepKind` — this step
        // computed its own real dispatch cost above but never recorded a
        // `MemberRecord`, so a crew with a verify seat still reported no
        // verify attribution in the posted review. (#1355 follow-up) Guarded
        // on `calls > 0` — the two early returns above already skip the "no
        // verify seat" / "zero confirmed docket" cases, but every item in
        // the docket could still hit remote-budget exhaustion (`made: 0`
        // each), leaving `calls == 0` despite a non-empty docket.
        if calls > 0 {
            self.members.lock().expect("members mutex poisoned").push(MemberRecord {
                model: identifier,
                seat: "review-verify".to_string(),
                draws: calls,
                wall_ms,
                total_tokens: tokens,
                remote: endpoint.is_some(),
                endpoint: endpoint_host,
                served_model,
            });
        }

        let output = serde_json::to_string(&judged).context("serializing verified flags")?;
        Ok(StepOutcome { output, flow_records: Vec::new() })
    }

    fn residency(&self, _step: &Step, _task: &Task) -> Option<darkmux_gestalt::Placement> {
        let vstaff = self.verify.as_ref()?;
        if vstaff.pm.is_remote() {
            return None;
        }
        // (#1360 follow-up) Same reasoning as ReviewJudgeStepKind's own
        // residency() — a truly empty bundle set means every upstream
        // step's output is transitively guaranteed empty too, so verify is
        // certain to have nothing confirmed to check.
        if self.ctx.bundles.is_empty() {
            return None;
        }
        let n_ctx = vstaff.pm.n_ctx?;
        let identifier = darkmux_gestalt::namespaced_identifier(&vstaff.pm.id, vstaff.pm.identifier.as_deref());
        Some(darkmux_gestalt::Placement {
            model_key: vstaff.pm.id.clone(),
            identifier,
            min_ctx: n_ctx,
            seat: "review-verify".to_string(),
        })
    }
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
pub struct ReviewSynthesisStepKind {
    pub ctx: Arc<ReviewStepContext>,
    pub env: SharedReviewEnvelope,
    /// (#1341) `gather_inputs` now keys a Step's `input` map by the
    /// DEPENDENCY TASK's id (dependency/data-flow lives at Task level) —
    /// so this step reads its two upstream contributions by TASK id, not
    /// step id.
    pub dedup_task_id: String,
    pub verify_task_id: String,
}

impl StepKind for ReviewSynthesisStepKind {
    fn id(&self) -> &'static str {
        "review.synthesis"
    }

    fn run(&self, step: &Step, _task: &Task, input: &std::collections::BTreeMap<String, String>) -> Result<StepOutcome> {
        let t0 = Instant::now();
        let dedup_output = input.get(&self.dedup_task_id).cloned().unwrap_or_default();
        let verify_output = input.get(&self.verify_task_id).cloned().unwrap_or_default();
        let flags: Vec<ProbeFlag> = if dedup_output.is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&dedup_output).context("deserializing deduped flags")?
        };
        let judged: Vec<JudgedFlag> = if verify_output.is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&verify_output).context("deserializing final judged flags")?
        };

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
/// equivalent of `mission_run.rs`'s `Arc<Mutex<Option<T>>>` result-slot
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
    probe_bucket: Arc<StdMutex<RemoteBucket>>,
}

/// Build the review's complete Task/Step graph across three Phases
/// (investigate / adjudicate / report — see the module doc) PLUS the
/// registry every step kind resolves through — see [`BuiltReviewGraph`].
/// Caller persists `tasks`/`steps`, then runs the graph via
/// [`run_review_graph`].
///
/// `case_id` seeds every Step/Task id so a single mission running multiple
/// PR reviews (unlikely today, but not precluded) never collides.
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
) -> BuiltReviewGraph {
    let mut steps = std::collections::BTreeMap::new();
    let mut tasks = Vec::new();
    let mut phase_id_of_step = std::collections::BTreeMap::new();
    let registry = StepKindRegistry::new();

    let bundle_step_id = "review-bundle-step".to_string();
    let bundle_task_id = "review-bundle-task".to_string();
    tasks.push(Task {
        id: bundle_task_id.clone(),
        phase_id: investigate_phase_id.to_string(),
        description: "resolve review bundles from the diff".to_string(),
        step_ids: vec![bundle_step_id.clone()],
        depends_on: Vec::new(),
        role_id: None,
        profile_name: None,
        workdir: None,
        image: None,
    });
    steps.insert(
        bundle_step_id.clone(),
        Step {
            id: bundle_step_id.clone(),
            task_id: bundle_task_id.clone(),
            kind: "review.bundle".to_string(),
            status: NodeStatus::Planned,
            config: serde_json::Value::Null,
            started_ts: None,
            completed_ts: None,
            output: None,
        },
    );
    let bundle_kind = Arc::new(ReviewBundleStepKind { ctx: ctx.clone() });
    registry.register(bundle_kind.clone()).expect("review.bundle registered once");
    // (#1349) Legacy alias — a `Step.kind` persisted before the funnel->review
    // rename must still resolve if anything ever re-reads it back through a
    // fresh registry (see `StepKindRegistry::register_alias`'s doc).
    registry
        .register_alias("funnel.bundle", bundle_kind)
        .expect("funnel.bundle legacy alias registered once");

    let bucket = Arc::new(StdMutex::new(RemoteBucket::new("probe", ctx.remote_max_tokens_per_execution)));
    // (#1354 follow-up) Named for its original probe-only scope, but now
    // shared with the judge/verify step kinds below too — one accumulator
    // for every dispatching step kind, merged into `shared_env` once
    // `run_step_graph` returns.
    let probe_members = Arc::new(StdMutex::new(Vec::new()));
    let probe_warnings = Arc::new(StdMutex::new(Vec::new()));
    let mut probe_task_ids = Vec::new();
    for (idx, staffing) in probes.iter().enumerate() {
        let step_id = format!("review-probe-{idx}-step");
        let task_id = format!("review-probe-{idx}-task");
        let kind_id = format!("review.probe:{}", staffing.name);
        tasks.push(Task {
            id: task_id.clone(),
            phase_id: investigate_phase_id.to_string(),
            description: format!("probe seat `{}`", staffing.name),
            step_ids: vec![step_id.clone()],
            depends_on: vec![bundle_task_id.clone()],
            role_id: None,
            profile_name: None,
            workdir: None,
            image: None,
        });
        steps.insert(
            step_id.clone(),
            Step {
                id: step_id.clone(),
                task_id: task_id.clone(),
                kind: kind_id.clone(),
                status: NodeStatus::Planned,
                config: serde_json::Value::Null,
                started_ts: None,
                completed_ts: None,
                output: None,
            },
        );
        let kind = Arc::new(ReviewProbeStepKind::new(
            ctx.clone(),
            staffing.clone(),
            bucket.clone(),
            probe_members.clone(),
            probe_warnings.clone(),
        ));
        registry.register(kind.clone()).expect("each probe seat's kind id is unique per staffing name");
        // (#1349) Legacy alias — see the bundle step's registration above.
        registry
            .register_alias(&format!("funnel.probe:{}", staffing.name), kind)
            .expect("each probe seat's legacy funnel alias id is unique per staffing name");
        probe_task_ids.push(task_id);
    }

    let dedup_step_id = "review-dedup-step".to_string();
    let dedup_task_id = "review-dedup-task".to_string();
    tasks.push(Task {
        id: dedup_task_id.clone(),
        phase_id: investigate_phase_id.to_string(),
        description: "dedup probe flags — mechanism-family keying + anchor matching".to_string(),
        step_ids: vec![dedup_step_id.clone()],
        depends_on: probe_task_ids,
        role_id: None,
        profile_name: None,
        workdir: None,
        image: None,
    });
    steps.insert(
        dedup_step_id.clone(),
        Step {
            id: dedup_step_id.clone(),
            task_id: dedup_task_id.clone(),
            kind: "review.dedup".to_string(),
            status: NodeStatus::Planned,
            config: serde_json::Value::Null,
            started_ts: None,
            completed_ts: None,
            output: None,
        },
    );
    let dedup_kind = Arc::new(ReviewDedupStepKind { ctx: ctx.clone() });
    registry.register(dedup_kind.clone()).expect("review.dedup registered once");
    // (#1349) Legacy alias — see the bundle step's registration above.
    registry
        .register_alias("funnel.dedup", dedup_kind)
        .expect("funnel.dedup legacy alias registered once");

    let judge_step_id = "review-judge-step".to_string();
    let judge_task_id = "review-judge-task".to_string();
    tasks.push(Task {
        id: judge_task_id.clone(),
        phase_id: adjudicate_phase_id.to_string(),
        description: "double-confirm judge — internal pass1/pass2 loop over deduped flags".to_string(),
        step_ids: vec![judge_step_id.clone()],
        depends_on: vec![dedup_task_id.clone()],
        role_id: None,
        profile_name: None,
        workdir: None,
        image: None,
    });
    steps.insert(
        judge_step_id.clone(),
        Step {
            id: judge_step_id.clone(),
            task_id: judge_task_id.clone(),
            kind: "review.judge".to_string(),
            status: NodeStatus::Planned,
            config: json!({ "concurrency": judge_concurrency }),
            started_ts: None,
            completed_ts: None,
            output: None,
        },
    );
    let judge_kind = Arc::new(ReviewJudgeStepKind { ctx: ctx.clone(), judge, members: probe_members.clone() });
    registry.register(judge_kind.clone()).expect("review.judge registered once");
    // (#1349) Legacy alias — see the bundle step's registration above.
    registry
        .register_alias("funnel.judge", judge_kind)
        .expect("funnel.judge legacy alias registered once");

    let verify_step_id = "review-verify-step".to_string();
    let verify_task_id = "review-verify-task".to_string();
    tasks.push(Task {
        id: verify_task_id.clone(),
        phase_id: report_phase_id.to_string(),
        description: "verify — adjudicate confirmed findings".to_string(),
        step_ids: vec![verify_step_id.clone()],
        depends_on: vec![judge_task_id],
        role_id: None,
        profile_name: None,
        workdir: None,
        image: None,
    });
    steps.insert(
        verify_step_id.clone(),
        Step {
            id: verify_step_id.clone(),
            task_id: verify_task_id.clone(),
            kind: "review.verify".to_string(),
            status: NodeStatus::Planned,
            config: serde_json::Value::Null,
            started_ts: None,
            completed_ts: None,
            output: None,
        },
    );
    let verify_kind = Arc::new(ReviewVerifyStepKind { ctx: ctx.clone(), verify, members: probe_members.clone() });
    registry.register(verify_kind.clone()).expect("review.verify registered once");
    // (#1349) Legacy alias — see the bundle step's registration above.
    registry
        .register_alias("funnel.verify", verify_kind)
        .expect("funnel.verify legacy alias registered once");

    let synthesis_step_id = "review-synthesis-step".to_string();
    let synthesis_task_id = "review-synthesis-task".to_string();
    tasks.push(Task {
        id: synthesis_task_id.clone(),
        phase_id: report_phase_id.to_string(),
        description: "synthesis — finalize tier counts + needs-check clustering".to_string(),
        step_ids: vec![synthesis_step_id.clone()],
        depends_on: vec![dedup_task_id.clone(), verify_task_id.clone()],
        role_id: None,
        profile_name: None,
        workdir: None,
        image: None,
    });
    steps.insert(
        synthesis_step_id.clone(),
        Step {
            id: synthesis_step_id.clone(),
            task_id: synthesis_task_id,
            kind: "review.synthesis".to_string(),
            status: NodeStatus::Planned,
            config: serde_json::Value::Null,
            started_ts: None,
            completed_ts: None,
            output: None,
        },
    );

    let shared_env: SharedReviewEnvelope = Arc::new(StdMutex::new(ReviewEnvelope {
        case_id: ctx.case_id.clone(),
        bundles: ctx.bundles.len(),
        ..Default::default()
    }));
    let synthesis_kind = Arc::new(ReviewSynthesisStepKind {
        ctx: ctx.clone(),
        env: shared_env.clone(),
        dedup_task_id,
        verify_task_id,
    });
    registry.register(synthesis_kind.clone()).expect("review.synthesis registered once");
    // (#1349) Legacy alias — see the bundle step's registration above.
    registry
        .register_alias("funnel.synthesis", synthesis_kind)
        .expect("funnel.synthesis legacy alias registered once");

    // `step_id -> Task.phase_id`, derived once from `tasks` (each Task
    // already carries both) rather than threaded through every push site
    // above.
    for task in &tasks {
        for step_id in &task.step_ids {
            phase_id_of_step.insert(step_id.clone(), task.phase_id.clone());
        }
    }

    BuiltReviewGraph {
        tasks,
        steps,
        registry,
        shared_env,
        synthesis_step_id,
        phase_id_of_step,
        probe_members,
        probe_warnings,
        probe_bucket: bucket,
    }
}

/// Run the review's complete Task/Step graph via ONE `run_step_graph` call
/// (the module's whole point — see its doc). Runs the host telemetry
/// sampler [`run_review_impl`] always used, but — as of #1349 — does NOT
/// wrap the call in its own task-level liveness bookend. Every production
/// caller of this function (`src/pr_review.rs`'s `run_dispatch`) already
/// invokes it from INSIDE `with_dispatch_bookends`, which opens/closes the
/// canonical `dispatch start`/`dispatch complete`/`dispatch error` record
/// (`darkmux_flow::bookend::BookendGuard`, #1230 Packet 0) around the whole
/// call — the SAME liveness edge #1272 fixed the viewer's running-dispatch
/// surfaces to key on. A second, review-scoped `review.task` bookend here
/// was pure duplication of that outer wrap, not an independent liveness
/// fix, and its competing vocabulary is exactly the "bespoke top-level
/// record instead of the generic mechanism" bug #1349 retires — see
/// `with_dispatch_bookends`'s payload construction in `pr_review.rs` for
/// where this function's former `review.task` payload fields (exec mode,
/// bundle count, confirmed/needs_check/archived, degenerate reason) now
/// ride instead, so no data is lost, only the redundant vocabulary.
/// Assembles the final [`ReviewEnvelope`] from the synthesis step's output
/// merged with the shared cross-cutting state, and returns the COMPLETED
/// `steps` map (status/output/timestamps all reflect the real run) so the
/// caller can persist the final Step records — `darkmux mission status`/the
/// graph lens must show what actually happened, never the pre-run
/// `Planned` snapshot `build_review_graph` produced.
pub fn run_review_graph(
    ctx: &ReviewStepContext,
    crew_name: &str,
    mode: ExecMode,
    fingerprint_val: serde_json::Value,
    staffing: CrewStaffingSnapshot,
    graph: BuiltReviewGraph,
    emitter: &mut dyn ReviewEmitter,
) -> Result<(ReviewEnvelope, std::collections::BTreeMap<String, Step>)> {
    let BuiltReviewGraph {
        tasks,
        mut steps,
        registry,
        shared_env,
        synthesis_step_id,
        probe_members,
        probe_warnings,
        probe_bucket,
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
    );

    // Merge the probe stage's NOW-populated accumulators (every probe step
    // has run by the time `run_step_graph` returns, whether it errored or
    // not) into the shared envelope — this can only happen AFTER the run,
    // not at `build_review_graph` time when they were still empty.
    {
        let mut env = shared_env.lock().expect("shared review envelope mutex poisoned");
        env.members
            .extend(probe_members.lock().expect("probe members mutex poisoned").iter().cloned());
        env.warnings
            .extend(probe_warnings.lock().expect("probe warnings mutex poisoned").iter().cloned());
        if let Some(rec) = probe_bucket.lock().expect("probe bucket mutex poisoned").record() {
            if rec.skipped_calls > 0 {
                env.warnings.push(format!(
                    "remote probe token budget exhausted — {} draw(s) skipped after the \
                     per-execution allowance ({} tokens) ran out; reduced coverage",
                    rec.skipped_calls, rec.max_tokens
                ));
            }
            env.remote_budgets.push(rec);
        }
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
        env
    } else {
        let mut env = shared_env.lock().expect("shared review envelope mutex poisoned").clone();
        if env.degenerate.is_none() {
            env.degenerate = Some(format!(
                "review graph: step(s) errored: {}",
                report.errored.join(", ")
            ));
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
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    // ── fixtures ────────────────────────────────────────────────────

    const DIFF: &str = "--- a/billing.ts\n+++ b/billing.ts\n@@ -1,3 +1,4 @@\n context line\n+const end = start.plus(30)\n+const total = base * rate\n more context\n";

    fn pm(id: &str) -> ProfileModel {
        ProfileModel { id: id.to_string(), n_ctx: Some(32_000), ..Default::default() }
    }

    fn staffing(profile: &str, model: &str, k: u32) -> ResolvedSeatStaffing {
        ResolvedSeatStaffing {
            name: profile.to_string(),
            pm: pm(model),
            k,
            // Default double-confirm — a test needing a different judge depth
            // sets `.passes` on the returned staffing (#1266).
            passes: 2,
            max_tokens: None,
            selector: None,
        }
    }

    fn crew_with(seats: Vec<(&str, Vec<ResolvedSeatStaffing>)>) -> ResolvedCrew {
        let mut m = BTreeMap::new();
        for (k, v) in seats {
            m.insert(k.to_string(), v);
        }
        ResolvedCrew { name: "test-crew".to_string(), seats: m, request_changes: false }
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
            also_flagged: Vec::new(),
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
        assert_eq!(ruling, JudgeRuling::Confirmed);
        assert_eq!(evidence, "the clamp is bypassed");
        assert_eq!(note, "real bug");
    }

    #[test]
    fn parse_judge_ruling_prose_wrapped_still_parses() {
        let text = "Some long reasoning about the code goes here, spanning several\nsentences before the verdict.\n```json\n{\"ruling\": \"false_positive\", \"decisive_evidence\": \"input is clamped upstream\", \"note_for_author\": \"no action needed\"}\n```";
        let (ruling, ..) = parse_judge_ruling(text).expect("parses");
        assert_eq!(ruling, JudgeRuling::FalsePositive);
    }

    #[test]
    fn parse_judge_ruling_needs_check_and_case_insensitive() {
        let text = "```json\n{\"ruling\": \"NEEDS_CHECK\", \"decisive_evidence\": \"outside the bundle\", \"note_for_author\": \"verify manually\"}\n```";
        let (ruling, ..) = parse_judge_ruling(text).expect("parses");
        assert_eq!(ruling, JudgeRuling::NeedsCheck);
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

    /// (#1299 recall guard) Two unanchored flags — no resolvable location —
    /// must NOT collapse even in the same family, because the dedup
    /// predicate requires a shared LOCATION and a shared SYMBOL, and neither
    /// is present. Under the asymmetric objective ("a leaked duplicate beats
    /// a false cut") a missing location keeps findings separate. This
    /// replaces the pre-#1299 family-only collapse, which was the over-cut
    /// path the location/symbol rules close.
    #[test]
    fn dedup_no_location_no_symbol_flags_do_not_collapse() {
        let flags = vec![
            flag("b1", "member-a", 0, "This is a null pointer risk on the branch."),
            flag("b1", "member-b", 0, "A null value can reach this path unchecked."),
        ];
        let (deduped, stats) = dedup_flags(flags, DIFF);
        assert_eq!(
            stats.deduped, 2,
            "no anchor + no symbol → no location/symbol overlap → both survive (recall-safe)"
        );
        assert!(deduped[0].anchor.is_none());
        assert!(deduped[1].anchor.is_none());
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
    /// classified "tenant", "covenant", and "finance" as `null/bounds` (all
    /// contain "nan"), so two DISTINCT unanchored charges on a billing
    /// corpus keyed identically and one real defect was silently dropped
    /// in dedup. Word-boundary matching must not fire on those words.
    #[test]
    fn mechanism_family_does_not_substring_match_inside_words() {
        assert_eq!(
            mechanism_family("The tenant covenant check is skipped for finance accounts."),
            "other",
            "'tenant'/'covenant'/'finance' must not classify as null/bounds"
        );
        // The real keywords still classify as whole tokens.
        assert_eq!(mechanism_family("A null value reaches this branch."), "null/bounds");
        assert_eq!(mechanism_family("NaN propagates into the total."), "null/bounds");
        assert_eq!(mechanism_family("None is returned on the error path."), "null/bounds");
        // Punctuation-adjacent tokens still match (tokenizer strips it).
        assert_eq!(mechanism_family("Uses `Date.now()` for the cutoff."), "timezone/ambient-time");
        // "nonexistent" must not token-match "none".
        assert_eq!(mechanism_family("References a nonexistent column."), "other");
    }

    /// Two unanchored flags on the SAME bundle whose charges describe
    /// genuinely different mechanisms must both survive dedup — the
    /// substring bug collapsed them (both misclassified `null/bounds`) and
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

    // ── #1299: symbol extraction + the #396 production case ───────────

    #[test]
    fn referenced_symbols_extracts_code_identifiers_not_prose() {
        // camelCase, PascalCase, snake_case, and call sites are symbols;
        // plain English words (even in backticks) are NOT.
        let s = referenced_symbols(
            "The `docFileEntry` from FinancialStatement uses doc_file_entry and calls record(x).",
        );
        assert!(s.contains("docfileentry"), "camelCase is a symbol");
        assert!(s.contains("financialstatement"), "PascalCase is a symbol");
        assert!(s.contains("doc_file_entry"), "snake_case is a symbol");
        assert!(s.contains("record"), "a call site `record(` is a symbol");
        // Plain lowercase prose words are excluded — no false symbols that
        // could over-collapse two unrelated bugs.
        assert!(!s.contains("the"));
        assert!(!s.contains("from"));
        assert!(!s.contains("uses"));
        assert!(!s.contains("calls"));
        // A bare lowercase word not followed by `(` is not a symbol.
        assert!(referenced_symbols("the value is dropped").is_empty());
    }

    // The #396 diff — the new-side lines every golden charge quotes so its
    // anchor resolves to a real site.
    const DIFF_396: &str = "--- a/src/domain/extraction/financialStatementSpec.ts\n+++ b/src/domain/extraction/financialStatementSpec.ts\n@@ -10,2 +10,3 @@\n ctx\n+  if (isInThousands) recordDerived(value * 1000)\n--- a/src/services/ihsService.ts\n+++ b/src/services/ihsService.ts\n@@ -20,2 +20,10 @@\n ctx\n+  const docFileEntry = bankStatements[idx]\n+  const docFileEntry = invoices[idx]\n+  const docFileEntry = epfFiles[idx]\n+  const docFileEntry = payslips[idx]\n+  const docFileEntry = financialStatements[idx]\n+  writeDocumentInstance(docFileEntry)\n+  provenance.incorporatedDate = record.date\n";

    const SPEC_FILE: &str = "src/domain/extraction/financialStatementSpec.ts";
    const IHS_FILE: &str = "src/services/ihsService.ts";

    /// The 9 "confirmed" #396 findings — 3 distinct bugs stated many ways.
    fn flags_396() -> Vec<ProbeFlag> {
        vec![
            // Bug A — isInThousands drops the provenance source field. Three
            // restatements, all quoting the SAME recordDerived site.
            flag(SPEC_FILE, "gpt-4o", 0, "`recordDerived(value * 1000)` in the isInThousands branch drops the provenance source field."),
            flag(SPEC_FILE, "gpt-4o", 1, "`recordDerived(value * 1000)` is called unconditionally, losing the source mapping — a provenance defect."),
            flag(SPEC_FILE, "gpt-4o", 2, "`recordDerived(value * 1000)` records the derived value but omits the provenance source field."),
            // Bug B — docFileEntry undefined / out-of-bounds before
            // writeDocumentInstance. Five branches, five DISTINCT sites.
            flag(IHS_FILE, "gpt-4o", 0, "`docFileEntry = bankStatements[idx]` can be undefined before writeDocumentInstance — out of bounds on an empty array."),
            flag(IHS_FILE, "gpt-4o", 1, "`docFileEntry = invoices[idx]` may be undefined; the index can exceed the array length."),
            flag(IHS_FILE, "gpt-4o", 2, "`docFileEntry = epfFiles[idx]` is out of bounds when epfFiles is empty; undefined reaches writeDocumentInstance."),
            flag(IHS_FILE, "gpt-4o", 3, "`docFileEntry = payslips[idx]` — index-based selection can return undefined for the payslips branch."),
            flag(IHS_FILE, "gpt-4o", 4, "`docFileEntry = financialStatements[idx]` can be undefined / out of bounds in the financialStatements branch before writeDocumentInstance."),
            // Bug C — incorporatedDate recorded under the wrong field name.
            // Same FILE as B, but a DIFFERENT bug (provenance, not bounds).
            flag(IHS_FILE, "gpt-4o", 5, "`incorporatedDate` is recorded under the wrong field name, and there is no write-gate."),
        ]
    }

    /// The #396 golden case. Recall guards are HARD asserts; the exact
    /// collapse count is NOT pinned (the asymmetric objective — "a leaked
    /// duplicate beats a false cut"), only bounded to a range.
    #[test]
    fn dedup_396_collapses_duplicates_but_keeps_the_three_bugs_separate() {
        let (deduped, stats) = dedup_flags(flags_396(), DIFF_396);
        assert_eq!(stats.raw, 9);

        // HARD — Bug A's three same-site restatements collapse to ONE.
        let a: Vec<&ProbeFlag> = deduped.iter().filter(|f| f.bundle_id == SPEC_FILE).collect();
        assert_eq!(a.len(), 1, "Bug A (isInThousands provenance) collapses to one finding");
        assert_eq!(mechanism_family(&a[0].charge_text), "provenance/sibling");

        // HARD — every docFileEntry SITE survives (five distinct branches):
        // same symbol at different locations is NOT collapsed (recall).
        let b: Vec<&ProbeFlag> = deduped
            .iter()
            .filter(|f| {
                f.bundle_id == IHS_FILE && referenced_symbols(&f.charge_text).contains("docfileentry")
            })
            .collect();
        assert_eq!(b.len(), 5, "every docFileEntry branch keeps its own finding — no site hidden");
        let sites: std::collections::BTreeSet<Option<String>> =
            b.iter().map(|f| f.anchor.clone()).collect();
        assert_eq!(sites.len(), 5, "the five docFileEntry findings anchor to five distinct sites");
        assert!(
            b.iter().all(|f| mechanism_family(&f.charge_text) == "null/bounds"),
            "Bug B is the null-safety/bounds family"
        );

        // HARD (the recall guard) — Bug C is PRESENT, exactly once, and is
        // NOT merged into Bug B: different family AND different symbol, same
        // file notwithstanding.
        let c: Vec<&ProbeFlag> = deduped
            .iter()
            .filter(|f| referenced_symbols(&f.charge_text).contains("incorporateddate"))
            .collect();
        assert_eq!(c.len(), 1, "Bug C (incorporatedDate provenance) is present, exactly once");
        assert!(
            !referenced_symbols(&c[0].charge_text).contains("docfileentry"),
            "Bug C must not carry Bug B's symbol"
        );
        assert_eq!(
            mechanism_family(&c[0].charge_text),
            "provenance/sibling",
            "Bug C is provenance/field-name, a DIFFERENT family than Bug B (null/bounds)"
        );

        // SOFT — some collapse happened (A's three → one) and no over-merge:
        // a range, never a pinned count. 9 raw → 7 here (A collapses, B's
        // five distinct sites and C survive); anything in-range is a PASS.
        assert!(
            (3..=7).contains(&deduped.len()),
            "recall-safe collapse expected in 3..=7, got {}",
            deduped.len()
        );
    }

    /// Recall/negative guard: two GENUINELY DIFFERENT bugs in the same file
    /// and the same mechanism-family, but naming different symbols at
    /// different sites, must both survive — never over-collapsed.
    #[test]
    fn dedup_recall_same_file_family_different_symbol_stay_separate() {
        let diff = "--- a/svc.ts\n+++ b/svc.ts\n@@ -1,2 +1,3 @@\n ctx\n+  const a = parseAmount(row)\n+  const b = docFileEntry[idx]\n";
        let flags = vec![
            flag("svc.ts", "m", 0, "`parseAmount(row)` can return undefined for an empty row."),
            flag("svc.ts", "m", 1, "`docFileEntry[idx]` may be undefined / out of bounds."),
        ];
        let (deduped, stats) = dedup_flags(flags, diff);
        assert_eq!(
            stats.deduped, 2,
            "same file + same null/bounds family but different symbols → two distinct bugs, never merged"
        );
        assert_eq!(deduped.len(), 2);
    }

    /// Same symbol, same family, same file — but at DIFFERENT sites (the
    /// #396 docFileEntry shape). Location divergence keeps them separate:
    /// different sites can be different bugs.
    #[test]
    fn dedup_same_symbol_different_location_stays_separate() {
        let diff = "--- a/svc.ts\n+++ b/svc.ts\n@@ -1,2 +1,3 @@\n ctx\n+  const docFileEntry = a[idx]\n+  const docFileEntry = b[idx]\n";
        let flags = vec![
            flag("svc.ts", "m", 0, "`docFileEntry = a[idx]` can be undefined / out of bounds."),
            flag("svc.ts", "m", 1, "`docFileEntry = b[idx]` can be undefined / out of bounds."),
        ];
        let (_deduped, stats) = dedup_flags(flags, diff);
        assert_eq!(stats.deduped, 2, "same symbol at two different sites stays as two findings");
    }

    /// No resolvable location (the #396 frontier reality — 0/9 anchored)
    /// means NO collapse, even for obvious same-symbol restatements. The
    /// honest outcome is "more duplicates," never an over-merge.
    #[test]
    fn dedup_no_location_never_collapses_even_same_symbol() {
        // A diff that shares NO line with the charges → anchors stay None.
        let diff = "--- a/svc.ts\n+++ b/svc.ts\n@@ -1,1 +1,1 @@\n+ unrelated\n";
        let flags = vec![
            flag("svc.ts", "m", 0, "`docFileEntry` may be undefined here."),
            flag("svc.ts", "m", 1, "`docFileEntry` may be undefined here."),
        ];
        let (deduped, stats) = dedup_flags(flags, diff);
        assert!(deduped.iter().all(|f| f.anchor.is_none()), "no anchor resolved");
        assert_eq!(stats.deduped, 2, "no location → no collapse (recall-safe)");
    }

    /// (#1299 MUST_FIX 2) The adversarial shape the first golden test
    /// MISSED: a provenance / wrong-source bug and a bounds bug share a
    /// line, a symbol, AND an anchor, and the provenance bug's prose even
    /// mentions "array"/"index". It must NOT collapse into the bounds bug —
    /// bare generic tokens no longer classify `null/bounds`, and the
    /// specific `provenance/sibling` family is table-ordered first, so the
    /// two land in different families and stay separate.
    #[test]
    fn dedup_provenance_worded_with_index_does_not_merge_into_bounds() {
        let diff = "--- a/svc.ts\n+++ b/svc.ts\n@@ -1,2 +1,2 @@\n ctx\n+  const docFileEntry = sources[idx]\n";
        let flags = vec![
            flag("svc.ts", "m", 0, "`docFileEntry = sources[idx]` can be undefined / out of bounds when sources is empty."),
            flag("svc.ts", "m", 1, "`docFileEntry = sources[idx]` reads the wrong source at this array index — a provenance mismatch, not a bounds error."),
        ];
        // Same file + same symbol + same anchor, but DIFFERENT families.
        assert_eq!(mechanism_family(&flags[0].charge_text), "null/bounds");
        assert_eq!(mechanism_family(&flags[1].charge_text), "provenance/sibling");
        let (deduped, stats) = dedup_flags(flags, diff);
        assert_eq!(
            stats.deduped, 2,
            "a provenance bug worded with index/array must not merge into a co-located bounds bug"
        );
        assert!(
            deduped.iter().all(|f| f.also_flagged.is_empty()),
            "no false collapse → nothing absorbed"
        );
    }

    /// (#1299 MUST_FIX 2) Bare generic tokens (`index`/`array`/`bounds`) no
    /// longer classify `null/bounds` — they co-occur across unrelated defect
    /// classes. Only anchored phrases do; a provenance finding that also
    /// mentions index/array lands in provenance.
    #[test]
    fn mechanism_family_bare_index_array_bounds_are_not_null_bounds() {
        assert_eq!(mechanism_family("the loop reads the index into the array"), "other");
        assert_eq!(mechanism_family("a bounds concern on this record"), "other");
        assert_eq!(mechanism_family("this is out of bounds on an empty list"), "null/bounds");
        assert_eq!(mechanism_family("the value can be undefined here"), "null/bounds");
        assert_eq!(
            mechanism_family("reads the wrong source at this array index"),
            "provenance/sibling"
        );
    }

    /// (#1299 MUST_FIX 1) Collapse AGGREGATES, never discards: when Bug A's
    /// three same-site restatements collapse, the survivor retains its own
    /// framing AND carries the two absorbed ones in `also_flagged`, so a
    /// rendered finding can show every framing — a residual false cut can
    /// never vanish a defect's description.
    #[test]
    fn dedup_collapse_retains_absorbed_charge_texts() {
        let (deduped, _stats) = dedup_flags(flags_396(), DIFF_396);
        let a = deduped
            .iter()
            .find(|f| f.bundle_id == SPEC_FILE)
            .expect("Bug A survivor present");
        assert_eq!(
            a.also_flagged.len(),
            2,
            "the two absorbed Bug A restatements are retained, not dropped"
        );
        // The retained framings are the OTHER two, distinct from the survivor's own.
        assert!(a.also_flagged.iter().all(|t| *t != a.charge_text));
    }

    #[test]
    fn dedup_396_is_deterministic() {
        let (d1, s1) = dedup_flags(flags_396(), DIFF_396);
        let (d2, s2) = dedup_flags(flags_396(), DIFF_396);
        assert_eq!(s1.deduped, s2.deduped);
        let shape = |d: &[ProbeFlag]| -> Vec<(String, String, Option<String>)> {
            d.iter()
                .map(|f| (f.bundle_id.clone(), f.charge_text.clone(), f.anchor.clone()))
                .collect()
        };
        assert_eq!(shape(&d1), shape(&d2), "same input twice → identical dedup output");
    }

    // ── #1299: needs_check tier clustering ───────────────────────────

    fn nc_flag(bundle_id: &str, charge_text: &str) -> JudgedFlag {
        JudgedFlag {
            flag: flag(bundle_id, "gpt-4o", 0, charge_text),
            pass1: JudgeRecord {
                ruling: JudgeRuling::NeedsCheck,
                decisive_evidence: "e".into(),
                note_for_author: "n".into(),
                pass: 1,
                seconds: 0.0,
            },
            pass2: None,
            tier: Tier::NeedsCheck,
            demoted_by_pass2: false,
            verify: None,
            demoted_by_verify: false,
        }
    }

    #[test]
    fn cluster_needs_check_below_threshold_returns_empty() {
        let judged: Vec<JudgedFlag> = (0..NEEDS_CHECK_CLUSTER_THRESHOLD)
            .map(|_| nc_flag("f.ts", "possible undefined index"))
            .collect();
        assert!(
            cluster_needs_check(&judged).is_empty(),
            "at or below the threshold, needs_check renders raw"
        );
    }

    #[test]
    fn cluster_needs_check_396_caps_and_conserves_every_concern() {
        // ~25 heavily-duplicative needs_check items across files + families.
        let mut judged: Vec<JudgedFlag> = Vec::new();
        for _ in 0..12 {
            judged.push(nc_flag(IHS_FILE, "the partial-update DTO may drop a field"));
        }
        for _ in 0..8 {
            judged.push(nc_flag(IHS_FILE, "`incorporatedDate` recorded under the wrong field name"));
        }
        for _ in 0..5 {
            judged.push(nc_flag(SPEC_FILE, "index may be undefined / out of bounds"));
        }
        // Confirmed flags must be ignored by the clusterer.
        let mut confirmed = nc_flag(IHS_FILE, "a real confirmed bug");
        confirmed.tier = Tier::Confirmed;
        confirmed.pass1.ruling = JudgeRuling::Confirmed;
        judged.push(confirmed);

        let clusters = cluster_needs_check(&judged);
        assert!(!clusters.is_empty(), "25 needs_check > threshold → clustered");

        // NEVER a drop: the clusters' counts sum to the needs_check total.
        let total: usize = clusters.iter().map(|c| c.count).sum();
        assert_eq!(total, 25, "clustering conserves every concern — nothing hidden");

        // Deterministic — same input, identical clusters.
        assert_eq!(cluster_needs_check(&judged), clusters);

        // The rendered bullet names the count + file + mechanism.
        let biggest = clusters.iter().max_by_key(|c| c.count).unwrap();
        let bullet = biggest.bullet();
        assert!(bullet.contains("12 related concerns"), "bullet names the count: {bullet}");
        assert!(bullet.contains(IHS_FILE), "bullet names the file: {bullet}");
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
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::Confirmed);
        assert_eq!(o.pass2.unwrap().ruling, JudgeRuling::Confirmed);
        assert_eq!(o.tier, Tier::Confirmed);
        assert!(!o.demoted_by_pass2);
        assert_eq!(o.calls, 2, "one clean dispatch per pass");
    }

    #[test]
    fn double_confirm_confirm_then_false_positive_demotes_to_needs_check() {
        let mut chat = scripted_chat(RefCell::new(vec![CONFIRM_JSON, FP_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::Confirmed);
        assert_eq!(o.pass2.unwrap().ruling, JudgeRuling::FalsePositive);
        assert_eq!(o.tier, Tier::NeedsCheck, "disagreement demotes, never ships as confirmed");
        assert!(o.demoted_by_pass2);
    }

    #[test]
    fn double_confirm_pass1_needs_check_skips_pass2() {
        let mut chat = scripted_chat(RefCell::new(vec![NEEDS_CHECK_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::NeedsCheck);
        assert!(o.pass2.is_none());
        assert_eq!(o.tier, Tier::NeedsCheck);
        assert!(!o.demoted_by_pass2);
        assert_eq!(o.calls, 1);
        assert_eq!(o.pass2_ms, 0, "no pass-2 dispatch, no pass-2 wall time");
    }

    #[test]
    fn double_confirm_pass1_false_positive_archives_without_pass2() {
        let mut chat = scripted_chat(RefCell::new(vec![FP_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::FalsePositive);
        assert!(o.pass2.is_none());
        assert_eq!(o.tier, Tier::Archived);
    }

    #[test]
    fn double_confirm_unparsed_retries_then_archives() {
        // Two garbage replies: pass-1 attempt, retry — still unparsed.
        let mut chat = scripted_chat(RefCell::new(vec!["no verdict here", "still nothing"]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::Unparsed);
        assert!(o.pass2.is_none());
        assert_eq!(o.tier, Tier::Archived);
        assert!(!o.demoted_by_pass2);
        assert_eq!(o.calls, 2, "the unparsed retry is a real dispatch and is counted");
    }

    #[test]
    fn double_confirm_unparsed_retry_recovers() {
        // First attempt garbage, retry succeeds — the retry's ruling wins.
        let mut chat = scripted_chat(RefCell::new(vec!["garbage", CONFIRM_JSON, CONFIRM_JSON]));
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::Confirmed, "the retry's clean ruling survives");
        assert_eq!(o.pass2.unwrap().ruling, JudgeRuling::Confirmed);
        assert_eq!(o.tier, Tier::Confirmed);
        assert_eq!(o.calls, 3, "pass-1 attempt + retry + pass-2 = three real dispatches");
    }

    // ── passes knob (#1266): single pass (passes: 1) ─────────────────
    // pass-1's ruling IS the tier; no confirmation pass ever runs — the
    // frontier cost lever.

    #[test]
    fn passes_one_confirm_is_confirmed_with_a_single_call() {
        // A counting closure (not `scripted_chat`) so the "invoked exactly
        // once" claim is literal, not inferred from the outcome's own count.
        let mut calls = 0u32;
        let mut chat = |_call: &ChatCall| {
            calls += 1;
            Ok(reply(CONFIRM_JSON))
        };
        let o =
            judge_one_flag_with_passes(1, "prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::Confirmed);
        assert!(o.pass2.is_none(), "passes: 1 never runs a confirmation pass");
        assert_eq!(o.tier, Tier::Confirmed, "the single pass IS the tier directly");
        assert!(!o.demoted_by_pass2);
        assert_eq!(o.calls, 1);
        assert_eq!(o.pass2_ms, 0, "no confirmation pass, no confirmation wall time");
        assert_eq!(calls, 1, "the judge chat closure fired exactly once for this flag");
    }

    #[test]
    fn passes_one_needs_check_tiers_directly() {
        let mut calls = 0u32;
        let mut chat = |_call: &ChatCall| {
            calls += 1;
            Ok(reply(NEEDS_CHECK_JSON))
        };
        let o =
            judge_one_flag_with_passes(1, "prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::NeedsCheck);
        assert_eq!(o.tier, Tier::NeedsCheck, "pass-1's needs_check IS the tier");
        assert!(o.pass2.is_none());
        assert_eq!(calls, 1, "a non-confirmed pass-1 earns no second call under any passes");
    }

    #[test]
    fn passes_one_false_positive_archives_directly() {
        let mut chat = scripted_chat(RefCell::new(vec![FP_JSON]));
        let o =
            judge_one_flag_with_passes(1, "prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::FalsePositive);
        assert_eq!(o.tier, Tier::Archived, "pass-1's false_positive tiers out directly");
        assert!(o.pass2.is_none());
        assert_eq!(o.calls, 1);
    }

    // ── passes knob (#1266): N-pass unanimous consensus (passes: 3) ──
    // A flag stays Confirmed only if EVERY pass that runs confirms it; the
    // first non-confirm demotes and early-exits (N passes is never N× cost).

    #[test]
    fn passes_three_all_confirm_is_confirmed_after_three_calls() {
        let mut calls = 0u32;
        let mut chat = |_call: &ChatCall| {
            calls += 1;
            Ok(reply(CONFIRM_JSON))
        };
        let o =
            judge_one_flag_with_passes(3, "prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.tier, Tier::Confirmed, "unanimous confirms hold the bar");
        assert!(!o.demoted_by_pass2);
        assert_eq!(o.pass1.ruling, JudgeRuling::Confirmed);
        // The decisive `pass2` slot holds the LAST confirmation pass (pass-3),
        // carrying its real pass number.
        let last = o.pass2.as_ref().expect("a later confirmation pass survives into the slot");
        assert_eq!(last.ruling, JudgeRuling::Confirmed);
        assert_eq!(last.pass, 3, "the decisive slot carries the real pass number, not a hardcoded 2");
        assert_eq!(o.calls, 3);
        assert_eq!(calls, 3, "pass-1 + two confirmation passes");
    }

    #[test]
    fn passes_three_final_disagreement_demotes_after_three_calls() {
        // confirm → confirm → false_positive: unanimity breaks on the last
        // pass, so all three ran before the demotion landed.
        let mut calls = 0u32;
        let mut chat = |_call: &ChatCall| {
            calls += 1;
            Ok(reply(if calls < 3 { CONFIRM_JSON } else { FP_JSON }))
        };
        let o =
            judge_one_flag_with_passes(3, "prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.tier, Tier::NeedsCheck, "one disagreement breaks unanimity, never ships confirmed");
        assert!(o.demoted_by_pass2);
        assert_eq!(o.pass2.as_ref().unwrap().ruling, JudgeRuling::FalsePositive);
        assert_eq!(o.calls, 3);
        assert_eq!(calls, 3, "all three passes ran before the late disagreement");
    }

    #[test]
    fn passes_three_early_disagreement_exits_after_two_calls() {
        // confirm → false_positive: the unanimous early-exit fires at pass-2,
        // so pass-3 never runs — N passes is not N× cost.
        let mut calls = 0u32;
        let mut chat = |_call: &ChatCall| {
            calls += 1;
            Ok(reply(if calls < 2 { CONFIRM_JSON } else { FP_JSON }))
        };
        let o =
            judge_one_flag_with_passes(3, "prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.tier, Tier::NeedsCheck);
        assert!(o.demoted_by_pass2);
        assert_eq!(o.calls, 2, "early-exit — the third pass is skipped");
        assert_eq!(calls, 2, "the unanimous rule stops at the first non-confirm");
    }

    // ── passes knob (#1266): passes: 2 IS the historical double-confirm ─

    #[test]
    fn passes_two_reproduces_double_confirm_exactly() {
        // The explicit `passes: 2` path and the `double_confirm_*` wrapper
        // (which delegates passes=2) must agree — confirm→confirm Confirmed,
        // confirm→false_positive demoted.
        let mut chat = scripted_chat(RefCell::new(vec![CONFIRM_JSON, CONFIRM_JSON]));
        let ok =
            judge_one_flag_with_passes(2, "prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(ok.tier, Tier::Confirmed);
        assert_eq!(ok.pass2.as_ref().unwrap().pass, 2);
        assert_eq!(ok.calls, 2);

        let mut chat = scripted_chat(RefCell::new(vec![CONFIRM_JSON, FP_JSON]));
        let demoted =
            judge_one_flag_with_passes(2, "prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(demoted.tier, Tier::NeedsCheck);
        assert!(demoted.demoted_by_pass2);
        assert_eq!(demoted.calls, 2);
    }

    // ── empty probe draw ─────────────────────────────────────────────

    #[test]
    fn probe_one_draw_empty_content_retries_once_then_skips() {
        let mut calls = 0u32;
        let mut chat = |_call: &ChatCall| {
            calls += 1;
            Ok(reply(""))
        };
        let (content, tokens, _served) =
            probe_one_draw(&mut chat, "m", "sys", "user", 100, None).expect("no dispatch error");
        assert!(content.is_none(), "still empty after retry -> skipped, not a flag");
        assert_eq!(calls, 2, "exactly one retry (two total attempts)");
        // (#1260) BOTH empty attempts are billed — a hosted reasoning model
        // that burns its budget thinking and returns empty still spent real
        // tokens the caller must account.
        assert_eq!(tokens, 20, "empty-empty bills both attempts (10 + 10)");
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
        let (content, tokens, _served) = probe_one_draw(&mut chat, "m", "sys", "user", 100, None).unwrap();
        assert_eq!(content.unwrap(), "a real defect description");
        assert_eq!(calls, 2);
        // (#1260) The discarded empty attempt is still billed alongside the
        // recovering one.
        assert_eq!(tokens, 20, "empty-then-recover bills both attempts (10 + 10)");
    }

    #[test]
    fn probe_one_draw_propagates_dispatch_error() {
        let mut chat = |_call: &ChatCall| -> Result<SingleShotReply> { Err(anyhow!("network down")) };
        let err = probe_one_draw(&mut chat, "m", "sys", "user", 100, None).unwrap_err();
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
    fn validate_review_crew_happy_path() {
        let crew = valid_crew();
        let ReviewSeats { probes, judge, verify: _ } = validate_review_crew(&crew).expect("valid");
        assert_eq!(probes.len(), 1);
        assert_eq!(judge.pm.id, "judge-model");
    }

    #[test]
    fn validate_review_crew_missing_probe_seat_rejected() {
        let crew = crew_with(vec![("review-judge", vec![staffing("fast", "j", 1)])]);
        let err = validate_review_crew(&crew).unwrap_err();
        assert!(err.to_string().contains("review-probe"));
    }

    #[test]
    fn validate_review_crew_empty_probe_staffing_rejected() {
        let crew = crew_with(vec![
            ("review-probe", vec![]),
            ("review-judge", vec![staffing("fast", "j", 1)]),
        ]);
        let err = validate_review_crew(&crew).unwrap_err();
        assert!(err.to_string().contains("review-probe"));
    }

    #[test]
    fn validate_review_crew_missing_judge_seat_rejected() {
        let crew = crew_with(vec![("review-probe", vec![staffing("fast", "p", 1)])]);
        let err = validate_review_crew(&crew).unwrap_err();
        assert!(err.to_string().contains("review-judge"));
    }

    #[test]
    fn validate_review_crew_multiple_judge_staffings_rejected() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "p", 1)]),
            ("review-judge", vec![staffing("fast", "j1", 1), staffing("fast", "j2", 1)]),
        ]);
        let err = validate_review_crew(&crew).unwrap_err();
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
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply("a real defect `const end = start.plus(30)`"));
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("review runs");
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
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
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
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("review runs");
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

    /// Recording [`ReviewEmitter`] mock — pushes every emitted record into
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
    impl ReviewEmitter for RecordingEmitter {
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
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
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
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut emitter).expect("review runs");
        assert!(env.degenerate.is_none());

        let actions: Vec<&str> = emitter.records.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(
            actions.first(),
            Some(&"review.task"),
            "the run's first emitted record is the task-started bookend: {actions:?}"
        );
        assert_eq!(
            actions.last(),
            Some(&"review.task"),
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
        // `review.step` record too (the live-run counterpart of the
        // end-of-run summary), plus one seat-level `probe:<name>` record.
        let step_records: Vec<&darkmux_flow::FlowRecord> =
            emitter.records.iter().filter(|r| r.action == "review.step").collect();
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
            emitter.records.iter().filter(|r| r.action == "review.ruling").collect();
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
                r.action == "review.step"
                    && r.payload.as_ref().unwrap()["step_id"] == json!("judge-pass2")
                    && r.payload.as_ref().unwrap()["status"] == json!("started")
            })
            .expect("judge-pass2 started record emitted");
        let first_pass2_ruling_idx = emitter
            .records
            .iter()
            .position(|r| r.action == "review.ruling" && r.payload.as_ref().unwrap()["pass"] == json!(2))
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
        assert!(emitter.records.iter().all(|r| r.source.as_deref() == Some("review")));
    }

    #[test]
    fn flow_emission_degenerate_zero_bundles_emits_only_task_and_bundle_step() {
        let crew = valid_crew();
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "",
            intent_body: "",
            diff: "",
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut emitter = RecordingEmitter::new();
        let mut chat = |_call: &ChatCall| Ok(reply("unused"));
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut emitter).expect("review runs");
        assert!(env.degenerate.is_some());

        // Zero bundles short-circuits before any probe/judge work: task
        // started, bundle step finished, task finished — nothing else.
        let actions: Vec<&str> = emitter.records.iter().map(|r| r.action.as_str()).collect();
        assert_eq!(actions, vec!["review.task", "review.step", "review.task"], "{actions:?}");
        let finished = emitter.records.last().unwrap();
        assert!(
            matches!(finished.level, darkmux_flow::Level::Warn),
            "a degenerate run's task-finished record is Warn, not Info"
        );
        assert_eq!(finished.payload.as_ref().unwrap()["degenerate"].as_str().unwrap(), env.degenerate.unwrap());
    }

    // ── bookend guard (#1247 review round) — no orphaned started records ──

    /// A probe dispatch error propagates out of `run_review` via `?` AFTER
    /// `review.task started`, `probe started`, and `probe:<seat> started`
    /// were emitted. Without the guard those three would dangle forever;
    /// with it, the Drop path must close each open step (innermost-first,
    /// `status: "error"`) and emit a terminal error task record — every
    /// `started` gets a matching terminal event even on the abort path.
    #[test]
    fn bookend_guard_probe_dispatch_error_closes_open_steps_and_emits_terminal_task_record() {
        let crew = valid_crew();
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut emitter = RecordingEmitter::new();
        let mut chat =
            |_call: &ChatCall| -> Result<SingleShotReply> { Err(anyhow!("network down")) };
        let err = run_review(&inputs, &mut chat, &mut cycler, &mut emitter).unwrap_err();
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
                ("review.task".to_string(), "error".to_string()),
                ("review.step".to_string(), "probe".to_string()),
                ("review.step".to_string(), "probe:fast".to_string()),
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
                r.action == "review.task"
                    && r.payload.as_ref().unwrap()["status"].as_str() != Some("started")
            })
            .count();
        assert_eq!(task_terminals, 1);
    }

    /// The reviewer-named scenario: a chat closure that errors mid-JUDGE-
    /// docket. Judge dispatch errors are deliberately swallowed per-flag
    /// (`JudgeRuling::Error` → `Tier::Archived` — one bad call must not
    /// abort the docket), so the run COMPLETES and the terminal task record
    /// is the ordinary `finished` one (degenerate-marked by the judge-dead
    /// honesty gate, since NO flag got a usable ruling). Either way the
    /// invariant under test holds: a terminal task record exists — no
    /// orphaned `started`.
    #[test]
    fn bookend_guard_chat_error_mid_judge_docket_still_yields_terminal_task_record() {
        let crew = valid_crew();
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
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
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut emitter)
            .expect("judge dispatch errors are swallowed per-flag, never abort the run");
        assert!(env.degenerate.is_some(), "the judge-dead honesty gate marks the envelope");

        let last = emitter.records.last().unwrap();
        assert_eq!(last.action, "review.task");
        assert_eq!(
            last.payload.as_ref().unwrap()["status"].as_str(),
            Some("finished"),
            "the run completed cleanly, so the terminal record is finished (degenerate), not the guard's error"
        );
        assert!(last.payload.as_ref().unwrap()["degenerate"].is_string());
        // The judge-pass1 step still closed normally.
        assert!(emitter.records.iter().any(|r| {
            r.action == "review.step"
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
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
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
        let err = run_review(&inputs, &mut chat, &mut cycler, &mut emitter).unwrap_err();
        assert!(err.to_string().contains("release failure"), "{err:#}");

        let last = emitter.records.last().unwrap();
        assert_eq!(last.action, "review.task");
        assert_eq!(last.payload.as_ref().unwrap()["status"].as_str(), Some("error"));
        // Innermost-first: `judge-pass2` (opened last, mid-loop, once its
        // first pass-2 ruling fired) closes before `judge-pass1` (opened
        // first, before the loop).
        let second_to_last = &emitter.records[emitter.records.len() - 2];
        assert_eq!(second_to_last.action, "review.step");
        assert_eq!(
            second_to_last.payload.as_ref().unwrap()["step_id"].as_str(),
            Some("judge-pass1"),
            "judge-pass1 was the outermost open step at the release failure"
        );
        assert_eq!(second_to_last.payload.as_ref().unwrap()["status"].as_str(), Some("error"));
        let third_to_last = &emitter.records[emitter.records.len() - 3];
        assert_eq!(third_to_last.action, "review.step");
        assert_eq!(
            third_to_last.payload.as_ref().unwrap()["step_id"].as_str(),
            Some("judge-pass2"),
            "judge-pass2 was ALSO open (its ruling already fired mid-loop) and closes first, innermost"
        );
        assert_eq!(third_to_last.payload.as_ref().unwrap()["status"].as_str(), Some("error"));
        // The rulings the docket DID produce before the abort are on the
        // stream — partial progress is preserved, not retconned.
        assert!(emitter.records.iter().any(|r| r.action == "review.ruling"));
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

    /// `ReviewRunGuard` owns the sampler's whole-run lifecycle (see its
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
            let mut sink = EmitterSink(&mut emitter);
            let mut guard = ReviewRunGuard::new_with_telemetry(
                &mut sink,
                "case-1",
                "crew-1",
                Duration::from_millis(5),
                Duration::from_millis(2),
                fake_sample,
            );
            guard.task_started(json!({"status": "started"}));
            let env = ReviewEnvelope {
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
                let mut sink = EmitterSink(&mut emitter);
                let mut guard = ReviewRunGuard::new_with_telemetry(
                    &mut sink,
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

    /// End-to-end: a scripted `run_review` (via the test-only
    /// `run_review_with_telemetry` seam) with a fast sampler cadence, an
    /// injected instant fake sampler (hermetic — no real subprocess
    /// timing), and a small sleep per scripted dispatch (so the run's own
    /// wall-clock comfortably exceeds several sample intervals) must show
    /// at least one `telemetry.process` record on the `RecordingEmitter`,
    /// with the same field shape `dispatch_internal`'s sampler already
    /// produces (`category=telemetry, source="process"`) plus this run's
    /// own identity (`session_id=case_id`, `handle=crew name`) — the
    /// convention `review_flow_record` already uses for the `review.*`
    /// action family, so a telemetry record groups with a run's other
    /// records under the same `session_id`.
    #[test]
    fn flow_emission_includes_host_telemetry_when_sampler_cadence_is_fast() {
        let crew = valid_crew();
        let inputs = ReviewInputs {
            case_id: "c-telemetry".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
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
        let env = run_review_with_telemetry(
            &inputs,
            &mut chat,
            &mut cycler,
            &mut emitter,
            Duration::from_millis(5),
            Duration::from_millis(2),
            fake_sample,
        )
        .expect("review runs");
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
                "session_id must match the review's case_id — same convention review_flow_record uses"
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
        // `ReviewInputs` — `run_review` never re-reads a registry, so the
        // snapshot can only ever reflect what it was actually handed.
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 9)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply("a real defect `const end = start.plus(30)`"));
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("review runs");

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
        assert_eq!(snapshot.probes[0].n_ctx, Some(32_000));
        assert_eq!(judge.n_ctx, Some(32_000));

        // The shape `reviews.json` persists — a JSON round trip must
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

    /// (#1266) The judge seat's resolved `passes` (consensus depth) rides
    /// into the envelope's staffing snapshot, so every run is self-describing
    /// about the knob it ran under (the knob-snapshot discipline). A probe
    /// seat that omits `passes` carries the visible default 2.
    #[test]
    fn staffing_snapshot_carries_the_judge_passes_knob() {
        let mut judge = staffing("fast", "judge-model", 1);
        judge.passes = 3; // an N-pass consensus judge
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 2)]),
            ("review-judge", vec![judge]),
        ]);
        let inputs = ReviewInputs {
            case_id: "c-passes".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply("a real defect `const end = start.plus(30)`"));
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("review runs");

        let snapshot = env.staffing.as_ref().expect("staffing snapshot present on a normal run");
        assert_eq!(
            snapshot.judge.as_ref().unwrap().passes,
            3,
            "the judge's resolved consensus depth is snapshotted"
        );
        assert_eq!(
            snapshot.probes[0].passes, 2,
            "a probe seat omitting passes carries the visible default"
        );

        // Survives the JSON round trip `reviews.json` persists.
        let json = serde_json::to_string(&env).expect("envelope serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("envelope parses back");
        assert_eq!(value["staffing"]["judge"]["passes"], json!(3));
        assert_eq!(value["staffing"]["probes"][0]["passes"], json!(2));
    }

    /// (#1302) The crew's `request_changes` flag is snapshotted onto the
    /// envelope's staffing and survives the JSON round trip — so the render
    /// path reads the run's own blocking-vs-advisory choice from its
    /// self-describing artifact. Default `false` is skipped on serialize
    /// (pre-#1302 round-trip), `true` is present.
    #[test]
    fn staffing_snapshot_carries_the_request_changes_flag() {
        let mut crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 2)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        crew.request_changes = true; // opt into the blocking review event
        let inputs = ReviewInputs {
            case_id: "c-rc".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply("a real defect `const end = start.plus(30)`"));
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("review runs");

        let snapshot = env.staffing.as_ref().expect("staffing snapshot present on a normal run");
        assert!(snapshot.request_changes, "the crew's request_changes flag is snapshotted");

        // `true` is serialized; the default `false` is skipped (round-trips
        // pre-#1302 snapshots unchanged).
        let json = serde_json::to_string(&env).expect("envelope serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("envelope parses back");
        assert_eq!(value["staffing"]["request_changes"], json!(true));

        let mut advisory = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 2)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        advisory.request_changes = false; // the default — advisory
        let inputs2 = ReviewInputs { case_id: "c-adv".to_string(), crew: &advisory, ..inputs };
        let mut cycler2 = RecordingCycler::new();
        let env2 = run_review(&inputs2, &mut chat, &mut cycler2, &mut NullEmitter).expect("review runs");
        let json2 = serde_json::to_string(&env2).expect("envelope serializes");
        let value2: serde_json::Value = serde_json::from_str(&json2).expect("envelope parses back");
        assert!(
            value2["staffing"].get("request_changes").is_none(),
            "the advisory default is skipped on serialize"
        );
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
        let env: ReviewEnvelope = serde_json::from_str(legacy).expect("legacy envelope without staffing parses");
        assert!(env.staffing.is_none());
    }

    #[test]
    fn degenerate_zero_bundles_never_silently_passes() {
        let crew = valid_crew();
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "",
            intent_body: "",
            diff: "",
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply("unused"));
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("review runs");
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
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            bundles: None,
        };
        let mut cycler = RecordingCycler::new();
        // Every probe draw comes back empty — retried, then skipped.
        let mut chat = |_call: &ChatCall| Ok(reply(""));
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("review runs");
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
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
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
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("review runs");
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
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
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
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("review runs");
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
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
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
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Parallel,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            bundles: None,
        };
        let flags = vec![flag("billing.ts", "member-a", 0, "`const end = start.plus(30)` off by one")];
        let mut cycler = RecordingCycler::new();
        let mut chat = |_call: &ChatCall| Ok(reply(FP_JSON));
        let env = run_judge_only(flags, &inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        assert_eq!(env.mode, "parallel", "a judge-only re-run of a parallel review keeps its provenance");
    }

    // ── ExecMode auto-resolution (#1230 Packet 1: gestalt wave scheduler) ──

    /// A minimal, valid `WaveSchedule` for [`wave_schedule_to_exec_mode`]'s
    /// pure-projection tests — the wave PARTITIONING itself is already
    /// covered by `darkmux-gestalt`'s own `plan_waves` table tests; this
    /// only pins the wave-count → `ExecMode` mapping this module owns.
    fn schedule_with_waves(n: usize) -> darkmux_gestalt::WaveSchedule {
        let placement = |i: usize| darkmux_gestalt::Placement {
            model_key: format!("m{i}"),
            identifier: format!("darkmux:m{i}"),
            min_ctx: 8_000,
            seat: "probe".to_string(),
        };
        darkmux_gestalt::WaveSchedule {
            waves: (0..n).map(|i| vec![placement(i)]).collect(),
            refusals: Vec::new(),
            mode: darkmux_gestalt::WaveMode::Auto,
            effective_limit_bytes: None,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn wave_schedule_to_exec_mode_one_wave_is_parallel_more_is_sequential() {
        assert_eq!(wave_schedule_to_exec_mode(&schedule_with_waves(0)), ExecMode::Parallel);
        assert_eq!(wave_schedule_to_exec_mode(&schedule_with_waves(1)), ExecMode::Parallel);
        assert_eq!(wave_schedule_to_exec_mode(&schedule_with_waves(2)), ExecMode::Sequential);
        assert_eq!(wave_schedule_to_exec_mode(&schedule_with_waves(3)), ExecMode::Sequential);
    }

    #[test]
    fn resolve_auto_via_waves_empty_placements_is_parallel_without_touching_lms() {
        // No distinct local models (e.g. every probe + the judge are
        // remote) short-circuits to Parallel without any `LmsHost`/
        // `MacProbe` I/O — nothing to co-reside.
        assert_eq!(resolve_auto_via_waves(&[]), ExecMode::Parallel);
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
        assert_eq!(ruling, JudgeRuling::FalsePositive, "the LAST fenced JSON wins, not the first");
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
        assert_eq!(ruling, JudgeRuling::Confirmed);
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
        let o = judge_one_flag("prompt", "judge-model", "sys", 1000, None, None, &mut chat);
        assert_eq!(o.pass1.ruling, JudgeRuling::Confirmed);
        assert_eq!(o.pass2.as_ref().unwrap().ruling, JudgeRuling::Unparsed);
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
    /// flags are discarded (never surfaced, since `run_review` returns
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
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            bundles: None,
        };
        let mut cycler = FailingLoadCycler::new("member-b");
        let mut chat = |_call: &ChatCall| Ok(reply("a real defect `const end = start.plus(30)`"));
        let err = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).unwrap_err();
        assert!(
            err.to_string().contains("probe phase"),
            "run_review wraps the propagated load error with phase context"
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
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Parallel,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
            bundles: None,
        };
        let mut cycler = FailingLoadCycler::new("member-b");
        let mut dispatch_count = 0u32;
        let mut chat = |_call: &ChatCall| {
            dispatch_count += 1;
            Ok(reply("a real defect `const end = start.plus(30)`"))
        };
        let err = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).unwrap_err();
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
        let model = ProfileModel { id: "devstral".to_string(), n_ctx: Some(32768), ..Default::default() };
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
        let model = ProfileModel { id: "devstral".to_string(), n_ctx: Some(32768), ..Default::default() };
        cycler.ensure_loaded(&model).expect("reuse succeeds");
        assert_eq!(env.log(), "", "sufficient ctx already resident — no load/unload issued");
    }

    /// (c) a resident sharing the modelKey that is NOT darkmux-owned (no
    /// `darkmux:` prefix) — operator state. (#1230 Packet 1 cutover — a
    /// deliberate behavior change, see `darkmux_gestalt::planner`'s "Cutover
    /// behavior changes" doc): the cycler no longer hard-blocks around it.
    /// The foreign resident's load configuration is unknown (the #1135
    /// ghost) — never reused, never touched — but darkmux loads its OWN
    /// namespaced copy ALONGSIDE it (absolute namespace ownership, operator
    /// decision 2026-07-10, #1274) instead of refusing outright.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_user_owned_same_model_key_loads_alongside_not_blocked() {
        let env = LmsStubEnv::new(
            r#"[{"identifier":"devstral-manual","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":40960}]"#,
        );
        let mut cycler = LmsCycler;
        let model = ProfileModel { id: "devstral".to_string(), n_ctx: Some(32768), ..Default::default() };
        cycler.ensure_loaded(&model).expect("loads darkmux's own copy alongside the foreign resident");
        let log = env.log();
        assert!(
            log.contains("load devstral --context-length 32768 --identifier darkmux:devstral"),
            "darkmux's own copy loads: {log}"
        );
        assert!(!log.contains("unload"), "the foreign resident is never touched: {log}");
    }

    /// (d) no resident shares the modelKey — plain load, unchanged.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_no_resident_loads_plain() {
        let env = LmsStubEnv::new("[]");
        let mut cycler = LmsCycler;
        let model = ProfileModel { id: "devstral".to_string(), n_ctx: Some(32768), ..Default::default() };
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
            n_ctx: Some(32768),
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
            n_ctx: Some(32768),
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

    /// (#1230 Packet 1 cutover — a deliberate behavior change) Multi-resident,
    /// user-owned listed AHEAD of a darkmux-stale instance: under gestalt's
    /// `decide_residency`, ownership partitions BEFORE position-matching (see
    /// `darkmux_gestalt::planner`'s "Cutover behavior changes" doc — "a
    /// foreign copy listed ahead of a darkmux copy also no longer shadows
    /// it"), so listing order no longer decides the outcome the way the old
    /// review-private `.find()` did. The owned-but-stale instance is found
    /// regardless of position → Reconcile, exactly like the mirror-ordering
    /// case below; the foreign resident is never touched either way.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn lms_cycler_multi_resident_user_owned_first_still_reconciles_owned_stale() {
        let env = LmsStubEnv::new(
            r#"[
                {"identifier":"devstral-manual","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":40960},
                {"identifier":"darkmux:devstral","modelKey":"devstral","status":"loaded","sizeBytes":14000000000,"contextLength":20000}
            ]"#,
        );
        let mut cycler = LmsCycler;
        let model = ProfileModel { id: "devstral".to_string(), n_ctx: Some(32768), ..Default::default() };
        cycler.ensure_loaded(&model).expect("reconciles the owned-but-stale instance regardless of listing order");
        let log = env.log();
        assert!(log.contains("unload darkmux:devstral"), "the owned stale instance reconciles: {log}");
        assert!(!log.contains("unload devstral-manual"), "the foreign resident is never touched: {log}");
        assert!(
            log.contains("load devstral --context-length 32768 --identifier darkmux:devstral"),
            "{log}"
        );
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
        let model = ProfileModel { id: "devstral".to_string(), n_ctx: Some(32768), ..Default::default() };
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
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
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
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("review runs");
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
    /// pass2_ms` (`finish_review`), and the `judge-pass1`/`judge-pass2`
    /// step rows carry those same two values — so their sum must equal the
    /// judge member's wall_ms EXACTLY, not just approximately (both are
    /// derived from the same accumulator variables, so this holds
    /// regardless of real elapsed time).
    #[test]
    fn step_telemetry_judge_steps_sum_equals_judge_member_wall_ms() {
        let crew = valid_crew();
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
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
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("review runs");
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

    /// `ReviewEnvelope` derives `Serialize` only (no `Deserialize`), so a
    /// literal `ReviewEnvelope -> ReviewEnvelope` round trip isn't
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
                    ruling: JudgeRuling::Confirmed,
                    decisive_evidence: "e1".into(),
                    note_for_author: "n1".into(),
                    pass: 1,
                    seconds: 0.5,
                },
                pass2: Some(JudgeRecord {
                    ruling: JudgeRuling::Confirmed,
                    decisive_evidence: "e1b".into(),
                    note_for_author: "n1b".into(),
                    pass: 2,
                    seconds: 0.4,
                }),
                tier: Tier::Confirmed,
                demoted_by_pass2: false,
                verify: None,
                demoted_by_verify: false,
            },
            JudgedFlag {
                flag: flag_needs_check.clone(),
                pass1: JudgeRecord {
                    ruling: JudgeRuling::Confirmed,
                    decisive_evidence: "e2".into(),
                    note_for_author: "n2".into(),
                    pass: 1,
                    seconds: 0.3,
                },
                pass2: Some(JudgeRecord {
                    ruling: JudgeRuling::FalsePositive,
                    decisive_evidence: "e2b".into(),
                    note_for_author: "n2b".into(),
                    pass: 2,
                    seconds: 0.2,
                }),
                tier: Tier::NeedsCheck,
                demoted_by_pass2: true,
                verify: None,
                demoted_by_verify: false,
            },
            JudgedFlag {
                flag: flag_archived.clone(),
                pass1: JudgeRecord {
                    ruling: JudgeRuling::FalsePositive,
                    decisive_evidence: "e3".into(),
                    note_for_author: "n3".into(),
                    pass: 1,
                    seconds: 0.1,
                },
                pass2: None,
                tier: Tier::Archived,
                demoted_by_pass2: false,
                verify: None,
                demoted_by_verify: false,
            },
        ];

        let env = ReviewEnvelope {
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
                    remote: false,
                    endpoint: None,
                    served_model: None,
                },
                MemberRecord {
                    model: "darkmux:judge-model".to_string(),
                    seat: "review-judge".to_string(),
                    draws: 5,
                    wall_ms: 800,
                    total_tokens: 600,
                    remote: false,
                    endpoint: None,
                    served_model: None,
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
                        verified: 0,
            refuted: 0,
fingerprint: fingerprint("darkmux:judge-model", "judge sys"),
            staffing: None,
            warnings: Vec::new(),
            remote_budgets: Vec::new(),
            needs_check_clusters: Vec::new(),
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
    /// `bundler.py`'s bundles carry no such field. The Rust review's
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
        let inputs = ReviewInputs {
            case_id: "c1".to_string(),
            crew: &crew,
            intent_title: "add a feature",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            remote_max_tokens_per_execution: 500_000,
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

    // ═══════════════════════════════════════════════════════════════
    // Remote (endpoint-staffed) seats (#1260/#1177) — routing,
    // provenance, per-execution token buckets, failure semantics
    // ═══════════════════════════════════════════════════════════════

    fn remote_pm(id: &str) -> ProfileModel {
        // No `n_ctx` — endpoint models have no local context (#1282). The
        // URL deliberately carries a deployment PATH so provenance tests can
        // prove only the HOST ever serializes.
        ProfileModel {
            id: id.to_string(),
            endpoint: Some(ModelEndpoint {
                url: Some(
                    "https://myorg.cognitiveservices.azure.com/openai/deployments/gpt-51"
                        .to_string(),
                ),
                api_version: Some("2025-01-01-preview".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn remote_staffing(profile: &str, model: &str, k: u32) -> ResolvedSeatStaffing {
        ResolvedSeatStaffing {
            name: profile.to_string(),
            pm: remote_pm(model),
            k,
            passes: 2,
            max_tokens: None,
            selector: None,
        }
    }

    fn bundle_input(id: &str) -> BundleInput {
        BundleInput {
            id: id.to_string(),
            fact_family: "unscoped".to_string(),
            code: "const x = 1".to_string(),
            probe_code: "const x = 1".to_string(),
            facts: vec![],
            manifest: vec![],
        }
    }

    fn inputs_for<'a>(crew: &'a ResolvedCrew, budget: u64) -> ReviewInputs<'a> {
        ReviewInputs {
            case_id: "remote-case".to_string(),
            crew,
            intent_title: "t",
            intent_body: "",
            diff: DIFF,
            mode: ExecMode::Sequential,
            probe_system: "probe sys",
            judge_system: "judge sys",
            verify_system: "verify sys",
            bundles: None,
            remote_max_tokens_per_execution: budget,
        }
    }

    /// The three remote-seat routing invariants at once: (1) the cycler is
    /// never touched for a remote seat (no load, no release, no namespace
    /// entry); (2) a remote seat's calls carry the endpoint and the BARE
    /// profile model id (never a `darkmux:` identifier — nothing is
    /// resident); (3) member records + the staffing snapshot mark the seat
    /// remote with the endpoint HOST only.
    #[test]
    fn remote_seats_skip_cycler_route_endpoint_and_stamp_host_only_provenance() {
        let crew = crew_with(vec![
            (
                "review-probe",
                vec![staffing("fast", "local-probe", 1), remote_staffing("cloud", "gpt-remote", 1)],
            ),
            ("review-judge", vec![remote_staffing("cloud-judge", "gpt-judge", 1)]),
        ]);
        let inputs = inputs_for(&crew, 500_000);
        let mut cycler = RecordingCycler::new();
        let calls: RefCell<Vec<(String, bool)>> = RefCell::new(Vec::new());
        let mut chat = |call: &ChatCall| {
            calls.borrow_mut().push((call.model.to_string(), call.endpoint.is_some()));
            if call.model == "gpt-judge" {
                Ok(reply(CONFIRM_JSON))
            } else {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            }
        };
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");

        // (1) Cycler saw ONLY the local probe — remote seats have no residency.
        assert!(!cycler.log.is_empty(), "the local seat still cycles");
        assert!(
            cycler.log.iter().all(|e| e.contains("local-probe")),
            "no cycler operation may name a remote model: {:?}",
            cycler.log
        );

        // (2) Local call: namespaced identifier, no endpoint. Remote calls:
        // bare profile id + endpoint.
        let calls = calls.borrow();
        assert!(calls.iter().any(|(m, r)| m == "darkmux:local-probe" && !r));
        assert!(calls.iter().any(|(m, r)| m == "gpt-remote" && *r), "remote probe routes hosted");
        assert!(calls.iter().any(|(m, r)| m == "gpt-judge" && *r), "remote judge routes hosted");

        // (3) Provenance: remote + HOST only, on members and the snapshot.
        let probe = env.members.iter().find(|m| m.model == "gpt-remote").expect("remote member");
        assert!(probe.remote);
        assert_eq!(probe.endpoint.as_deref(), Some("myorg.cognitiveservices.azure.com"));
        let judge = env.members.iter().find(|m| m.seat == "review-judge").unwrap();
        assert!(judge.remote);
        let snap = env.staffing.as_ref().unwrap();
        assert!(snap
            .probes
            .iter()
            .any(|s| s.remote && s.endpoint.as_deref() == Some("myorg.cognitiveservices.azure.com")));
        assert!(snap.judge.as_ref().unwrap().remote);
        let local_snap = snap.probes.iter().find(|s| !s.remote).unwrap();
        assert!(local_snap.endpoint.is_none(), "local seats carry no endpoint field");
        // Never the full deployment path (and with it, never a key).
        let json = serde_json::to_string(&env).unwrap();
        assert!(
            !json.contains("/openai/deployments"),
            "the full deployment URL must never serialize into the envelope"
        );
    }

    /// (#1300) The endpoint's response `model` field — the SERVED model,
    /// which can differ from the requested deployment name on an aliased
    /// Azure deployment — is captured into `MemberRecord.served_model` for
    /// BOTH the probe and judge seats, distinct from `model` (the requested
    /// id, unchanged). A local seat's replies never carry `model` at all.
    #[test]
    fn served_model_captured_distinct_from_requested_on_probe_and_judge() {
        let crew = crew_with(vec![
            ("review-probe", vec![remote_staffing("cloud", "gpt-4o", 1)]),
            ("review-judge", vec![remote_staffing("cloud-judge", "gpt-4o", 1)]),
            // (#1300 QA follow-up) A verify seat too — the third of three
            // independently-threaded capture sites; only probe/judge had
            // coverage.
            ("review-verify", vec![remote_staffing("cloud-verify", "gpt-4o", 1)]),
        ]);
        let inputs = inputs_for(&crew, 500_000);
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            let content = if call.system.contains("verify") {
                "```json\n{\"ruling\": \"verified\", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```"
                    .to_string()
            } else if call.model == "gpt-4o" && call.system.contains("judge") {
                CONFIRM_JSON.to_string()
            } else {
                "a real defect".to_string()
            };
            Ok(SingleShotReply {
                content,
                total_tokens: Some(10),
                model: Some("gpt-4o-2026-08-01".to_string()),
            })
        };
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");

        let probe = env.members.iter().find(|m| m.seat == "review-probe").expect("probe member");
        assert_eq!(probe.model, "gpt-4o", "requested id is unchanged");
        assert_eq!(
            probe.served_model.as_deref(),
            Some("gpt-4o-2026-08-01"),
            "the probe's served model must be captured distinct from the requested id"
        );
        let judge = env.members.iter().find(|m| m.seat == "review-judge").expect("judge member");
        assert_eq!(judge.model, "gpt-4o");
        assert_eq!(
            judge.served_model.as_deref(),
            Some("gpt-4o-2026-08-01"),
            "the judge's served model must be captured distinct from the requested id"
        );
        let verify = env.members.iter().find(|m| m.seat == "review-verify").expect("verify member");
        assert_eq!(verify.model, "gpt-4o");
        assert_eq!(
            verify.served_model.as_deref(),
            Some("gpt-4o-2026-08-01"),
            "the verify seat's served model must be captured distinct from the requested id too"
        );
    }

    /// (#1300) A LOCAL seat's replies never carry a served model — `lms ps`
    /// is ground truth for local dispatch, not the response body.
    #[test]
    fn served_model_absent_for_local_seats() {
        // (#1300 QA follow-up) The mock deliberately reports a served model
        // on the LOCAL calls too — exactly what a real LMStudio response
        // does (it's OpenAI-compatible and echoes a `model` field). This
        // proves the gate in `probe_one_draw`/`run_judge_pass` actually
        // filters it out; a mock that hardcoded `model: None` for local
        // calls would pass even with the gate missing entirely.
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
            ("review-verify", vec![staffing("fast", "verify-model", 1)]),
        ]);
        let inputs = inputs_for(&crew, 500_000);
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            let content = if call.system.contains("verify") {
                "```json\n{\"ruling\": \"verified\", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```"
                    .to_string()
            } else if call.model == "darkmux:judge-model" {
                CONFIRM_JSON.to_string()
            } else {
                "a real defect".to_string()
            };
            Ok(SingleShotReply {
                content,
                total_tokens: Some(10),
                model: Some(call.model.to_string()),
            })
        };
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");

        assert_eq!(env.members.len(), 3, "probe + judge + verify all dispatched");
        for m in &env.members {
            assert!(
                m.served_model.is_none(),
                "a local seat must never report a served_model, even when the response body carries \
                 one (LMStudio's does): {m:?}"
            );
        }
    }

    /// Probe-stage bucket exhaustion (operator decision on #1260): the
    /// remaining REMOTE draws stop with the reason named — a
    /// reduced-coverage WARNING, never a degraded run; whatever landed
    /// before the cap still goes to the judge.
    #[test]
    fn remote_probe_budget_exhaustion_is_reduced_coverage_not_a_dead_run() {
        let crew = crew_with(vec![
            ("review-probe", vec![remote_staffing("cloud", "gpt-remote", 3)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let inputs = inputs_for(&crew, 100); // one 600-token draw exhausts it
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            if call.model == "darkmux:judge-model" {
                Ok(reply(CONFIRM_JSON))
            } else {
                Ok(SingleShotReply {
                    content: "a real defect `const end = start.plus(30)`".to_string(),
                    total_tokens: Some(600),
                    model: None,
                })
            }
        };
        let mut emitter = RecordingEmitter::new();
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut emitter).expect("runs");

        assert!(env.degenerate.is_none(), "probe exhaustion never degrades the run");
        assert_eq!(env.raw_flags, 1, "only the pre-exhaustion draw landed");
        assert_eq!(env.confirmed, 1, "the surviving flag still went through the judge");
        let rec = env.remote_budgets.iter().find(|r| r.stage == "probe").expect("probe budget row");
        assert!(rec.exhausted);
        assert_eq!(rec.used_tokens, 600);
        assert_eq!(rec.skipped_calls, 2, "the remaining k-1 draws were skipped, not billed");
        assert!(
            env.warnings.iter().any(|w| w.contains("reduced coverage")),
            "the named reason lands in the envelope: {:?}",
            env.warnings
        );

        // Live observability: the remote seat's step records carry the
        // remote marker + host, and the terminal task record carries the
        // separated remote-token figure + the warnings (#1186 — downstream
        // savings surfaces exclude these tokens).
        let probe_step = emitter
            .records
            .iter()
            .filter_map(|r| r.payload.as_ref())
            .find(|p| p["step_id"] == "probe:cloud" && p["status"] == "finished")
            .expect("remote probe step record");
        assert_eq!(probe_step["remote"], true);
        assert_eq!(probe_step["endpoint"], "myorg.cognitiveservices.azure.com");
        let finished = emitter
            .records
            .iter()
            .filter(|r| r.action == "review.task")
            .filter_map(|r| r.payload.as_ref())
            .find(|p| p["status"] == "finished")
            .expect("terminal task record");
        assert_eq!(finished["remote_tokens"], 600);
        assert!(finished["warnings"].as_array().is_some_and(|w| !w.is_empty()));
    }

    /// Judge-stage bucket exhaustion is a LOAD-BEARING failure (operator
    /// decision): the run goes degraded with the reason named — never a
    /// silent "none confirmed" pass. Pass-1 and pass-2 are separate
    /// executions, each with its own allowance.
    #[test]
    fn remote_judge_budget_exhaustion_is_an_honest_degraded_run() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![remote_staffing("cloud", "gpt-judge", 1)]),
        ]);
        let mut inputs = inputs_for(&crew, 100); // one 600-token ruling exhausts a pass bucket
        // Two bundles ⇒ two anchor-less flags in different bundles ⇒ both
        // survive dedup ⇒ the second flag's pass-1 hits the exhausted bucket.
        inputs.bundles = Some(vec![bundle_input("a.ts"), bundle_input("b.ts")]);
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            if call.endpoint.is_some() {
                Ok(SingleShotReply {
                    content: CONFIRM_JSON.to_string(),
                    total_tokens: Some(600),
                    model: None,
                })
            } else {
                Ok(reply("a real defect"))
            }
        };
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");

        let reason = env.degenerate.as_deref().expect("judge exhaustion degrades the run");
        assert!(reason.contains("remote judge token budget exhausted"), "got: {reason}");
        assert_eq!(env.judged.len(), 2);
        assert!(
            env.judged.iter().any(|j| j.tier == Tier::Confirmed),
            "the pre-exhaustion flag still carries its real ruling"
        );
        let skipped = env
            .judged
            .iter()
            .find(|j| j.pass1.ruling == JudgeRuling::Error)
            .expect("the post-exhaustion flag is ruled Error, never silently confirmed");
        assert!(skipped.pass1.note_for_author.contains("remote token budget exhausted"));
        let p1 = env
            .remote_budgets
            .iter()
            .find(|r| r.stage == "judge-pass1")
            .expect("judge-pass1 budget row");
        assert!(p1.exhausted);
        assert_eq!(p1.skipped_calls, 1);
        let p2 = env
            .remote_budgets
            .iter()
            .find(|r| r.stage == "judge-pass2")
            .expect("judge-pass2 budget row — a separate execution");
        assert_eq!(p2.skipped_calls, 0, "pass-2 drew from its own fresh allowance");
    }

    /// A remote probe seat FAILING (after the transport's bounded retries)
    /// is a warning + reduced coverage — the local seats and the judge
    /// still run. (A LOCAL probe failure keeps the abort behavior —
    /// covered by `bookend_guard_probe_dispatch_error_*`.)
    #[test]
    fn remote_probe_failure_is_a_warning_and_the_run_continues() {
        let crew = crew_with(vec![
            (
                "review-probe",
                vec![staffing("fast", "local-probe", 1), remote_staffing("cloud", "gpt-remote", 2)],
            ),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let inputs = inputs_for(&crew, 500_000);
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            if call.endpoint.is_some() {
                Err(anyhow!("endpoint 401"))
            } else if call.model == "darkmux:judge-model" {
                Ok(reply(CONFIRM_JSON))
            } else {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            }
        };
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter)
            .expect("a remote probe failure must not abort the run");
        assert!(
            env.warnings.iter().any(|w| w.contains("reduced coverage") && w.contains("endpoint 401")),
            "the named failure lands as a warning: {:?}",
            env.warnings
        );
        assert_eq!(env.confirmed, 1, "the local seat's flag still confirmed");
        let remote = env.members.iter().find(|m| m.model == "gpt-remote").expect("remote member row");
        assert!(remote.remote);
        assert_eq!(remote.total_tokens, 0, "a failed seat billed nothing");
    }

    // ═══════════════════════════════════════════════════════════════
    // The review-verify seat (#1260/#1177) — optional adjudication stage
    // ═══════════════════════════════════════════════════════════════

    const VERIFIED_JSON: &str = "```json\n{\"ruling\": \"verified\", \"decisive_evidence\": \"ve\", \"note_for_author\": \"vn\"}\n```";
    const REFUTED_JSON: &str = "```json\n{\"ruling\": \"refuted\", \"decisive_evidence\": \"re\", \"note_for_author\": \"rn\"}\n```";
    const UNCERTAIN_JSON: &str = "```json\n{\"ruling\": \"uncertain\", \"decisive_evidence\": \"ue\", \"note_for_author\": \"un\"}\n```";

    /// (contract 6) Byte-lock for the verify prompt — the full assembled
    /// string, mirroring `judge_prompt_matches_phase_a_golden_*`. The
    /// evidence sections are the judge's exact assembly (one shared
    /// implementation, `review_prompt_with_tail`); only the frozen tail
    /// differs, and this golden pins every byte of it.
    #[test]
    fn verify_prompt_matches_frozen_golden() {
        let golden = "## The author's stated case (the pull request description)\nBound retry backoff to a sane ceiling\nCaps the exponential backoff delay so a large attempt count cannot stall retries indefinitely.\n\n## The code under review\n```typescript\n// src/example.ts (lines 1-4)\nexport function clampRetryDelay(attempt: number, base: number): number {\n  const delay = base * Math.pow(2, attempt);\n  return Math.min(delay, 30000);\n}\n```\n\n## Fact sheet given to the flagging reviewer\n`attempt` is caller-controlled and unbounded\n\n## The flagged item to investigate\nThe delay calculation in `clampRetryDelay` never verifies `attempt` is non-negative — a negative attempt shrinks the delay below the intended floor.\n\nAdjudicate the confirmed finding against the code above. End your reply with exactly one fenced JSON block:\n```json\n{\"ruling\": \"verified\" | \"refuted\" | \"uncertain\", \"decisive_evidence\": \"<the specific code line or checked claim that decided it>\", \"note_for_author\": \"<one or two sentences the author reads>\"}\n```";
        let p = verify_prompt(
            "Bound retry backoff to a sane ceiling",
            "Caps the exponential backoff delay so a large attempt count cannot stall retries indefinitely.",
            GOLDEN_CODE,
            &["`attempt` is caller-controlled and unbounded".to_string()],
            "The delay calculation in `clampRetryDelay` never verifies `attempt` is non-negative — a negative attempt shrinks the delay below the intended floor.",
        );
        assert_eq!(p, golden);
    }

    #[test]
    fn parse_verify_ruling_vocabulary_and_rejections() {
        let (r, e, n) = parse_verify_ruling(VERIFIED_JSON).expect("parses");
        assert_eq!(r, VerifyRuling::Verified);
        assert_eq!((e.as_str(), n.as_str()), ("ve", "vn"));
        assert_eq!(parse_verify_ruling(REFUTED_JSON).unwrap().0, VerifyRuling::Refuted);
        assert_eq!(parse_verify_ruling(UNCERTAIN_JSON).unwrap().0, VerifyRuling::Uncertain);
        // Case-insensitive + trimmed, same as the judge parser.
        let upper = "```json\n{\"ruling\": \" VERIFIED \", \"decisive_evidence\": \"e\", \"note_for_author\": \"n\"}\n```";
        assert_eq!(parse_verify_ruling(upper).unwrap().0, VerifyRuling::Verified);
        // The JUDGE vocabulary is NOT the verify vocabulary — a verify seat
        // answering "confirmed" is off-contract and must read as Unparsed.
        assert!(parse_verify_ruling(CONFIRM_JSON).is_none());
        assert!(parse_verify_ruling("no verdict here").is_none());
    }

    /// The whole verify state machine in one run: three double-confirmed
    /// flags adjudicated `verified` / `refuted` / `uncertain`. Also pins
    /// the residency ordering (a LOCAL verify seat loads after the judge
    /// releases) and the envelope's verify accounting.
    #[test]
    fn verify_stage_verified_refuted_uncertain_state_machine() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
            ("review-verify", vec![staffing("frontier", "verify-model", 1)]),
        ]);
        let mut inputs = inputs_for(&crew, 500_000);
        inputs.bundles = Some(vec![bundle_input("a.ts"), bundle_input("b.ts"), bundle_input("c.ts")]);
        let mut cycler = RecordingCycler::new();
        let verify_replies = RefCell::new(vec![VERIFIED_JSON, REFUTED_JSON, UNCERTAIN_JSON]);
        let mut chat = |call: &ChatCall| {
            if call.model == "darkmux:verify-model" {
                assert_eq!(call.system, "verify sys", "the verify seat gets its own persona");
                Ok(reply(verify_replies.borrow_mut().remove(0)))
            } else if call.model == "darkmux:judge-model" {
                Ok(reply(CONFIRM_JSON))
            } else {
                Ok(reply("a real defect"))
            }
        };
        let mut emitter = RecordingEmitter::new();
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut emitter).expect("runs");

        assert_eq!(env.judged.len(), 3);
        // verified: stays confirmed, record present.
        let v = &env.judged[0];
        assert_eq!(v.tier, Tier::Confirmed);
        assert_eq!(v.verify.as_ref().unwrap().ruling, VerifyRuling::Verified);
        assert_eq!(v.verify.as_ref().unwrap().model, "darkmux:verify-model");
        assert!(!v.demoted_by_verify);
        // refuted: demoted to archived, demotion recorded.
        let r = &env.judged[1];
        assert_eq!(r.tier, Tier::Archived);
        assert!(r.demoted_by_verify);
        assert_eq!(r.verify.as_ref().unwrap().ruling, VerifyRuling::Refuted);
        assert_eq!(r.verify.as_ref().unwrap().note_for_author, "rn");
        // uncertain: stays confirmed (keeps the marker downstream).
        let u = &env.judged[2];
        assert_eq!(u.tier, Tier::Confirmed);
        assert_eq!(u.verify.as_ref().unwrap().ruling, VerifyRuling::Uncertain);
        assert!(!u.demoted_by_verify);
        // Envelope accounting.
        assert_eq!(env.confirmed, 2);
        assert_eq!(env.archived, 1);
        assert_eq!(env.verified, 1);
        assert_eq!(env.refuted, 1);
        let member = env.members.iter().find(|m| m.seat == "review-verify").expect("verify member");
        assert_eq!(member.draws, 3, "one adjudication per confirmed flag");
        assert!(!member.remote);
        assert!(env.steps.iter().any(|s| s.step_id == "verify" && s.items_in == 3));
        assert!(env.staffing.as_ref().unwrap().verify.is_some(), "snapshot carries the verify seat");
        // Residency: the local verify seat loads AFTER the judge releases.
        let judge_release = cycler.log.iter().position(|e| e == "release:judge-model").unwrap();
        let verify_load = cycler.log.iter().position(|e| e == "load:verify-model").unwrap();
        let verify_release = cycler.log.iter().position(|e| e == "release:verify-model").unwrap();
        assert!(judge_release < verify_load && verify_load < verify_release);
        // Emission: the verify stage brackets itself with step records and
        // emits one ruling per adjudication, inside the run's existing
        // bookend guard (contract 2).
        let payloads: Vec<&serde_json::Value> =
            emitter.records.iter().filter_map(|r| r.payload.as_ref()).collect();
        assert!(payloads.iter().any(|p| p["step_id"] == "verify" && p["status"] == "started"));
        assert!(payloads.iter().any(|p| p["step_id"] == "verify" && p["status"] == "finished"));
        assert_eq!(payloads.iter().filter(|p| p["stage"] == "verify").count(), 3);
    }

    /// A crew WITHOUT the seat is byte-identical to today: no verify step,
    /// no verify records, and the serialized envelope carries none of the
    /// verify fields.
    #[test]
    fn crew_without_verify_seat_is_unchanged() {
        let crew = valid_crew();
        let inputs = inputs_for(&crew, 500_000);
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            if call.model == "darkmux:judge-model" {
                Ok(reply(CONFIRM_JSON))
            } else {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            }
        };
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        assert!(env.judged.iter().all(|j| j.verify.is_none()));
        assert!(!env.steps.iter().any(|s| s.step_id == "verify"));
        assert!(!env.members.iter().any(|m| m.seat == "review-verify"));
        let value = serde_json::to_value(&env).unwrap();
        assert!(value.get("verified").is_none(), "zero verified never serializes");
        assert!(value.get("refuted").is_none());
        assert!(value["staffing"].get("verify").is_none());
        for j in value["judged"].as_array().unwrap() {
            assert!(j.get("verify").is_none());
            assert!(j.get("demoted_by_verify").is_none());
        }
    }

    /// Zero confirms ⇒ the verify stage never dispatches at all — no step,
    /// no member row, no call (the scripted closure would panic on one).
    #[test]
    fn verify_stage_skips_entirely_on_zero_confirms() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
            ("review-verify", vec![staffing("frontier", "verify-model", 1)]),
        ]);
        let inputs = inputs_for(&crew, 500_000);
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            assert_ne!(call.model, "darkmux:verify-model", "no confirms ⇒ no verify dispatch");
            if call.model == "darkmux:judge-model" {
                Ok(reply(FP_JSON))
            } else {
                Ok(reply("a real defect"))
            }
        };
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        assert_eq!(env.confirmed, 0);
        assert!(!env.steps.iter().any(|s| s.step_id == "verify"));
        assert!(!env.members.iter().any(|m| m.seat == "review-verify"));
        assert!(!cycler.log.iter().any(|e| e.contains("verify-model")));
    }

    /// A REMOTE verify seat draws from its own execution bucket, and
    /// (#1260, ruling applied) A REMOTE verify seat exhausting its execution
    /// bucket degrades the STAGE, not the run: the run is NEVER marked
    /// degenerate (findings already verified would be discarded as "no
    /// signal" — factually false). Instead the skipped flag keeps its
    /// `Confirmed` tier + manual-verification marker (recorded per-flag as
    /// Error), a verified flag posts as verified, and the envelope carries a
    /// loud "verify budget exhausted after N of M adjudications" warning.
    #[test]
    fn remote_verify_budget_exhaustion_degrades_the_stage_not_the_run() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
            ("review-verify", vec![remote_staffing("frontier", "gpt-verify", 1)]),
        ]);
        let mut inputs = inputs_for(&crew, 100); // one 600-token adjudication exhausts it
        inputs.bundles = Some(vec![bundle_input("a.ts"), bundle_input("b.ts")]);
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            if call.endpoint.is_some() {
                Ok(SingleShotReply {
                    content: VERIFIED_JSON.to_string(),
                    total_tokens: Some(600),
                    model: None,
                })
            } else if call.model == "darkmux:judge-model" {
                Ok(reply(CONFIRM_JSON))
            } else {
                Ok(reply("a real defect"))
            }
        };
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        assert!(env.degenerate.is_none(), "verify exhaustion NEVER degrades the whole run");
        let warning = env
            .warnings
            .iter()
            .find(|w| w.contains("verify budget exhausted"))
            .expect("the exhaustion is named as a loud warning");
        // "after N of M adjudications" — one landed, one skipped, of two.
        assert!(warning.contains("after 1 of 2 adjudications"), "got: {warning}");
        let rec = env.remote_budgets.iter().find(|r| r.stage == "verify").expect("verify budget row");
        assert!(rec.exhausted);
        assert_eq!(rec.skipped_calls, 1);
        // The first flag adjudicated `verified`; the second was skipped and
        // stays Confirmed (marker downstream) with the reason named per-flag.
        assert_eq!(env.verified, 1, "the pre-exhaustion adjudication still counts");
        let skipped = env
            .judged
            .iter()
            .find(|j| matches!(&j.verify, Some(v) if v.ruling == VerifyRuling::Error))
            .expect("skipped adjudication recorded as Error");
        assert_eq!(skipped.tier, Tier::Confirmed);
        assert!(skipped.verify.as_ref().unwrap().note_for_author.contains("remote token budget exhausted"));
        // The verify member is marked remote with the bare model id.
        let member = env.members.iter().find(|m| m.seat == "review-verify").unwrap();
        assert!(member.remote);
        assert_eq!(member.model, "gpt-verify");
        // No cycler traffic for a remote verify seat.
        assert!(!cycler.log.iter().any(|e| e.contains("gpt-verify")));
    }

    /// The verify seat's staffing shape is validated like the judge's:
    /// exactly one staffing when declared; absent is fine (optional seat).
    #[test]
    fn validate_review_crew_verify_seat_shape() {
        let ok = crew_with(vec![
            ("review-probe", vec![staffing("fast", "a", 1)]),
            ("review-judge", vec![staffing("fast", "b", 1)]),
            ("review-verify", vec![staffing("frontier", "c", 1)]),
        ]);
        let seats = validate_review_crew(&ok).expect("verify seat accepted");
        assert!(seats.verify.is_some());

        let absent = valid_crew();
        assert!(validate_review_crew(&absent).expect("optional").verify.is_none());

        let two = crew_with(vec![
            ("review-probe", vec![staffing("fast", "a", 1)]),
            ("review-judge", vec![staffing("fast", "b", 1)]),
            ("review-verify", vec![staffing("frontier", "c", 1), staffing("frontier", "d", 1)]),
        ]);
        let err = validate_review_crew(&two).unwrap_err().to_string();
        assert!(err.contains("review-verify"), "{err}");
        assert!(err.contains("EXACTLY 1"), "{err}");
    }

    /// Local-only runs serialize with none of the #1260 fields — the
    /// envelope shape is byte-compatible with pre-#1260 consumers.
    #[test]
    fn local_only_envelope_carries_no_remote_fields() {
        let crew = valid_crew();
        let inputs = inputs_for(&crew, 500_000);
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            if call.model == "darkmux:judge-model" {
                Ok(reply(CONFIRM_JSON))
            } else {
                Ok(reply("a real defect `const end = start.plus(30)`"))
            }
        };
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        let value = serde_json::to_value(&env).unwrap();
        assert!(value.get("warnings").is_none(), "empty warnings never serialize");
        assert!(value.get("remote_budgets").is_none(), "no budget rows on a local-only run");
        for m in value["members"].as_array().unwrap() {
            assert!(m.get("remote").is_none(), "local members carry no remote flag");
            assert!(m.get("endpoint").is_none());
        }
        for s in value["staffing"]["probes"].as_array().unwrap() {
            assert!(s.get("remote").is_none());
        }
    }

    // ═══════════════════════════════════════════════════════════════
    // Review-round fixes (#1260) — bill every attempt, stage-scoped verify
    // degradation, remote-judge honest-fail, reasoning-aware floor
    // ═══════════════════════════════════════════════════════════════

    /// (FIX 1) A REMOTE probe seat whose draw comes back EMPTY after the
    /// retry still bills BOTH attempts — to the member record, the probe
    /// bucket, and the envelope's separated `remote_tokens`. Hosted reasoning
    /// legitimately burns the whole budget thinking and returns empty; that
    /// spend must never be invisible to the meter.
    #[test]
    fn remote_probe_empty_draw_still_bills_both_attempts() {
        let crew = crew_with(vec![
            ("review-probe", vec![remote_staffing("cloud", "gpt-remote", 1)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let inputs = inputs_for(&crew, 500_000);
        let mut cycler = RecordingCycler::new();
        // Every remote call is empty content but bills 600 tokens — the draw
        // retries once, so two 600-token attempts.
        let mut chat = |call: &ChatCall| {
            if call.endpoint.is_some() {
                Ok(SingleShotReply { content: String::new(), total_tokens: Some(600), model: None })
            } else {
                Ok(reply(CONFIRM_JSON))
            }
        };
        let mut emitter = RecordingEmitter::new();
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut emitter).expect("runs");

        // Zero content ⇒ zero flags ⇒ the run is a degenerate zero-flag run,
        // but the SPEND is still fully accounted.
        assert!(env.degenerate.is_some(), "no flags landed, so the run is degenerate");
        let member = env.members.iter().find(|m| m.model == "gpt-remote").expect("remote member");
        assert!(member.remote);
        assert_eq!(member.total_tokens, 1200, "both empty attempts billed to the member (600 + 600)");
        let rec = env.remote_budgets.iter().find(|r| r.stage == "probe").expect("probe budget row");
        assert_eq!(rec.used_tokens, 1200, "both empty attempts billed to the bucket");
        let finished = emitter
            .records
            .iter()
            .filter(|r| r.action == "review.task")
            .filter_map(|r| r.payload.as_ref())
            .find(|p| p["status"] == "finished")
            .expect("terminal task record");
        assert_eq!(finished["remote_tokens"], 1200, "the envelope's separated remote figure bills both");
    }

    /// (FIX 4 / binding design, revised #1329) A REMOTE judge whose dispatch
    /// FAILS on EVERY flag (after the transport's bounded retries) marks the
    /// run degraded with a reason naming the failed-flag count — never a
    /// silent fake adjudication that archives the flag and leaves the run
    /// green. This is the `usable == 0` case (total loss); see
    /// `remote_judge_dispatch_error_on_minority_of_flags_does_not_degrade_the_run`
    /// below for the partial-failure case, which must NOT degrade.
    #[test]
    fn remote_judge_dispatch_failure_degrades_the_run() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![remote_staffing("cloud", "gpt-judge", 1)]),
        ]);
        let inputs = inputs_for(&crew, 500_000);
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            if call.endpoint.is_some() {
                Err(anyhow!("endpoint 503"))
            } else {
                Ok(reply("a real defect"))
            }
        };
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        let reason = env.degenerate.as_deref().expect("remote judge dispatch failure degrades the run");
        assert!(reason.contains("remote judge dispatch failed on 1 of 1 flag"), "got: {reason}");
    }

    /// (#1329) The bug: a REMOTE judge dispatch failure on ONE flag out of
    /// many was forcing the ENTIRE run degenerate — discarding every other
    /// flag's real, valid adjudication (9 confirmed + 9 needs-check lost on
    /// a real 37-flag production run, darkmux#1329). The per-flag outcome
    /// was always safe (a pass-2 dispatch error demotes just that flag to
    /// NeedsCheck, same as any other pass-2 disagreement) — only the
    /// run-level gate over-reacted. Three flags, pass-1 confirms all three,
    /// pass-2 dispatch-errors on the MIDDLE flag only: the other two stay
    /// cleanly confirmed, the middle one demotes (not lost), and the run
    /// renders normally.
    #[test]
    fn remote_judge_dispatch_error_on_minority_of_flags_does_not_degrade_the_run() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![remote_staffing("cloud", "gpt-judge", 1)]),
        ]);
        let mut inputs = inputs_for(&crew, 500_000);
        inputs.bundles = Some(vec![bundle_input("a.ts"), bundle_input("b.ts"), bundle_input("c.ts")]);
        let mut cycler = RecordingCycler::new();
        let judge_call_index = RefCell::new(0u32);
        let mut chat = |call: &ChatCall| {
            if call.endpoint.is_some() {
                let idx = *judge_call_index.borrow();
                *judge_call_index.borrow_mut() += 1;
                // Calls land flag-major: f1.p1, f1.p2, f2.p1, f2.p2, f3.p1,
                // f3.p2. Fail ONLY f2's pass-2 (call index 3) — one dispatch
                // out of six, on a flag pass-1 already confirmed.
                if idx == 3 {
                    Err(anyhow!("endpoint 503"))
                } else {
                    Ok(reply(CONFIRM_JSON))
                }
            } else {
                Ok(reply("a real defect"))
            }
        };
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");

        assert!(
            env.degenerate.is_none(),
            "a minority dispatch error with real usable signal must not degrade the run: {:?}",
            env.degenerate
        );
        assert_eq!(env.judged.len(), 3);
        assert_eq!(env.confirmed, 2, "the two clean flags stay confirmed");
        assert_eq!(env.needs_check, 1, "the dispatch-error flag demotes, it is not lost");
        assert_eq!(env.archived, 0);
        let demoted = &env.judged[1];
        assert_eq!(demoted.tier, Tier::NeedsCheck);
        assert!(demoted.demoted_by_pass2);
        // A green run must still SURFACE the transient failure — never fully
        // silent (this repo's doctrine: loud beats quiet, no blind runs).
        assert!(
            env.warnings.iter().any(|w| w.contains("remote judge dispatch failed on 1 of 3 flag")),
            "a minority dispatch error must be named in env.warnings even on a healthy run: {:?}",
            env.warnings
        );
    }

    /// (FIX 4) The LOCAL judge dispatch-failure path is UNCHANGED — a bad
    /// LOCAL judge call is swallowed to `Archived` and the run only degrades
    /// via the pre-existing judge-dead honesty gate (all rulings unusable),
    /// never via the new remote honest-fail reason.
    #[test]
    fn local_judge_dispatch_failure_keeps_today_behavior() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let inputs = inputs_for(&crew, 500_000);
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            if call.model == "darkmux:judge-model" {
                Err(anyhow!("lmstudio down"))
            } else {
                Ok(reply("a real defect"))
            }
        };
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        // Degenerate via the JUDGE-DEAD gate (all rulings unusable), NOT the
        // remote honest-fail reason — local semantics are untouched.
        let reason = env.degenerate.as_deref().expect("a fully-dead local judge is degenerate (judge-dead gate)");
        assert!(reason.contains("no usable ruling"), "local path uses the judge-dead gate: {reason}");
        assert!(!reason.contains("remote judge dispatch failed"), "the remote reason must not fire for a local judge");
    }

    /// (CONSIDER g) When the JUDGE bucket is already exhausted (run destined
    /// for degraded), the verify stage is SKIPPED entirely — no frontier
    /// spend on a doomed run. The scripted verify closure would panic if
    /// called.
    #[test]
    fn verify_stage_skipped_when_judge_already_degraded() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![remote_staffing("cloud", "gpt-judge", 1)]),
            ("review-verify", vec![staffing("frontier", "verify-model", 1)]),
        ]);
        let mut inputs = inputs_for(&crew, 100); // one 600-token ruling exhausts a pass bucket
        inputs.bundles = Some(vec![bundle_input("a.ts"), bundle_input("b.ts")]);
        let mut cycler = RecordingCycler::new();
        let mut chat = |call: &ChatCall| {
            assert_ne!(call.model, "darkmux:verify-model", "a judge-doomed run must not spend on verify");
            if call.endpoint.is_some() {
                Ok(SingleShotReply { content: CONFIRM_JSON.to_string(), total_tokens: Some(600), model: None })
            } else {
                Ok(reply("a real defect"))
            }
        };
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        assert!(env.degenerate.as_deref().unwrap().contains("remote judge token budget exhausted"));
        assert!(!env.steps.iter().any(|s| s.step_id == "verify"), "no verify step on a doomed run");
        assert!(!env.members.iter().any(|m| m.seat == "review-verify"), "no verify member on a doomed run");
    }

    /// (FIX 5) Reasoning-aware completion floor: a REMOTE seat with NO
    /// explicit staffing `max_tokens` floors at 16384 (never the local-tuned
    /// probe default of 4000 — the reasoning-guillotine class); an explicit
    /// staffing `max_tokens` always wins verbatim; the floor never LOWERS an
    /// already-higher local default; LOCAL seats are unaffected.
    #[test]
    fn resolve_seat_max_tokens_remote_reasoning_floor() {
        let local = staffing("fast", "m", 1);
        assert_eq!(resolve_seat_max_tokens(&local, DEFAULT_PROBE_MAX_TOKENS), DEFAULT_PROBE_MAX_TOKENS);

        let remote = remote_staffing("cloud", "gpt", 1); // max_tokens: None
        assert_eq!(
            resolve_seat_max_tokens(&remote, DEFAULT_PROBE_MAX_TOKENS),
            REMOTE_REASONING_MAX_TOKENS_FLOOR,
            "a remote probe seat floors at 16384, not the 4000 local default"
        );
        assert_eq!(
            resolve_seat_max_tokens(&remote, DEFAULT_JUDGE_MAX_TOKENS),
            DEFAULT_JUDGE_MAX_TOKENS,
            "the floor never lowers an already-higher local default (a floor, not a clamp)"
        );

        let mut remote_explicit = remote_staffing("cloud", "gpt", 1);
        remote_explicit.max_tokens = Some(500);
        assert_eq!(
            resolve_seat_max_tokens(&remote_explicit, DEFAULT_PROBE_MAX_TOKENS),
            500,
            "an explicit staffing max_tokens always wins verbatim (operator sovereignty)"
        );
    }

    /// (FIX 5, live) A REMOTE probe seat with no explicit staffing max_tokens
    /// sends `max_completion_tokens = 16384` on the wire, not 4000.
    #[test]
    fn remote_probe_seat_sends_reasoning_floor_on_the_wire() {
        let crew = crew_with(vec![
            ("review-probe", vec![remote_staffing("cloud", "gpt-remote", 1)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let inputs = inputs_for(&crew, 500_000);
        let mut cycler = RecordingCycler::new();
        let seen_cap = RefCell::new(0u32);
        let mut chat = |call: &ChatCall| {
            if call.endpoint.is_some() {
                *seen_cap.borrow_mut() = call.max_tokens;
                Ok(reply("a real defect"))
            } else {
                Ok(reply(CONFIRM_JSON))
            }
        };
        let _ = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        assert_eq!(*seen_cap.borrow(), REMOTE_REASONING_MAX_TOKENS_FLOOR);
    }

    /// (CONSIDER d) The verify seat's inconclusive paths — a dispatch `Err`
    /// and an unparsed reply (real chat outcomes, not the synthetic budget
    /// record) — each keep the flag `Confirmed` WITH the manual-verification
    /// marker, never promote, and never degrade the run.
    #[test]
    fn verify_dispatch_error_and_unparsed_keep_confirmed_with_marker() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
            ("review-verify", vec![staffing("frontier", "verify-model", 1)]),
        ]);
        let mut inputs = inputs_for(&crew, 500_000);
        inputs.bundles = Some(vec![bundle_input("a.ts"), bundle_input("b.ts")]);
        let mut cycler = RecordingCycler::new();
        // Flag a: the verify call errors. Flag b: garbage both attempts (the
        // unparsed retry fires, then stays Unparsed).
        let verify_calls = RefCell::new(0u32);
        let mut chat = |call: &ChatCall| {
            if call.model == "darkmux:verify-model" {
                let mut n = verify_calls.borrow_mut();
                *n += 1;
                match *n {
                    1 => Err(anyhow!("verify endpoint down")),
                    _ => Ok(reply("no verdict here")),
                }
            } else if call.model == "darkmux:judge-model" {
                Ok(reply(CONFIRM_JSON))
            } else {
                Ok(reply("a real defect"))
            }
        };
        let env = run_review(&inputs, &mut chat, &mut cycler, &mut NullEmitter).expect("runs");
        assert!(env.degenerate.is_none(), "an inconclusive verify never degrades the run");
        assert_eq!(env.confirmed, 2, "both stay confirmed (marker downstream)");
        assert_eq!(env.verified, 0, "an inconclusive adjudication never promotes");
        let errored = env
            .judged
            .iter()
            .find(|j| matches!(&j.verify, Some(v) if v.ruling == VerifyRuling::Error))
            .expect("dispatch-error adjudication recorded as Error");
        assert_eq!(errored.tier, Tier::Confirmed);
        let unparsed = env
            .judged
            .iter()
            .find(|j| matches!(&j.verify, Some(v) if v.ruling == VerifyRuling::Unparsed))
            .expect("garbage adjudication recorded as Unparsed after the retry");
        assert_eq!(unparsed.tier, Tier::Confirmed);
    }

    /// (CONSIDER c) `RemoteBucket::exhausted()` boundary: under < at == over.
    /// A mutation of `>=` to `>` must fail this table (the `at` row).
    #[test]
    fn remote_bucket_exhausted_boundary_table() {
        let mut under = RemoteBucket::new("s", 100);
        under.spend(99, 1);
        assert!(!under.exhausted(), "under budget: 99 < 100");

        let mut at = RemoteBucket::new("s", 100);
        at.spend(100, 1);
        assert!(at.exhausted(), "at budget: 100 >= 100 (a `>` mutation breaks here)");

        let mut over = RemoteBucket::new("s", 100);
        over.spend(101, 1);
        assert!(over.exhausted(), "over budget: 101 >= 100");
    }

    /// (CONSIDER e) The terminal `review.task` record carries `remote_tokens`
    /// when a seat dispatched remotely, and OMITS it entirely on a local-only
    /// run — the separated cloud figure never counts as savings (#1186).
    #[test]
    fn remote_tokens_bookend_present_when_remote_absent_when_local() {
        // Local-only: field absent.
        let local_crew = valid_crew();
        let local_inputs = inputs_for(&local_crew, 500_000);
        let mut cyc1 = RecordingCycler::new();
        let mut chat1 = |call: &ChatCall| {
            if call.model == "darkmux:judge-model" { Ok(reply(CONFIRM_JSON)) } else { Ok(reply("a real defect")) }
        };
        let mut em1 = RecordingEmitter::new();
        let _ = run_review(&local_inputs, &mut chat1, &mut cyc1, &mut em1).expect("runs");
        let local_finished = em1
            .records
            .iter()
            .filter(|r| r.action == "review.task")
            .filter_map(|r| r.payload.as_ref())
            .find(|p| p["status"] == "finished")
            .expect("terminal record");
        assert!(local_finished.get("remote_tokens").is_none(), "local-only omits remote_tokens");

        // Remote judge: field present.
        let remote_crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model", 1)]),
            ("review-judge", vec![remote_staffing("cloud", "gpt-judge", 1)]),
        ]);
        let remote_inputs = inputs_for(&remote_crew, 500_000);
        let mut cyc2 = RecordingCycler::new();
        let mut chat2 = |call: &ChatCall| {
            if call.endpoint.is_some() {
                Ok(SingleShotReply { content: CONFIRM_JSON.to_string(), total_tokens: Some(42), model: None })
            } else {
                Ok(reply("a real defect"))
            }
        };
        let mut em2 = RecordingEmitter::new();
        let _ = run_review(&remote_inputs, &mut chat2, &mut cyc2, &mut em2).expect("runs");
        let remote_finished = em2
            .records
            .iter()
            .filter(|r| r.action == "review.task")
            .filter_map(|r| r.payload.as_ref())
            .find(|p| p["status"] == "finished")
            .expect("terminal record");
        assert!(remote_finished.get("remote_tokens").is_some(), "a remote seat stamps remote_tokens");
    }

    // ─── (#1230/#1341 DRY pass) Task/Step graph orchestration ───────────

    fn step_ctx(crew: &ResolvedCrew, bundles: Vec<BundleInput>) -> Arc<ReviewStepContext> {
        Arc::new(ReviewStepContext {
            case_id: "case-1".to_string(),
            crew: crew.clone(),
            intent_title: String::new(),
            intent_body: String::new(),
            diff: DIFF.to_string(),
            probe_system: "probe prior".to_string(),
            judge_system: "judge persona".to_string(),
            verify_system: "verify persona".to_string(),
            bundles,
            remote_max_tokens_per_execution: 500_000,
            timeout_seconds: 30,
        })
    }

    /// The graph's SHAPE is fully knowable upfront (the redesign's whole
    /// point): three Phases, `depends_on` edges crossing Phase boundaries
    /// exactly like they cross Task boundaries within one, and every Step
    /// resolvable through the registry `build_review_graph` also builds.
    /// Pure structural assertion — no dispatch, no network.
    #[test]
    fn build_review_graph_has_three_phases_and_correct_dependencies() {
        let crew = crew_with(vec![
            ("review-probe", vec![staffing("fast", "probe-model-a", 1), staffing("slow", "probe-model-b", 1)]),
            ("review-judge", vec![staffing("fast", "judge-model", 1)]),
        ]);
        let seats = validate_review_crew(&crew).expect("valid crew");
        let ctx = step_ctx(&crew, vec![]);

        let graph = build_review_graph(
            ctx,
            seats.judge.clone(),
            seats.verify.cloned(),
            seats.probes,
            "investigate",
            "adjudicate",
            "report",
            1,
        );

        // bundle(1) + probe(2 seats) + dedup(1) = investigate's 4 tasks.
        let investigate_tasks: Vec<_> = graph.tasks.iter().filter(|t| t.phase_id == "investigate").collect();
        assert_eq!(investigate_tasks.len(), 4, "bundle + 2 probe seats + dedup");
        let adjudicate_tasks: Vec<_> = graph.tasks.iter().filter(|t| t.phase_id == "adjudicate").collect();
        assert_eq!(adjudicate_tasks.len(), 1, "judge only");
        let report_tasks: Vec<_> = graph.tasks.iter().filter(|t| t.phase_id == "report").collect();
        assert_eq!(report_tasks.len(), 2, "verify + synthesis");

        // (#1341) Cross-phase dependency now lives on `Task.depends_on`
        // (Steps have none of their own) — adjudicate's judge TASK depends
        // on investigate's dedup TASK, no special cross-phase mechanism.
        let tasks_by_id: std::collections::BTreeMap<&str, &Task> =
            graph.tasks.iter().map(|t| (t.id.as_str(), t)).collect();
        let dedup_step_id = "review-dedup-step";
        let judge_task = tasks_by_id["review-judge-task"];
        assert_eq!(judge_task.depends_on, vec!["review-dedup-task".to_string()]);
        assert_eq!(graph.phase_id_of_step[dedup_step_id], "investigate");
        assert_eq!(graph.phase_id_of_step["review-judge-step"], "adjudicate");

        // report's synthesis TASK depends on BOTH dedup (investigate) and
        // verify (report) — graph-native cross-phase data flow, not a side
        // channel.
        let synth_task = tasks_by_id["review-synthesis-task"];
        assert!(synth_task.depends_on.contains(&"review-dedup-task".to_string()));
        assert!(synth_task.depends_on.contains(&"review-verify-task".to_string()));
        assert_eq!(graph.phase_id_of_step["review-synthesis-step"], "report");

        // Every step's `kind` resolves through the SAME registry — the
        // scheduler contract this whole redesign hangs on.
        for step in graph.steps.values() {
            assert!(graph.registry.get(&step.kind).is_ok(), "step `{}` kind `{}` must resolve", step.id, step.kind);
        }

        // (#1349) The pre-rename `funnel.*` kind ids also resolve — a
        // `Step.kind` persisted before this rename shipped must not become
        // "unknown step kind" if anything ever re-reads it back through a
        // fresh registry (see `StepKindRegistry::register_alias`'s doc).
        for legacy in [
            "funnel.bundle",
            "funnel.probe:fast",
            "funnel.probe:slow",
            "funnel.dedup",
            "funnel.judge",
            "funnel.verify",
            "funnel.synthesis",
        ] {
            assert!(graph.registry.get(legacy).is_ok(), "legacy kind id `{legacy}` must still resolve");
        }

        // ONE call is the whole point: no separate driver loop needed to
        // reach every step — `depends_on` alone determines readiness.
        assert_eq!(graph.steps.len(), 7, "bundle + 2 probe + dedup + judge + verify + synthesis");
    }

    /// End-to-end through the REAL scheduler (`run_step_graph`, one call —
    /// see the module doc) with an EMPTY bundle set: every dispatch-shaped
    /// step (probe/judge/verify) iterates zero items and makes ZERO chat
    /// calls (probe's `select_bundles_for_staffing` returns empty; judge's
    /// deduped list is empty; verify's confirmed docket is empty) — so this
    /// exercises the full graph, all three Phases, without a live LMStudio
    /// or network. Confirms the degenerate reason ends up in the FINAL
    /// envelope regardless of which stage would have detected it.
    #[test]
    fn run_review_graph_with_empty_bundles_completes_with_zero_dispatches() {
        let crew = valid_crew();
        let seats = validate_review_crew(&crew).expect("valid crew");
        let judge = seats.judge.clone();
        let verify = seats.verify.cloned();
        let probes: Vec<_> = seats.probes.clone();
        let ctx = step_ctx(&crew, vec![]);

        let graph = build_review_graph(ctx.clone(), judge.clone(), verify.clone(), &probes, "investigate", "adjudicate", "report", 1);
        let fingerprint_val = fingerprint(&seat_identifier(&judge.pm), &ctx.judge_system);
        let staffing_snap = staffing_snapshot(&probes, &judge, verify.as_ref(), false);

        let mut emitter = RecordingEmitter::new();
        let (env, steps) =
            run_review_graph(&ctx, "test-crew", ExecMode::Sequential, fingerprint_val, staffing_snap, graph, &mut emitter)
                .expect("graph run completes even with zero bundles");

        assert_eq!(env.bundles, 0);
        assert_eq!(env.deduped_flags, 0);
        assert_eq!(env.confirmed, 0);
        assert_eq!(env.needs_check, 0);
        // Every declared step reached a terminal status — the graph never
        // stalls on a "ready but never scheduled" node.
        for step in steps.values() {
            assert!(
                matches!(step.status, NodeStatus::Complete | NodeStatus::Error),
                "step `{}` (kind `{}`) must reach a terminal status, got {:?}",
                step.id,
                step.kind,
                step.status
            );
        }
        // The scheduler's own generic step-lifecycle bookends fired for
        // every step (free observability — see the module doc).
        let starts = emitter.records.iter().filter(|r| r.action == "step start").count();
        assert_eq!(starts, steps.len(), "every declared step got a lifecycle start record");
        // (#1349) `run_review_graph` itself must emit NO task-level bookend
        // at all — that liveness edge belongs entirely to the caller's
        // `with_dispatch_bookends` wrap (`src/pr_review.rs`), which brackets
        // the WHOLE call in the canonical `dispatch start`/`dispatch
        // complete` record. A `review.task` (or any `dispatch *`) record
        // emitted from inside this function would be the exact redundant,
        // competing-vocabulary bug #1349 retired.
        assert!(
            emitter.records.iter().all(|r| r.action != "review.task" && !r.action.starts_with("dispatch ")),
            "run_review_graph must not emit its own task-level bookend: {:?}",
            emitter.records.iter().map(|r| r.action.as_str()).collect::<Vec<_>>()
        );
    }
}
