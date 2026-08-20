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
//! ## Timing: two scopes, not one duplicated (#1877, correcting an earlier
//! plan in that issue)
//!
//! Every graph step kind below (`ReviewBundleStepKind`, `ReviewDedupStepKind`,
//! `ReviewJudgeStepKind`, etc.) takes its own `Instant` at the top of
//! `run_streaming`, computes its own `wall_ms`, and passes it to
//! [`emit_review_step_result`] — this is the emission site referenced below.
//! Separately, `darkmux_crew::scheduler::run_step_graph` (the caller that
//! invokes `run_streaming`, see `build_review_graph_from_config` below) wraps
//! that SAME call in its own `Instant` pair and pushes the result into
//! `SchedulerReport::step_records` (see that field's doc in
//! `crates/darkmux-crew/src/scheduler.rs` and the module doc in
//! `crates/darkmux-crew/src/run_record.rs`).
//!
//! An earlier plan for this arc proposed collapsing these into one
//! measurement — "the kinds should stop re-measuring, read the scheduler's
//! record instead." That is not implementable and not desirable, and it is
//! worth stating plainly here so nobody "consolidates" this module's own
//! `t0`/`wall_ms` locals and quietly loses either half:
//!
//! - **It is not implementable**: a step kind's `wall_ms` above is computed
//!   and emitted BEFORE `run_streaming` returns. The scheduler's own timer
//!   does not stop, and its `StepRecord` does not exist, until AFTER
//!   `run_streaming` returns. There is no ordering in which this module's
//!   emission site could read the scheduler's number instead of its own.
//! - **It is not duplication even where both exist for the same step**: this
//!   module's record carries `items_in`/`items_out` and, for the judge step,
//!   a `pass1_wall_ms`/`pass2_wall_ms` breakdown — real per-kind business
//!   semantics the scheduler cannot observe from outside the call (see
//!   `StepRecord::items_in`'s own doc in `run_record.rs`: the scheduler
//!   always leaves those `None`). This is INNER work, with a breakdown,
//!   emitted into the flow stream the viewer renders. The scheduler's
//!   number is timed strictly around that SAME `run_streaming` call, on
//!   the step's own worker thread (`scheduler.rs:808`-`:811`) — it
//!   EXCLUDES queueing behind `remote_cap`, `ensure_wave_loaded`,
//!   `apply_step_terminal`, and `persist` (see
//!   `SchedulerReport::step_records`'s own doc in `scheduler.rs` for the
//!   full list of what a `StepRecord` does and doesn't cover), and it is
//!   recorded uniformly for every step of every mission, whether or not
//!   the kind cooperates. Dropping either loses something real: the
//!   breakdown, or the uniform coverage.
//!
//! What the two numbers DO guarantee, because one strictly contains the
//! other in wall-clock: for the same step, the scheduler's `wall_ms` is
//! always `>=` the `wall_ms` THIS MODULE emits in its own `step result`
//! flow record (the `t0.elapsed()` passed to [`emit_review_step_result`]
//! above — e.g. the judge step's combined pass-1 + pass-2 wall time).
//! This is deliberately NOT the same thing as `MemberRecord::wall_ms`:
//! for the judge seat that field is `pass1_wall_ms + pass2_wall_ms`
//! summed ACROSS EVERY FLAG's own dispatches — a COST metric, not a
//! timeline (see the doc at its assignment site) — and under
//! `review.judge_concurrency > 1` (an operator knob, always stamped onto
//! `review-judge-step` as a step-config override) those per-flag
//! dispatches overlap in wall-clock, so the sum can exceed both this
//! module's own elapsed `wall_ms` and the scheduler's number. The `>=`
//! relationship holds for the `step result` `wall_ms`; it does NOT hold
//! for `MemberRecord::wall_ms`. That relationship, not either side's
//! absolute duration, is the thing worth pinning in a test — see
//! `scheduler.rs`'s `#1877` invariant test for where it's asserted.
//!
//! (Today the two producers are also structurally disjoint at the envelope
//! level — `SchedulerReport::step_records` is not merged into
//! `ReviewEnvelope::steps` on the graph path, so there is no double count to
//! reconcile there either. See `run_record.rs`'s module doc for that half of
//! the story.)
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
use darkmux_crew::remote_budget::{RemoteBudget, RemoteBudgetRecord};
use darkmux_crew::run_outcome::RunOutcome;
// (#1877 item 2) The run-record + run-observability substrate — see this
// file's own "moved to" comments at the old call sites for the full
// rationale. `RunObs`/`RunEmitter` are aliased back to their pre-move names
// (`ReviewObs`/`ReviewEmitter`) so this file's ~15 internal call sites and
// every external `impl ReviewEmitter for X` (`src/mission_launch_review.rs`,
// `review_bench.rs`, this module's own tests) keep compiling unchanged;
// `NullEmitter`/`HostTelemetrySampler` keep their names as-is (never
// review-flavored to begin with).
use darkmux_crew::run_obs::{self, HostTelemetrySampler, RunObs as ReviewObs};
pub use darkmux_crew::run_obs::{NullEmitter, RunEmitter as ReviewEmitter};
// `seat_endpoint`/`seat_endpoint_host` were PRIVATE on `origin/main` and
// have no caller outside this file (only doc-comment mentions in
// `src/mission_launch_review.rs` and `crates/darkmux-flow/src/bookend.rs`)
// — a plain `use` keeps `seat_endpoint_host`'s #1530 credential-sanitizer
// off the crate's public surface. The other five names DO have external
// `impl`/reference sites (mirroring `ReviewEmitter` above), so they stay
// `pub use`.
use darkmux_crew::run_record::{seat_endpoint, seat_endpoint_host};
pub use darkmux_crew::run_record::{
    seat_identifier, staffing_snapshot, MemberRecord, SeatStaffingSnapshot, StaffingSnapshot, StepRecord,
};
use darkmux_crew::single_shot::SingleShotReply;
use darkmux_crew::step_kinds::patterns::dedup::{dedup as pattern_dedup, DedupStrategy};
use darkmux_crew::step_kinds::patterns::multi_pass_confirm::{multi_pass_confirm, ConfirmTier, PassClass};
// (#1877 item 2) `HostSample` itself is no longer named directly in this
// file's production code (only passed through as `sample_host`'s inferred
// return type) — it moved to `review_tests.rs`'s own `use` block, the same
// "test-only import stays out of the production block so a non-test build
// never warns about it going unused" convention `ArtifactBus`'s import
// already follows there.
use darkmux_crew::telemetry_sampler::sample_host;
// (#1230 Packet 1) LmsCycler's residency mechanism now routes through
// gestalt's pure planner, executed via the real LmsHost/MacProbe port
// adapters (their first production call site) — see the "model cycling"
// section below.
use darkmux_gestalt::{AcquireOpts, AcquireScope, Action, CallerIntent, Facts, ModelHost, Placement, ResourceProbe, V1Estimator};
use darkmux_crew::resourcing::{ResolvedReviewRoles, ResolvedSeatStaffing};
use darkmux_profiles::gestalt_host::{resolved_load_deadline, LmsHost, MacProbe};
use darkmux_types::{BundleSelector, ModelEndpoint, ProfileModel};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;

// ─── execution mode ───────────────────────────────────────────────────────

// (#1877, this issue) `ExecMode` and `wave_schedule_to_exec_mode` moved to
// `darkmux_gestalt::waves` — see their doc comments there for the full
// argument (deciding sequential-vs-parallel residency cycling is a hardware
// residency question gestalt already owns; this module used to ask gestalt
// for a `WaveSchedule` and then re-derive the answer itself). Re-exported
// under the SAME names so every existing reference in this crate and in
// `src/mission_launch_review.rs` (the binary crate) keeps resolving
// unchanged — `pub use` for `ExecMode` since it crosses that crate
// boundary; a plain `use` for `wave_schedule_to_exec_mode`, which was never
// `pub` (no caller outside this module ever named it), so this import
// keeps it at the exact same visibility it had before the move.

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
///
/// (This module's own usage doc — the type itself, `Sequential`/`Parallel`/
/// `Auto`, now lives in `darkmux_gestalt::waves` since it is genuinely
/// general, not review-specific; see the `pub use` below.)
pub use darkmux_gestalt::ExecMode;
use darkmux_gestalt::wave_schedule_to_exec_mode;

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
    /// (#1748) Present iff the mechanical, zero-token absence-claim
    /// backstop ([`apply_absence_backstop`]) found the claimed-absent
    /// token in the WHOLE FILE and demoted this flag from `Confirmed` to
    /// `NeedsCheck`. Distinct from `demoted_by_pass2`/`demoted_by_verify`
    /// (both AI-driven demotions) — this one is a plain substring check
    /// against `FileSource`, run BEFORE the (optional, costlier) verify
    /// stage even sees the flag. Absent (and never serialized) whenever
    /// the check never fired or agreed with the finding — a flag the
    /// backstop left untouched serializes byte-identically to today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absence_backstop: Option<AbsenceBackstopNote>,
}

/// (#1748) The mechanical absence-claim backstop's per-flag outcome —
/// present on a [`JudgedFlag`] only when the check actually demoted it.
/// See [`apply_absence_backstop`] for the full check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbsenceBackstopNote {
    /// The token the finding claimed was absent (`process.exitCode`,
    /// `.catch`) — the single backtick-quoted span
    /// [`extract_claimed_absent_token`] pulled from the decisive judge
    /// record's `note_for_author`/`decisive_evidence`.
    pub token: String,
    /// The repo-relative file the token was found in.
    pub file: String,
    /// 1-indexed line number the token was found on, when the token is
    /// confined to a single line (`None` when `content.contains(token)`
    /// held but no single line matched — e.g. the token spans a wrap —
    /// still a real contradiction, just without a precise line to cite).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

// (#1877 item 2) `MemberRecord`/`StepRecord`/`ReviewEmitter`/`NullEmitter`/
// `HostTelemetrySampler`/`ReviewObs` moved to `darkmux_crew::run_record`/
// `darkmux_crew::run_obs` — the shared run-record + run-observability
// substrate a second mission (coder-phase) can now reach directly instead
// of copying it. Re-exported/aliased below under their original names so
// every existing reference in this file (and every external
// `darkmux_lab::lab::review::<Name>` reference) keeps resolving unchanged.
// See both modules' doc comments for the full rationale, including the
// two deliberate renames (`ReviewEmitter` -> `RunEmitter`, `ReviewObs` ->
// `RunObs`) and why `HostTelemetrySampler`'s `rx` field is no longer
// directly reachable (`try_drain()` replaces the old `.rx.try_iter()`
// field access at this file's two remaining call sites).

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
    /// (#1605) The bundler's own per-file decline accounting for this run's
    /// diff — `None` only when the bundle stage never ran with real
    /// bookkeeping available (`bundle_override` test seam, or an external
    /// `--bundler` plugin, which carries no notion of this crate's internal
    /// decline reasons). Populated by `ReviewBundleStepKind::run_streaming`
    /// alongside `bundles` itself. This is the data `degenerate`'s own
    /// message is built from when `bundles == 0`, and what
    /// [`ReviewEnvelope::degenerate_kind`] classifies against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_skip: Option<BundleSkipReport>,
    /// (#1605) Distinguishes a genuinely EMPTY-but-honest diff (every
    /// touched file declined for a benign reason — non-code content, no
    /// signal to review) from a real ERROR/unexplained degenerate outcome
    /// (a bundler bug, an internal limit hit, every probe draw failing, a
    /// judge with no usable ruling). `None` when the run isn't degenerate at
    /// all. The workflow/render layer branches on this typed field instead
    /// of string-matching `degenerate`'s prose — see
    /// [`crate::lab::review::classify_zero_bundle_degenerate`] for how a
    /// zero-bundle run is classified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degenerate_kind: Option<DegenerateKind>,
    /// (#1605) Total number of probe draws that recovered on a bounded
    /// transient-error retry (see [`darkmux_crew::step_kinds::builtins`]'s
    /// `retry_on_error` — the probe stage's `dispatch.map` step opts in;
    /// nothing downstream of a successful probe run does). Zero (and never
    /// serialized) on a run where no retry fired — a retry that happened is
    /// recorded here rather than only inferable from wall-clock.
    #[serde(default, skip_serializing_if = "usize_is_zero")]
    pub probe_retries: usize,
}

/// (#1605) See [`ReviewEnvelope::degenerate_kind`]'s doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DegenerateKind {
    /// Every touched file declined for a reason that means "nothing here to
    /// review" (today: every skip is [`SkipReason::NonCodeExtension`]) —
    /// posted as a neutral no-op comment, never a red/failed run.
    BenignEmpty,
    /// (#1757) Every touched file declined for a benign reason OR because
    /// it's real source in a language the built-in (TypeScript-only)
    /// bundler doesn't parse ([`SkipReason::SourceLanguageUnsupported`]),
    /// with at least one file in the latter bucket and NO genuine error
    /// reason mixed in. Distinct from [`DegenerateKind::BenignEmpty`]:
    /// there IS real code here, just not in a language darkmux's built-in
    /// bundler reads — so the run stays neutral (never fails the check),
    /// but the posted note names what went unreviewed and points at the
    /// `--bundler` escape hatch instead of reading as "nothing to see
    /// here." A `.sql`-only or `.css`-only PR is the motivating case: it
    /// used to read identically to a real bundler failure.
    UnsupportedLanguage,
    /// Anything else: a bundler bug candidate, an internal limit
    /// (`OverSizeCap`), an unreadable file, every probe draw erroring, or a
    /// judge phase with no usable ruling. Stays the existing loud,
    /// never-a-silent-pass "degraded" treatment.
    Error,
}

/// (#1605) Classify a `bundles == 0` run from its skip breakdown and build
/// the degenerate message as a SUMMARY of that breakdown — replacing the
/// old fixed "no bundles produced from the diff" string, which couldn't
/// distinguish "diff was entirely non-code" from "bundler bug" from "diff
/// exceeded some internal bound" (darkmux#1605).
///
/// Three buckets, in priority order:
///
/// - **Benign** iff every skipped file's reason is deliberate-and-expected —
///   [`SkipReason::NonCodeExtension`] or [`SkipReason::TestFileExcluded`]
///   (#1605 QA finding: the bundler EXCLUDES test files rather than failing
///   to understand them; both are benign, but they must be named differently
///   or the no-op comment calls real test code "fixtures and lockfiles") —
///   AND at least one file WAS skipped.
/// - **Unsupported-language** (#1757) iff every skipped file's reason is one
///   of the two benign ones above OR [`SkipReason::SourceLanguageUnsupported`],
///   AND at least one is the latter. Real source the bundler simply can't
///   parse is not the same finding as "nothing here" — it gets its own
///   neutral (non-failing) outcome that names the escape hatch, rather than
///   collapsing into either the benign no-op or the loud error treatment.
/// - **Error** is the fallback: an empty `files_skipped` (no skip data at
///   all, e.g. `bundle_override`/an external bundler) or any genuine error
///   reason mixed in stays `Error` — the honest "can't explain this"
///   default, never a guessed benign or unsupported-language classification.
pub(crate) fn classify_zero_bundle_degenerate(skip: &Option<BundleSkipReport>) -> (String, DegenerateKind) {
    let Some(report) = skip else {
        return ("no bundles produced from the diff".to_string(), DegenerateKind::Error);
    };
    let benign = !report.files_skipped.is_empty()
        && report.files_skipped.iter().all(|f| {
            matches!(f.reason, SkipReason::NonCodeExtension | SkipReason::TestFileExcluded)
        });
    // (#1757) A diff whose declines are entirely explained by the two benign
    // reasons plus real-but-unparseable source, with at least one of the
    // latter — never a mix that also carries a genuine error reason (an
    // `OverSizeCap`/`UnreadableInWorktree`/etc. entry keeps this `false`,
    // same as it already keeps `benign` above `false`).
    let unsupported_language = !report.files_skipped.is_empty()
        && report.files_skipped.iter().any(|f| f.reason == SkipReason::SourceLanguageUnsupported)
        && report.files_skipped.iter().all(|f| {
            matches!(
                f.reason,
                SkipReason::NonCodeExtension | SkipReason::TestFileExcluded | SkipReason::SourceLanguageUnsupported
            )
        });
    let mut by_reason: std::collections::BTreeMap<&'static str, usize> = std::collections::BTreeMap::new();
    for f in &report.files_skipped {
        let label = match f.reason {
            SkipReason::NonCodeExtension => "non-code extension",
            // (#1752) Deliberately NOT grouped with `NonCodeExtension` —
            // this is real source code in a language the bundler doesn't
            // parse, not benign data. Kept out of the `benign` match
            // above too, so this reason alone never reads as "nothing to
            // review." (#1757) It's classified separately from `benign`
            // into its own `unsupported_language` bucket below, which
            // stays neutral (never fails the run) but names the
            // `--bundler` escape hatch instead of a bare no-op.
            SkipReason::SourceLanguageUnsupported => "real source in an unsupported language",
            SkipReason::TestFileExcluded => "test file (excluded by the bundler)",
            SkipReason::UnreadableInWorktree => "unreadable in worktree",
            SkipReason::NoSurvivingLines => "no surviving lines",
            SkipReason::NoEnclosingFunction => "no enclosing function",
            SkipReason::OverSizeCap => "over the bundler's size cap",
            SkipReason::TopLevelOverSizeCap => "top-level run over the bundler's size cap",
        };
        *by_reason.entry(label).or_insert(0) += 1;
    }
    let breakdown = if by_reason.is_empty() {
        "no per-file breakdown available".to_string()
    } else {
        by_reason.iter().map(|(reason, n)| format!("{n} {reason}")).collect::<Vec<_>>().join(", ")
    };
    let msg = format!(
        "no bundles produced from the diff — {} file(s) considered, {} skipped ({breakdown})",
        report.files_considered,
        report.files_skipped.len()
    );
    let kind = if benign {
        DegenerateKind::BenignEmpty
    } else if unsupported_language {
        DegenerateKind::UnsupportedLanguage
    } else {
        DegenerateKind::Error
    };
    (msg, kind)
}

/// Serde helper for the skip-if-zero count fields — keeps envelopes from
/// crews without the verify seat byte-identical to pre-#1260 ones.
fn usize_is_zero(n: &usize) -> bool {
    *n == 0
}

/// (#1877 item 5, fixing #1876) Review's OWN predicate mapping from its
/// existing envelope fields onto the generic
/// [`darkmux_crew::run_outcome::RunOutcome`] — the type names three shapes;
/// this function is the review-specific decision of which of THIS run's
/// facts lands in which one. Never mutates the envelope; a read-only view
/// `synthesize_review` (`src/pr_review.rs`) builds its render decision on.
///
/// - [`RunOutcome::Empty`] — `env.degenerate` is set. Unchanged by #1876:
///   this is Gate 2's zero-usable-rulings honesty gate (or a genuinely dead
///   bundle/probe stage, or the `strict` judge-exhaustion policy opting back
///   into the pre-#1876 behavior via `judge_gate_outcome`'s Gate 1) — the
///   SAME condition that has always meant "produced no signal."
/// - [`RunOutcome::Partial`] — `env.degenerate` is `None` but at least one
///   `remote_budgets` row for a JUDGE stage (`"judge-pass1"`/`"judge-pass2"`)
///   carries `skipped_calls > 0`. This is the #1876 fix's own case: the
///   judge stage's remote token bucket ran out before the whole docket was
///   judged, but usable rulings exist. Scoped to judge stages ONLY —
///   probe-stage exhaustion already renders as a "reduced coverage" warning
///   on a healthy run (never touched `env.degenerate`, nothing to fix), and
///   verify-stage exhaustion already renders normally with its own
///   `env.warnings` note (`run_verify_stage` never sets `env.degenerate`
///   either) — folding those into `Partial` too would double-announce an
///   already-correct treatment, not fix a bug.
/// - [`RunOutcome::Complete`] — neither of the above.
pub fn review_outcome(env: &ReviewEnvelope) -> RunOutcome {
    if let Some(reason) = &env.degenerate {
        return RunOutcome::Empty { reason: reason.clone() };
    }
    let reasons: Vec<String> = env
        .remote_budgets
        .iter()
        // (#1876/#1877 QA follow-up) Exact stage names, not a `starts_with`
        // prefix — a future `judge-*` row that ISN'T one of the two real
        // stages should never silently flip a run to Partial by accident.
        .filter(|r| matches!(r.stage.as_str(), "judge-pass1" | "judge-pass2") && r.skipped_calls > 0)
        .map(|r| judge_budget_shortfall_reason(env, r))
        .collect();
    if reasons.is_empty() {
        RunOutcome::Complete
    } else {
        RunOutcome::Partial { reasons }
    }
}

/// (#1876/#1877 QA follow-up) `judge-pass1` and `judge-pass2` skips mean
/// DIFFERENT things and need different wording — conflating them was a
/// real bug this function's own predecessor had. A pass-1 skip means the
/// flag NEVER got a ruling at all (`budget_exhausted_outcome` -> `Error` ->
/// excluded from `usable`, per `judge_gate_outcome`'s own filter) — the
/// flag is genuinely unjudged. A pass-2 skip means the flag's pass-1
/// ALREADY ruled it `Confirmed`; only the CONFIRMATION pass was skipped,
/// which `multi_pass_confirm`'s `PassClass::Reject` arm demotes to
/// `Tier::NeedsCheck` (`demoted_by_pass2 = true`) — that flag WAS judged
/// and DOES render, just at a lower tier than a from-scratch double-confirm
/// would have given it. Reporting a pass-2 skip as "N flags went unjudged"
/// (the pass-1 wording) would be factually wrong on both halves: the flags
/// were judged, and `env.judged.len()` is the wrong denominator (pass-2's
/// docket is pass-1's CONFIRMS, not the whole run).
fn judge_budget_shortfall_reason(env: &ReviewEnvelope, r: &RemoteBudgetRecord) -> String {
    // (#1876/#1877 QA follow-up) "{used} of {max} tokens used" reads like a
    // typo when `used` overshoots `max` — which it routinely does, by
    // design: the ceiling is SOFT (`RemoteBudget`'s own module doc), so a
    // grant can land a reply that reports usage slightly above what was
    // admitted. "exceeded its N-token allowance" states the same fact
    // without inviting a "did you mean 500000 of 500497?" double-take.
    if r.stage == "judge-pass1" {
        format!(
            "{} of {} flags went unjudged on the `judge-pass1` stage — it exceeded its {}-token \
             allowance ({} used)",
            r.skipped_calls,
            env.judged.len(),
            r.max_tokens,
            r.used_tokens
        )
    } else {
        format!(
            "{} confirmed finding(s) were conservatively demoted to needs-check because their \
             confirmation pass was skipped on the `judge-pass2` stage — it exceeded its {}-token \
             allowance ({} used)",
            r.skipped_calls, r.max_tokens, r.used_tokens
        )
    }
}

/// (#1877 item 2) Review's OWN predicate mapping from its envelope fields
/// onto [`RunOutcome`], scoped for what `src/mission_launch_review.rs`'s
/// `review_result_to_mission_envelope` puts on `MissionEnvelope::outcome`.
/// **Deliberately a DIFFERENT function from [`review_outcome`] above**, not
/// a thin wrapper around it, because the two answer different questions
/// with different consumers:
///
/// - [`review_outcome`] answers "did every item in the docket get a
///   ruling" for the PR-COMMENT banner (`src/pr_review.rs`'s
///   `synthesize_review`) — narrowly scoped to judge-stage coverage on
///   purpose (see its own doc: probe/verify-stage warnings already render
///   normally and folding them in would double-announce an
///   already-correct treatment).
/// - This function answers "does the MISSION BOARD need to flag this run"
///   — the question `review_result_to_mission_envelope` has always
///   answered via two signals that predate `RunOutcome` entirely:
///   `env.degenerate` (with the `BenignEmpty`/`UnsupportedLanguage`
///   carve-out folded back to healthy, #1654/#1757 — a diff with nothing
///   to review is not a failure worth flagging) and `env.warnings` (ANY
///   non-empty warning, not only a judge-stage one — a probe-seat retry
///   failure is real degraded coverage the board should show, #1876).
///
/// Reusing [`review_outcome`] here would silently CHANGE existing mission
/// statuses for two real input shapes: a benign/unsupported-language
/// degenerate run (today `Clean`; `review_outcome` would call it `Empty` ->
/// `Degenerate`) and a probe/verify-only warning with no judge-stage skip
/// (today `Degraded`; `review_outcome` would call it `Complete` -> `Clean`,
/// since its `Partial` predicate is judge-stage-scoped by design). This
/// function exists so that does NOT happen — every existing
/// `review_result_to_mission_envelope` test still gets the SAME status it
/// got before #1877, because this function reproduces that same predicate,
/// just expressed as a `RunOutcome` instead of an inline if/else chain.
pub fn review_mission_outcome(env: &ReviewEnvelope) -> RunOutcome {
    let neutral_zero_bundle = matches!(
        env.degenerate_kind,
        Some(DegenerateKind::BenignEmpty) | Some(DegenerateKind::UnsupportedLanguage)
    );
    if let Some(reason) = &env.degenerate {
        if !neutral_zero_bundle {
            return RunOutcome::Empty { reason: reason.clone() };
        }
    }
    if !env.warnings.is_empty() {
        return RunOutcome::Partial { reasons: env.warnings.clone() };
    }
    RunOutcome::Complete
}

// (#1877) `RemoteBudgetRecord`/`RemoteBucket` moved to
// `darkmux_crew::remote_budget` (as `RemoteBudgetRecord`/`RemoteBudget`) —
// the shared home for what used to be two hand-copied buckets, this one and
// `step_kinds::MapRemoteBucket`. `MIN_VIABLE_JUDGE_GRANT` below stays here,
// unmoved: it is THIS pipeline's own floor policy, passed to
// `RemoteBudget::with_stage` at construction rather than baked into the
// type, so darkmux-crew's own `MIN_VIABLE_MAP_GRANT` never has to reference
// it (or vice versa) — see `remote_budget`'s module doc.

// (#1877 item 2) `seat_identifier`/`seat_endpoint_host`/`seat_endpoint`/
// `SeatStaffingSnapshot`/`StaffingSnapshot`/`staffing_snapshot` moved to
// `darkmux_crew::run_record` — see that module's doc. `seat_identifier`,
// `staffing_snapshot`, `MemberRecord`, `SeatStaffingSnapshot`, and
// `StaffingSnapshot` are re-exported below under their original names;
// `seat_endpoint`/`seat_endpoint_host` are imported plain (not
// re-exported) — both were private on `origin/main` with no caller
// outside this file, so keeping them off the crate's public surface
// preserves that.

// ─── model cycling ────────────────────────────────────────────────────────
//
// (#1877, this issue) `ModelCycler`, `LmsCycler`, and `gather_facts` were
// candidates to move into `darkmux-gestalt` alongside `ExecMode` above —
// deciding whether/how a model gets loaded is exactly the residency
// question gestalt owns. They stay here instead, and this is the honest
// boundary rather than a forced move:
//
// - `gather_facts` takes `&mut LmsHost` and `LmsCycler::ensure_loaded`/
//   `release` construct one directly — `LmsHost`/`MacProbe` live in
//   `darkmux_profiles::gestalt_host`, and `darkmux-profiles` already
//   depends on `darkmux-gestalt` (it implements gestalt's `ModelHost`/
//   `ResourceProbe` port traits). `darkmux-gestalt` depending back on
//   `darkmux-profiles` for these would be the exact cycle the #602/#604
//   port-adapter split exists to prevent — the same shape that already
//   blocks `resolve_auto_via_waves` below.
// - `ModelCycler`'s own trait signature (`fn ensure_loaded(&mut self, pm:
//   &ProfileModel) -> Result<()>`, `Result` = `anyhow::Result`) is the
//   deeper reason even the TRAIT alone does not move cleanly: every
//   existing gestalt port trait (`ModelHost`, `ResourceProbe` in
//   `ports.rs`) deliberately uses gestalt's OWN typed error vocabulary
//   (`HostError`/`ProbeError`), and `darkmux-gestalt`'s `Cargo.toml` has
//   no `anyhow` dependency today — that omission is part of the crate's
//   stated "pure planning core" discipline, not an oversight. Moving
//   `ModelCycler` as literally written would mean either adding a new
//   dependency edge to a crate whose own module doc enumerates its
//   dependency/purity rules (out of scope for a move-only change, and
//   against this task's own "no new dependencies" constraint), or
//   retyping it to `HostError`/a new gestalt-native error — a real API
//   change to every caller, not a move. So `ModelCycler` stays paired
//   with its one production implementor, `LmsCycler`, which needs the
//   same host types anyway.
//
// None of the three cross a worker-thread boundary that would otherwise
// impose a `Send`/`Sync` requirement on the trait — `LmsCycler` is used
// only on the sequential `run_judge_only` path (see `ReviewStepContext`'s
// doc above: "no `ModelCycler` anywhere in the graph's dispatch path").

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
/// (#1610) The smallest completion cap that can still buy a parseable judge
/// ruling. Below this, `admit_reserve` denies (a visible skip) rather than
/// granting a cap that can only produce a truncated, unparseable reply — which
/// the pipeline would otherwise read as the model rejecting the flag.
///
/// Sized well under a normal ruling and well over a truncation: a ruling is a
/// small JSON object, and anything under a few hundred tokens cannot close it.
///
/// Capped at the configured budget where that is smaller — see `admit_reserve`.
/// A deliberately tiny budget is an operator policy, not a starved grant.
const MIN_VIABLE_JUDGE_GRANT: u32 = 512;

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

// (#1877, this issue) The old `wave_schedule_to_exec_mode` used to live
// here: `Parallel` iff gestalt's co-residency wave scheduler
// ([`darkmux_gestalt::plan_waves`], `WaveMode::Auto`) packs every distinct
// LOCAL model (probe seats + judge, deduped) into ONE wave — i.e. the same
// arithmetic `plan_acquire`'s budget/pool-headroom arms use judges them
// safe to hold resident together, against REAL facts. It moved to
// `darkmux_gestalt::waves::wave_schedule_to_exec_mode` unchanged (imported
// above) — this was the pure projection half of the review's last
// hardware-tier-threshold holdout; `darkmux_gestalt::waves`'s own module
// doc already claimed that concept "was removed end-to-end in
// #602/#604/#605," and this move is what makes that claim true rather than
// aspirational (see that function's doc for the full argument).

/// Gathers real facts and asks gestalt's wave scheduler whether `placements`
/// fit one wave. Separated from [`wave_schedule_to_exec_mode`] (the pure
/// projection, now `darkmux_gestalt::waves::wave_schedule_to_exec_mode` —
/// see its own doc for why it moved and this function did not) so
/// the I/O — `LmsHost::list_resident` + `MacProbe::pools`, the SAME adapters
/// [`LmsCycler`] wires — lives in exactly one place. A residency-read
/// failure degrades to `Sequential` (never guess `Parallel` without knowing
/// what's already resident) with a loud stderr line, never a silent
/// downgrade to a riskier mode.
///
/// (#1877, this issue) This function itself stays here, unmoved: it opens
/// its own `LmsHost` and calls [`gather_facts`], both of which need
/// `darkmux_profiles::gestalt_host` — a crate that already depends on
/// `darkmux-gestalt` (for the port traits it implements), so `darkmux-
/// gestalt` depending back on it would be a cycle. Only the pure
/// wave-count judgment moved; the host-facing glue that gathers the facts
/// stays with its caller, exactly like [`LmsCycler`]/[`gather_facts`] below.
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

/// (#1877, this issue) Stays here rather than moving to `darkmux-gestalt`
/// with the rest of this arc's move. Its signature is
/// `probes: &[ResolvedSeatStaffing], judge: &ResolvedSeatStaffing`, and the
/// real blocker is the same dependency cycle that kept the rest of this
/// arc's non-moved functions in place: `ResolvedSeatStaffing`
/// (`darkmux_crew::resourcing`, documented there as "the resolver's per-seat
/// output") is NOT review-specific — the review driver + envelope snapshot
/// consume it, but so do crew's own `run_record.rs` and the binary
/// (`src/mission_launch_review.rs`) — it just lives in `darkmux-crew`,
/// which depends on `darkmux-gestalt`
/// (`crates/darkmux-crew/Cargo.toml:19`). Moving this function down into
/// `darkmux-gestalt` would need `darkmux-gestalt` to depend back on
/// `darkmux-crew` for that type, inverting the edge into a cycle. The
/// `probes`/`judge` two-arg shape IS review-shaped, but it's cosmetic
/// (`probes.iter().chain(once(judge))` — trivially generalizable to "a
/// slice of seats") and will not survive re-litigation on its own; the
/// cycle is the real reason it stays. What IS general here — turning a set
/// of local placements into an `ExecMode` — is exactly
/// [`resolve_auto_via_waves`] and
/// `darkmux_gestalt::waves::wave_schedule_to_exec_mode`, both already moved
/// or already gestalt's; this function's own job is purely the review-
/// specific translation from probe/judge staffing into that generic
/// `Placement` list.
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
    /// (#1876/#1877) The judge stage's remote-budget exhaustion policy —
    /// `false` (the operator default) treats a skipped judge call as a
    /// coverage fact, never a verdict, as long as some flag was usably
    /// judged; `true` restores the pre-#1876 "any skip degrades the whole
    /// run" behavior. Callers resolve it through `darkmux_types::
    /// config_access::review_judge_fail_on_any_skip()`, same injection
    /// discipline as `remote_max_tokens_per_execution` above.
    pub judge_exhaustion_strict: bool,
    /// (#1748) The SAME `FileSource` the bundler read from — threaded
    /// through so [`apply_absence_backstop`] can check a confirmed
    /// finding's absence claim against the WHOLE FILE, not just the
    /// (possibly truncated) bundle excerpt the AI seats saw. `None` means
    /// the backstop is a no-op (never a hard error) — most of this
    /// module's own tests have no real file tree to check against.
    pub source: Option<&'a FileSource>,
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
///
/// (#1755 — DECIDED) Also deliberately NO `bundle.manifest` anywhere in
/// this prompt, matching [`judge_prompt`]'s own #1256 exclusion of the
/// same field. Pinned by
/// `manifest_never_reaches_the_probe_user_message` in `review_tests.rs`
/// as a structural contract (byte-identical output regardless of
/// `bundle.manifest`'s content), not merely an absence nobody checked.
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
    pass1: RemoteBudget,
    pass2: RemoteBudget,
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
    budgets: Option<&std::sync::Mutex<JudgeBudgets>>,
    model: &str,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    endpoint: Option<&ModelEndpoint>,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
) -> PassOutcome {
    // (#swarm-6) The bucket mutex is held for the ADMIT and the SETTLE only
    // — never across the network dispatch. The previous shape locked at the
    // judge spawn site and held the guard through `judge_one_flag_with_passes`
    // entirely, which serialized every concurrent judge on remote runs: the
    // `review.judge_concurrency` knob spun up N threads that then queued on
    // one mutex, silently degrading to sequential. Reservation is what makes
    // the narrow lock safe: the granted cap is debited at admission, so
    // siblings can't collectively overshoot the #1260 ceiling by admitting
    // against an untouched balance (the same discipline the probe stage's
    // `RemoteBudget` uses, #1442).
    match budgets {
        Some(m) => {
            let granted = {
                let mut b = m.lock().expect("judge budgets mutex poisoned");
                let bucket = if pass == 1 { &mut b.pass1 } else { &mut b.pass2 };
                bucket.admit_reserve(max_tokens)
            };
            let Some(clamped) = granted else {
                return budget_exhausted_outcome(pass);
            };
            let o = judge_pass_with_retry(pass, model, system, prompt, clamped, endpoint, chat);
            {
                let mut b = m.lock().expect("judge budgets mutex poisoned");
                let bucket = if pass == 1 { &mut b.pass1 } else { &mut b.pass2 };
                bucket.settle(clamped, o.tokens, o.calls);
            }
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
    budgets: Option<&std::sync::Mutex<JudgeBudgets>>,
    chat: &mut dyn FnMut(&ChatCall) -> Result<SingleShotReply>,
) -> JudgeOutcome {
    let result = multi_pass_confirm(
        passes,
        |pass_no| {
            // Pass selection moved INTO run_budgeted_pass, under its brief
            // lock — the closure no longer needs `&mut` access to the
            // budgets at all (#swarm-6).
            run_budgeted_pass(pass_no as u8, budgets, model, system, prompt, max_tokens, endpoint, chat)
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
    budgets: Option<&std::sync::Mutex<JudgeBudgets>>,
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

fn verify_budget_outcome(bucket: &RemoteBudget, docket: usize) -> VerifyBudgetOutcome {
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
    let mut bucket = RemoteBudget::with_stage("verify", inputs.remote_max_tokens_per_execution, MIN_VIABLE_JUDGE_GRANT);

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
        items_in: Some(docket),
        items_out: Some(docket),
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

/// (#1373 gates a/b/c + the reason-specificity fix; #1876/#1877 Gate 1
/// rewrite) One judge stage's honesty-gate decision — the SAME
/// budget-exhaustion / dispatch-error / no-usable-ruling logic
/// `finish_review` has always applied, extracted so `ReviewJudgeStepKind`
/// (the graph path) can apply it too without the two callers drifting again
/// (CLAUDE.md's #1352 tiering: "shared logic that both `run_judge_only` and
/// the graph path use should live once").
///
/// At most ONE `degenerate_reason` ever comes back — budget exhaustion
/// (under the `strict` policy — see below) wins over the "no usable ruling"
/// gate, mirroring the original `degen_reasons.is_empty()` short-circuit
/// this was extracted from (never a "combine every reason" accumulator,
/// #1329). `dispatch_error_warning` is independent and UNCONDITIONAL
/// (#1329's loud-beats-quiet half) — present whenever a remote judge had
/// ANY per-flag dispatch failure, whether or not the run also degenerates.
/// `coverage_warning` (#1876/#1877 QA follow-up) is the same shape for the
/// non-strict Gate 1 skip: `env.warnings` is what `review_result_to_mission_
/// envelope` (`src/mission_launch_review.rs`) reads to classify a run
/// `Degraded` vs `Clean` for the mission board, and what
/// `with_dispatch_bookends` (same file) reads to flip the flow record's
/// `dispatch complete` `result_class` from `"ok"` to `"partial"` — those two
/// consumers never see `env.remote_budgets` or `env.judged`, only
/// `degenerate`/`warnings`. NOT the CLI exit code: `mission launch review`
/// always returns `Ok(0)` (`src/cli.rs:552` documents why — CI-facing
/// pass/fail comes from the rendered payload's `mode`, not the process exit
/// status). Without stamping `env.warnings`, a partial-coverage run (real
/// signal, real gap) read `Clean` everywhere except the posted PR comment —
/// exactly the "board and the comment must agree" property this module's
/// own `review_result_to_mission_envelope` doc already promises, silently
/// broken by the render-only half of this fix. Probe exhaustion
/// (`review.rs`'s probe stage) and verify exhaustion (`verify_budget_
/// outcome`) already push their own warning this same way; this closes the
/// one stage that didn't.
struct JudgeGateOutcome {
    remote_budget_rows: Vec<RemoteBudgetRecord>,
    dispatch_error_warning: Option<String>,
    coverage_warning: Option<String>,
    degenerate_reason: Option<String>,
}

/// (#1876/#1877) `strict` is `darkmux_types::config_access::
/// review_judge_fail_on_any_skip()` (`env(DARKMUX_REVIEW_JUDGE_FAIL_ON_ANY_
/// SKIP) > config.review.judge_fail_on_any_skip > false`), resolved by the
/// CALLER (this function stays config-free/pure, per `ReviewInputs::
/// remote_max_tokens_per_execution`'s own "injected here, not read in the
/// driver" doctrine) and threaded through `ReviewInputs`/`ReviewStepContext`.
fn judge_gate_outcome(
    is_remote: bool,
    judged_len: usize,
    usable: usize,
    dispatch_errors: usize,
    budgets: Option<&JudgeBudgets>,
    remote_max_tokens_per_execution: u64,
    strict: bool,
) -> JudgeGateOutcome {
    let mut degen_reasons: Vec<String> = Vec::new();
    let mut remote_budget_rows = Vec::new();
    let mut coverage_warning: Option<String> = None;

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

    // Gate 1 (#1876 fix): a REMOTE judge whose per-pass token bucket
    // EXHAUSTED (a load-bearing stage — operator decision,
    // DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION) is a COVERAGE fact, not a
    // verdict, under the default `strict == false` policy — the row below
    // still reaches `env.remote_budgets` regardless, so `review_outcome`
    // can build the honest Partial outcome (and its rendered banner) from
    // it. Production incident this replaces: a judge that had ruled 123 of
    // 134 flags (7 confirmed, 67 needs-check, complete with evidence)
    // discarded all of it and posted "the review produced no signal"
    // because the last 11 calls were skipped when the bucket ran out — one
    // skipped call out of a thousand did the exact same thing, regardless
    // of how much real signal existed. `strict == true` (an explicit
    // operator opt-in, `review.judge_fail_on_any_skip`) restores that exact
    // pre-#1876 behavior: ANY skip degrades the whole run, no matter how
    // much was judged.
    let mut skipped_total: u32 = 0;
    if let Some(b) = budgets {
        if let Some(rec) = b.pass1.record() {
            remote_budget_rows.push(rec);
        }
        if let Some(rec) = b.pass2.record() {
            remote_budget_rows.push(rec);
        }
        skipped_total = b.pass1.skipped() + b.pass2.skipped();
        if skipped_total > 0 {
            if strict {
                degen_reasons.push(format!(
                    "remote judge token budget exhausted — {skipped_total} judge call(s) skipped after \
                     the per-execution allowance ({remote_max_tokens_per_execution} tokens per stage) \
                     ran out; degenerate run, never a silent pass (review.judge_fail_on_any_skip is set)"
                ));
            } else {
                coverage_warning = Some(format!(
                    "remote judge token budget exhausted — {skipped_total} judge call(s) skipped after \
                     the per-execution allowance ({remote_max_tokens_per_execution} tokens per stage) \
                     ran out — the flags that WERE judged still render; see the envelope's \
                     remote_budgets for the full accounting"
                ));
            }
        }
    }

    // Gate 2: the judge-dead honesty gate — NO flag produced a usable
    // pass-1 ruling, so the whole judge phase produced no signal worth
    // rendering. Names the specific shape that caused it (budget
    // exhaustion, then a remote dispatch failure) rather than the generic
    // wording, so the operator sees WHY the judge went dead, not just THAT
    // it did. (#1876/#1877 QA follow-up) The budget-exhaustion arm fires on
    // `skipped_total > 0` regardless of `strict` — a NON-strict policy
    // still reaches this gate when literally nothing was usable (Gate 1
    // above deliberately left `degen_reasons` empty in that case), and the
    // operator still deserves the specific diagnosis, not the generic
    // "all errored/unparsed" wording that used to be the only option here.
    if degen_reasons.is_empty() && judged_len > 0 && usable == 0 {
        if skipped_total > 0 {
            degen_reasons.push(format!(
                "remote judge token budget exhausted — {skipped_total} judge call(s) skipped after the \
                 per-execution allowance ({remote_max_tokens_per_execution} tokens per stage) ran out, \
                 and none of the flags that WERE judged produced a usable ruling — degenerate run, \
                 never a silent pass"
            ));
        } else if is_remote && dispatch_errors > 0 {
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
        coverage_warning,
        degenerate_reason: if degen_reasons.is_empty() { None } else { Some(degen_reasons.join("; ")) },
    }
}

// ─── absence-claim backstop (#1748) — a cheap, zero-token mechanical gate ──
//
// Production incident: a `confirmed` finding claimed a line of code was
// ABSENT ("does not assign process.exitCode", "there is no .catch") when
// both were present in the file. The judge seat had been shown a
// TRUNCATED bundle excerpt and reported honestly about its own window;
// the pipeline then promoted that into a claim about the WHOLE FILE. The
// pipeline already has the whole file available via `FileSource` — this
// check costs zero tokens and would have caught it.
//
// Deliberately conservative at every step: a phrase-match predicate
// (never a fuzzy heuristic), single-token extraction that ABSTAINS on any
// ambiguity, and a plain substring check against real file content. A
// false POSITIVE here (flagging a non-absence claim) costs one wasted
// file read; a false NEGATIVE just leaves the AI seats' own ruling
// standing — either way, `apply_absence_backstop` never invents a
// finding and never deletes one, only demotes a contradicted `Confirmed`
// to `NeedsCheck` with a note (#1748's "aggregate, never discard" spirit,
// same as #1299's dedup safety net).

/// The literal vocabulary [`is_absence_claim`] matches (case-insensitively,
/// as a plain substring) to decide a finding's text is asserting something
/// is MISSING. Kept in ONE named, auditable list — never scattered regexes
/// — so the phrase set is reviewable and testable on its own. Narrow and
/// literal on purpose: the real precision guard is downstream
/// ([`extract_claimed_absent_token`] additionally requires exactly one
/// confident backtick-quoted token before the check acts at all), so this
/// list can afford to name the common phrasings without needing to be
/// exhaustive or clever.
///
/// A handful of these ("does not check", "does not handle", "does not
/// catch", "never checks", "fails to check") are OPERAND-VERB phrases: in
/// a sentence like "does not check the return value of `X`", the
/// backticked span is the SUBJECT of the missing operation, not the
/// absent thing itself — and `X` is present in the file by construction
/// of the finding (it's the thing being called). Left unguarded, that
/// shape systematically demoted TRUE findings (PR #1765 merge-gate
/// finding). The vocabulary is kept as-is rather than pruned: the guard
/// is [`apply_absence_backstop`]'s adjacency requirement (the extracted
/// token must IMMEDIATELY follow the matched phrase, only whitespace
/// between) — "does not assign `process.exitCode`" keeps working, "does
/// not check the return value of `X`" does not, because real-world
/// operand-verb phrasing always has intervening words ("the return value
/// of", "the error from") between the verb and the subject.
pub const ABSENCE_CLAIM_PHRASES: &[&str] = &[
    "does not call",
    "does not assign",
    "does not invoke",
    "does not check",
    "does not handle",
    "does not catch",
    "does not set",
    "does not use",
    "doesn't call",
    "doesn't assign",
    "doesn't invoke",
    "doesn't check",
    "doesn't handle",
    "doesn't catch",
    "doesn't set",
    "never calls",
    "never assigns",
    "never invokes",
    "never checks",
    "never sets",
    "never catches",
    "there is no ",
    "there's no ",
    "no call to ",
    "not present in",
    "not found in",
    "is missing",
    "is not present",
    "is absent",
    "absent from",
    "missing from",
    "lacks a call to",
    "fails to call",
    "fails to check",
    "fails to invoke",
];

/// True iff `text` contains one of [`ABSENCE_CLAIM_PHRASES`], matched
/// case-insensitively. Pure string work — no model call, no regex crate
/// (matches the codebase's existing hand-rolled-parsing convention, see
/// `bundle/source.rs`'s `extract_from_specifier`).
pub fn is_absence_claim(text: &str) -> bool {
    let lower = text.to_lowercase();
    ABSENCE_CLAIM_PHRASES.iter().any(|p| phrase_occurs(&lower, p))
}

/// True iff `phrase` genuinely occurs in `lower` (an already-lowercased
/// copy of the finding text). Only one exclusion today (PR #1765
/// merge-gate finding): `"there is no "` / `"there's no "` must NOT fire
/// on `"...there is no longer..."` — that phrasing is a SUPERSESSION
/// claim ("the old thing is gone now"), not a claim that something is
/// missing from the file, and the two share a prefix by coincidence of
/// English grammar.
fn phrase_occurs(lower: &str, phrase: &str) -> bool {
    let mut start = 0;
    while let Some(rel) = lower[start..].find(phrase) {
        let idx = start + rel;
        let end = idx + phrase.len();
        let is_supersession =
            matches!(phrase, "there is no " | "there's no ") && lower[end..].starts_with("longer");
        if !is_supersession {
            return true;
        }
        // Every phrase in ABSENCE_CLAIM_PHRASES is plain ASCII, so `idx`
        // is a one-byte-per-char match and `idx + 1` is always a valid
        // UTF-8 boundary to resume scanning from.
        start = idx + 1;
    }
    false
}

/// The claimed-absent token, when exactly ONE backtick-quoted span exists
/// in `text` — the confident case. Zero spans (nothing quoted to check
/// against the file) or two-or-more spans (which one is "the thing that's
/// missing"? ambiguous) both return `None`, per the brief: a guess here is
/// worse than no check at all. Reuses `dialectic::backtick_spans` — the
/// SAME single-backtick-outside-fence scanner [`extract_new_side_anchor`]
/// already uses, so this stays one parsing discipline, not a second one.
/// Deliberately does NOT apply `dialectic::MIN_EVIDENCE_SPAN` (the anchor
/// matcher's 8-char floor) — a real claimed-absent token can be short and
/// still fully confident (`.catch`, `id`), and the single-span requirement
/// above is already the precision guard for this use, not a length floor.
pub fn extract_claimed_absent_token(text: &str) -> Option<String> {
    use super::dialectic::backtick_spans;
    let mut spans = backtick_spans(text);
    if spans.len() != 1 {
        return None;
    }
    let token = spans.remove(0);
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// The char-index of the OPENING backtick of the one confident span in
/// `text`, mirroring `dialectic::backtick_spans`' own fence-aware
/// single-backtick scanning — but tracking WHERE the span starts, which
/// `backtick_spans` itself discards. Returns `None` on zero or 2+ spans,
/// kept in lockstep with [`extract_claimed_absent_token`]'s own abstain
/// rule (this is only ever called after that function already confirmed
/// exactly one span exists, so the "not exactly one" branch here is
/// belt-and-suspenders, not a load-bearing distinct code path).
fn single_backtick_span_start(text: &str) -> Option<usize> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans_seen = 0usize;
    let mut only_start: Option<usize> = None;
    let mut in_fence = false;
    let mut span_start: Option<usize> = None;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '`' {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < chars.len() && chars[j] == '`' {
            j += 1;
        }
        if j - i >= 2 {
            in_fence = !in_fence;
            span_start = None;
        } else if !in_fence {
            match span_start.take() {
                None => span_start = Some(j),
                Some(start) => {
                    let inner: String = chars[start..i].iter().collect();
                    if !inner.trim().is_empty() {
                        spans_seen += 1;
                        // `start` is the char index right AFTER the opening
                        // backtick (it was set to `j == i + 1` when that
                        // backtick was seen), so `start - 1` is the
                        // backtick's own position.
                        only_start = Some(start - 1);
                    }
                }
            }
        }
        i = j;
    }
    if spans_seen == 1 { only_start } else { None }
}

/// (PR #1765 merge-gate finding, MUST FIX 1) True iff the claimed-absent
/// token's backtick span IMMEDIATELY follows (only whitespace between)
/// an occurrence of one of [`ABSENCE_CLAIM_PHRASES`] in `text`. This is
/// the guard that distinguishes "does not assign `X`" — where `X` IS the
/// claimed-absent thing — from "does not check the return value of `X`"
/// — where `X` is the SUBJECT of the missing operation, several words
/// from the verb, and is present in the file by construction of the
/// finding. Works in char space throughout (never slices `text` at a
/// byte offset derived from a lowercased copy) so a non-ASCII finding
/// text can never panic on a UTF-8 boundary; if lowercasing changed the
/// char count (rare non-ASCII case-folding), this abstains rather than
/// risk comparing misaligned indices.
fn claimed_token_immediately_follows_absence_phrase(text: &str) -> bool {
    let Some(span_start) = single_backtick_span_start(text) else {
        return false;
    };
    let chars: Vec<char> = text.chars().collect();
    let lower_chars: Vec<char> = text.to_lowercase().chars().collect();
    if lower_chars.len() != chars.len() {
        return false;
    }
    for phrase in ABSENCE_CLAIM_PHRASES {
        let phrase_chars: Vec<char> = phrase.chars().collect();
        if phrase_chars.is_empty() || phrase_chars.len() > lower_chars.len() {
            continue;
        }
        for start in 0..=(lower_chars.len() - phrase_chars.len()) {
            if lower_chars[start..start + phrase_chars.len()] != phrase_chars[..] {
                continue;
            }
            let mut end = start + phrase_chars.len();
            while end < chars.len() && chars[end].is_whitespace() {
                end += 1;
            }
            if end == span_start {
                return true;
            }
        }
    }
    false
}

/// True iff a non-empty identifier character — the boundary predicate
/// [`contains_token_at_boundary`] uses to reject a bare substring match
/// like `id` inside `identifier`.
fn is_identifier_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// (PR #1765 merge-gate finding, MUST FIX 2) True iff `token` occurs in
/// `content` at a genuine boundary — the characters immediately before
/// and after the match (when they exist) are not themselves identifier
/// characters. A bare `content.contains(token)` matched `id` inside
/// `identifier`, `err` inside `error`, `on` inside almost anything —
/// wrong tier AND fabricated evidence (the cited line has nothing to do
/// with the claim). Extraction still allows any non-empty token (no
/// minimum length, per [`extract_claimed_absent_token`]'s doc), so a
/// short real token like `.catch` keeps working: its own leading `.` is
/// itself a non-identifier character, so a boundary check that lands on
/// it needs no special-casing — the rule is symmetric on both sides of
/// the match, not about the token's own first/last character.
fn contains_token_at_boundary(content: &str, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let content_chars: Vec<char> = content.chars().collect();
    let token_chars: Vec<char> = token.chars().collect();
    if token_chars.len() > content_chars.len() {
        return false;
    }
    for start in 0..=(content_chars.len() - token_chars.len()) {
        let end = start + token_chars.len();
        if content_chars[start..end] != token_chars[..] {
            continue;
        }
        let left_ok = start == 0 || !is_identifier_char(content_chars[start - 1]);
        let right_ok = end == content_chars.len() || !is_identifier_char(content_chars[end]);
        if left_ok && right_ok {
            return true;
        }
    }
    false
}

/// The 1-based line number of the FIRST line in `content` where `token`
/// occurs at a genuine boundary (see [`contains_token_at_boundary`]) —
/// the line the demotion note cites as evidence. A bare `line.contains`
/// would cite a line whose only relationship to the token is an
/// incidental substring (e.g. citing the `identifier` line as "evidence"
/// for a claimed-absent `id`), which is fabricated evidence on top of
/// the wrong tier.
fn line_of_token_at_boundary(content: &str, token: &str) -> Option<u32> {
    content
        .lines()
        .position(|l| contains_token_at_boundary(l, token))
        .map(|i| i as u32 + 1)
}

/// The repo-relative file path a bundle's `id` names. Production bundle
/// ids follow `"<fn>@<path>"` (see `Bundle::id`'s doc) — the substring
/// after the LAST `@` is the path. The provisional test-only bundler
/// (`bundles_from_diff`) sets `id` to the bare changed-file path with no
/// `@` at all; treating an `@`-less id as already being the path keeps
/// this working against both bundlers. Returns `None` only for an empty
/// id — never fails, just means the caller has nothing to check.
fn bundle_file_path(bundle_id: &str) -> Option<&str> {
    let path = match bundle_id.rfind('@') {
        Some(idx) => &bundle_id[idx + 1..],
        None => bundle_id,
    };
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

/// (#1748) The mechanical, zero-token backstop: for every flag the judge
/// left `Confirmed`, check whether its decisive record's text is an
/// ABSENCE claim naming a confident token, and if so, whether that token
/// is actually present anywhere in the WHOLE FILE (via `source`, never the
/// bundle's own — possibly truncated — excerpt). A contradiction demotes
/// the flag to `NeedsCheck` (never deletes it — a true finding with sloppy
/// wording must not vanish) and attaches a short machine-generated note
/// naming what was found and where, both structurally
/// ([`JudgedFlag::absence_backstop`]) and in the human-facing
/// `note_for_author` text the posted review renders (`pr_review.rs` always
/// reads `pass2.unwrap_or(pass1)`, mutated here to match).
///
/// Applied BEFORE the optional AI verify stage — a mechanically-contradicted
/// claim never spends a verify dispatch adjudicating it. `source: None`
/// (no `FileSource` available — most of this module's own tests) makes the
/// whole pass a no-op; per-flag, any abstain condition (non-absence claim,
/// no confident token, an unresolvable bundle path, or an unreadable file)
/// leaves that flag completely untouched. Never errors and never touches
/// the run's overall `Result` — a `FileSource` read failure is exactly the
/// "doesn't exist / unreadable" case `FileSource::read_file` already
/// reports as `Ok(None)`, not a hard error, and this function treats a rare
/// `Err` (e.g. a `GithubApi` source whose `gh` shell-out itself failed to
/// spawn) the same way: abstain, never fail the run over an observability
/// nicety.
pub fn apply_absence_backstop(judged: &mut [JudgedFlag], bundles: &[BundleInput], source: Option<&FileSource>) {
    let Some(source) = source else {
        return;
    };
    for j in judged.iter_mut() {
        if j.tier != Tier::Confirmed {
            continue;
        }
        let (note, evidence) = {
            let record = j.pass2.as_ref().unwrap_or(&j.pass1);
            (record.note_for_author.clone(), record.decisive_evidence.clone())
        };
        let text = format!("{note} {evidence}");
        if !is_absence_claim(&text) {
            continue;
        }
        let Some(token) = extract_claimed_absent_token(&text) else {
            continue;
        };
        // (PR #1765 merge-gate finding, MUST FIX 1) The token must be the
        // claimed-absent THING, not the subject of a missing operation —
        // see `claimed_token_immediately_follows_absence_phrase`'s doc.
        if !claimed_token_immediately_follows_absence_phrase(&text) {
            continue;
        }
        let Some(bundle) = bundles.iter().find(|b| b.id == j.flag.bundle_id) else {
            continue;
        };
        let Some(path) = bundle_file_path(&bundle.id) else {
            continue;
        };
        let Ok(Some(content)) = source.read_file(path) else {
            continue;
        };
        // (PR #1765 merge-gate finding, MUST FIX 2) A boundary-aware
        // check, never a bare substring — `content.contains(token)` would
        // match `id` inside `identifier`.
        if !contains_token_at_boundary(&content, &token) {
            // Genuinely absent from the whole file too — the claim holds.
            // Leave the flag exactly as the judge left it (the mandatory
            // inverted case: this backstop must not demote everything).
            continue;
        }
        let line = line_of_token_at_boundary(&content, &token);
        // Wording is deliberately narrow: the check knows the token is
        // PRESENT somewhere in the file, not that the claim is FALSE (a
        // scope-qualified finding — "not assigned on the error path" when
        // it IS assigned on the happy path — is true but still gets
        // demoted here, since whole-file presence is all this mechanical
        // check can see; PR #1765 merge-gate finding).
        let mechanical_note = match line {
            Some(n) => format!(
                "mechanical backstop: `{token}` found elsewhere in {path} at line {n} (the AI \
                 seat may have seen only a truncated excerpt, or this may be a different scope \
                 than the one claimed); demoted for a human double check"
            ),
            None => format!(
                "mechanical backstop: `{token}` found elsewhere in {path} (the AI seat may have \
                 seen only a truncated excerpt, or this may be a different scope than the one \
                 claimed); demoted for a human double check"
            ),
        };
        j.absence_backstop = Some(AbsenceBackstopNote { token: token.clone(), file: path.to_string(), line });
        j.tier = Tier::NeedsCheck;
        let record = match j.pass2.as_mut() {
            Some(r) => r,
            None => &mut j.pass1,
        };
        record.note_for_author = if record.note_for_author.trim().is_empty() {
            mechanical_note
        } else {
            format!("{}\n\n{mechanical_note}", record.note_for_author.trim())
        };
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
        items_in: Some(env.raw_flags),
        items_out: Some(deduped.len()),
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
    // Wrapped in a Mutex purely for signature unity with the parallel
    // mission path (#swarm-6): this loop is sequential, the lock is
    // uncontended, and `into_inner` below reads the final state without one.
    let judge_budgets = judge_endpoint.map(|_| {
        std::sync::Mutex::new(JudgeBudgets {
            pass1: RemoteBudget::with_stage("judge-pass1", inputs.remote_max_tokens_per_execution, MIN_VIABLE_JUDGE_GRANT),
            pass2: RemoteBudget::with_stage("judge-pass2", inputs.remote_max_tokens_per_execution, MIN_VIABLE_JUDGE_GRANT),
        })
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
            judge_budgets.as_ref(),
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
            absence_backstop: None,
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
        items_in: Some(deduped.len()),
        items_out: Some(deduped.len()),
        wall_ms: pass1_ms,
    });
    if pass2_flags > 0 {
        env.steps.push(StepRecord {
            step_id: "judge-pass2".to_string(),
            kind: "dispatch".to_string(),
            items_in: Some(pass2_flags),
            items_out: Some(pass2_flags),
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
    // Judging is complete — take the budgets back out of the mutex for the
    // read-only gate report (mirrors the parallel path's `into_inner`).
    let judge_budgets = judge_budgets
        .map(|m| m.into_inner().expect("judge budgets mutex poisoned"));
    let gate = judge_gate_outcome(
        judge.pm.is_remote(),
        judged.len(),
        usable,
        judge_dispatch_errors,
        judge_budgets.as_ref(),
        inputs.remote_max_tokens_per_execution,
        inputs.judge_exhaustion_strict,
    );
    if let Some(w) = gate.dispatch_error_warning {
        env.warnings.push(w);
    }
    if let Some(w) = gate.coverage_warning {
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
        env.degenerate_kind = Some(DegenerateKind::Error);
    }

    // (#1748) The mechanical, zero-token absence-claim backstop — runs
    // BEFORE the optional (AI, costlier) verify stage, so a
    // mechanically-contradicted claim never spends a verify dispatch
    // adjudicating it. `inputs.source` is `None` for most of this module's
    // own tests, making this a no-op; see `apply_absence_backstop`'s doc.
    apply_absence_backstop(&mut judged, bundles, inputs.source);

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
    // (#1877 item 2) `source: "review"` — the one hardcoded bit
    // `RunObs`/`step_result_record` took out and made a parameter, so this
    // driver's own `step result` records stay stamped `source="review"`,
    // byte-identical to before the move.
    let mut obs = ReviewObs::new(emitter, &inputs.case_id, &crew_name, "review");
    env.steps.push(StepRecord {
        step_id: "bundle".to_string(),
        kind: "procedural".to_string(),
        items_in: Some(1),
        items_out: Some(bundles.len()),
        wall_ms: bundle_ms,
    });
    obs.step_result("review.bundle", "bundle", json!({ "items_out": bundles.len() }));
    if flags.is_empty() {
        env.degenerate = Some("--charges-file carried zero flags".to_string());
        env.degenerate_kind = Some(DegenerateKind::Error);
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
    MapDispatchOverride, MapItemResult, OverrideDispatchCall, Port, StepKind, StepKindRegistry,
    StepOutcome, StepRunCtx, MAP_BUDGET_SKIP_ERROR,
};
use darkmux_crew::types::{Step, Task};
use std::any::Any;
use std::sync::Mutex as StdMutex;
// (#1530) The real bundler — `review-bundle-step`'s `run_streaming` now
// calls these directly (moved out of `src/mission_launch_review.rs`'s
// pre-graph prelude); see `ReviewBundleStepKind`'s doc.
use super::bundle::{
    build_bundles, external_bundles, slice_code, slice_code_probe, BundleSet, BundleSkipReport,
    FileSource, SkipReason,
};
use std::path::{Path, PathBuf};

// ─── #1530 Packets 0/1/3a: run-scoped ArtifactBus artifact names ──────────
//
// The review pipeline's three cross-cutting accumulators — historically
// bespoke `Arc<Mutex<_>>` handles threaded by hand into `ReviewDedupStepKind`
// /`ReviewJudgeStepKind`/`ReviewVerifyRenderStepKind`/`ReviewSynthesisStepKind`
// (built once in `build_review_graph`, read back after `run_step_graph`
// returns in `run_review_graph`) — now ride the generic run-scoped
// `ArtifactBus` (#1530 Packet 0) instead. `ReviewDedupStepKind::provides()`
// declares all three (it is the pipeline's earliest of the four consumers,
// and the scheduler's `provides()` pre-scan runs before ANY wave, so
// declaring them there is sufficient regardless of wave order — see
// `scheduler::run_step_graph`'s own pre-scan doc); the const factories build
// EMPTY defaults. `run_review_graph` then SEEDS the real, run-stamped values
// over those defaults via `run_step_graph`'s caller-seed path (#1530 Packet
// 1) — the envelope needs to carry this run's case_id/crew/mode/fingerprint/
// staffing (plus the interpret-time warnings `build_review_graph` already
// collected) before any step reads it, which a context-free `Port::artifact`
// factory structurally cannot produce (see `Port`'s own doc).
//
// (#1530 Packet 3a) The context (`ReviewStepContext`, diff/prompts/bundles/…)
// USED to stay a plain constructor field on every kind below — Packet 1's
// own doc note (superseded here) explained why: `ReviewJudgeStepKind::
// residency()` reads `ctx.bundles` to decide whether to skip loading a model
// (#1426 ship-2), and `StepKind::residency` had NO `StepRunCtx` parameter at
// all, so a bus-only context would have silently broken that optimization.
// Packet 3a closes that gap at the ROOT instead of working around it:
// `StepKind::residency` now takes the SAME `&StepRunCtx` `run_streaming`
// does (see that trait method's own doc), so the context can move onto the
// bus, under `REVIEW_CONTEXT_ARTIFACT`, exactly like the three accumulators
// above — `make_review_context_artifact` builds a context-free
// `ReviewStepContext::default()` (the type gained `Clone`/`Default` derives
// FOR this; see its own doc), and `run_review_graph` seeds the real value
// over it via the SAME `seed_artifacts` call the accumulators already ride.
// Every review kind is now a stateless singleton: no kind holds an
// `Arc<ReviewStepContext>` (or a `ResolvedSeatStaffing` — the judge/verify
// seat's model+prompt moved to `Step.config`, stamped by
// `build_review_graph_from_config` the same way it already stamps the probe
// seats' `dispatch.map` config; see that function's own doc) as a
// constructor field; everything comes from `step.config` or this bus.
//
// (#1530, bundling-becomes-runtime-work follow-on) The paragraph above
// describes why `residency()` gained a `&StepRunCtx` — at the time, THAT
// was the only reason: `ctx.bundles` was a build-time snapshot, resolved
// before the graph ever ran, that just needed a bus seam to reach a trait
// method with no `StepRunCtx` parameter yet. `ReviewStepContext` no longer
// carries `bundles` at all — bundling itself is now `review-bundle-step`'s
// own run-time work (`ReviewBundleStepKind::run_streaming`), published onto
// its OWN artifact ([`REVIEW_BUNDLES_ARTIFACT`]) rather than folded into the
// context. `ReviewJudgeStepKind::residency()` now reads that artifact
// directly instead of `ctx.bundles`.
const REVIEW_ENVELOPE_ARTIFACT: &str = "review.envelope";
const REVIEW_MEMBERS_ARTIFACT: &str = "review.members";
const REVIEW_WARNINGS_ARTIFACT: &str = "review.warnings";
const REVIEW_CONTEXT_ARTIFACT: &str = "review.context";
/// (#1541) Per-seat probe bundle ATTRIBUTION, published by
/// [`ReviewProbeRenderStepKind::run_streaming`] and consumed by
/// [`reconstruct_probe_stage`] via [`ReviewDedupStepKind::run_streaming`].
/// Keyed by the probe TASK id (the same key `gather_inputs` already uses for
/// that task's dispatch results — see `ProbeSeatSpec::draw_task_ids`'s doc),
/// mapping to the ORDERED `(bundle_id, fact_family)` pairs the render step
/// selected for that task, index-aligned with the prompt collection it
/// emitted as `Step.output`. Before this artifact, `reconstruct_probe_stage`
/// aligned a probe seat's `dispatch.map` results back to bundles
/// POSITIONALLY against a build-time snapshot (`ProbeSeatSpec.bundles`,
/// retired — see git history) that only agreed with the run-time selection
/// because both sides called the same pure function over the same bundles.
/// Publishing the render step's ACTUAL selection onto the bus makes
/// attribution travel through the graph instead of relying on that
/// coincidence — see #1541 for the full failure mode this closes.
const REVIEW_PROBE_SELECTION_ARTIFACT: &str = "review.probe-selection";
/// (#1530) The resolved bundle set — published by
/// [`ReviewBundleStepKind::run_streaming`], the pipeline's new EARLIEST
/// data-producing step, and read by every downstream kind that used to read
/// `ReviewStepContext::bundles` directly: [`ReviewProbeRenderStepKind`]'s
/// selection, [`ReviewJudgeStepKind`]'s per-flag bundle lookup AND its
/// `residency()` skip-load check, and [`ReviewVerifyRenderStepKind`]'s
/// per-finding bundle lookup. Genuinely run-time data with no build-time
/// equivalent now (the whole point of this packet — bundling used to run in
/// `src/mission_launch_review.rs`'s pre-graph prelude; see
/// `ReviewBundleStepKind`'s doc), so [`make_review_bundles_artifact`]'s empty
/// default is the REAL starting state, mirroring
/// [`REVIEW_PROBE_SELECTION_ARTIFACT`]'s own "never caller-seeded" shape —
/// never [`REVIEW_CONTEXT_ARTIFACT`]'s "caller always overwrites the
/// default" one. `ReviewJudgeStepKind`'s task depends (transitively, via
/// dedup + the probe tasks) on `review-bundle-task`, so by the time its wave
/// runs — the ONLY place this artifact is read on a production path — the
/// bundle step's wave has already completed and this is populated; see
/// `build_review_graph_from_config`'s own doc for the depends_on chain.
const REVIEW_BUNDLES_ARTIFACT: &str = "review.bundles";

fn make_review_envelope_artifact() -> Arc<dyn Any + Send + Sync> {
    Arc::new(StdMutex::new(ReviewEnvelope::default()))
}

/// (#1530) Context-free default for [`REVIEW_BUNDLES_ARTIFACT`] — see that
/// constant's own doc for why empty is the real starting state, not a
/// placeholder some caller-seed later overwrites.
fn make_review_bundles_artifact() -> Arc<dyn Any + Send + Sync> {
    Arc::new(StdMutex::new(Vec::<BundleInput>::new()))
}

/// (#1541) Context-free default for [`REVIEW_PROBE_SELECTION_ARTIFACT`] — an
/// empty map, populated in place by every claimed probe seat's
/// `review.probe-render` step as it runs (one `insert` per seat, keyed by
/// that seat's probe task id). Unlike [`REVIEW_CONTEXT_ARTIFACT`] this is
/// never caller-seeded — genuinely run-time data with no build-time
/// equivalent — so the empty default here is the REAL starting state, not a
/// placeholder `run_review_graph` overwrites.
fn make_review_probe_selection_artifact() -> Arc<dyn Any + Send + Sync> {
    Arc::new(StdMutex::new(std::collections::BTreeMap::<String, Vec<(String, String)>>::new()))
}

/// (#1530 Packet 3a) Context-free default for [`REVIEW_CONTEXT_ARTIFACT`] —
/// ALWAYS overwritten by `run_review_graph`'s caller-seed before any step
/// reads it (see this constant's module-doc note). Unlike the envelope/
/// members/warnings accumulators, this artifact is never mutated in place —
/// every reader gets a read-only `Arc<ReviewStepContext>` (the SAME shape
/// each kind used to hold as a constructor field), so it needs no
/// `StdMutex` wrapper.
fn make_review_context_artifact() -> Arc<dyn Any + Send + Sync> {
    Arc::new(ReviewStepContext::default())
}

fn make_review_members_artifact() -> Arc<dyn Any + Send + Sync> {
    Arc::new(StdMutex::new(Vec::<MemberRecord>::new()))
}

fn make_review_warnings_artifact() -> Arc<dyn Any + Send + Sync> {
    Arc::new(StdMutex::new(Vec::<String>::new()))
}

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
///
/// (#1530 Packet 3a) No longer a per-kind CONSTRUCTOR field — every review
/// `StepKind` (bundle/dedup/judge/verify-render/synthesis) used to hold its
/// own `Arc<ReviewStepContext>`, which is exactly the per-run state the
/// #1530 arc's stateless-singleton goal retires. It now lives on the run's
/// `ArtifactBus` under [`REVIEW_CONTEXT_ARTIFACT`], materialized by
/// [`make_review_context_artifact`]'s context-free `ReviewStepContext::
/// default()` and overwritten by `run_review_graph`'s caller-seed with the
/// REAL, run-stamped value — the exact same seed-over-factory-default
/// pattern [`REVIEW_ENVELOPE_ARTIFACT`] already established in Packet 1.
/// `Clone`/`Default` are new derives added FOR this purpose (every field is
/// itself `Clone`/`Default`); nothing about the type's per-field semantics
/// changes. Every kind's `run_streaming` (and `ReviewJudgeStepKind::
/// residency`, which gained bus access for exactly this read) now looks it
/// up via `ctx.artifact::<ReviewStepContext>(REVIEW_CONTEXT_ARTIFACT)`
/// instead of reading `self.ctx` — same `&ReviewStepContext` shape at every
/// read site, only the SOURCE of the `Arc` changed.
#[derive(Clone, Default)]
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
    /// (#1530 follow-on, Packet A1) Per-PROBE-ROLE resolved system prompt —
    /// `role_id` (`"review-probe-high"`/`-mid`/`-low`) -> that role's OWN
    /// `role_prompt()` text, resolved once by the launcher
    /// (`src/mission_launch_review.rs`, `review_bench.rs`'s
    /// `resolve_funnel_ctx`), already falling back to `probe_system` above
    /// when a seat's specific role has no `.md` of its own. A key ABSENT
    /// from this map (every hand-built test fixture, which never
    /// populates it) makes [`ReviewProbeRenderStepKind::run_streaming`]'s
    /// own lookup fall through to `probe_system` too — the exact
    /// pre-fix behavior, byte-identical, since the three shipped
    /// `review-probe-high/-mid/-low.md` files are byte-copies of
    /// `review-probe.md` today (this map is a capability fix for when an
    /// operator diverges one of them, not a behavior change on its own).
    pub probe_role_prompts: std::collections::BTreeMap<String, String>,
    pub judge_system: String,
    pub verify_system: String,
    pub remote_max_tokens_per_execution: u64,
    /// (#1876/#1877) See [`ReviewInputs::judge_exhaustion_strict`]'s doc —
    /// same knob, same injection discipline, the graph path's own copy.
    /// Defaults to `false` (the partial-coverage policy) via `#[derive(Default)]`
    /// on this struct — every test-fixture construction that doesn't
    /// explicitly opt into `true` gets the operator-default behavior.
    pub judge_exhaustion_strict: bool,
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
    /// (#1530) Test/bench-only seam for [`ReviewBundleStepKind::
    /// run_streaming`] — `None` at the one PRODUCTION call site
    /// (`src/mission_launch_review.rs`'s graph path), which always falls
    /// through to reconstructing a real `FileSource` from `Step.config` and
    /// calling the real `build_bundles`/`external_bundles` — the whole point
    /// of this packet (#1530: "no data-producing work before graph
    /// execution"). When `Some`, the bundle step publishes this closure's
    /// result onto [`REVIEW_BUNDLES_ARTIFACT`] directly instead of touching
    /// the filesystem/network/an external command — the same "no seam for
    /// real I/O" problem [`chat_override`]'s own doc names for dispatch,
    /// applied to bundling.
    ///
    /// Two callers use it, deliberately: hermetic graph tests (synthetic
    /// `BundleInput`s, no real worktree or GitHub API call needed) AND
    /// `review_bench.rs`'s bench harness, which keeps its EAGER bundling
    /// (same real `build_bundles`/`external_bundles`/`slice_code*` calls, run
    /// before the graph starts — unchanged from before this packet) and
    /// hands the result through this seam. Bench is a per-run-local
    /// measurement tool, not the `mission launch review` launcher this
    /// packet's invariant targets — same out-of-scope reasoning as the
    /// `charges_file` re-judge side path (see `run_dispatch`'s own doc in
    /// `src/mission_launch_review.rs`), so this is a deliberate scope
    /// decision, not an oversight.
    ///
    /// Every downstream reader (probe-render/judge/verify-render) still
    /// reads ONLY [`REVIEW_BUNDLES_ARTIFACT`] off the bus, never this field
    /// directly — the override only changes how the bundle STEP fills that
    /// artifact, not who reads it.
    #[allow(clippy::type_complexity)]
    pub bundle_override: Option<Arc<dyn Fn() -> Result<Vec<BundleInput>> + Send + Sync>>,
    /// (#1641) The run's own Mission identity, carried as an OPAQUE tag
    /// string only — this module does not depend on `darkmux_crew::
    /// lifecycle` and does not resolve/create Mission records (the
    /// crate-boundary note above this struct's own doc), so this is
    /// deliberately just a string the CALLER already minted, not a live
    /// lookup. `Some(&mission_id)` for a real `mission launch review`
    /// (stamped by `src/mission_launch_review.rs` once `mission_launch::
    /// mint_run_id` has minted it), `None` for every lab-bench run
    /// (`review_bench.rs` — per-run-local, no Mission — lab/fleet sink
    /// boundary) and the `--charges-file` judge-only path (mints no
    /// Mission at all).
    ///
    /// Threaded into this module's OWN directly-emitted records
    /// ([`emit_review_step_result`], [`emit_review_token_telemetry`],
    /// [`apply_verify_results`]) — the ones that write straight to the
    /// global flow sink because they run inside `run_bounded` worker
    /// threads and can't hold the caller-injected [`ReviewEmitter`] (see
    /// [`emit_review_step_result`]'s own doc). Records that instead route
    /// through `run_step_graph`'s `emit` closure (the scheduler's generic
    /// step-lifecycle bookends, `dispatch.map`'s per-item records) get
    /// their `mission_id` backfilled at the LAUNCHER level instead
    /// (`FleetFlowEmitter`, `src/mission_launch_review.rs`) — this field
    /// covers the other, structurally-separate gap.
    pub mission_id: Option<String>,
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
        emit_review_token_telemetry(&ctx.case_id, ctx.mission_id.as_deref(), call.model, &reply);
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
    emit_review_token_telemetry(&ctx.case_id, ctx.mission_id.as_deref(), call.model, &reply);
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
///
/// `mission_id` (#1641): threaded from [`ReviewStepContext::mission_id`] —
/// this function writes straight to the global flow sink (it's called from
/// [`dispatch_chat`], which can run on a `run_bounded` worker thread with no
/// injected emitter to hold), so it's a producer this run's `mission_id`
/// would otherwise never reach, unlike records that route through
/// `run_step_graph`'s `emit` closure (backfilled at the launcher instead —
/// see [`ReviewStepContext::mission_id`]'s own doc).
fn emit_review_token_telemetry(
    case_id: &str,
    mission_id: Option<&str>,
    model: &str,
    reply: &SingleShotReply,
) {
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
        mission_id,
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
///
/// `mission_id` (#1641): `None` on the `run_judge_only` path (mints no
/// Mission — [`ReviewObs::step_result`]'s caller always passes `None`), and
/// on the [`ReviewStepContext`] passed in on the graph path, `Some` iff this
/// run's launcher minted one — see that field's own doc for why it's a
/// caller-supplied opaque tag rather than a live lookup.
fn review_step_result_record(
    kind: &str,
    step_id: &str,
    case_id: &str,
    mission_id: Option<&str>,
    payload: serde_json::Value,
) -> darkmux_flow::FlowRecord {
    // (#1877 item 2) The record shape itself moved to
    // `darkmux_crew::run_obs::step_result_record`, generalized so `source`
    // (hardcoded `"review"` here, always) is a parameter instead of a
    // literal baked into the builder — this wrapper is what keeps every
    // caller in this file byte-identical to before the move.
    run_obs::step_result_record("review", kind, step_id, case_id, mission_id, payload)
}

/// Emit a [`review_step_result_record`] to the GLOBAL flow sink
/// (`darkmux_flow::record`). Used by the graph step kinds, which run inside
/// the scheduler's `run_bounded` worker threads and so can't hold the
/// caller-injected emitter (`ReviewObs` covers the sequential path instead).
fn emit_review_step_result(
    kind: &str,
    step_id: &str,
    case_id: &str,
    mission_id: Option<&str>,
    payload: serde_json::Value,
) {
    let _ =
        darkmux_flow::record(review_step_result_record(kind, step_id, case_id, mission_id, payload));
}

// ─── investigate: bundle ────────────────────────────────────────────────

/// `Bundle` (`darkmux_lab::lab::bundle`) -> `BundleInput` (this module's own
/// shape). Each bundle's line-span pointers are rendered PER SEAT (#1256):
/// `slice_code` (the judge's `// path` raw format) into `code`,
/// `slice_code_probe` (the probe's fenced-code format) into `probe_code`.
///
/// (#1530) Moved here from `src/mission_launch_review.rs` — the ONLY caller
/// left in that file is the `charges_file` re-judge side path (not a graph;
/// see `run_dispatch`'s doc), which still needs it directly. The main
/// dispatch path's own call now lives in [`ReviewBundleStepKind::
/// run_streaming`], right next to `build_bundles`/`external_bundles`.
pub fn bundle_inputs_from_set(set: &BundleSet, source: &FileSource) -> Result<Vec<BundleInput>> {
    set.bundles
        .iter()
        .map(|b| {
            let code = slice_code(source, &b.code)
                .with_context(|| format!("slicing code for bundle \"{}\"", b.id))?;
            let probe_code = slice_code_probe(source, &b.code)
                .with_context(|| format!("probe-slicing code for bundle \"{}\"", b.id))?;
            Ok(BundleInput {
                id: b.id.clone(),
                fact_family: b.fact_family.clone(),
                code,
                probe_code,
                facts: b.facts.clone(),
                manifest: b.manifest.clone(),
            })
        })
        .collect()
}

/// (#1530) Which diff SOURCE `review-bundle-step` reconstructs a
/// [`FileSource`] from at run time — the launcher's already-validated
/// `--param worktree=<dir>` / `--param github=<repo> --param head_sha=<sha>`
/// resolution (`mission_launch_review::resolve_source`), carried as plain
/// data instead of the un-serializable `FileSource` itself (which holds a
/// `RefCell` fetch cache). Constructed by the launcher/bench caller, never
/// by the graph itself — see [`BundleBuildSpec`].
pub enum BundleSourceSpec {
    Worktree { path: PathBuf },
    Github { repo: String, head_sha: String },
}

/// (#1530) Everything `review-bundle-step`'s config needs to reconstruct a
/// [`FileSource`] and run the bundler AT RUN TIME — the launcher's
/// already-resolved diff source, optional external `--bundler` command, and
/// the diff file path (`external_bundles` shells out `<cmd> --worktree <dir>
/// --diff <file>`, so it needs a real path, not just the diff TEXT already
/// on [`ReviewStepContext::diff`]). Stamped once, at build time, into
/// `review-bundle-step`'s `Step.config` by [`build_review_graph_from_config`]
/// — the same "compute at build time, stamp, read back in `run_streaming`"
/// pattern every other per-seat config on this graph already uses. This is
/// the packet's whole point: everything here is DATA the launcher already
/// had before the graph ever ran; only the bundler INVOCATION itself (the
/// file reads / `gh api` calls / external-command shell-out) moves to run
/// time, inside [`ReviewBundleStepKind::run_streaming`].
pub struct BundleBuildSpec {
    pub source: BundleSourceSpec,
    pub bundler: Option<String>,
    pub diff_file: PathBuf,
}

/// `BundleSourceSpec` -> the `"source"` JSON block both `review-bundle-step`
/// AND (#1748) `review-judge-step` stamp onto their own `Step.config` — the
/// SAME shape, so ONE reader ([`file_source_from_step_config`]) reconstructs
/// a [`FileSource`] for either step. Extracted so the two stamp sites (see
/// `build_review_graph_from_config`) can't drift from each other.
fn bundle_source_spec_json(source: &BundleSourceSpec) -> serde_json::Value {
    match source {
        BundleSourceSpec::Worktree { path } => {
            json!({ "kind": "worktree", "path": path.display().to_string() })
        }
        BundleSourceSpec::Github { repo, head_sha } => {
            json!({ "kind": "github", "repo": repo, "head_sha": head_sha })
        }
    }
}

/// Reconstruct a [`FileSource`] from a `Step.config` that carries a
/// `"source"` block in [`bundle_source_spec_json`]'s shape — originally
/// `review-bundle-step`'s own contract (stamped by
/// `build_review_graph_from_config` from a [`BundleBuildSpec`] — see that
/// type's doc), and (#1748) reused verbatim by `review-judge-step`'s
/// [`apply_absence_backstop`] wiring, which stamps the SAME block onto its
/// own config. A malformed shape here is a BUILD-TIME bug (the launcher/
/// bench caller mis-stamped the config), not an operator input mistake, so
/// this errors loudly rather than defaulting — contract 7 (loud validation
/// at the consumption point). Callers for whom a missing `"source"` is a
/// legitimate, tolerable case (the judge step's backstop, on a hand-built
/// `Step.config` with no source at all) use `.ok()` at the call site rather
/// than this function silently defaulting — the function stays one honest
/// contract either way.
fn file_source_from_step_config(config: &serde_json::Value) -> Result<FileSource> {
    let source = config
        .get("source")
        .context("darkmux: review-bundle-step config is missing \"source\"")?;
    let kind = source
        .get("kind")
        .and_then(|v| v.as_str())
        .context("darkmux: review-bundle-step config \"source.kind\" is missing or not a string")?;
    match kind {
        "worktree" => {
            let path = source.get("path").and_then(|v| v.as_str()).context(
                "darkmux: review-bundle-step config \"source.path\" is missing (worktree source)",
            )?;
            Ok(FileSource::worktree(path))
        }
        "github" => {
            let repo = source.get("repo").and_then(|v| v.as_str()).context(
                "darkmux: review-bundle-step config \"source.repo\" is missing (github source)",
            )?;
            let head_sha = source.get("head_sha").and_then(|v| v.as_str()).context(
                "darkmux: review-bundle-step config \"source.head_sha\" is missing (github source)",
            )?;
            Ok(FileSource::github_api(repo, head_sha))
        }
        other => bail!(
            "darkmux: review-bundle-step config \"source.kind\" = \"{other}\" is not recognized \
             (expected \"worktree\" or \"github\")"
        ),
    }
}

/// Phase "investigate", step 1: resolves the review's bundle set AT RUN
/// TIME — the pipeline's own earliest data-producing step (#1530: "no graph
/// does data-producing work before graph execution — every graph runs the
/// same way"). Before this packet, `src/mission_launch_review.rs::
/// run_dispatch` resolved the diff source and called `build_bundles`/
/// `external_bundles` in a PRE-GRAPH prelude, then handed the finished
/// `Vec<BundleInput>` to `build_review_graph` as `ReviewStepContext::
/// bundles` — meaning nothing upstream of the graph (a coder step, say)
/// could ever feed review, because by the time ANY step ran, the bundles
/// were already required launch input. This kind does the SAME work
/// (`build_bundles`/`external_bundles` + `bundle_inputs_from_set` are
/// UNCHANGED — only WHERE they're called moved), reconstructing a
/// [`FileSource`] from `Step.config` (stamped by
/// [`build_review_graph_from_config`] from a [`BundleBuildSpec`] — see that
/// type's doc for why everything it needs is build-time-known data) and
/// publishing the result onto [`REVIEW_BUNDLES_ARTIFACT`] for every
/// downstream step.
///
/// **Tier 3 (#1352), on purpose.** Diff-parsing/bundle-resolution is
/// genuinely specific to the review pipeline — no second consumer is
/// visible today, and its whole job is unwrapping THIS module's own
/// `Step.config` shape. Stays physically co-located here, not moved to
/// `darkmux-crew`'s `step_kinds` — see that crate's `step_kinds::patterns`
/// module doc for the three-tier picture this classification follows.
///
/// (#1530 Packet 3a) A stateless singleton — no `Arc<ReviewStepContext>`
/// constructor field. It reads the run-scoped context off the `ArtifactBus`
/// instead (see [`REVIEW_CONTEXT_ARTIFACT`]'s module-doc note) — needed here
/// only for `ctx.case_id` (step-result logging) and `ctx.diff` (the diff
/// TEXT `build_bundles` needs; `external_bundles` instead needs the diff
/// FILE path, carried on `Step.config` since it shells a real command out to
/// it) and the `bundle_override` test seam.
pub struct ReviewBundleStepKind;

impl StepKind for ReviewBundleStepKind {
    fn id(&self) -> &'static str {
        "review.bundle"
    }

    fn display_name(&self) -> &'static str {
        "Bundle"
    }

    /// (#1530) `REVIEW_BUNDLES_ARTIFACT` is new here — this kind is its ONLY
    /// writer. The ordinary `Step.output` `Data` port and
    /// `REVIEW_CONTEXT_ARTIFACT` are unchanged from before this packet; this
    /// is still the pipeline's EARLIEST consumer of the run-scoped context
    /// (investigate phase, step 1), so declaring `provides()` here is
    /// sufficient for the scheduler's pre-scan regardless of which wave the
    /// other consumers land in (mirrors `ReviewDedupStepKind::provides()`'s
    /// own reasoning for its three accumulators). `run_review_graph`'s
    /// caller-seed always overwrites the context factory's context-free
    /// default with the real, run-stamped value before any step reads it;
    /// `REVIEW_BUNDLES_ARTIFACT`'s empty default is never caller-seeded —
    /// this kind is what fills it, at run time (see that constant's doc).
    fn provides(&self) -> &'static [Port] {
        const PORTS: [Port; 3] = [
            Port::data("bundles"),
            Port::artifact(REVIEW_CONTEXT_ARTIFACT, make_review_context_artifact),
            Port::artifact(REVIEW_BUNDLES_ARTIFACT, make_review_bundles_artifact),
        ];
        &PORTS
    }

    /// (#1530) `requires()` only for `REVIEW_ENVELOPE_ARTIFACT` — this kind
    /// WRITES the resolved bundle count onto it (mirroring the pattern every
    /// other artifact-writing kind in this file uses: declare `requires()`
    /// for an artifact some OTHER kind's `provides()` already materializes,
    /// per `ReviewJudgeStepKind::requires()`'s own doc). `ReviewDedupStepKind::
    /// provides()` already declares `REVIEW_ENVELOPE_ARTIFACT`, and the
    /// scheduler's pre-scan runs across every step kind present in the
    /// graph before ANY wave (review-dedup-task is always present), so this
    /// is materialized before `review-bundle-step`'s wave runs regardless.
    fn requires(&self) -> &'static [Port] {
        const PORTS: [Port; 1] = [Port::artifact(REVIEW_ENVELOPE_ARTIFACT, make_review_envelope_artifact)];
        &PORTS
    }

    fn run(&self, _s: &Step, _t: &Task, _i: &std::collections::BTreeMap<String, String>) -> Result<StepOutcome> {
        panic!(
            "ReviewBundleStepKind only runs through `run_streaming` — it reads the \
             run-scoped ArtifactBus (#1530 Packet 3a)"
        )
    }

    fn run_streaming(
        &self,
        step: &Step,
        _task: &Task,
        _input: &std::collections::BTreeMap<String, String>,
        run_ctx: &StepRunCtx,
    ) -> Result<StepOutcome> {
        let ctx = run_ctx
            .artifact::<ReviewStepContext>(REVIEW_CONTEXT_ARTIFACT)
            .expect("run_review_graph seeds the context artifact before the graph runs");

        // (#1530) The REAL work — resolve the diff source, run the bundler,
        // slice the code per seat — all UNCHANGED from what
        // `src/mission_launch_review.rs`'s pre-graph prelude used to do; it
        // just runs HERE now, at run time, instead of before the graph was
        // even built. `bundle_override` (`None` on the production
        // `mission launch review` path — see its own doc) skips all of it
        // for a hermetic graph test, or lets `review_bench.rs` hand in
        // bundles it still computes eagerly for its own reasons.
        // (#1605) `bundle_skip` carries the bundler's own per-file decline
        // accounting out of the `else` branch below (where `bundle_set`
        // lives) so it can be stamped onto the shared envelope alongside
        // `bundles` itself. Stays `None` on the `bundle_override` test seam
        // — that path hands in `Vec<BundleInput>` directly, with no
        // `BundleSet` (and so no skip bookkeeping) behind it.
        let mut bundle_skip: Option<BundleSkipReport> = None;
        let bundle_inputs: Vec<BundleInput> = if let Some(over) = &ctx.bundle_override {
            over()?
        } else {
            let source = file_source_from_step_config(&step.config)?;
            let bundler = step.config.get("bundler").and_then(|v| v.as_str());
            let bundle_set = match bundler {
                Some(cmd) => {
                    let diff_file = step
                        .config
                        .get("diff_file")
                        .and_then(|v| v.as_str())
                        .context("darkmux: review-bundle-step config is missing \"diff_file\"")?;
                    let worktree = match &source {
                        FileSource::Worktree(p) => Some(p.as_path()),
                        FileSource::GithubApi { .. } => None,
                    };
                    external_bundles(cmd, worktree, Path::new(diff_file))?
                }
                None => build_bundles(&source, &ctx.diff)?,
            };
            bundle_skip = Some(bundle_set.skip.clone());
            bundle_inputs_from_set(&bundle_set, &source)?
        };

        // Publish for every downstream reader (probe-render's selection,
        // judge's per-flag lookup + residency skip-load check,
        // verify-render's per-finding lookup — see `REVIEW_BUNDLES_ARTIFACT`'s
        // own doc for the full reader list).
        *run_ctx
            .artifact::<StdMutex<Vec<BundleInput>>>(REVIEW_BUNDLES_ARTIFACT)
            .expect("this kind's own provides() materializes review.bundles")
            .lock()
            .expect("review bundles mutex poisoned") = bundle_inputs.clone();

        // (#1530) The envelope's `bundles` count used to be stamped once at
        // BUILD time (`ctx.bundles.len()` in `build_review_graph_from_config`'s
        // `initial_env`) — now that the real count isn't known until this
        // step runs, it's written here instead, into the SAME shared
        // envelope every other step reads/writes through
        // `REVIEW_ENVELOPE_ARTIFACT`. This runs in the graph's FIRST wave,
        // well before `ReviewSynthesisStepKind` reads `env.bundles` (the
        // "no bundles produced from the diff" degenerate gate) or serializes
        // its own envelope snapshot, so both see the real count.
        {
            let env_artifact = run_ctx
                .artifact::<StdMutex<ReviewEnvelope>>(REVIEW_ENVELOPE_ARTIFACT)
                .expect("review-dedup-task's provides() materializes review.envelope");
            let mut env = env_artifact.lock().expect("shared review envelope mutex poisoned");
            env.bundles = bundle_inputs.len();
            // (#1605) Stamped alongside `bundles` for the same reason — the
            // synthesis step's zero-bundle degenerate gate reads this to
            // build a REASONED message instead of a bare count.
            env.bundle_skip = bundle_skip;
        }

        let output = serde_json::to_string(&bundle_inputs).context("serializing bundles")?;
        emit_review_step_result(
            "review.bundle",
            &step.id,
            &ctx.case_id,
            ctx.mission_id.as_deref(),
            json!({ "items_out": bundle_inputs.len() }),
        );
        Ok(StepOutcome { output, flow_records: Vec::new() })
    }
}

// ─── investigate: probe render (prompt render → generic dispatch.map) ───

/// (#1530 follow-on, Packet A1) Phase "investigate", step 1 of EACH probe
/// TASK: render one seat's `probe_user_message` collection AT RUN TIME —
/// the probe stage's own version of the render → generic `dispatch.map`
/// split [`ReviewVerifyRenderStepKind`] already established for the verify
/// stage (see that kind's doc for the shared shape this mirrors exactly:
/// a Tier-3 render step mints a JSON array as its `Step.output`, and the
/// task's SECOND step, a generic `dispatch.map` with no `config.collection`
/// stamped, resolves that array as its collection via
/// `resolve_map_collection`'s single-dependency fallback).
///
/// **Why this exists.** Before this kind, `build_review_graph_from_config`
/// called `select_bundles_for_staffing` + `probe_user_message` ONCE, at
/// graph-BUILD time, and stamped the rendered collection directly into the
/// probe seat's `dispatch.map` step (`config.collection`) — frozen before
/// the graph ever ran. That blocked extending the graph upstream of the
/// probe stage: nothing could feed a runtime-computed bundle set in: the
/// selection was already baked into static config by the time any step
/// executed. Moving the SAME two calls into a run-time step, over
/// byte-identical inputs, makes the probe stage's data flow through the
/// graph like every other stage's instead of being computed ahead of it —
/// `probe_user_message`/`select_bundles_for_staffing` themselves are
/// UNCHANGED; only WHERE they're called moves.
///
/// **Tier 3 (#1352), on purpose** — same reasoning as
/// `ReviewVerifyRenderStepKind`: this renders against THIS pipeline's own
/// `BundleInput`/`BundleSelector` types; the probe's dispatch half stays on
/// the generic `dispatch.map` builtin.
///
/// **Stateless singleton (#1530 Packet 3a discipline), one shared instance
/// across every probe task.** No `Arc<ReviewStepContext>`, no
/// `ResolvedSeatStaffing` constructor field, and (unlike the retired
/// per-instance-suffixed `review.probe:<seat>` kind) no per-seat id either
/// — every probe task's render step resolves through the SAME registered
/// `"review.probe-render"` kind. Which seat a given run is rendering for
/// comes entirely from `step.config`, stamped per-seat by
/// `build_review_graph_from_config`'s probe loop: `selector` (this seat's
/// [`BundleSelector`], `null` for "no restriction") and `role_id` (for the
/// per-seat prompt lookup below).
pub struct ReviewProbeRenderStepKind;

impl StepKind for ReviewProbeRenderStepKind {
    fn id(&self) -> &'static str {
        "review.probe-render"
    }

    fn display_name(&self) -> &'static str {
        "Probe prompts"
    }

    /// `requires()` only — this kind consumes `REVIEW_CONTEXT_ARTIFACT` (for
    /// `ctx.probe_system`/`ctx.probe_role_prompts`) and, since #1530,
    /// `REVIEW_BUNDLES_ARTIFACT` (the bundle set, published by
    /// `ReviewBundleStepKind::run_streaming` — this task depends directly on
    /// `review-bundle-task`, so it's always populated by the time this runs)
    /// but produces none of the three shared accumulators; `ReviewDedupStepKind::
    /// provides()`'s doc explains why a downstream consumer declares
    /// `requires()` rather than re-`provides()`ing an artifact another kind
    /// already does.
    fn requires(&self) -> &'static [Port] {
        const PORTS: [Port; 2] = [
            Port::artifact(REVIEW_CONTEXT_ARTIFACT, make_review_context_artifact),
            Port::artifact(REVIEW_BUNDLES_ARTIFACT, make_review_bundles_artifact),
        ];
        &PORTS
    }

    /// (#1541) Declares [`REVIEW_PROBE_SELECTION_ARTIFACT`] — this kind is
    /// the one and only PRODUCER (every claimed probe seat's render step
    /// `insert`s its own entry as it runs; `ReviewDedupStepKind::requires()`
    /// reads the finished map back at the dedup boundary). Declaring the
    /// `provides()` here, on the actual producer, rather than "the earliest
    /// consumer" (the convention the three older accumulators use, per
    /// `ReviewDedupStepKind::provides()`'s doc) is deliberate: this artifact
    /// has no build-time equivalent for `run_review_graph` to caller-seed,
    /// so the factory default genuinely IS the run's starting state, and the
    /// producer is the natural, honest owner of that declaration.
    fn provides(&self) -> &'static [Port] {
        const PORTS: [Port; 2] = [
            Port::data("probe-prompts"),
            Port::artifact(REVIEW_PROBE_SELECTION_ARTIFACT, make_review_probe_selection_artifact),
        ];
        &PORTS
    }

    fn run(&self, _s: &Step, _t: &Task, _i: &std::collections::BTreeMap<String, String>) -> Result<StepOutcome> {
        panic!(
            "ReviewProbeRenderStepKind only runs through `run_streaming` — it reads the \
             run-scoped ArtifactBus (#1530 follow-on)"
        )
    }

    fn run_streaming(
        &self,
        step: &Step,
        task: &Task,
        _input: &std::collections::BTreeMap<String, String>,
        run_ctx: &StepRunCtx,
    ) -> Result<StepOutcome> {
        let ctx = run_ctx
            .artifact::<ReviewStepContext>(REVIEW_CONTEXT_ARTIFACT)
            .expect("run_review_graph seeds the context artifact before the graph runs");

        // Stamped by `build_review_graph_from_config`'s probe loop at build
        // time — this seat's bundle selector (absent/null when the seat
        // runs unrestricted over every bundle, matching
        // `select_bundles_for_staffing`'s own `None` contract) and role id
        // (for the per-seat prompt lookup below). Both are known at build
        // time; only the SELECTION ITSELF (which needs the run's bundle
        // set, only stable once `review-bundle-task` has run — see
        // `REVIEW_BUNDLES_ARTIFACT`'s doc) moves to run time — see this
        // kind's own doc.
        let selector: Option<BundleSelector> = match step.config.get("selector") {
            None => None,
            Some(v) if v.is_null() => None,
            Some(v) => {
                Some(serde_json::from_value(v.clone()).context("deserializing probe-render selector")?)
            }
        };
        let role_id = step.config.get("role_id").and_then(|v| v.as_str());

        // (#1530 follow-on) Per-seat prompt resolution: this seat's OWN
        // role prompt when the launcher resolved one for it, else the
        // shared `probe_system` fallback — see `ReviewStepContext::
        // probe_role_prompts`'s doc for why this is a no-op by default.
        let prior: &str = role_id
            .and_then(|r| ctx.probe_role_prompts.get(r))
            .map(String::as_str)
            .unwrap_or(ctx.probe_system.as_str());

        // (#1530) The bundle set — published by `review-bundle-task`'s own
        // step, which this task depends on directly (`review.json`'s
        // `review-probe-*-task.depends_on: ["review-bundle-task"]`).
        let bundles = run_ctx
            .artifact::<StdMutex<Vec<BundleInput>>>(REVIEW_BUNDLES_ARTIFACT)
            .expect("review-bundle-task's step must run before any probe task's own")
            .lock()
            .expect("review bundles mutex poisoned")
            .clone();

        let selected = select_bundles_for_staffing(&bundles, selector.as_ref());
        let collection: Vec<String> = selected.iter().map(|b| probe_user_message(prior, b)).collect();

        // (#1541) Publish THIS run's actual selection — index-aligned with
        // `collection` above — onto the bus, keyed by this render step's OWN
        // task id (the same key `gather_inputs` hands the dedup step's
        // `input` for this task's dispatch results, since a probe task is
        // exactly one role / one task / one dispatch — #1512). This is what
        // lets `reconstruct_probe_stage` attribute results by data that
        // actually flowed through the graph instead of a build-time
        // snapshot; see `REVIEW_PROBE_SELECTION_ARTIFACT`'s own doc.
        let pairs: Vec<(String, String)> =
            selected.iter().map(|b| (b.id.clone(), b.fact_family.clone())).collect();
        run_ctx
            .artifact::<StdMutex<std::collections::BTreeMap<String, Vec<(String, String)>>>>(
                REVIEW_PROBE_SELECTION_ARTIFACT,
            )
            .expect("run_step_graph materializes review.probe-selection via this kind's own provides()")
            .lock()
            .expect("probe selection mutex poisoned")
            .insert(task.id.clone(), pairs);

        emit_review_step_result(
            "review.probe-render",
            &step.id,
            &ctx.case_id,
            ctx.mission_id.as_deref(),
            json!({ "items_out": collection.len() }),
        );

        let output = serde_json::to_string(&collection).context("serializing probe prompts")?;
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
///
/// (#1541) NO LONGER carries a `bundles` field. It used to hold a
/// build-time snapshot of `(bundle_id, fact_family)` pairs
/// (`select_bundles_for_staffing(&ctx.bundles, ...)`, computed once in
/// `build_review_graph_from_config`) that [`reconstruct_probe_stage`] used
/// to attribute each `dispatch.map` result back to its bundle POSITIONALLY.
/// That snapshot only agreed with the run-time render step's own selection
/// because both called the identical pure function over the identical
/// bundles — a coincidence that would silently break once bundling itself
/// becomes run-time work (#1541's own filing). Attribution now travels
/// through the graph instead, via [`REVIEW_PROBE_SELECTION_ARTIFACT`],
/// published by the render step and read back by
/// [`reconstruct_probe_stage`] keyed on `draw_task_ids` below — so this
/// struct no longer needs its own copy.
///
/// (#1530 Packet 3a follow-on) `Serialize`/`Deserialize` are new derives —
/// every field is a plain `String`/`bool`/`Option<String>`/`Vec<String>`, so
/// this round-trips losslessly through JSON. Added so `ReviewDedupStepKind`
/// can stamp the mint-time `Vec<ProbeSeatSpec>` onto `review-dedup-step`'s
/// own `Step.config` instead of holding it as a constructor field — see that
/// kind's own doc for why `Step.config` won over a bus artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// #1512 (one role, one task). Also the key
    /// [`REVIEW_PROBE_SELECTION_ARTIFACT`] uses per draw (#1541) — the same
    /// task id names both this seat's `dispatch.map` results (`input`) and
    /// its render step's published selection.
    pub(crate) draw_task_ids: Vec<String>,
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
    /// (#1605) Total `retry_on_error` attempts consumed across every probe
    /// draw (summed from each item's own [`MapItemResult::retried`]) — how
    /// many transient dispatch errors self-healed on the probe stage's
    /// bounded retry, folded into [`ReviewEnvelope::probe_retries`].
    pub(crate) retries: u32,
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
///
/// (#1541) Attribution (WHICH bundle a flag came from) now keys on
/// `selection` — the render step's OWN published `(bundle_id,
/// fact_family)` pairs, per draw task id, off [`REVIEW_PROBE_SELECTION_ARTIFACT`]
/// — rather than a build-time snapshot. A draw whose task id is absent from
/// `selection` (the render step never ran or never published for that
/// task), or whose published pair count doesn't match the `dispatch.map`
/// result count for the same task (a desync between what the render step
/// selected and what actually dispatched), is a loud, named warning and
/// that draw's flags are DROPPED rather than risk attributing a real
/// finding to the wrong bundle — see this function's own history for the
/// silent-`continue` bug this replaces.
pub(crate) fn reconstruct_probe_stage(
    specs: &[ProbeSeatSpec],
    input: &std::collections::BTreeMap<String, String>,
    selection: &std::collections::BTreeMap<String, Vec<(String, String)>>,
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
    // (#1605) Summed `MapItemResult::retried` across every probe draw —
    // how many transient dispatch errors self-healed on the bounded
    // `retry_on_error` retry, folded into `ReviewEnvelope::probe_retries`.
    let mut total_retries = 0u32;

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

        // Flags in the historical seat → bundle → draw order — with only
        // one draw task id per seat (#1512), draw-major and bundle-major
        // iteration produce an IDENTICAL sequence, so nesting draw outside
        // bundle here (rather than the retired bundle-outside-draw order)
        // is a byte-identical reordering, not a behavior change.
        //
        // (#1541) Scope of that equivalence, stated precisely: it holds for
        // every spec GRAPH CONSTRUCTION can mint, because `build_review_graph
        // _from_config` gives each seat exactly one draw task (#1512). This
        // function itself still SUPPORTS multi-draw specs — `reconstruct_
        // probe_stage_accounts_skips_errors_and_flags` hand-builds a two-draw
        // spec as the retained coverage for per-seat summing across sibling
        // draws — and for such a spec draw-major and bundle-major iteration
        // yield DIFFERENT flag orders. That is unreachable from any real
        // graph today, but dedup downstream is first-survivor-wins over this
        // vector's order, so anything that re-introduces multi-draw seats at
        // graph-construction time must restore bundle-major nesting here
        // first, or it will silently change which flag survives. (Asserting
        // single-draw here would be wrong: the multi-draw path is supported
        // and tested — the constraint belongs to graph construction, not to
        // this function.)
        for (draw, task_id) in spec.draw_task_ids.iter().enumerate() {
            let results = per_draw.get(draw).map(Vec::as_slice).unwrap_or(&[]);
            let pairs = match selection.get(task_id) {
                Some(pairs) if pairs.len() == results.len() => pairs,
                Some(pairs) => {
                    warnings.push(format!(
                        "probe seat \"{}\" draw {draw} (task `{task_id}`): render step selected \
                         {} bundle(s) but dispatch returned {} result(s) — attribution desync, \
                         this draw's flags are DROPPED rather than risk attributing them to the \
                         wrong bundle",
                        spec.name,
                        pairs.len(),
                        results.len()
                    ));
                    continue;
                }
                None => {
                    warnings.push(format!(
                        "probe seat \"{}\" draw {draw} (task `{task_id}`): no bundle selection \
                         published by the render step — attribution unavailable, this draw's \
                         flags are DROPPED",
                        spec.name
                    ));
                    continue;
                }
            };
            for (item, (bundle_id, fact_family)) in results.iter().zip(pairs.iter()) {
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
                total_retries += item.retried;
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
            // tokens, but the live `RemoteBudget` meters CONSERVATIVELY
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
    Ok(ProbeReconstruction { flags, members, warnings, budget_row, all_draws_failed, retries: total_retries })
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
///
/// (#1530 Packet 3a follow-on) A stateless singleton — no `probe_specs`/
/// `remote_budget` constructor fields. Both are mint-time values
/// `build_review_graph_from_config` computes once (while claiming each
/// staffing against its declared probe task) and stamps onto THIS step's
/// own `Step.config` (`"probe_specs"`/`"remote_budget"`), read back in
/// `run_streaming`. `Step.config` won over a bus artifact here because
/// `probe_specs` is genuinely per-STEP build-time wiring, not per-RUN
/// mutable state shared across kinds (the bus's actual job — see
/// `REVIEW_CONTEXT_ARTIFACT`'s doc): it's computed once, never mutated,
/// consumed by exactly one step, and — since [`ProbeSeatSpec`] dropped its
/// `bundles` snapshot (#1541) — small and losslessly JSON-serializable
/// (`ProbeSeatSpec` gained `Serialize`/`Deserialize` for exactly this). The
/// judge/verify-render/synthesis kinds' own build-time staffing values
/// already established this "compute once, stamp `Step.config`, read back"
/// precedent; this is the same pattern, not a new one.
pub struct ReviewDedupStepKind;

/// (#1530 Packet 3a follow-on) Reads `"probe_specs"`/`"remote_budget"` off
/// `review-dedup-step`'s config — stamped by `build_review_graph_from_config`
/// (see [`ReviewDedupStepKind`]'s own doc). Extracted into its own function,
/// called from BOTH `run_streaming` below and the stamp/read agreement test
/// (`dedup_and_synthesis_config_stamp_and_reader_agree`), so a key-name or
/// shape mismatch between the stamper and the reader surfaces at TEST time
/// instead of only inside a live graph run — mirrors
/// `file_source_from_step_config`'s own shape.
fn dedup_config_from_step(config: &serde_json::Value) -> Result<(Vec<ProbeSeatSpec>, u64)> {
    let probe_specs: Vec<ProbeSeatSpec> = config
        .get("probe_specs")
        .cloned()
        .map(|v| serde_json::from_value(v).context("deserializing \"probe_specs\" from step config"))
        .transpose()?
        .context("darkmux: review-dedup-step config is missing \"probe_specs\"")?;
    let remote_budget = config
        .get("remote_budget")
        .and_then(|v| v.as_u64())
        .context("darkmux: review-dedup-step config is missing \"remote_budget\" (or it is not a u64)")?;
    Ok((probe_specs, remote_budget))
}

impl StepKind for ReviewDedupStepKind {
    fn id(&self) -> &'static str {
        "review.dedup"
    }

    fn display_name(&self) -> &'static str {
        "Dedup"
    }

    /// (#1530 Packets 1/3a) Declares the three run-scoped accumulator
    /// `Artifact` handles this pipeline's dispatching kinds share — this is
    /// the EARLIEST of the four accumulator consumers (dedup/judge/
    /// verify-render/synthesis), and the scheduler's `provides()` pre-scan
    /// runs once, before ANY wave, for every kind actually present in the
    /// graph (see `scheduler::run_step_graph`'s pre-scan doc) — so declaring
    /// them here is sufficient regardless of which wave each consumer lands
    /// in. `run_review_graph` overwrites these context-free defaults with
    /// the run-stamped values via the caller-seed path (module-level doc
    /// note above `REVIEW_ENVELOPE_ARTIFACT`). Also declares the `Data` port
    /// this step's own `Step.output` satisfies — the ordinary wiring is
    /// unchanged; this is annotation only (`Port::data`'s doc).
    ///
    /// `REVIEW_CONTEXT_ARTIFACT` is declared as `requires()` below, not
    /// here — `ReviewBundleStepKind` is the pipeline's earliest consumer of
    /// THAT artifact (investigate phase, step 1, ahead of this step), so it
    /// owns the `provides()` declaration for it (see that kind's own doc).
    fn provides(&self) -> &'static [Port] {
        const PORTS: [Port; 4] = [
            Port::data("deduped-flags"),
            Port::artifact(REVIEW_ENVELOPE_ARTIFACT, make_review_envelope_artifact),
            Port::artifact(REVIEW_MEMBERS_ARTIFACT, make_review_members_artifact),
            Port::artifact(REVIEW_WARNINGS_ARTIFACT, make_review_warnings_artifact),
        ];
        &PORTS
    }

    /// (#1530 Packet 3a) `requires()` only — see `ReviewBundleStepKind::
    /// provides()`'s doc for why the context artifact's `provides()`
    /// declaration lives there instead. (#1541) Also `requires()`s
    /// `REVIEW_PROBE_SELECTION_ARTIFACT` — `ReviewProbeRenderStepKind::
    /// provides()`'s doc explains why the PRODUCER declares that one.
    fn requires(&self) -> &'static [Port] {
        const PORTS: [Port; 2] = [
            Port::artifact(REVIEW_CONTEXT_ARTIFACT, make_review_context_artifact),
            Port::artifact(REVIEW_PROBE_SELECTION_ARTIFACT, make_review_probe_selection_artifact),
        ];
        &PORTS
    }

    fn run(&self, _s: &Step, _t: &Task, _i: &std::collections::BTreeMap<String, String>) -> Result<StepOutcome> {
        panic!(
            "ReviewDedupStepKind only runs through `run_streaming` — it reads/writes the \
             run-scoped ArtifactBus (#1530 Packet 1)"
        )
    }

    fn run_streaming(
        &self,
        step: &Step,
        _task: &Task,
        input: &std::collections::BTreeMap<String, String>,
        run_ctx: &StepRunCtx,
    ) -> Result<StepOutcome> {
        let ctx = run_ctx
            .artifact::<ReviewStepContext>(REVIEW_CONTEXT_ARTIFACT)
            .expect("run_review_graph seeds the context artifact before the graph runs");
        let env = run_ctx
            .artifact::<StdMutex<ReviewEnvelope>>(REVIEW_ENVELOPE_ARTIFACT)
            .expect("run_review_graph seeds the envelope artifact before the graph runs");
        let members = run_ctx
            .artifact::<StdMutex<Vec<MemberRecord>>>(REVIEW_MEMBERS_ARTIFACT)
            .expect("run_review_graph seeds the members artifact before the graph runs");
        let warnings = run_ctx
            .artifact::<StdMutex<Vec<String>>>(REVIEW_WARNINGS_ARTIFACT)
            .expect("run_review_graph seeds the warnings artifact before the graph runs");
        // (#1541) The render step's published per-task selection — `None`
        // only when NO `review.probe-render` step is present anywhere in
        // this graph (so `probe_specs` below is empty too and the lookup
        // below is a no-op either way); `unwrap_or_default` treats that the
        // same as "materialized but still empty" rather than panicking on a
        // legitimately probe-less graph.
        let selection: std::collections::BTreeMap<String, Vec<(String, String)>> = run_ctx
            .artifact::<StdMutex<std::collections::BTreeMap<String, Vec<(String, String)>>>>(
                REVIEW_PROBE_SELECTION_ARTIFACT,
            )
            .map(|s| s.lock().expect("probe selection mutex poisoned").clone())
            .unwrap_or_default();

        // (#1530 Packet 3a follow-on) `probe_specs`/`remote_budget` now
        // arrive via `step.config` — stamped once by
        // `build_review_graph_from_config` — rather than constructor
        // fields; see [`dedup_config_from_step`]'s own doc for why the read
        // is a shared function rather than inlined here.
        let (probe_specs, remote_budget) = dedup_config_from_step(&step.config)?;

        let t0 = Instant::now();
        // (#1442 ship-2b) Reconstruction boundary: raw flags + per-seat
        // member accounting + warnings + the probe budget row, rebuilt from
        // the seats x k map steps' per-item results.
        let recon = reconstruct_probe_stage(&probe_specs, input, &selection, remote_budget)?;
        members.lock().expect("probe members mutex poisoned").extend(recon.members);
        warnings.lock().expect("probe warnings mutex poisoned").extend(recon.warnings);
        let raw = recon.flags;
        let raw_count = raw.len();
        {
            let mut env = env.lock().expect("shared review envelope mutex poisoned");
            env.raw_flags = env.raw_flags.max(raw_count);
            if let Some(row) = recon.budget_row {
                env.remote_budgets.push(row);
            }
            if env.degenerate.is_none() {
                if let Some(reason) = recon.all_draws_failed {
                    env.degenerate = Some(reason);
                    env.degenerate_kind = Some(DegenerateKind::Error);
                }
            }
            env.probe_retries += recon.retries as usize;
        }
        let (deduped, _stats) = dedup_flags(raw, &ctx.diff);
        let wall_ms = t0.elapsed().as_millis() as u64;
        emit_review_step_result(
            "review.dedup",
            &step.id,
            &ctx.case_id,
            ctx.mission_id.as_deref(),
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
/// (#1530 Packet 3a) A stateless singleton — no `Arc<ReviewStepContext>` and
/// no `ResolvedSeatStaffing` constructor field. The context comes off the
/// `ArtifactBus` (`REVIEW_CONTEXT_ARTIFACT`, readable from BOTH
/// `run_streaming` and `residency()` now that the latter takes a
/// `&StepRunCtx` — see that trait method's own doc); the judge seat's
/// model/passes/max_tokens/endpoint come off `step.config`, stamped once by
/// `build_review_graph_from_config` the same way it already stamps the
/// probe seats' `dispatch.map` config (see that function's own doc).
pub struct ReviewJudgeStepKind;

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

    /// (#1530 Packets 1/3a) `requires()` only — this kind writes into the
    /// `Artifact` handles `ReviewDedupStepKind::provides()` already
    /// declares (materialized once, before any wave; see that method's
    /// doc), and reads `REVIEW_CONTEXT_ARTIFACT` which `ReviewBundleStepKind::
    /// provides()` declares — so it does not need to declare any of them
    /// again as `provides()` itself. `Port::artifact`'s factory is never
    /// invoked for a `requires()` port (only `provides()` ports are scanned
    /// — see `scheduler::run_step_graph`'s pre-scan doc); it's supplied only
    /// to satisfy `Port::artifact`'s constructor signature.
    fn requires(&self) -> &'static [Port] {
        const PORTS: [Port; 5] = [
            Port::data("deduped-flags"),
            Port::artifact(REVIEW_CONTEXT_ARTIFACT, make_review_context_artifact),
            Port::artifact(REVIEW_ENVELOPE_ARTIFACT, make_review_envelope_artifact),
            Port::artifact(REVIEW_MEMBERS_ARTIFACT, make_review_members_artifact),
            // (#1530) The bundle set, published by `ReviewBundleStepKind::
            // run_streaming` — `review-judge-task` depends (transitively, via
            // dedup + the probe tasks) on `review-bundle-task`, so it's
            // always populated by the time this kind's wave runs.
            Port::artifact(REVIEW_BUNDLES_ARTIFACT, make_review_bundles_artifact),
        ];
        &PORTS
    }

    fn provides(&self) -> &'static [Port] {
        const PORTS: [Port; 1] = [Port::data("judged-flags")];
        &PORTS
    }

    fn run(&self, _s: &Step, _t: &Task, _i: &std::collections::BTreeMap<String, String>) -> Result<StepOutcome> {
        panic!(
            "ReviewJudgeStepKind only runs through `run_streaming` — it reads/writes the \
             run-scoped ArtifactBus (#1530 Packet 1)"
        )
    }

    fn run_streaming(
        &self,
        step: &Step,
        _task: &Task,
        input: &std::collections::BTreeMap<String, String>,
        run_ctx: &StepRunCtx,
    ) -> Result<StepOutcome> {
        let ctx = run_ctx
            .artifact::<ReviewStepContext>(REVIEW_CONTEXT_ARTIFACT)
            .expect("run_review_graph seeds the context artifact before the graph runs");
        let env = run_ctx
            .artifact::<StdMutex<ReviewEnvelope>>(REVIEW_ENVELOPE_ARTIFACT)
            .expect("run_review_graph seeds the envelope artifact before the graph runs");
        let members = run_ctx
            .artifact::<StdMutex<Vec<MemberRecord>>>(REVIEW_MEMBERS_ARTIFACT)
            .expect("run_review_graph seeds the members artifact before the graph runs");

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

        // (#1530 Packet 3a) The judge seat's model/passes/max_tokens/endpoint
        // now arrive via `step.config` — stamped once by
        // `build_review_graph_from_config` (the SAME pattern the probe
        // seats' `dispatch.map` config already uses) — rather than a
        // `ResolvedSeatStaffing` constructor field. Always present in
        // production; `.expect` is loud-fail wiring, matching this kind's
        // existing `.expect()`s on missing bus artifacts.
        let judge_identifier = step
            .config
            .get("model")
            .and_then(|v| v.as_str())
            .expect("build_review_graph_from_config always stamps \"model\" onto review-judge-step")
            .to_string();
        // A `&str` (`Copy`) so the `move` closure below can capture ITS
        // OWN copy of the reference on every loop iteration without moving
        // the owned `judge_identifier` String out from under a later one.
        let judge_identifier_ref: &str = &judge_identifier;
        let judge_endpoint: Option<ModelEndpoint> = step
            .config
            .get("endpoint")
            .map(|v| serde_json::from_value(v.clone()).context("deserializing judge seat endpoint"))
            .transpose()?;
        let judge_endpoint = judge_endpoint.as_ref();
        let judge_max_tokens = step
            .config
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .expect("build_review_graph_from_config always stamps \"max_tokens\" onto review-judge-step")
            as u32;
        let judge_passes = step
            .config
            .get("passes")
            .and_then(|v| v.as_u64())
            .expect("build_review_graph_from_config always stamps \"passes\" onto review-judge-step")
            as u32;
        let judge_system = ctx.judge_system.as_str();
        let judge_budgets = judge_endpoint.map(|_| {
            StdMutex::new(JudgeBudgets {
                pass1: RemoteBudget::with_stage("judge-pass1", ctx.remote_max_tokens_per_execution, MIN_VIABLE_JUDGE_GRANT),
                pass2: RemoteBudget::with_stage("judge-pass2", ctx.remote_max_tokens_per_execution, MIN_VIABLE_JUDGE_GRANT),
            })
        });
        // (#1530) The bundle set — published by `review-bundle-task`'s own
        // step, well before this kind's wave runs (see `requires()`'s doc).
        let bundles = run_ctx
            .artifact::<StdMutex<Vec<BundleInput>>>(REVIEW_BUNDLES_ARTIFACT)
            .expect("review-bundle-task's step must run before review-judge-task's own")
            .lock()
            .expect("review bundles mutex poisoned")
            .clone();

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
                    let bundle = bundles.iter().find(|b| b.id == flag.bundle_id);
                    let code = bundle.map(|b| b.code.as_str()).unwrap_or_default();
                    let facts: &[String] = bundle.map(|b| b.facts.as_slice()).unwrap_or_default();
                    let prompt = judge_prompt(&ctx.intent_title, &ctx.intent_body, code, facts, &flag.charge_text);
                    // (#1374) `chunk_start + offset` = this flag's stable index
                    // in `deduped`, independent of thread completion order.
                    let index = chunk_start + offset;
                    let ctx = &ctx;
                    let judge_budgets = judge_budgets.as_ref();
                    let results = &results;
                    scope.spawn(move || {
                        let mut chat = |call: &ChatCall| dispatch_chat(ctx, call);
                        // (#swarm-6) No guard held here anymore: the old
                        // shape locked the budgets mutex ACROSS the whole
                        // judge dispatch, so the N threads this scope spawns
                        // (`review.judge_concurrency`) queued on one mutex
                        // and silently ran sequentially on every remote
                        // run. `run_budgeted_pass` now locks only around
                        // admit_reserve and settle — the reservation is
                        // what keeps the #1260 ceiling intact with the lock
                        // narrowed (see its doc).
                        let outcome = judge_one_flag_with_passes(
                            judge_passes,
                            &prompt,
                            judge_identifier_ref,
                            judge_system,
                            judge_max_tokens,
                            judge_endpoint,
                            judge_budgets,
                            &mut chat,
                        );
                        emit_review_step_result(
                            "review.judge",
                            "review-ruling",
                            &ctx.case_id,
                            ctx.mission_id.as_deref(),
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
                                ctx.mission_id.as_deref(),
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
                                absence_backstop: None,
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

        let mut judged: Vec<JudgedFlag> = results.into_iter().map(|r| r.judged).collect();

        // (#1748) The mechanical, zero-token absence-claim backstop — see
        // `apply_absence_backstop`'s doc. Runs single-threaded, AFTER the
        // concurrent judge dispatches above have all joined (no `FileSource`
        // Send/Sync concern), and BEFORE `judged` is serialized as this
        // step's output — so a demotion here is visible to every downstream
        // step (the optional verify stage skips an already-demoted flag;
        // `ReviewSynthesisStepKind`'s tier counts see it too), the exact
        // same ordering `finish_review` (the sequential path) applies.
        // `file_source_from_step_config` errors loudly when `step.config`
        // has no `"source"` key at all (the bundle step's contract) — here
        // that's tolerated as "no FileSource available" via `.ok()`, not
        // promoted to a hard failure: a hand-built `Step.config` (this
        // module's own `run_streaming`-level tests, or an older persisted
        // graph from before this packet) simply gets a no-op backstop,
        // never a broken judge step.
        let file_source = file_source_from_step_config(&step.config).ok();
        apply_absence_backstop(&mut judged, &bundles, file_source.as_ref());

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
            ctx.remote_max_tokens_per_execution,
            ctx.judge_exhaustion_strict,
        );
        {
            let mut env = env.lock().expect("shared review envelope mutex poisoned");
            env.remote_budgets.extend(gate.remote_budget_rows);
            if let Some(w) = gate.dispatch_error_warning {
                env.warnings.push(w);
            }
            if let Some(w) = gate.coverage_warning {
                env.warnings.push(w);
            }
            if gate.degenerate_reason.is_some() {
                env.degenerate = gate.degenerate_reason;
                env.degenerate_kind = Some(DegenerateKind::Error);
            }
        }

        emit_review_step_result(
            "review.judge",
            &step.id,
            &ctx.case_id,
            ctx.mission_id.as_deref(),
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
            members.lock().expect("members mutex poisoned").push(MemberRecord {
                model: judge_identifier,
                seat: "review-judge".to_string(),
                draws: judge_calls,
                wall_ms: pass1_wall_ms + pass2_wall_ms,
                total_tokens: judge_tokens,
                remote: judge_endpoint.is_some(),
                // (#1530 Packet 3a) `endpoint_host` is stamped into config
                // at build time (`build_review_graph_from_config`) from
                // `seat_endpoint_host(&judge.pm)` — the same value this used
                // to compute here from a `self.judge` constructor field.
                endpoint: step.config.get("endpoint_host").and_then(|v| v.as_str()).map(String::from),
                served_model: judge_served_model,
            });
        }

        let output = serde_json::to_string(&judged).context("serializing judged flags")?;
        Ok(StepOutcome { output, flow_records: Vec::new() })
    }

    /// (#1530 Packet 3a) Reads `step.config` (the judge seat's stamped
    /// staffing, per `run_streaming`'s own doc note above) instead of a
    /// `self.judge` constructor field, and the run-scoped bus `run_ctx`
    /// carries instead of a `self.ctx` constructor field — the ONLY reason
    /// this hook gained a `&StepRunCtx` parameter in #1530 Packet 3a (see
    /// `StepKind::residency`'s own doc). The skip-load decision logic below
    /// is otherwise BYTE-IDENTICAL to the pre-Packet-3a version: same two
    /// early-outs, same order, same `Placement` fields — see this method's
    /// original doc (preserved below) for the empty-bundle-set reasoning.
    ///
    /// (#1530 bundling-becomes-runtime-work follow-on) The empty-bundle-set
    /// check now reads [`REVIEW_BUNDLES_ARTIFACT`] directly instead of
    /// `ctx.bundles` (retired — see that field's removal note on
    /// `ReviewStepContext`), so the `ReviewStepContext` fetch this method
    /// used to need is gone too. Safe by construction, not by luck:
    /// `review-judge-task` depends (transitively, via dedup + the probe
    /// tasks) on `review-bundle-task`, so by the time THIS step's wave
    /// runs — the only time `residency()` is ever called for it — the
    /// bundle step has already run and populated the artifact for real (see
    /// `REVIEW_BUNDLES_ARTIFACT`'s own doc for why this ordering is
    /// guaranteed, not assumed).
    ///
    /// (#1360 follow-up, preserved) Unlike probe, judge can't know upfront
    /// whether dedup will hand it any flags — that's genuinely data-dependent
    /// on an earlier step's real output, not knowable at graph-build time.
    /// But a TRULY empty bundle set is a safe, conservative exception: every
    /// probe seat's selector operates on the same bundle set, so if that set
    /// is empty, dedup's output is guaranteed empty too, transitively — no
    /// seat's selector matters. Skips loading a model this step is certain
    /// not to use.
    fn residency(
        &self,
        step: &Step,
        _task: &Task,
        _input: &std::collections::BTreeMap<String, String>,
        run_ctx: &StepRunCtx,
    ) -> Option<darkmux_gestalt::Placement> {
        if step.config.get("endpoint").is_some() {
            return None;
        }
        let bundles = run_ctx.artifact::<StdMutex<Vec<BundleInput>>>(REVIEW_BUNDLES_ARTIFACT)?;
        if bundles.lock().expect("review bundles mutex poisoned").is_empty() {
            return None;
        }
        let model_key = step.config.get("model_key").and_then(|v| v.as_str())?;
        let identifier = step.config.get("identifier").and_then(|v| v.as_str())?;
        let n_ctx = step.config.get("n_ctx").and_then(|v| v.as_u64())? as u32;
        Some(darkmux_gestalt::Placement {
            model_key: model_key.to_string(),
            identifier: identifier.to_string(),
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
///
/// (#1530 Packet 3a) A stateless singleton — no `Arc<ReviewStepContext>` and
/// no `ResolvedSeatStaffing` constructor field. This kind's ONLY use of the
/// verify staffing was `.is_none()` (the "no verify seat staffed" skip
/// reason) — it never reads `.pm`/`.max_tokens`/anything else — so
/// `build_review_graph_from_config` stamps just that one bit
/// (`"verify_seat_staffed"`) onto this step's own config, rather than the
/// verify seat's full model identity (which the SEPARATE `review-verify-step`
/// `dispatch.map` step's own config already carries, unchanged, since that
/// step already rode the generic block's config-driven pattern before this
/// packet).
pub struct ReviewVerifyRenderStepKind;

impl StepKind for ReviewVerifyRenderStepKind {
    fn id(&self) -> &'static str {
        "review.verify-render"
    }

    fn display_name(&self) -> &'static str {
        "Verify prompts"
    }

    /// (#1530 Packets 1/3a) `requires()` only — see `ReviewJudgeStepKind::
    /// requires()`'s doc for why a downstream consumer of
    /// `ReviewDedupStepKind::provides()`'s/`ReviewBundleStepKind::
    /// provides()`'s artifacts declares `requires()` rather than
    /// re-`provides()`ing them.
    fn requires(&self) -> &'static [Port] {
        const PORTS: [Port; 4] = [
            Port::data("judged-flags"),
            Port::artifact(REVIEW_CONTEXT_ARTIFACT, make_review_context_artifact),
            Port::artifact(REVIEW_ENVELOPE_ARTIFACT, make_review_envelope_artifact),
            // (#1530) The bundle set — always populated by this point:
            // `review-verify-task` depends on `review-judge-task`, which
            // itself depends (transitively) on `review-bundle-task`.
            Port::artifact(REVIEW_BUNDLES_ARTIFACT, make_review_bundles_artifact),
        ];
        &PORTS
    }

    fn provides(&self) -> &'static [Port] {
        const PORTS: [Port; 1] = [Port::data("verify-prompts")];
        &PORTS
    }

    fn run(&self, _s: &Step, _t: &Task, _i: &std::collections::BTreeMap<String, String>) -> Result<StepOutcome> {
        panic!(
            "ReviewVerifyRenderStepKind only runs through `run_streaming` — it reads the \
             run-scoped ArtifactBus (#1530 Packet 1)"
        )
    }

    fn run_streaming(
        &self,
        step: &Step,
        _task: &Task,
        input: &std::collections::BTreeMap<String, String>,
        run_ctx: &StepRunCtx,
    ) -> Result<StepOutcome> {
        let ctx = run_ctx
            .artifact::<ReviewStepContext>(REVIEW_CONTEXT_ARTIFACT)
            .expect("run_review_graph seeds the context artifact before the graph runs");
        let env = run_ctx
            .artifact::<StdMutex<ReviewEnvelope>>(REVIEW_ENVELOPE_ARTIFACT)
            .expect("run_review_graph seeds the envelope artifact before the graph runs");

        let judge_output = input.values().next().cloned().unwrap_or_default();
        let judged: Vec<JudgedFlag> = if judge_output.is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&judge_output).context("deserializing judged flags")?
        };

        // (#1530 Packet 3a) Stamped by `build_review_graph_from_config` —
        // `verify.is_some()` at build time, read back here instead of a
        // `self.verify` constructor field.
        let verify_seat_staffed =
            step.config.get("verify_seat_staffed").and_then(|v| v.as_bool()).unwrap_or(false);
        let confirmed: Vec<&JudgedFlag> = judged.iter().filter(|j| j.tier == Tier::Confirmed).collect();
        let skip_reason: Option<&str> = if !verify_seat_staffed {
            Some("no verify seat staffed — judged flags pass through unchanged")
        } else if env
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
            // (#1530) The bundle set — published by `review-bundle-task`'s
            // own step; see `requires()`'s doc for why it's guaranteed
            // populated by this point.
            let bundles = run_ctx
                .artifact::<StdMutex<Vec<BundleInput>>>(REVIEW_BUNDLES_ARTIFACT)
                .expect("review-bundle-task's step must run before review-verify-task's own")
                .lock()
                .expect("review bundles mutex poisoned")
                .clone();
            confirmed
                .iter()
                .map(|j| {
                    let bundle = bundles.iter().find(|b| b.id == j.flag.bundle_id);
                    let code = bundle.map(|b| b.code.as_str()).unwrap_or_default();
                    let facts: &[String] = bundle.map(|b| b.facts.as_slice()).unwrap_or_default();
                    verify_prompt(&ctx.intent_title, &ctx.intent_body, code, facts, &j.flag.charge_text)
                })
                .collect()
        };

        let mut payload = json!({ "items_in": confirmed.len(), "items_out": prompts.len() });
        if let Some(reason) = skip_reason {
            payload["short_circuit"] = json!(reason);
        }
        emit_review_step_result(
            "review.verify-render",
            &step.id,
            &ctx.case_id,
            ctx.mission_id.as_deref(),
            payload,
        );

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
///
/// (#1530 Packet 3a) Takes the seat's already-derived identity
/// (`identifier`/`remote`/`endpoint_host` — `seat_identifier(&vstaff.pm)`/
/// `vstaff.pm.is_remote()`/`seat_endpoint_host(&vstaff.pm)`) rather than the
/// whole `&ResolvedSeatStaffing` this used to take. Its one caller
/// (`ReviewSynthesisStepKind::run_streaming`) no longer HOLDS a
/// `ResolvedSeatStaffing` — `build_review_graph_from_config` computes these
/// same three values at build time and stamps them onto the synthesis
/// step's own config, the same "compute once, stamp, read back" pattern the
/// judge/verify-render kinds now use — so this function's OWN logic is
/// unchanged, only what it derives the values FROM moved to its caller.
///
/// Argument count exceeds clippy's default threshold (8 vs 7) since #1641's
/// `mission_id` addition — every parameter is inherent to the call (the
/// per-item results, the seat's derived identity, the budget, the run's own
/// case/mission identity for the "step result" records this emits), not
/// incidental bloat; mirrors the same accepted trade-off `run_step_graph`'s
/// own `#[allow(clippy::too_many_arguments)]` documents.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_verify_results(
    judged: &mut [JudgedFlag],
    results: &[MapItemResult],
    identifier: &str,
    remote: bool,
    endpoint_host: Option<&str>,
    budget: u64,
    case_id: &str,
    mission_id: Option<&str>,
) -> VerifyApplyOutcome {
    let identifier = identifier.to_string();
    let endpoint_host = endpoint_host.map(String::from);
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
            mission_id,
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
        // while the live `RemoteBudget` meters conservatively — a
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
///
/// (#1530 Packet 3a) No `Arc<ReviewStepContext>` and no `ResolvedSeatStaffing`
/// constructor field — `ctx` moved to the bus; the verify seat's derived
/// identity (`identifier`/`remote`/`endpoint_host`, the only three things
/// this step's [`apply_verify_results`] call ever read off the staffing)
/// moved to `step.config`, stamped by `build_review_graph_from_config` at
/// build time.
///
/// (#1530 Packet 3a follow-on) A stateless singleton — `dedup_task_id`/
/// `judge_task_id`/`verify_task_id`/`remote_budget` ALSO moved to
/// `step.config` (`"dedup_task_id"`/`"judge_task_id"`/`"verify_task_id"`/
/// `"remote_budget"`), always stamped by `build_review_graph_from_config`
/// (unconditionally, unlike the `verify_*` trio above which is present only
/// when a verify seat is staffed). Packet 3a's own note above once kept
/// these as plain constructor fields — "graph-wiring state, not one of the
/// two patterns this packet retires" — but a global step-kind registry (the
/// next packet in the #1530 arc) registers each kind ONCE as a shared
/// singleton, so even build-time-only wiring like these task ids can't
/// survive as a constructor field on a kind meant to be process-lifetime
/// and mission-independent. All four are plain `String`/`u64` values,
/// trivially round-tripped through JSON.
pub struct ReviewSynthesisStepKind;

/// (#1530 Packet 3a follow-on) Reads `"dedup_task_id"`/`"judge_task_id"`/
/// `"verify_task_id"`/`"remote_budget"` off `review-synthesis-step`'s
/// config — stamped unconditionally by `build_review_graph_from_config`
/// (see [`ReviewSynthesisStepKind`]'s own doc). Extracted into its own
/// function for the same reason [`dedup_config_from_step`] is — a
/// key-name mismatch between stamper and reader surfaces at TEST time,
/// not only inside a live graph run.
fn synthesis_task_ids_from_step(config: &serde_json::Value) -> Result<(String, String, String, u64)> {
    let dedup_task_id = config
        .get("dedup_task_id")
        .and_then(|v| v.as_str())
        .context("darkmux: review-synthesis-step config is missing \"dedup_task_id\" (or it is not a string)")?
        .to_string();
    let judge_task_id = config
        .get("judge_task_id")
        .and_then(|v| v.as_str())
        .context("darkmux: review-synthesis-step config is missing \"judge_task_id\" (or it is not a string)")?
        .to_string();
    let verify_task_id = config
        .get("verify_task_id")
        .and_then(|v| v.as_str())
        .context("darkmux: review-synthesis-step config is missing \"verify_task_id\" (or it is not a string)")?
        .to_string();
    let remote_budget = config
        .get("remote_budget")
        .and_then(|v| v.as_u64())
        .context("darkmux: review-synthesis-step config is missing \"remote_budget\" (or it is not a u64)")?;
    Ok((dedup_task_id, judge_task_id, verify_task_id, remote_budget))
}

impl StepKind for ReviewSynthesisStepKind {
    fn id(&self) -> &'static str {
        "review.synthesis"
    }

    fn display_name(&self) -> &'static str {
        "Synthesis"
    }

    /// (#1530 Packets 1/3a) `requires()` only — see `ReviewJudgeStepKind::
    /// requires()`'s doc.
    fn requires(&self) -> &'static [Port] {
        const PORTS: [Port; 6] = [
            Port::data("deduped-flags"),
            Port::data("judged-flags"),
            Port::data("verify-results"),
            Port::artifact(REVIEW_CONTEXT_ARTIFACT, make_review_context_artifact),
            Port::artifact(REVIEW_ENVELOPE_ARTIFACT, make_review_envelope_artifact),
            Port::artifact(REVIEW_MEMBERS_ARTIFACT, make_review_members_artifact),
        ];
        &PORTS
    }

    fn provides(&self) -> &'static [Port] {
        const PORTS: [Port; 1] = [Port::data("envelope")];
        &PORTS
    }

    fn run(&self, _s: &Step, _t: &Task, _i: &std::collections::BTreeMap<String, String>) -> Result<StepOutcome> {
        panic!(
            "ReviewSynthesisStepKind only runs through `run_streaming` — it reads/writes the \
             run-scoped ArtifactBus (#1530 Packet 1)"
        )
    }

    fn run_streaming(
        &self,
        step: &Step,
        _task: &Task,
        input: &std::collections::BTreeMap<String, String>,
        run_ctx: &StepRunCtx,
    ) -> Result<StepOutcome> {
        let ctx = run_ctx
            .artifact::<ReviewStepContext>(REVIEW_CONTEXT_ARTIFACT)
            .expect("run_review_graph seeds the context artifact before the graph runs");
        // (named `shared_env`, not `env` — the function body below rebinds
        // `env` to an owned, cloned-out `ReviewEnvelope` value partway
        // through; this handle is what that clone gets written BACK onto.)
        let shared_env = run_ctx
            .artifact::<StdMutex<ReviewEnvelope>>(REVIEW_ENVELOPE_ARTIFACT)
            .expect("run_review_graph seeds the envelope artifact before the graph runs");
        let members = run_ctx
            .artifact::<StdMutex<Vec<MemberRecord>>>(REVIEW_MEMBERS_ARTIFACT)
            .expect("run_review_graph seeds the members artifact before the graph runs");

        // (#1530 Packet 3a follow-on) `dedup_task_id`/`judge_task_id`/
        // `verify_task_id`/`remote_budget` now arrive via `step.config` —
        // stamped once by `build_review_graph_from_config`, unconditionally
        // (unlike the `verify_*` trio below, which is present only when a
        // verify seat is staffed) — rather than constructor fields; see
        // [`synthesis_task_ids_from_step`]'s own doc for why the read is a
        // shared function rather than inlined here.
        let (dedup_task_id, judge_task_id, verify_task_id, remote_budget) =
            synthesis_task_ids_from_step(&step.config)?;

        let t0 = Instant::now();
        let dedup_output = input.get(&dedup_task_id).cloned().unwrap_or_default();
        let judge_output = input.get(&judge_task_id).cloned().unwrap_or_default();
        let verify_output = input.get(&verify_task_id).cloned().unwrap_or_default();
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
        //
        // (#1530 Packet 3a) The verify seat's derived identity
        // (`identifier`/`remote`/`endpoint_host`) is stamped by
        // `build_review_graph_from_config` onto THIS step's own config
        // (`"verify_identifier"` present iff a verify seat was staffed —
        // the same `.is_some()` test `if let Some(vstaff) = &self.verify`
        // used to make) instead of a `self.verify: Option<ResolvedSeatStaffing>`
        // constructor field.
        if let Some(identifier) = step.config.get("verify_identifier").and_then(|v| v.as_str()) {
            if !verify_results.is_empty() {
                let remote = step.config.get("verify_remote").and_then(|v| v.as_bool()).unwrap_or(false);
                let endpoint_host = step.config.get("verify_endpoint_host").and_then(|v| v.as_str());
                let outcome = apply_verify_results(
                    &mut judged,
                    &verify_results,
                    identifier,
                    remote,
                    endpoint_host,
                    remote_budget,
                    &ctx.case_id,
                    ctx.mission_id.as_deref(),
                );
                if let Some(member) = outcome.member {
                    members.lock().expect("members mutex poisoned").push(member);
                }
                if outcome.warning.is_some() || outcome.budget_row.is_some() {
                    let mut env = shared_env.lock().expect("shared review envelope mutex poisoned");
                    if let Some(w) = outcome.warning {
                        env.warnings.push(w);
                    }
                    if let Some(rec) = outcome.budget_row {
                        env.remote_budgets.push(rec);
                    }
                }
            }
        }

        let mut env = shared_env.lock().expect("shared review envelope mutex poisoned").clone();
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
                // (#1605) The classifier reads `env.bundle_skip` (stamped by
                // `ReviewBundleStepKind::run_streaming`, above) to build a
                // REASONED summary and decide benign-vs-error — replacing
                // the old fixed string, which could not distinguish "diff
                // was entirely non-code" from "bundler bug" from "diff
                // exceeded some internal bound".
                let (msg, kind) = classify_zero_bundle_degenerate(&env.bundle_skip);
                env.degenerate = Some(msg);
                env.degenerate_kind = Some(kind);
            } else if env.deduped_flags == 0 {
                env.degenerate = Some("zero flags from all probe draws — never a silent pass".to_string());
                env.degenerate_kind = Some(DegenerateKind::Error);
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
                env.degenerate_kind = Some(DegenerateKind::Error);
            }
        }

        *shared_env.lock().expect("shared review envelope mutex poisoned") = env.clone();

        let wall_ms = t0.elapsed().as_millis() as u64;
        emit_review_step_result(
            "review.synthesis",
            &step.id,
            &ctx.case_id,
            ctx.mission_id.as_deref(),
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
/// crate-boundary note), the resolved [`StepKindRegistry`], the envelope's
/// BUILD-TIME contents, and a `step_id -> Task.phase_id` map (so the caller
/// can persist each Step under the SAME Phase its owning Task belongs to
/// without re-deriving the lookup).
///
/// (#1530 Packet 1) `initial_env` replaces the pre-Packet-1
/// `shared_env: SharedReviewEnvelope` field — this pipeline's cross-cutting
/// accumulators (the envelope, the run-wide member/warning accumulators) now
/// live on the run-scoped `ArtifactBus` instead of a bespoke `Arc<Mutex<_>>`
/// threaded by hand from `build_review_graph` into `run_review_graph` (see
/// the module-level doc note above `REVIEW_ENVELOPE_ARTIFACT`). `initial_env`
/// is the PLAIN, unwrapped envelope value this function assembles at build
/// time (case_id/bundle count/interpret-time warnings, e.g. the "pruned an
/// unclaimed probe task" warning) — `run_review_graph` pre-stamps it further
/// (case_id/crew/mode/fingerprint/staffing) and SEEDS the wrapped
/// `Arc<StdMutex<_>>` result onto the bus via `run_step_graph`'s caller-seed
/// path, exactly where the Arc gets minted now (previously minted here,
/// before the run even started).
pub struct BuiltReviewGraph {
    pub tasks: Vec<Task>,
    pub steps: std::collections::BTreeMap<String, Step>,
    pub registry: StepKindRegistry,
    pub initial_env: ReviewEnvelope,
    pub synthesis_step_id: String,
    pub phase_id_of_step: std::collections::BTreeMap<String, String>,
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
    // (#1530 follow-on, Packet A1) `review.probe-render` — the probe
    // stage's render step, mirroring `review.verify-render` below.
    if kind == "review.probe-render" {
        return Some("Probe prompts");
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

/// Register every review-pipeline Tier 3 step kind (#1352) — plus the
/// pre-#1349-rename `funnel.*` legacy aliases — onto `registry`.
///
/// (#1530 — one global step-kind registry) Extracted out of what used to be
/// inline registrations inside `build_review_graph_from_config`, so
/// `src/`'s cross-family assembly point (`all_step_kinds`, `src/
/// mission_launch.rs`) can build ONE registry that resolves both `review.*`
/// and `mission.*` (coder-phase) kinds at once — a capability that was
/// structurally impossible while each launcher only ever populated its OWN
/// partial registry. `build_review_graph_from_config` still builds a
/// `StepKindRegistry::with_builtins()` and calls this function immediately
/// after (see its own call site) — the SAME two-call sequence
/// `all_step_kinds` performs — so `build_review_graph`'s existing callers
/// (`review_bench`, `mission_launch_review::launch`) see byte-identical
/// behavior. This is a pure extraction, not a behavior change.
///
/// Every kind registered here is a stateless unit struct (#1536/#1537/
/// #1553), so ONE shared `Arc` instance per kind is registered — never
/// per-graph-per-call construction — which is exactly what makes sharing a
/// single registry across callers safe.
///
/// **Registration ownership:** this function is the ONE place review's
/// Tier 3 kinds get registered. A caller must not also hand-register any
/// of these ids onto the same `registry` — `StepKindRegistry::register`
/// errors loud on a duplicate id, so a double-call surfaces immediately
/// rather than silently overwriting.
pub fn register_review_kinds(registry: &StepKindRegistry) -> Result<()> {
    let bundle_kind = Arc::new(ReviewBundleStepKind);
    registry.register(bundle_kind.clone()).context("registering review.bundle")?;
    // (#1349) Legacy alias — a `Step.kind` persisted before the funnel->review
    // rename must still resolve if anything ever re-reads it back through a
    // fresh registry (see `StepKindRegistry::register_alias`'s doc).
    registry
        .register_alias("funnel.bundle", bundle_kind)
        .context("registering funnel.bundle legacy alias")?;

    // (#1530 follow-on Packet A1) The probe render step — the Tier-3 half
    // of the probe stage that stays bespoke; no legacy alias (new as of the
    // render/dispatch split, not a renamed `funnel.*` kind).
    registry
        .register(Arc::new(ReviewProbeRenderStepKind))
        .context("registering review.probe-render")?;

    let dedup_kind = Arc::new(ReviewDedupStepKind);
    registry.register(dedup_kind.clone()).context("registering review.dedup")?;
    // (#1349) Legacy alias — see the bundle kind's registration above.
    registry
        .register_alias("funnel.dedup", dedup_kind)
        .context("registering funnel.dedup legacy alias")?;

    let judge_kind = Arc::new(ReviewJudgeStepKind);
    registry.register(judge_kind.clone()).context("registering review.judge")?;
    // (#1349) Legacy alias — see the bundle kind's registration above.
    registry
        .register_alias("funnel.judge", judge_kind)
        .context("registering funnel.judge legacy alias")?;

    // (#1442 ship-2b) The verify render step — the Tier-3 half of the
    // verify stage that stays bespoke; no legacy alias, same reason as the
    // probe render step above.
    registry
        .register(Arc::new(ReviewVerifyRenderStepKind))
        .context("registering review.verify-render")?;

    let synthesis_kind = Arc::new(ReviewSynthesisStepKind);
    registry.register(synthesis_kind.clone()).context("registering review.synthesis")?;
    // (#1349) Legacy alias — see the bundle kind's registration above.
    registry
        .register_alias("funnel.synthesis", synthesis_kind)
        .context("registering funnel.synthesis legacy alias")?;

    Ok(())
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
    bundle_spec: &BundleBuildSpec,
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
        bundle_spec,
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
pub fn build_review_graph_from_config(
    config: &darkmux_crew::mission_config::MissionConfig,
    source_detail: &str,
    ctx: Arc<ReviewStepContext>,
    bundle_spec: &BundleBuildSpec,
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

    // (#1512) The probe stage is static tasks in the document, not a
    // template — no expansion collection to feed (the primitive that would
    // have needed one was retired in #1550 cluster item 2).
    let params = LaunchParams {
        phase_ids,
        task_overrides: std::collections::BTreeMap::new(),
        step_config_overrides,
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

    // (#1373; #1530 Packet 1 — this became `initial_env`, no longer
    // wrapped in `Arc<Mutex<_>>` here) Built EARLY (moved up from its
    // former place right before `ReviewSynthesisStepKind`'s construction)
    // so it's ready alongside every other build-time value this function
    // hands back. `run_review_graph` pre-stamps it further (case_id is
    // re-stamped there too — this run-launch's `ctx.case_id`, not a stale
    // build-time snapshot — plus crew/mode/fingerprint/staffing) and SEEDS
    // the wrapped `Arc<StdMutex<_>>` result onto the run's `ArtifactBus` —
    // see `BuiltReviewGraph::initial_env`'s own doc for the full handoff.
    //
    // (#1530) `bundles` starts at 0 now, not `ctx.bundles.len()` — the real
    // bundle set isn't resolved until `review-bundle-step` runs (the whole
    // point of this packet). `ReviewBundleStepKind::run_streaming` writes
    // the real count onto the shared envelope artifact once it knows it,
    // well before `ReviewSynthesisStepKind` (the only other reader) runs.
    let initial_env =
        ReviewEnvelope { case_id: ctx.case_id.clone(), warnings: interpret_warnings, ..Default::default() };

    // (#1442 ship-2b) The probe/verify stages ride the GENERIC
    // `dispatch.map` builtin, so the registry starts from the Tier-1
    // builtin set instead of empty.
    //
    // (#1530 — one global step-kind registry) Every review Tier 3 kind
    // (bundle/probe-render/dedup/judge/verify-render/synthesis) plus their
    // legacy `funnel.*` aliases registers in ONE call now
    // ([`register_review_kinds`]) instead of six inline blocks scattered
    // through this function — see that function's own doc for why the
    // extraction matters (it's what lets `src/mission_launch.rs`'s
    // `all_step_kinds` build one registry spanning review AND coder-phase).
    let registry = StepKindRegistry::with_builtins();
    register_review_kinds(&registry).context("registering review step kinds")?;

    // (#1530) Stamp `review-bundle-step`'s config from the caller's already-
    // resolved `bundle_spec` — see [`BundleBuildSpec`]'s own doc for why
    // this is DATA the launcher/bench caller already had, not new work.
    {
        let bundle_step = steps
            .get_mut("review-bundle-step")
            .expect("interpreted \"review\" graph must have a review-bundle-step");
        bundle_step.config = json!({
            "source": bundle_source_spec_json(&bundle_spec.source),
            "bundler": bundle_spec.bundler,
            "diff_file": bundle_spec.diff_file.display().to_string(),
        });
    }
    // (#1442 ship-2b) The probe/verify legacy aliases (`funnel.probe:<seat>`,
    // `funnel.verify`) retired WITH their kinds — there is no live
    // implementation left to alias to (pre-1.0, no compat baggage); the
    // read-path labeling of persisted historical steps lives in
    // `review_step_kind_display_name` instead. `funnel.bundle`/
    // `review.probe-render` (below) are registered by [`register_review_kinds`]
    // above.
    //
    // (#1530 follow-on Packet A1) The probe render step is the Tier-3 half
    // of the probe stage that stays bespoke (rendering against this
    // pipeline's own `BundleInput`/`BundleSelector` types); the dispatch
    // half is the generic map, exactly mirroring the verify stage's own
    // render/dispatch split below.
    //
    // (#1512, #1530 follow-on Packet A1) Each CLAIMED probe task now has
    // TWO steps — a `review.probe-render` step (this seat's selector +
    // role id, resolved into a rendered prompt collection AT RUN TIME) then
    // a generic `dispatch.map` (this seat's dispatch identity, the shared
    // `bucket_group: "probe"` allowance, `retry_on_empty: 1`, residency
    // hints — everything the OLD single-step probe task carried, MINUS
    // `collection`, which the map step now resolves at runtime from the
    // render step's `Step.output` via `resolve_map_collection`'s
    // single-dependency fallback, the exact mechanism
    // `review-verify-step` already exercises). NOTE: a hosted seat's
    // `endpoint` block carries only the URL / auth MECHANICS (Keychain item
    // name / env-var NAME — never a secret value; see `EndpointAuth`), the
    // same material `profiles.json` already persists on disk.
    //
    // One role, one task, one dispatch (#1512) — the old (seat, draw) fan-
    // out (`ResolvedSeatStaffing::k` multiplying a seat into several sibling
    // tasks) retired with the `expand` template it depended on. `k` still
    // exists on the staffing (snapshotted into the envelope verbatim for
    // back-compat/bench reporting) but is no longer read here — probe
    // recall breadth is a config edit (add another probe role/task to
    // review.json), never a per-run draw multiplier.
    //
    // (#1541) `ProbeSeatSpec` no longer computes or carries a build-time
    // `bundles` snapshot — the OLD `select_bundles_for_staffing(&ctx.bundles,
    // ...)` call that used to live here duplicated the render step's own
    // run-time call, and the two were only guaranteed to agree because both
    // were the identical pure function over the identical `ctx.bundles`. The
    // dedup boundary now reads the render step's PUBLISHED selection off
    // `REVIEW_PROBE_SELECTION_ARTIFACT` instead (see that constant's doc and
    // `reconstruct_probe_stage`'s), so `ProbeSeatSpec` only needs this seat's
    // TOPOLOGY (identity, remote-ness, its one claimed task id) — never a
    // second copy of WHICH bundles it covers.
    let remote_budget = ctx.remote_max_tokens_per_execution;
    let mut probe_specs: Vec<ProbeSeatSpec> = Vec::new();
    for (staffing, task_id) in probes.iter().zip(claims.iter()) {
        let identifier = seat_identifier(&staffing.pm);
        let endpoint = seat_endpoint(&staffing.pm);
        let endpoint_host = seat_endpoint_host(&staffing.pm);
        let max_tokens = resolve_seat_max_tokens(staffing, DEFAULT_PROBE_MAX_TOKENS);

        let task = tasks.iter().find(|t| &t.id == task_id).unwrap_or_else(|| {
            panic!("the claimed probe task `{task_id}` must survive pruning")
        });
        // (#1530) A user-tier `~/.darkmux/mission-configs/review.json` may
        // still declare the pre-#1530 one-step probe task. That is an
        // operator-config shape mismatch, not an internal invariant break —
        // contract 7 puts loud validation at the consumption point and keeps
        // panics off the hot path, so this bails with the fix named rather
        // than aborting the process.
        if task.step_ids.len() != 2 {
            anyhow::bail!(
                "darkmux: \"review\" mission config's probe task `{task_id}` declares {} step(s), \
                 but a probe task needs exactly two: a `review.probe-render` step followed by a \
                 `dispatch.map` step. A user-tier copy at \
                 ~/.darkmux/mission-configs/review.json predating the render-step split needs the \
                 render step added (or delete the copy to fall back to the built-in document).",
                task.step_ids.len()
            );
        }
        let render_step_id = task.step_ids[0].clone();
        let map_step_id = task.step_ids[1].clone();

        // (#1541) The FIRST step must actually be the render kind, not just
        // present. A hand-edited user-tier review.json whose probe task leads
        // with some other step would otherwise build a graph where nothing
        // publishes the seat's bundle selection — every seat would then hit
        // the attribution-desync warning at run time and DROP its flags. Fail
        // here, at the consumption point, naming the fix (contract 7).
        if steps.get(&render_step_id).map(|s| s.kind.as_str()) != Some("review.probe-render") {
            anyhow::bail!(
                "darkmux: \"review\" mission config's probe task `{task_id}` must lead with a \
                 `review.probe-render` step (its first step is `{}`), because that step is what \
                 publishes the seat's bundle selection for attribution. A user-tier copy at \
                 ~/.darkmux/mission-configs/review.json needs the render step first (or delete \
                 the copy to fall back to the built-in document).",
                steps.get(&render_step_id).map(|s| s.kind.as_str()).unwrap_or("<missing>")
            );
        }

        // The render step's config: this seat's selector (data — WHICH
        // bundles) and role id (WHICH prior text) — both known at build
        // time; only the selection ITSELF moves to run time (see
        // `ReviewProbeRenderStepKind`'s doc).
        let selector_val: Option<serde_json::Value> = staffing
            .selector
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .context("serializing probe seat selector")?;
        let render_step = steps.get_mut(&render_step_id).unwrap_or_else(|| {
            // (#1284 review round 2, consider 3) Hard assert posture
            // preserved: a release build must not silently mint a spec no
            // interpreted step backs.
            panic!(
                "the interpreted graph must have a step `{render_step_id}` for probe task `{task_id}`"
            )
        });
        render_step.config = json!({
            "selector": selector_val,
            "role_id": staffing.role_id,
        });

        let map_step = steps.get_mut(&map_step_id).unwrap_or_else(|| {
            panic!("the interpreted graph must have a step `{map_step_id}` for probe task `{task_id}`")
        });
        let mut config = json!({
            "model": identifier,
            "system": "",
            "user_template": "{item}",
            "temperature": PROBE_TEMPERATURE,
            "max_tokens": max_tokens,
            "timeout_seconds": ctx.timeout_seconds,
            "retry_on_empty": 1,
            // (#1605 cause 2 — "every probe draw errored") ONE bounded retry
            // (with a short backoff, `RETRY_ON_ERROR_BACKOFF` in
            // `darkmux-crew`'s `dispatch.map` builtin) on a probe draw's
            // dispatch ERROR, not just an empty reply. The sampled darkmux#1605
            // failures showed batches where EVERY draw errored together —
            // reads like a transient endpoint-side blip, not a structural
            // break — so the probe stage alone opts into this generic,
            // default-off `dispatch.map` knob. Deliberately NOT set on the
            // verify map step below: retrying anything downstream of a
            // successful probe run re-runs paid inference to chase a flaky
            // post, which the issue explicitly rules out.
            "retry_on_error": 1,
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
        map_step.config = config;

        probe_specs.push(ProbeSeatSpec {
            name: staffing.name.clone(),
            identifier,
            remote: endpoint.is_some(),
            endpoint_host,
            draw_task_ids: vec![task_id.clone()],
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

    // (#1530 Packet 3a) Stamp the judge seat's model/passes/max_tokens/
    // endpoint onto `review-judge-step`'s config — the SAME "compute at
    // build time, stamp, read back in run_streaming" pattern the probe/
    // verify seats above already use, now mirrored here so
    // `ReviewJudgeStepKind` needs no `ResolvedSeatStaffing` constructor
    // field. UNLIKE probe/verify (whose pre-interpret config is `null`),
    // `review-judge-step`'s config already carries `concurrency` (this
    // function's own `step_config_overrides` stamp, above) — so this MERGES
    // into the existing config object rather than overwriting it wholesale.
    {
        let judge_identifier = seat_identifier(&judge.pm);
        let judge_endpoint = seat_endpoint(&judge.pm);
        let judge_endpoint_host = seat_endpoint_host(&judge.pm);
        let judge_max_tokens = resolve_seat_max_tokens(&judge, DEFAULT_JUDGE_MAX_TOKENS);
        let judge_step = steps
            .get_mut("review-judge-step")
            .expect("interpreted \"review\" graph must have a review-judge-step");
        let config_obj = judge_step.config.as_object_mut().expect(
            "review-judge-step config is always an object (step_config_overrides stamps \"concurrency\")",
        );
        config_obj.insert("model".to_string(), json!(judge_identifier));
        config_obj.insert("passes".to_string(), json!(judge.passes));
        config_obj.insert("max_tokens".to_string(), json!(judge_max_tokens));
        config_obj.insert("endpoint_host".to_string(), json!(judge_endpoint_host));
        // (#1748) The SAME `"source"` block `review-bundle-step` carries —
        // `ReviewJudgeStepKind::run_streaming` reconstructs its own
        // `FileSource` from it (via the SAME reader,
        // `file_source_from_step_config`) so the mechanical absence-claim
        // backstop can check a confirmed finding against the whole file,
        // not just the bundle excerpt the AI seats saw.
        config_obj.insert("source".to_string(), bundle_source_spec_json(&bundle_spec.source));
        if let Some(ep) = judge_endpoint {
            config_obj.insert(
                "endpoint".to_string(),
                serde_json::to_value(ep).context("serializing judge seat endpoint")?,
            );
        } else {
            config_obj.insert("model_key".to_string(), json!(judge.pm.id));
            config_obj.insert("identifier".to_string(), json!(judge_identifier));
            if let Some(n_ctx) = judge.pm.n_ctx {
                config_obj.insert("n_ctx".to_string(), json!(n_ctx));
            }
        }
    }

    // (#1530 Packet 3a follow-on) Stamp the mint-time `probe_specs`/
    // `remote_budget` onto `review-dedup-step`'s own config — the SAME
    // "compute at build time, stamp, read back in run_streaming" pattern
    // the judge seat's staffing above already uses — so `ReviewDedupStepKind`
    // needs no constructor fields (see that kind's own doc for why
    // `Step.config` won over a bus artifact). UNLIKE the judge step, this
    // step's pre-interpret config is `null` (see review.json's
    // `review-dedup-task`), so this overwrites wholesale rather than
    // merging into an existing object.
    //
    // (#1530) Located by KIND, not by the literal step id. A hand-edited
    // user-tier review.json that renames this step still builds and runs —
    // routing on what a step IS rather than what it's named is the same
    // principle #1538 applied to config ids, and it keeps a rename from
    // aborting the process here.
    {
        let dedup_step_id = steps
            .values()
            .find(|s| s.kind == "review.dedup")
            .map(|s| s.id.clone())
            .context("darkmux: the interpreted \"review\" graph declares no `review.dedup` step")?;
        let dedup_step = steps
            .get_mut(&dedup_step_id)
            .expect("the step id was just read out of this same map");
        dedup_step.config = json!({
            "probe_specs": probe_specs,
            "remote_budget": remote_budget,
        });
    }

    // (`review.dedup`/`review.judge` registered by [`register_review_kinds`]
    // above, alongside their `funnel.*` legacy aliases.)

    // (#1442 ship-2b) The render step — the Tier-3 half of the verify
    // stage that stayed bespoke (frozen `verify_prompt` assembly against
    // this pipeline's own types); the dispatch half is the generic map.
    // (`review.verify-render` registered by [`register_review_kinds`] above.)
    //
    // (#1530 Packet 3a) `ReviewVerifyRenderStepKind`'s only use of the
    // verify staffing was `.is_none()` — stamp just that bit onto its own
    // step's config instead of cloning the whole staffing into a
    // constructor field.
    {
        let render_step = steps
            .get_mut("review-verify-render-step")
            .expect("interpreted \"review\" graph must have a review-verify-render-step");
        render_step.config = json!({ "verify_seat_staffed": verify.is_some() });
    }

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

    // (#1530 Packet 3a) `ReviewSynthesisStepKind::run_streaming`'s ONLY use
    // of the verify staffing is `apply_verify_results`'s three derived
    // values (`identifier`/`remote`/`endpoint_host`) — stamp those onto this
    // step's own config (present iff a verify seat was staffed, mirroring
    // the `if let Some(vstaff) = &self.verify` test this replaces) instead
    // of cloning the whole staffing into a constructor field.
    //
    // (#1530 Packet 3a follow-on) ALSO stamp `dedup_task_id`/`judge_task_id`/
    // `verify_task_id`/`remote_budget` here, unconditionally (this step's
    // pre-interpret config is `null` — see review.json's
    // `review-synthesis-task` — so the first write below always establishes
    // the config object; the verify-seat block afterward MERGES into it,
    // mirroring the judge step's stamp above) — so `ReviewSynthesisStepKind`
    // needs no constructor fields at all.
    {
        // (#1530) Reuses `synthesis_step_id`, already resolved BY KIND just
        // above — so this stamp, which now runs unconditionally (it used to
        // happen only when a verify seat was staffed), can't turn a renamed
        // step in a user-tier config into a process abort.
        let synthesis_step = steps
            .get_mut(&synthesis_step_id)
            .expect("the step id was just read out of this same map");
        synthesis_step.config = json!({
            "dedup_task_id": dedup_task_id,
            "judge_task_id": judge_task_id,
            "verify_task_id": verify_task_id,
            "remote_budget": remote_budget,
        });
        if let Some(vstaff) = &verify {
            let identifier = seat_identifier(&vstaff.pm);
            let remote = vstaff.pm.is_remote();
            let endpoint_host = seat_endpoint_host(&vstaff.pm);
            let config_obj = synthesis_step
                .config
                .as_object_mut()
                .expect("review-synthesis-step config is always an object (stamped just above)");
            config_obj.insert("verify_identifier".to_string(), json!(identifier));
            config_obj.insert("verify_remote".to_string(), json!(remote));
            config_obj.insert("verify_endpoint_host".to_string(), json!(endpoint_host));
        }
    }
    // (`review.synthesis` registered by [`register_review_kinds`] above,
    // alongside its `funnel.synthesis` legacy alias.)

    Ok(BuiltReviewGraph {
        tasks,
        steps,
        registry,
        initial_env,
        synthesis_step_id,
        phase_id_of_step,
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
    let BuiltReviewGraph { tasks, mut steps, registry, initial_env, synthesis_step_id, .. } = graph;
    let tasks_by_id: std::collections::BTreeMap<String, Task> =
        tasks.into_iter().map(|t| (t.id.clone(), t)).collect();

    // (#1530 Packet 1) The run-scoped state this pipeline's dispatching
    // step kinds share, now minted HERE (at RUN time, not build time — see
    // `BuiltReviewGraph::initial_env`'s doc for why that's the more honest
    // home) and handed to `run_step_graph` via its caller-seed path, which
    // MERGES them onto the `ArtifactBus` over whatever default
    // `ReviewDedupStepKind::provides()`'s factories would otherwise
    // materialize (`ArtifactBus::seed`'s own doc). `shared_env` starts from
    // `initial_env` (already carrying the interpret-time warnings/bundle
    // count from `build_review_graph`) plus this run's own
    // case_id/crew/mode/fingerprint/staffing — exactly the same pre-stamp
    // this function applied in place before #1530 Packet 1, just built
    // fresh here instead of mutated through an `Arc` built earlier.
    let shared_env: SharedReviewEnvelope = Arc::new(StdMutex::new(ReviewEnvelope {
        case_id: ctx.case_id.clone(),
        crew: crew_name.to_string(),
        mode: mode_label(mode).to_string(),
        fingerprint: fingerprint_val,
        staffing: Some(staffing),
        ..initial_env
    }));
    let probe_members: Arc<StdMutex<Vec<MemberRecord>>> = Arc::new(StdMutex::new(Vec::new()));
    let probe_warnings: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    // (#1530 Packet 3a) The run-scoped context — every review kind now reads
    // this off the bus (`ctx.artifact::<ReviewStepContext>(REVIEW_CONTEXT_ARTIFACT)`
    // in `run_streaming`, and `ReviewJudgeStepKind::residency`) instead of
    // holding its own `Arc<ReviewStepContext>` constructor field. Seeded the
    // SAME way the three accumulators above are: a real, run-owned value
    // overwriting `make_review_context_artifact`'s context-free default via
    // the caller-seed path. `Arc::new(ctx.clone())` — cheap (a handful of
    // strings + a `Vec<BundleInput>` clone), once per run, not per step.
    let run_ctx_artifact: Arc<ReviewStepContext> = Arc::new(ctx.clone());
    let seed_artifacts: [(&'static str, Arc<dyn Any + Send + Sync>); 4] = [
        (REVIEW_CONTEXT_ARTIFACT, run_ctx_artifact as Arc<dyn Any + Send + Sync>),
        (REVIEW_ENVELOPE_ARTIFACT, shared_env.clone() as Arc<dyn Any + Send + Sync>),
        (REVIEW_MEMBERS_ARTIFACT, probe_members.clone() as Arc<dyn Any + Send + Sync>),
        (REVIEW_WARNINGS_ARTIFACT, probe_warnings.clone() as Arc<dyn Any + Send + Sync>),
    ];

    // (#1349) Host telemetry only — no bookend struct. The caller already
    // owns the run's liveness bookend (see this function's doc); this
    // sampler's samples are drained and forwarded to `emitter` alongside
    // `run_step_graph`'s own step-lifecycle records, same interleaving
    // discipline `HostTelemetrySampler`'s doc describes.
    let telemetry = HostTelemetrySampler::start(
        ctx.case_id.clone(),
        crew_name.to_string(),
        run_obs::DEFAULT_TELEMETRY_INTERVAL,
        run_obs::DEFAULT_TELEMETRY_POLL,
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
            for sample in telemetry.try_drain() {
                emitter.emit(sample);
            }
            emitter.emit(record);
        },
        persist,
        // (#1684 Packet 2) No gate handler — the built-in `review` config
        // declares no `gate: "operator"` step, and this driver is never
        // pointed at an operator-authored config that might. `None` still
        // fails CLOSED (never silently ungated) if a gated step somehow
        // reached this graph — see `darkmux_crew::gate::resolve_gate`'s
        // `None`-handler fallback.
        None,
        // (#1442 ship-2b) The ctx-mock adapter — `None` in production; a
        // mocked test's probe/verify `dispatch.map` items dispatch through
        // the same `chat_override` every bespoke review kind uses.
        review_dispatch_override(ctx),
        // (#1530 Packet 1) The caller-seed path — the run-stamped envelope
        // + fresh member/warning accumulators, minted above.
        &seed_artifacts,
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
            env.degenerate_kind = Some(DegenerateKind::Error);
            for sample in telemetry.try_drain() {
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
                // (#1530) Both causes this used to name were DEAD: per-seat
                // `selector` is hardcoded `None` by the only production
                // constructor of `ResolvedSeatStaffing`, and the "crew's probe
                // expansion" retired in #1512 when the probe stage became
                // static tasks in the document. A diagnostic that sends the
                // operator hunting two knobs that cannot exist is worse than a
                // terse one — it costs them the debugging session. Name what
                // can actually be true instead.
                format!(
                    "no probe seat placed a call: zero draws across {} staffed probe seat(s), \
                     though the diff produced {} bundle(s) — every seat returned nothing usable \
                     rather than failing, so check the probe seats' own output (their model may \
                     be replying in a shape the parser rejects); a review that examined nothing \
                     is never a clean pass",
                    // (#1530) `env.staffing`, NOT `env.members`: a member record
                    // is pushed only `if draws > 0`, and this branch's guard is
                    // `total_draws == 0` — so `members` is EMPTY by construction
                    // here and would render "across 0 staffed seat(s)" on a run
                    // that staffed three. The staffing snapshot is the count the
                    // operator actually means.
                    env.staffing.as_ref().map(|s| s.probes.len()).unwrap_or(0),
                    env.bundles
                ),
            );
            env.degenerate_kind = Some(DegenerateKind::Error);
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
            env.degenerate_kind = Some(DegenerateKind::Error);
        }
        // (#1530) Say it on STDERR too, not only in the envelope. Since
        // bundling moved into the graph, a launch MISCONFIGURATION (a typo'd
        // `--bundler`, a `source.path` that doesn't exist) is no longer an
        // `Err` out of `launch` that `main` prints as an anyhow chain — it is
        // a step error, and the scheduler prints nothing. Without this the
        // operator at a terminal sees a `{"mode":"degraded",…}` object scroll
        // past and `echo $?` print 0, with the actual cause only inside the
        // JSON. The coder path got this treatment when its composition moved
        // in (#1546); bundling is the likelier thing to be misconfigured, so
        // it needs it more. The exit code is deliberately unchanged here —
        // `launch`'s documented contract is "0 on any produced review output;
        // CI-facing pass/fail comes from `mode`", and darkmux-review.yml
        // reads `mode` — but silence was never part of that contract.
        if let Some(reason) = &env.degenerate {
            eprintln!("{}", darkmux_types::style::error(&format!("✗ review: {reason}")));
        }
        env
    };

    // Final drain before `telemetry` drops (its own `Drop` then stops the
    // sampler thread) — same "known, accepted loss window" the retired
    // bookend guard documented: at most one final-tick sample can land in
    // the brief window between this drain and the thread join completing.
    for sample in telemetry.try_drain() {
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
