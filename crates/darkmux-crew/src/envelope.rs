//! `MissionEnvelope` — the standard output contract EVERY mission emits
//! (#1284 Packet 2), plus generalized finalization.
//!
//! Prior to this packet, mission finalization was a bespoke, per-mission-
//! type shape: `src/pr_review.rs::finalize_review_mission` (#1365) hand-
//! rolled the "clean run completes phases + closes the mission; anything
//! else abandons phases + closes with a reason" decision directly against
//! `ReviewEnvelope`. That logic is genuinely mission-type-agnostic — the
//! decision only ever needed an overall outcome + a reason, never
//! `ReviewEnvelope`'s review-specific fields (tier counts, judged flags,
//! …). This module extracts the AGNOSTIC part into a standard contract
//! ([`MissionEnvelope`]) plus ONE finalization function ([`finalize_mission`])
//! every future mission-type driver can reuse, while the mission-specific
//! payload rides untouched in [`MissionEnvelope::payload`].
//!
//! **Producers and consumers.** A mission driver (today: `run_review_graph`
//! via `src/pr_review.rs`'s conversion; future: any config-launched mission,
//! #1284 Packet 3+) builds a `MissionEnvelope` at the end of its run and
//! hands it to [`finalize_mission`], which drives the phase/mission
//! terminal transitions in [`crate::lifecycle`]. The SAME envelope is the
//! natural artifact for the mission board (`darkmux mission status`) and
//! the future graph-lens viewer (#1284 Packet 5) to render — both read
//! `status`/`reason`/`warnings`/`remote_budgets` without needing to
//! understand any mission type's own payload shape.
//!
//! # Status decision — who decides what, and who consumes it
//!
//! [`MissionOutcomeStatus`] has four values. A mission driver's OWN
//! conversion logic decides which one applies (this module does not
//! prescribe the mission-type-specific thresholds — see `CLAUDE.md`'s
//! `DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION` budget-policy text for the
//! review pipeline's own rules), but the SHAPE of the decision is uniform:
//!
//! - **Clean** — the mission produced its full intended output with no
//!   constrained sub-work. (Review: `env.degenerate.is_none() &&
//!   env.warnings.is_empty()`.)
//! - **Degraded** — real, postable output was produced, but some sub-stage
//!   was CONSTRAINED (a remote budget exhausted, a bounded-retry failure
//!   reduced coverage) — the run's warnings channel is non-empty but no
//!   "treat as dead" gate fired. Degraded is NOT a lesser Clean for
//!   finalization purposes — see the mapping below. (Review:
//!   `env.degenerate.is_none() && !env.warnings.is_empty()`.)
//! - **Degenerate** — the mission ran to completion but hit an
//!   operator-decision "this run produced no usable signal" gate. (Review:
//!   `env.degenerate.is_some()`.)
//! - **Error** — the mission's driver returned a hard `Err` before
//!   producing any envelope at all (a scheduling failure, a resolution
//!   failure before dispatch began).
//!
//! # `outcome` — the typed source `status` derives from (#1877 item 2)
//!
//! [`MissionEnvelope::outcome`] carries a [`crate::run_outcome::RunOutcome`]
//! when the mission driver has one — today, only the PR-review pipeline
//! (`src/mission_launch_review.rs`'s `review_result_to_mission_envelope`)
//! does. Before this field existed, "a run was partially covered" lived as
//! a CONVENTION: review pushed a warning string into `warnings` and this
//! module's own status derivation read "non-empty warnings" as `Degraded`.
//! That convention still decides `Degraded` today (see the table below),
//! but now it does so by way of a typed [`RunOutcome::Partial`] a driver
//! constructs explicitly, rather than an ad hoc `if !warnings.is_empty()`
//! a driver has to reinvent. [`MissionOutcomeStatus::from_outcome`] is the
//! ONE place that mapping lives — a driver with a `RunOutcome` calls
//! [`MissionEnvelope::from_outcome`] and never separately decides `status`
//! by hand.
//!
//! **`MissionOutcomeStatus` itself gains no `Partial` variant.** A driver
//! that adopts `RunOutcome` gets a typed, inspectable `Partial { reasons }`
//! on `outcome` — real progress over the untyped-warning convention — but
//! `status` still collapses `Partial` into `Degraded` for every
//! finalization/exit-code/render consumer, because every one of those
//! consumers already treats `Degraded` as "real output, something was
//! constrained" and none of them has a THIRD bucket to put a partial run
//! in without a deliberate, separately-reviewed change to what each of
//! those consumers reports (mission-board coloring, `mission launch`'s
//! exit code, the `/runs` `RunStatus` a poller sees). Collapsing here is a
//! stated decision, not an oversight — see the per-consumer notes in
//! `crates/darkmux-serve/src/runs.rs`, `crates/darkmux-crew/src/dispatch_as_crew_of_one.rs`,
//! and `src/mission_launch.rs` for why each one keeps reading `status`
//! (never `outcome`) and is therefore unaffected by this field's arrival.
//!
//! **Finalization consumes ONLY the status**, via
//! [`MissionOutcomeStatus::phase_outcome`]:
//!
//! | Status       | Phase outcome | Mission outcome        |
//! |--------------|---------------|-------------------------|
//! | Clean        | Complete      | Finalized (real reason) |
//! | Degraded     | Complete      | Finalized (real reason) |
//! | Degenerate   | Abandoned     | Finalized (real reason) |
//! | Error        | Abandoned     | Finalized (real reason) |
//!
//! Degraded folds into the SAME phase/mission outcome as Clean — a
//! degraded run still posted real findings (CLAUDE.md: "DEGRADED-but-
//! posted runs must keep posting"; degraded means honest LABELING, not
//! discarding). The `status` field itself is what a consumer (the board,
//! the viewer) reads to tell Clean and Degraded apart; finalization's own
//! phase/mission-status decision doesn't need to.
//!
//! Every mission ALWAYS reaches a terminal phase/mission status —
//! `finalize_mission` never leaves anything `Running`/`Active` (#1365's
//! own finding: a mid-flight failure that stays `Active` forever is
//! indistinguishable from a stuck mission in `darkmux mission status`).

use crate::lifecycle;
use crate::run_outcome::RunOutcome;
use crate::types::{MissionStatus, PhaseStatus};
use serde::{Deserialize, Serialize};

/// Current schema version for `MissionEnvelope` documents. Plain semver
/// applied to the DATA SHAPE — mirrors `MISSION_CONFIG_SCHEMA`
/// (`crate::mission_config`), `RULES_SCHEMA_VERSION` (`darkmux-eureka`),
/// `FLOW_SCHEMA_VERSION` (`darkmux-flow`). See `CLAUDE.md`'s "Versioning"
/// section for the additive-minor / breaking-major bump discipline.
///
/// **1.0 -> 1.1 (#1877 item 2):** adds the optional `outcome` field
/// ([`MissionEnvelope::outcome`]). Additive per the documented rule — a new
/// OPTIONAL field a future consumer can safely ignore — so this is a MINOR
/// bump, not major. Readers stay lenient: `outcome` is `#[serde(default)]`,
/// so a 1.0 envelope with no `outcome` key at all deserializes cleanly as
/// `outcome: None`, and nothing about `status`/`reason`/`warnings` changes
/// shape for an old reader.
///
/// **1.1 -> 1.2 (#1881), CLOSES the exposure the paragraph above used to
/// document.** Both `RunOutcome` (`outcome`'s tag) and `MissionOutcomeStatus`
/// (`status` itself) now carry a `#[serde(other)]` catch-all
/// ([`RunOutcome::Unknown`], [`MissionOutcomeStatus::Unknown`]). Verified
/// directly: `an_unrecognized_outcome_variant_degrades_to_unknown_and_the_rest_of_the_document_still_parses`
/// (below) decodes `{"status":"degraded","outcome":{"state":"throttled"}}` and
/// gets back a full `MissionEnvelope` with `status`/`mission_id`/`phases`
/// intact and `outcome: Some(RunOutcome::Unknown)` — not an `Err`. Adding a
/// new `RunOutcome` variant OR a new `MissionOutcomeStatus` variant is
/// therefore safely additive (a MINOR bump) again, same as any other
/// optional field: an older reader degrades the unrecognized value to its
/// own `Unknown` rather than failing the whole document.
///
/// **This is additive for the PARSE, not for the VERDICT (#1881, QA-caught).**
/// The first time a real fifth `MissionOutcomeStatus` variant ships, every
/// OLDER binary still running in the fleet renders every run carrying it as
/// amber `RunStatus::Unparseable` and raises a `darkmux doctor` warning —
/// on purpose, the honest degradation this whole fix exists to produce, but
/// worth knowing up front: adding a variant is safe to DO without breaking
/// an older reader's parse, but it is NOT free of fleet-wide operator-
/// visible effect until every machine has upgraded. A future variant-adder
/// reading only the "safely additive" sentence above should expect that
/// surprise, not be blindsided by it.
///
/// **What stayed true from 1.1, unaffected by this bump:** a document that
/// fails to parse for a reason NEITHER catch-all can rescue (malformed
/// JSON, a required field missing or of the wrong type, `outcome` present
/// but not even a JSON object) is still a hard error from
/// `darkmux_crew::lifecycle::load_envelope` — that is precisely what
/// `RunStatus::Unparseable` exists for
/// (`crates/darkmux-serve/src/runs.rs`'s `mission_run_status`). Leniency
/// widens what counts as "understood," it does not promise every possible
/// malformed document parses.
pub const MISSION_ENVELOPE_SCHEMA: &str = "1.2";

/// The overall outcome a mission's run reached — see the module doc's
/// "Status decision" section for how each value is decided and consumed.
///
/// **Forward-compat leniency (#1881):** [`MissionOutcomeStatus::Unknown`]
/// is a `#[serde(other)]` catch-all — a `status` value this binary doesn't
/// recognize (written by a newer darkmux) degrades to `Unknown` instead of
/// failing the WHOLE `MissionEnvelope` deserialize. This is the field
/// `crates/darkmux-serve/src/runs.rs`'s `mission_run_status` actually
/// reads to decide `RunStatus`, so unlike `RunOutcome::Unknown` (supplementary,
/// never read for that decision — see `run_outcome.rs`'s own note),
/// `MissionOutcomeStatus::Unknown` maps to `RunStatus::Unparseable` there —
/// the honest "this binary cannot tell you whether this run succeeded,"
/// never the pre-#1881 silent fallback to `Complete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MissionOutcomeStatus {
    Clean,
    Degraded,
    Degenerate,
    Error,
    /// (#1881) A `status` this binary does not recognize. Forward-compat
    /// READ-ONLY fallback, same shape as [`RunOutcome::Unknown`] — never
    /// constructed on purpose by any writer in this codebase.
    #[serde(other)]
    Unknown,
}

impl MissionOutcomeStatus {
    /// Derive a `status` from a [`RunOutcome`] — the ONE place a driver's
    /// typed outcome becomes the untyped four-value status every existing
    /// consumer reads. See the module doc's "`outcome` — the typed source"
    /// section for why `Partial` collapses into `Degraded` rather than
    /// growing a fifth `MissionOutcomeStatus` variant.
    ///
    /// - [`RunOutcome::Complete`] -> `Clean` — the docket was fully covered.
    /// - [`RunOutcome::Partial`] -> `Degraded` — same bucket every
    ///   `warnings`-driven degraded run already lands in; a driver that
    ///   ALSO wants `warnings` non-empty on a `Complete` outcome to read
    ///   `Degraded` (review's own pre-#1877 rule: any warning, not only a
    ///   docket-coverage one, downgrades from `Clean`) folds that into its
    ///   own `RunOutcome` construction rather than this generic mapping —
    ///   see `darkmux_lab::lab::review::review_mission_outcome`'s doc.
    /// - [`RunOutcome::Empty`] -> `Degenerate` — no usable signal, the
    ///   "worth retrying" gate.
    /// - [`RunOutcome::Unknown`] -> `Unknown` (#1881, corrected — an earlier
    ///   version of this mapping used `Degraded` here, reasoned as
    ///   "conservative." It was the opposite: `Degraded` is the bucket
    ///   `mission_run_status` (`crates/darkmux-serve/src/runs.rs`) and
    ///   `mission_launch.rs`'s exit-code match both read as a PASS
    ///   (`RunStatus::Complete`, exit code zero), and it directly
    ///   contradicted [`Self::phase_outcome`]'s OWN `Unknown` arm ten lines
    ///   below, which deliberately chose `Abandoned` because "a status we
    ///   can't interpret must never be assumed to have completed." Routing
    ///   through `Degraded` laundered an unrecognized outcome into that
    ///   assumption before `phase_outcome` ever got a chance to apply its
    ///   own safety — so the Abandoned guard never actually fired. Mapping
    ///   to `Unknown` instead keeps every downstream consumer consistent —
    ///   `phase_outcome` abandons, `mission_run_status` reports
    ///   `RunStatus::Unparseable`, and `mission_launch.rs`'s exit-code match
    ///   fails loudly — rather than any of them silently passing.
    ///   Unreachable from any real driver today either way: every caller of
    ///   `from_outcome` passes a freshly-computed `RunOutcome` it just
    ///   built, never one round-tripped through deserialize first, and
    ///   `Unknown` only ever arises from deserializing (see
    ///   `run_outcome.rs`) — but exhaustive (not a wildcard) so a future
    ///   caller that DOES round-trip an outcome through this function gets
    ///   the same honest "I don't know" every other unrecognized-value path
    ///   in this fix gets, not an accidental pass.
    pub fn from_outcome(outcome: &RunOutcome) -> Self {
        match outcome {
            RunOutcome::Complete => MissionOutcomeStatus::Clean,
            RunOutcome::Partial { .. } => MissionOutcomeStatus::Degraded,
            RunOutcome::Empty { .. } => MissionOutcomeStatus::Degenerate,
            RunOutcome::Unknown => MissionOutcomeStatus::Unknown,
        }
    }

    /// The phase-level outcome [`finalize_mission`] applies for every phase
    /// named in [`MissionEnvelope::phases`] — see the module doc's mapping
    /// table. Clean/Degraded both complete (a degraded run still posted
    /// real findings); Degenerate/Error both abandon.
    pub fn phase_outcome(self) -> PhaseOutcomeKind {
        match self {
            MissionOutcomeStatus::Clean | MissionOutcomeStatus::Degraded => PhaseOutcomeKind::Complete,
            // (#1881) `Unknown` only ever arises from deserializing a
            // status this binary doesn't recognize — `finalize_mission`
            // always drives a FRESHLY-constructed `status`, never a loaded
            // one, so this arm is unreachable in practice. Abandoned is the
            // conservative default if it's ever reached: a status we can't
            // interpret must never be assumed to have completed.
            MissionOutcomeStatus::Degenerate | MissionOutcomeStatus::Error | MissionOutcomeStatus::Unknown => {
                PhaseOutcomeKind::Abandoned
            }
        }
    }

    /// A short, human-readable default reason when the caller didn't
    /// supply one via [`MissionEnvelope::reason`] — finalization always
    /// records SOME reason on the mission-close flow record, never a bare
    /// close with no provenance.
    fn default_reason(self) -> &'static str {
        match self {
            MissionOutcomeStatus::Clean => "mission completed",
            MissionOutcomeStatus::Degraded => "mission completed (degraded)",
            MissionOutcomeStatus::Degenerate => "mission ended degenerate",
            MissionOutcomeStatus::Error => "mission errored",
            MissionOutcomeStatus::Unknown => "mission status unrecognized by this darkmux version",
        }
    }
}

/// The terminal `PhaseStatus` a phase reaches under [`finalize_mission`] —
/// a narrower enum than `crate::types::PhaseStatus` (only the two terminal
/// values finalization ever drives a phase to; `Planned`/`Running` are not
/// finalization outcomes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PhaseOutcomeKind {
    Complete,
    Abandoned,
}

/// One phase's declared finalization outcome — see [`MissionEnvelope::phases`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseOutcome {
    pub phase_id: String,
    pub outcome: PhaseOutcomeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One pipeline stage's remote token-bucket outcome — the generic shape
/// every mission-type-specific budget record (e.g. `darkmux-lab`'s
/// `ReviewEnvelope::remote_budgets`, typed
/// `Vec<crate::remote_budget::RemoteBudgetRecord>`) maps onto. Field-for-
/// field identical by design so a mapping is a pure struct-literal copy,
/// never a lossy translation. `RemoteBudgetRecord` moved into this crate
/// alongside its producing bucket type in #1877, but stays a DISTINCT type
/// from this row on purpose — `RemoteBudgetRecord` is a review-pipeline
/// stage's own accounting output, `RemoteBudgetRow` is `MissionEnvelope`'s
/// generic mapping target; unifying them is a separate call from the
/// bucket-type extraction #1877 made.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteBudgetRow {
    pub stage: String,
    pub max_tokens: u64,
    pub used_tokens: u64,
    pub exhausted: bool,
    pub skipped_calls: u32,
}

/// The standard output contract every mission emits (#1284 Packet 2). See
/// the module doc for the status-decision rules and who consumes each
/// field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissionEnvelope {
    pub mission_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<String>,
    pub status: MissionOutcomeStatus,
    /// (#1877 item 2) The typed docket-coverage outcome `status` was
    /// derived from, when the driver has one — see the module doc's
    /// `outcome` section. `None` for every driver that still constructs
    /// `status` directly via [`MissionEnvelope::new`] (the crew-of-one
    /// dispatch path, the generic scheduler-graph path, coder-phase — none
    /// of those partition their work into an independently-judgeable
    /// docket the way review does, so there is no `RunOutcome` for them to
    /// report yet) and for every envelope written before this field
    /// existed (1.0 envelopes deserialize with `outcome: None`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RunOutcome>,
    /// Human-readable provenance for `status` — e.g. the review pipeline's
    /// `env.degenerate` text, or a hard error's `Display` — recorded on the
    /// mission-close flow record so the board never shows a bare status
    /// with no explanation. `None` falls back to
    /// [`MissionOutcomeStatus::default_reason`] at finalization time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Per-phase finalization outcomes — see [`finalize_mission`]. Ordered
    /// (a `Vec`, not a set) so a future graph lens can render phases in
    /// declaration order without a separate sort key.
    #[serde(default)]
    pub phases: Vec<PhaseOutcome>,
    /// Non-fatal run findings the operator should read — the SAME concept
    /// `ReviewEnvelope::warnings` names (a bounded-retry failure, a
    /// partially-exhausted remote budget), generalized. Empty on a clean
    /// run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_budgets: Vec<RemoteBudgetRow>,
    /// Mission-type-specific payload — `ReviewEnvelope` (or any future
    /// mission type's own envelope) maps INTO this field rather than being
    /// replaced by `MissionEnvelope`. `Null` when the caller has nothing
    /// type-specific to attach (never used for `serde_json::Value::is_null`
    /// on read — always present on write, just possibly `Null`).
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub payload: serde_json::Value,
}

impl MissionEnvelope {
    /// Construct a `MissionEnvelope` with the given `status`, deriving
    /// every named phase's outcome from [`MissionOutcomeStatus::phase_outcome`]
    /// — the common case (a mission's phases all share ONE run-level
    /// outcome, exactly like the PR-review pipeline's three phases today).
    /// A mission type whose phases genuinely diverge (some complete, some
    /// abandon, from the SAME run) constructs `phases` by hand instead of
    /// calling this constructor.
    pub fn new(mission_id: impl Into<String>, status: MissionOutcomeStatus, phase_ids: &[&str]) -> Self {
        // Named `phase_outcome_kind`, not `outcome`, to keep this local
        // distinct from `MissionEnvelope::outcome` (the `RunOutcome` field
        // added in #1877 item 2) — different concept entirely, this is
        // per-phase `Complete`/`Abandoned`, that one is docket coverage.
        let phase_outcome_kind = status.phase_outcome();
        MissionEnvelope {
            mission_id: mission_id.into(),
            schema_version: Some(MISSION_ENVELOPE_SCHEMA.to_string()),
            status,
            outcome: None,
            reason: None,
            phases: phase_ids
                .iter()
                .map(|id| PhaseOutcome { phase_id: id.to_string(), outcome: phase_outcome_kind, reason: None })
                .collect(),
            warnings: Vec::new(),
            remote_budgets: Vec::new(),
            payload: serde_json::Value::Null,
        }
    }

    /// (#1877 item 2) Construct a `MissionEnvelope` from a typed
    /// [`RunOutcome`] rather than a hand-computed `status` — the
    /// "callers set both independently" gap this field closes. `status` is
    /// ALWAYS [`MissionOutcomeStatus::from_outcome`] applied to `outcome`;
    /// there is no way to call this constructor and get a `status` that
    /// disagrees with the `outcome` sitting right next to it on the same
    /// envelope. `outcome` is recorded on the returned envelope so a later
    /// reader (the mission board, a future graph lens) can render the real
    /// `Partial { reasons }` detail `status` alone collapses away.
    pub fn from_outcome(mission_id: impl Into<String>, outcome: RunOutcome, phase_ids: &[&str]) -> Self {
        let status = MissionOutcomeStatus::from_outcome(&outcome);
        let mut envelope = MissionEnvelope::new(mission_id, status, phase_ids);
        envelope.outcome = Some(outcome);
        envelope
    }
}

/// Drive every phase named in `envelope.phases` to its declared outcome,
/// then close the mission — the generalized #1365 finalization shape,
/// absorbing `src/pr_review.rs::finalize_review_mission`'s logic.
///
/// Best-effort throughout, matching the discipline `finalize_review_mission`
/// already established: a persistence hiccup here must never propagate as a
/// caller-visible error — only the mission-status/graph-lens VIEW of the run
/// would be degraded, not the run itself (the run already finished by the
/// time this is called).
///
/// (#1406) A refused phase transition is NO LONGER silently swallowed
/// (`let _`). The underlying `lifecycle` verbs are idempotent-safe via their
/// own state-machine guards, so a BENIGN no-op (the phase is already in the
/// terminal state the declared outcome asks for, e.g. a reopen re-finalizing
/// a phase a prior run already completed) stays quiet. But a GENUINE drift
/// refusal, where the phase is in a state the declared outcome can't reach
/// (e.g. a `Planned` phase asked to `Complete`, the exact #1406 bug the
/// honest per-phase derivation upstream is meant to prevent), is surfaced as
/// a loud warning naming the phase, the intended outcome, and the refusal,
/// instead of leaving a Finalized mission whose envelope disagrees with disk
/// with no signal. This function still applies the declared outcome and lets
/// `lifecycle` do the legality check; it only inspects current state to
/// classify a refusal, never to gate the transition.
///
/// (#1284 Packet 3) ALSO persists `envelope` itself via
/// `lifecycle::save_envelope` (`envelope.json` beside `mission.json`,
/// atomic-rename write like every other lifecycle save) — makes the
/// envelope a real on-disk artifact instead of constructed-and-dropped, and
/// is what makes a future graph-lens viewer (#1284 Packet 5) able to read a
/// mission's last-run outcome without re-deriving it from flow records.
/// Best-effort like everything else here: a persistence hiccup degrades
/// only the VIEW, never the run itself (already finished by this point).
/// (#1406/#1433) A finalize transition refusal, classified for signaling.
/// `finalize_mission` applies each phase's declared outcome and closes the
/// mission through the `lifecycle` legality check; when that check REFUSES
/// (`Err`), this says whether the refusal is a benign idempotent no-op (the
/// target already sits in the terminal state the finalize asked for) or
/// genuine drift worth a loud, named warning.
#[derive(Debug, PartialEq, Eq)]
enum FinalizeRefusal {
    /// The target is already in the terminal state the outcome wanted — a
    /// reopen re-finalizing an already-terminal phase/mission. Stay quiet.
    Benign,
    /// The target is in a state the declared outcome can't reach — the #1406
    /// bug shape (a `Planned` phase asked to `Complete`; an `Active` mission a
    /// close refused for a non-idempotent reason). Warn, loud and named.
    Drift,
}

/// Classify a PHASE finalize refusal from the outcome finalize wanted and the
/// phase's CURRENT on-disk status. Benign iff the phase already sits in the
/// matching terminal state; a `Planned` phase asked to `Complete` is Drift
/// (the exact #1406 bug the honest per-phase derivation upstream prevents).
fn classify_phase_refusal(
    outcome: PhaseOutcomeKind,
    current: Option<PhaseStatus>,
) -> FinalizeRefusal {
    let already_terminal = matches!(
        (outcome, current),
        (PhaseOutcomeKind::Complete, Some(PhaseStatus::Complete))
            | (PhaseOutcomeKind::Abandoned, Some(PhaseStatus::Abandoned))
    );
    if already_terminal {
        FinalizeRefusal::Benign
    } else {
        FinalizeRefusal::Drift
    }
}

/// Classify a MISSION-close refusal from the mission's CURRENT status. Benign
/// iff the mission is already `Finalized` (an idempotent re-finalize); every
/// other refusal — including an unknown status because the mission can't be
/// read — is Drift (#1433: the mission-close arm was previously
/// `let _`-swallowed, hiding a Finalized mission whose envelope disagreed
/// with disk).
fn classify_mission_close_refusal(current: Option<MissionStatus>) -> FinalizeRefusal {
    match current {
        Some(MissionStatus::Finalized) => FinalizeRefusal::Benign,
        _ => FinalizeRefusal::Drift,
    }
}

pub fn finalize_mission(envelope: &MissionEnvelope) {
    finalize_mission_with_payload(envelope, None)
}

/// (#2301) [`finalize_mission`] with a `mission close` PAYLOAD.
///
/// A generic graph's last phase can produce a run summary (the crawl's
/// `crawl.summary` step is the first one that does), and the operator-
/// facing home for a run's own numbers has always been the `mission close`
/// record's payload — that is where the retired crawl launcher wrote them.
/// Rather than give one mission kind a private close path, the generic
/// launcher promotes its LAST phase's last step output to this payload
/// when that output is a JSON object; everything else about finalization
/// is unchanged, and a `None` payload is byte-identical to before.
pub fn finalize_mission_with_payload(envelope: &MissionEnvelope, payload: Option<serde_json::Value>) {
    for phase in &envelope.phases {
        let result = match phase.outcome {
            PhaseOutcomeKind::Complete => lifecycle::phase_complete(&phase.phase_id),
            PhaseOutcomeKind::Abandoned => lifecycle::phase_abandon(&phase.phase_id),
        };
        if let Err(e) = result {
            let current = lifecycle::load_phase_by_id(&phase.phase_id).map(|p| p.status).ok();
            if classify_phase_refusal(phase.outcome, current) == FinalizeRefusal::Drift {
                eprintln!(
                    "warning: mission `{}` finalize could not drive phase `{}` to {:?}: {e:#}; \
                     phase left as-is, reconcile with `darkmux mission finalize` / `darkmux mission abort` (#1463)",
                    envelope.mission_id, phase.phase_id, phase.outcome
                );
            }
        }
    }
    let reason = envelope
        .reason
        .clone()
        .unwrap_or_else(|| envelope.status.default_reason().to_string());
    if let Err(e) = lifecycle::mission_terminal_with_reasoning_and_payload(
        &envelope.mission_id,
        crate::types::MissionStatus::Finalized,
        Some(&reason),
        payload,
    ) {
        // (#1433 follow-up) Was `let _`-swallowed — a mission that couldn't be
        // closed left its envelope.json disagreeing with an Active mission on
        // disk, silently. Classify like the phase refusals: quiet only when the
        // mission is ALREADY Finalized (idempotent re-finalize), loud otherwise.
        let current = lifecycle::load_mission_by_id(&envelope.mission_id).map(|m| m.status).ok();
        if classify_mission_close_refusal(current) == FinalizeRefusal::Drift {
            eprintln!(
                "warning: mission `{}` finalize could not close it: {e:#}; mission left as-is, \
                 reconcile with `darkmux mission finalize` (#1463)",
                envelope.mission_id
            );
        }
    }
    let _ = lifecycle::save_envelope(&envelope.mission_id, envelope);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Mission, MissionStatus, Phase, PhaseStatus};
    use std::env;
    use tempfile::TempDir;

    // ── Finalize refusal classifier (#1406/#1433) — pure, no I/O ──────────

    #[test]
    fn phase_refusal_loud_on_planned_asked_to_complete() {
        // The exact #1406 bug shape: a phase that never started, asked to
        // Complete, must classify as Drift (a loud, named warning).
        assert_eq!(
            classify_phase_refusal(PhaseOutcomeKind::Complete, Some(PhaseStatus::Planned)),
            FinalizeRefusal::Drift
        );
    }

    #[test]
    fn phase_refusal_quiet_when_already_in_the_target_terminal_state() {
        assert_eq!(
            classify_phase_refusal(PhaseOutcomeKind::Complete, Some(PhaseStatus::Complete)),
            FinalizeRefusal::Benign
        );
        assert_eq!(
            classify_phase_refusal(PhaseOutcomeKind::Abandoned, Some(PhaseStatus::Abandoned)),
            FinalizeRefusal::Benign
        );
    }

    #[test]
    fn phase_refusal_loud_on_cross_terminal_mismatch_and_unknown() {
        // Abandoned-asked-to-Complete (and vice versa) is drift, not idempotent.
        assert_eq!(
            classify_phase_refusal(PhaseOutcomeKind::Complete, Some(PhaseStatus::Abandoned)),
            FinalizeRefusal::Drift
        );
        // Unreadable phase (None) can't be proven idempotent -> loud.
        assert_eq!(
            classify_phase_refusal(PhaseOutcomeKind::Abandoned, None),
            FinalizeRefusal::Drift
        );
    }

    #[test]
    fn mission_close_refusal_quiet_only_when_already_closed() {
        assert_eq!(
            classify_mission_close_refusal(Some(MissionStatus::Finalized)),
            FinalizeRefusal::Benign
        );
        assert_eq!(
            classify_mission_close_refusal(Some(MissionStatus::Active)),
            FinalizeRefusal::Drift
        );
        assert_eq!(
            classify_mission_close_refusal(Some(MissionStatus::Paused)),
            FinalizeRefusal::Drift
        );
        // Unknown status (mission unreadable) is loud.
        assert_eq!(classify_mission_close_refusal(None), FinalizeRefusal::Drift);
    }

    /// Mirrors `lifecycle::tests::CrewGuard` — isolates both the crew root
    /// (mission/phase JSON) and the flow-record sink so these tests never
    /// touch the operator's real `~/.darkmux` state.
    struct CrewGuard {
        _tmp_crew: TempDir,
        _tmp_flows: TempDir,
        prev_crew: Option<String>,
        prev_flows: Option<String>,
    }

    impl CrewGuard {
        fn new() -> Self {
            let tmp_crew = TempDir::new().unwrap();
            let tmp_flows = TempDir::new().unwrap();
            let prev_crew = env::var("DARKMUX_CREW_DIR").ok();
            let prev_flows = env::var("DARKMUX_FLOWS_DIR").ok();
            // SAFETY: serialized via #[serial_test::serial] on every caller.
            unsafe {
                env::set_var("DARKMUX_CREW_DIR", tmp_crew.path());
                env::set_var("DARKMUX_FLOWS_DIR", tmp_flows.path());
            }
            Self { _tmp_crew: tmp_crew, _tmp_flows: tmp_flows, prev_crew, prev_flows }
        }
    }

    impl Drop for CrewGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_crew {
                    Some(v) => env::set_var("DARKMUX_CREW_DIR", v),
                    None => env::remove_var("DARKMUX_CREW_DIR"),
                }
                match &self.prev_flows {
                    Some(v) => env::set_var("DARKMUX_FLOWS_DIR", v),
                    None => env::remove_var("DARKMUX_FLOWS_DIR"),
                }
            }
        }
    }

    /// `lifecycle::save_json` is private to that module — these tests
    /// write the fixture JSON directly (same atomic-rename discipline
    /// isn't needed for a test fixture write).
    fn write_json<T: Serialize>(path: &std::path::Path, value: &T) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    fn seed_mission(id: &str) -> Mission {
        let m = Mission {
            id: id.to_string(),
            description: "test mission".to_string(),
            status: MissionStatus::Active,
            phase_ids: vec!["p1".to_string(), "p2".to_string()],
            created_ts: 1_700_000_000,
            started_ts: Some(1_700_000_000),
            finalized_ts: None,
            paused_ts: None,
            source_input: None,
            ticket: None,
            spec: None,
        };
        write_json(&lifecycle::mission_path(id), &m);
        m
    }

    fn seed_phase(mission_id: &str, phase_id: &str) -> Phase {
        let p = Phase {
            id: phase_id.to_string(),
            mission_id: mission_id.to_string(),
            description: "test phase".to_string(),
            display_name: None,
            status: PhaseStatus::Running,
            created_ts: 1_700_000_000,
            started_ts: Some(1_700_000_000),
            completed_ts: None,
            abandoned_ts: None,
            task_ids: Vec::new(),
        };
        write_json(&lifecycle::phase_path(mission_id, phase_id), &p);
        p
    }

    fn phase_status(mission_id: &str, phase_id: &str) -> PhaseStatus {
        let text = std::fs::read_to_string(lifecycle::phase_path(mission_id, phase_id)).unwrap();
        serde_json::from_str::<Phase>(&text).unwrap().status
    }

    fn mission_status(mission_id: &str) -> MissionStatus {
        let text = std::fs::read_to_string(lifecycle::mission_path(mission_id)).unwrap();
        serde_json::from_str::<Mission>(&text).unwrap().status
    }

    #[serial_test::serial]
    #[test]
    fn clean_status_completes_phases_and_closes_mission() {
        let _g = CrewGuard::new();
        seed_mission("m1");
        seed_phase("m1", "p1");
        seed_phase("m1", "p2");

        let envelope = MissionEnvelope::new("m1", MissionOutcomeStatus::Clean, &["p1", "p2"]);
        finalize_mission(&envelope);

        assert_eq!(phase_status("m1", "p1"), PhaseStatus::Complete);
        assert_eq!(phase_status("m1", "p2"), PhaseStatus::Complete);
        assert_eq!(mission_status("m1"), MissionStatus::Finalized);
    }

    #[serial_test::serial]
    #[test]
    fn degraded_status_completes_phases_same_as_clean() {
        // (deliverable 1's mapping table) Degraded folds into the SAME
        // phase/mission outcome as Clean — a degraded run still posted
        // real findings; degraded means honest labeling, not discarding.
        let _g = CrewGuard::new();
        seed_mission("m2");
        seed_phase("m2", "p1");
        seed_phase("m2", "p2");

        let mut envelope = MissionEnvelope::new("m2", MissionOutcomeStatus::Degraded, &["p1", "p2"]);
        envelope.warnings = vec!["remote probe seat failed after bounded retries".to_string()];
        finalize_mission(&envelope);

        assert_eq!(phase_status("m2", "p1"), PhaseStatus::Complete);
        assert_eq!(phase_status("m2", "p2"), PhaseStatus::Complete);
        assert_eq!(mission_status("m2"), MissionStatus::Finalized);
    }

    #[serial_test::serial]
    #[test]
    fn degenerate_status_abandons_phases_and_closes_mission() {
        let _g = CrewGuard::new();
        seed_mission("m3");
        seed_phase("m3", "p1");
        seed_phase("m3", "p2");

        let mut envelope = MissionEnvelope::new("m3", MissionOutcomeStatus::Degenerate, &["p1", "p2"]);
        envelope.reason = Some("no bundles produced from the diff".to_string());
        finalize_mission(&envelope);

        assert_eq!(phase_status("m3", "p1"), PhaseStatus::Abandoned);
        assert_eq!(phase_status("m3", "p2"), PhaseStatus::Abandoned);
        assert_eq!(mission_status("m3"), MissionStatus::Finalized);
    }

    #[serial_test::serial]
    #[test]
    fn error_status_abandons_phases_and_closes_mission() {
        let _g = CrewGuard::new();
        seed_mission("m4");
        seed_phase("m4", "p1");
        seed_phase("m4", "p2");

        let mut envelope = MissionEnvelope::new("m4", MissionOutcomeStatus::Error, &["p1", "p2"]);
        envelope.reason = Some("review errored: probe dispatch failed".to_string());
        finalize_mission(&envelope);

        assert_eq!(phase_status("m4", "p1"), PhaseStatus::Abandoned);
        assert_eq!(phase_status("m4", "p2"), PhaseStatus::Abandoned);
        assert_eq!(mission_status("m4"), MissionStatus::Finalized);
    }

    #[serial_test::serial]
    #[test]
    fn payload_round_trips_a_mission_type_specific_shape() {
        let envelope = MissionEnvelope {
            payload: serde_json::json!({"confirmed": 3, "needs_check": 1}),
            ..MissionEnvelope::new("m5", MissionOutcomeStatus::Clean, &[])
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let back: MissionEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.payload["confirmed"], serde_json::json!(3));
        assert_eq!(back.payload["needs_check"], serde_json::json!(1));
    }

    #[serial_test::serial]
    #[test]
    fn finalize_mission_persists_envelope_json_beside_mission_json() {
        // (#1284 Packet 3) `envelope.json` is a real artifact, not
        // constructed-and-dropped — verifies BOTH the path (beside
        // `mission.json`, per `lifecycle::envelope_path`'s doc) and that
        // the round-tripped content matches what was finalized.
        let _g = CrewGuard::new();
        seed_mission("m6");
        seed_phase("m6", "p1");

        let mut envelope = MissionEnvelope::new("m6", MissionOutcomeStatus::Degraded, &["p1"]);
        envelope.warnings = vec!["remote probe token budget exhausted".to_string()];
        envelope.payload = serde_json::json!({"confirmed": 2});
        finalize_mission(&envelope);

        let path = lifecycle::envelope_path("m6");
        assert_eq!(path, lifecycle::mission_path("m6").parent().unwrap().join("envelope.json"));
        let persisted = lifecycle::load_envelope("m6")
            .expect("load_envelope must succeed")
            .expect("an envelope.json must exist after finalize_mission");
        assert_eq!(persisted.mission_id, "m6");
        assert_eq!(persisted.status, MissionOutcomeStatus::Degraded);
        assert_eq!(persisted.warnings, vec!["remote probe token budget exhausted".to_string()]);
        assert_eq!(persisted.payload["confirmed"], serde_json::json!(2));
    }

    #[serial_test::serial]
    #[test]
    fn load_envelope_returns_none_when_no_envelope_json_exists_yet() {
        let _g = CrewGuard::new();
        seed_mission("m7");
        assert!(lifecycle::load_envelope("m7").unwrap().is_none());
    }

    // ── #1877 item 2: `outcome` field + `MissionOutcomeStatus::from_outcome` ──

    #[test]
    fn from_outcome_maps_all_three_run_outcome_variants() {
        assert_eq!(MissionOutcomeStatus::from_outcome(&RunOutcome::Complete), MissionOutcomeStatus::Clean);
        assert_eq!(
            MissionOutcomeStatus::from_outcome(&RunOutcome::Partial { reasons: vec!["x".to_string()] }),
            MissionOutcomeStatus::Degraded
        );
        assert_eq!(
            MissionOutcomeStatus::from_outcome(&RunOutcome::Empty { reason: "y".to_string() }),
            MissionOutcomeStatus::Degenerate
        );
    }

    /// (#1881, QA-caught) `RunOutcome::Unknown` must map to
    /// `MissionOutcomeStatus::Unknown`, not `Degraded` — an earlier version
    /// of this mapping used `Degraded`, which `mission_run_status` and
    /// `mission_launch.rs`'s exit-code match both read as a PASS, directly
    /// contradicting `phase_outcome`'s own `Unknown -> Abandoned` safety
    /// choice a few lines below (see `from_outcome`'s doc for the full
    /// story). This is the ONE place that inconsistency could have entered
    /// the system — `phase_outcome`/`mission_run_status` are read straight
    /// off whatever `from_outcome` decided, never re-derived — so pinning
    /// this mapping directly is what keeps the whole chain honest.
    #[test]
    fn from_outcome_maps_unknown_to_unknown_not_degraded() {
        assert_eq!(MissionOutcomeStatus::from_outcome(&RunOutcome::Unknown), MissionOutcomeStatus::Unknown);
        // And the consequence that matters: routed through `phase_outcome`,
        // an unrecognized outcome must abandon, never complete.
        assert_eq!(
            MissionOutcomeStatus::from_outcome(&RunOutcome::Unknown).phase_outcome(),
            PhaseOutcomeKind::Abandoned
        );
    }

    #[test]
    fn mission_envelope_from_outcome_never_lets_status_disagree_with_outcome() {
        // The whole point of `from_outcome`: there is no path to construct
        // an envelope where `status` and `outcome` tell a different story —
        // unlike the pre-#1877 shape, where a driver computed `status` by
        // hand and could (in principle) drift from what `outcome` says.
        let reasons = vec!["3 of 12 flags went unjudged on the `judge-pass1` stage".to_string()];
        let envelope =
            MissionEnvelope::from_outcome("m-partial", RunOutcome::Partial { reasons: reasons.clone() }, &["p1"]);
        assert_eq!(envelope.status, MissionOutcomeStatus::Degraded);
        assert_eq!(envelope.outcome, Some(RunOutcome::Partial { reasons: reasons.clone() }));
        // The REAL numbers survive intact on `outcome.reasons` — not folded
        // into a generic string the way `warnings` alone would lose them.
        assert_eq!(
            match &envelope.outcome {
                Some(RunOutcome::Partial { reasons }) => reasons.clone(),
                other => panic!("expected Partial, got {other:?}"),
            },
            reasons
        );
        // Phase outcome still derives from `status`, same as `new()`.
        assert_eq!(envelope.phases[0].outcome, PhaseOutcomeKind::Complete);
    }

    #[test]
    fn mission_envelope_new_leaves_outcome_none() {
        // Every driver that hasn't adopted `RunOutcome` yet (crew-of-one,
        // the generic scheduler graph, coder-phase) keeps calling `new()`
        // directly — its envelopes must have no `outcome` at all, not a
        // fabricated `Some(Complete)` that would misrepresent "this driver
        // has no docket-coverage concept" as "this driver checked and the
        // docket was fully covered."
        let envelope = MissionEnvelope::new("m-plain", MissionOutcomeStatus::Clean, &[]);
        assert!(envelope.outcome.is_none());
    }

    #[test]
    fn older_envelope_json_without_the_outcome_field_deserializes_with_a_sensible_default() {
        // A real 1.0-era envelope.json, hand-written (no `outcome` key at
        // all — the field did not exist when this was written). Must load
        // cleanly under the 1.1 struct, per the additive-minor discipline:
        // an older document is not just tolerated, it round-trips its own
        // fields unchanged.
        let old_json = r#"{
            "mission_id": "m-old",
            "schema_version": "1.0",
            "status": "degraded",
            "reason": "remote judge token budget exhausted",
            "phases": [],
            "warnings": ["remote judge token budget exhausted"],
            "remote_budgets": []
        }"#;
        let envelope: MissionEnvelope =
            serde_json::from_str(old_json).expect("a pre-#1877 envelope must still deserialize");
        assert_eq!(envelope.mission_id, "m-old");
        assert_eq!(envelope.status, MissionOutcomeStatus::Degraded);
        assert_eq!(envelope.warnings, vec!["remote judge token budget exhausted".to_string()]);
        // The sensible default for a document that predates the field:
        // `None`, never a guessed `RunOutcome`.
        assert!(envelope.outcome.is_none());
    }

    #[test]
    fn outcome_field_round_trips_through_json_when_present() {
        let envelope = MissionEnvelope::from_outcome(
            "m-rt",
            RunOutcome::Empty { reason: "no bundles produced from the diff".to_string() },
            &[],
        );
        let json = serde_json::to_string(&envelope).unwrap();
        assert!(json.contains(r#""outcome":{"state":"empty""#), "outcome must serialize under its own key: {json}");
        let back: MissionEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.outcome, envelope.outcome);
        assert_eq!(back.status, MissionOutcomeStatus::Degenerate);
    }

    #[serial_test::serial]
    #[test]
    fn finalize_mission_persists_the_outcome_field_alongside_status() {
        let _g = CrewGuard::new();
        seed_mission("m8");
        seed_phase("m8", "p1");

        let envelope = MissionEnvelope::from_outcome(
            "m8",
            RunOutcome::Partial { reasons: vec!["2 of 9 flags went unjudged".to_string()] },
            &["p1"],
        );
        finalize_mission(&envelope);

        let persisted = lifecycle::load_envelope("m8")
            .expect("load_envelope must succeed")
            .expect("an envelope.json must exist after finalize_mission");
        assert_eq!(persisted.status, MissionOutcomeStatus::Degraded);
        assert_eq!(
            persisted.outcome,
            Some(RunOutcome::Partial { reasons: vec!["2 of 9 flags went unjudged".to_string()] })
        );
    }

    /// (#1881, supersedes the pre-#1881 `an_unrecognized_outcome_variant_fails_the_whole_envelope_parse`)
    /// Structural proof for `MISSION_ENVELOPE_SCHEMA` 1.2's doc claim: an
    /// `outcome` key present with a `RunOutcome` variant this binary
    /// doesn't recognize now degrades `outcome` to `Unknown` via
    /// `#[serde(other)]` — it no longer fails the whole `MissionEnvelope`
    /// parse. `status`/`mission_id`/`phases` come through intact, unaware
    /// anything was unrecognized. Before #1881, decoding this exact JSON
    /// errored with `` unknown variant `throttled`, expected one of
    /// `complete`, `partial`, `empty` `` (this test's own earlier
    /// version — see the git history on this test name — pinned exactly
    /// that failure and observed it passing on unmodified pre-#1881 code).
    #[test]
    fn an_unrecognized_outcome_variant_degrades_to_unknown_and_the_rest_of_the_document_still_parses() {
        let json = r#"{
            "mission_id": "m1",
            "schema_version": "1.2",
            "status": "degraded",
            "outcome": {"state": "throttled"},
            "phases": []
        }"#;
        let envelope: MissionEnvelope =
            serde_json::from_str(json).expect("an unrecognized RunOutcome variant must degrade, not fail the whole parse");
        assert_eq!(envelope.outcome, Some(RunOutcome::Unknown));
        assert_eq!(envelope.mission_id, "m1");
        assert_eq!(envelope.status, MissionOutcomeStatus::Degraded);
        assert!(envelope.phases.is_empty());
    }

    /// (#1881) The `status` field itself — not just `outcome` — also
    /// degrades leniently now: a `status` value this binary doesn't
    /// recognize becomes `MissionOutcomeStatus::Unknown` rather than
    /// failing the whole parse, and the rest of the document (mission_id,
    /// phases) still comes through. This is the field
    /// `RunStatus` derivation actually reads
    /// (`crates/darkmux-serve/src/runs.rs`'s `mission_run_status`), unlike
    /// `outcome` above — see that module's own tests for what `Unknown`
    /// status maps to (`RunStatus::Unparseable`, never `Complete`).
    #[test]
    fn an_unrecognized_status_degrades_to_unknown_and_the_rest_of_the_document_still_parses() {
        let json = r#"{
            "mission_id": "m2",
            "schema_version": "1.2",
            "status": "throttled",
            "phases": []
        }"#;
        let envelope: MissionEnvelope =
            serde_json::from_str(json).expect("an unrecognized status value must degrade, not fail the whole parse");
        assert_eq!(envelope.status, MissionOutcomeStatus::Unknown);
        assert_eq!(envelope.mission_id, "m2");
    }
}
