//! (#2310 P4d) The review ENVELOPE — the recorded shape of a PR review run,
//! and the mapping from that record onto a [`RunOutcome`].
//!
//! The executable review funnel this module used to drive (bundles → probe
//! seats → dedup → double-confirm judge → verify → synthesis, and the ten
//! Tier-3 step kinds that ran it) was DELETED in #2310 P4d: `review` is now
//! a mission config on the crawl's shared building blocks
//! (`templates/builtin/mission-configs/review.json` — `plan.sites` +
//! `crawl.unit` + `records.gather` + `deliver.github_review`), launched by
//! the generic launcher with no bespoke launcher of its own.
//!
//! What survives here is the DATA: [`ReviewEnvelope`] and the types it
//! carries ([`ProbeFlag`], [`JudgedFlag`], [`JudgeRecord`], [`VerifyRecord`],
//! [`Tier`], [`DegenerateKind`], [`NeedsCheckCluster`]), plus
//! [`review_outcome`]/[`review_mission_outcome`]. Envelopes recorded by past
//! runs still deserialize, and `darkmux-serve` still renders them — deleting
//! the types would blind the viewer to every review run already on disk.
//! Nothing in this module dispatches a model any more.

use super::bundle::BundleSkipReport;
use darkmux_crew::remote_budget::RemoteBudgetRecord;
use darkmux_crew::run_outcome::RunOutcome;
// (#1877 item 2) The run-record + run-observability substrate lives in
// `darkmux-crew`; the run emitter aliases keep their review-era names so
// every external `impl ReviewEmitter for X` (`review_bench.rs`) keeps
// compiling unchanged.
pub use darkmux_crew::run_obs::{NullEmitter, RunEmitter as ReviewEmitter};
pub use darkmux_crew::run_record::{
    seat_identifier, staffing_snapshot, MemberRecord, SeatStaffingSnapshot, StaffingSnapshot, StepRecord,
};
use serde::{Deserialize, Serialize};

// ─── execution mode ───────────────────────────────────────────────────────

// (#1877, this issue) `ExecMode` and `wave_schedule_to_exec_mode` moved to
// `darkmux_gestalt::waves` — see their doc comments there for the full
// argument (deciding sequential-vs-parallel residency cycling is a hardware
// residency question gestalt already owns; this module used to ask gestalt
// for a `WaveSchedule` and then re-derive the answer itself). Re-exported
// under the SAME names so every existing reference in this crate and in
// the retired review funnel launcher (the binary crate) keeps resolving
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

// ─── probe flags ──────────────────────────────────────────────────────────

/// One probe draw's finding, post-parse but pre-dedup. `anchor` starts
/// `None` at construction — [`dedup_flags`] is where anchor extraction
/// happens (it needs the diff to validate a quote against, so doing the
/// extraction there keeps ONE place responsible for both jobs at once).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// One `(file, mechanism-family)` cluster of `needs_check` findings — a
/// count, never a drop (#1299). Carried on [`ReviewEnvelope`] so a recorded
/// run's own clusters stay readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeedsCheckCluster {
    /// The bundle id (file path) the clustered findings share.
    pub file: String,
    /// The mechanism family the clustered findings share.
    pub mechanism: String,
    /// How many `needs_check` findings this cluster stands in for.
    pub count: usize,
}

// ─── the envelope ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    /// (#2119) Set only when a pinned `--bundler` plugin declined this diff
    /// (its own stderr matched
    /// [`crate::lab::bundle::external::PLUGIN_DECLINE_MARKER`] — "not my
    /// language," not a crash) and `ReviewBundleStepKind::run_streaming`
    /// fell back to the built-in bundler over the same diff instead of
    /// erroring the whole review graph. `"built-in (plugin declined:
    /// <reason>)"`, where `<reason>` is the plugin's own error text. `None`
    /// on every other run: no `--bundler` pinned, a pinned plugin that
    /// produced bundles normally, or one that failed for a real reason
    /// (still a hard step error, unchanged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundler_fallback: Option<String>,
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
/// onto [`RunOutcome`], scoped for what the retired review funnel launcher's
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
