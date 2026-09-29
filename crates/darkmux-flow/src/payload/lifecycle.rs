//! Payloads of the graph and run lifecycle records: `step.*`, `mission.run.*`,
//! `mission.grow`, `run.*`, `phase.review.verdict`, `radio.route`,
//! `gh.verb.executed` and the endpoint budget's `budget.*`.

use super::dispatch::ResultClass;
use super::Attribution;
use serde::{Deserialize, Serialize};

/// What a step consumes: the model seat it claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum SeatClass {
    /// A local model, placed and leased.
    LocalModel,
    /// A hosted endpoint.
    RemoteEndpoint,
    /// No model at all.
    NoModel,
    /// A local model whose placement could not be resolved: it runs with no wave load and no
    /// residency lease.
    LocalModelUnresolved,
}

/// A step began: the payload of `step.start`, stamped with what the step consumes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct StepStartPayload {
    pub seat_class: SeatClass,
}

impl Attribution for StepStartPayload {}

/// A local seat whose placement could not be resolved: the payload of `step.seat_unresolved`.
/// `Warn`, because it names a real lost guarantee: the dispatch meant to run a local model and has
/// no residency lease.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct StepSeatUnresolvedPayload {
    pub step_id: String,
    /// The step kind.
    pub kind: String,
    pub seat_class: SeatClass,
    /// The resolver's own reason, verbatim, so the operator fixes the actual cause.
    pub reason: String,
    /// What the step went without.
    pub lost: String,
}

impl Attribution for StepSeatUnresolvedPayload {
    fn step(&self) -> Option<&str> {
        Some(&self.step_id)
    }
}

/// One step's in/out counts and wall time: the payload of `step.timing`, and the very shape of the
/// scheduler's in-memory step summary, so there is exactly one such shape in the tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct StepTimingPayload {
    /// The step's own id; review's steps are `bundle`, `probe`, `dedup`, `judge-pass1`, `judge-
    /// pass2`.
    pub step_id: String,
    /// The step's kind: the full step-kind id (`dispatch.internal`, `procedural.shell`), or
    /// review's coarser `procedural` and `dispatch`.
    pub kind: String,
    /// How many items the step consumed, when its producer knows; absent means "not known to this
    /// producer", never a zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub items_in: Option<u64>,
    /// The output side of `items_in`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub items_out: Option<u64>,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub wall_ms: u64,
}

impl Attribution for StepTimingPayload {
    fn step(&self) -> Option<&str> {
        Some(&self.step_id)
    }
}

/// A bash verifier command the runtime classified as FAILED TO RUN: the binary was missing, not
/// executable, or its toolchain failed to load, so it never verified anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct FailedVerifier {
    pub command: String,
    pub reason: String,
}

/// The rich, kind-specific result of a step: the payload of `step.result`, the companion to the
/// scheduler's generic step bookends. One type for every producer: a hosted single-shot (spend and
/// caps), a `dispatch.map` (per item, per aggregate, or the empty-collection short circuit), a
/// verifier check, and the coder-phase steps (worktree, coder, verify). `step_id` and `kind` are
/// always there; the rest belongs to the kind that wrote it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct StepResultPayload {
    pub step_id: String,
    /// The step kind, or the coder-phase step (`mission.worktree`, `mission.coder`,
    /// `mission.verify`).
    pub kind: String,
    /// A hosted single-shot's per-step cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub remote_max_tokens_per_execution: Option<u64>,
    /// What the step asked the endpoint for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub max_tokens_requested: Option<u64>,
    /// What it sent after capping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub max_tokens_sent: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub completion_tokens: Option<u64>,
    /// A hosted step's spend, an item's, an aggregate's sum, or a coder-phase step's total.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub cached_tokens: Option<u64>,
    /// On a `dispatch.map` item: its position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub index: Option<u64>,
    /// On a `dispatch.map` item: whether it produced a reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub ok: Option<bool>,
    /// On a `dispatch.map` item or aggregate: whether it ran on a hosted endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub remote: Option<bool>,
    /// On a hosted `dispatch.map` item: the endpoint-reported served model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub served_model: Option<String>,
    /// On a `dispatch.map` item: its cumulative dispatch wall-clock across every attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub wall_ms: Option<u64>,
    /// On a failed item or coder-phase step: why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub error: Option<String>,
    /// On a `dispatch.map` aggregate or short circuit: how many items came in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub items_in: Option<u64>,
    /// On a short circuit: how many went out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub items_out: Option<u64>,
    /// On a `dispatch.map` aggregate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub ok_count: Option<u64>,
    /// On a `dispatch.map` aggregate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub failed_count: Option<u64>,
    /// On a `dispatch.map` aggregate: the summed per-item dispatch wall-clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub total_wall_ms: Option<u64>,
    /// On a short circuit: why the map did not dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub short_circuit: Option<String>,
    /// Verifier commands that failed to run: a soft signal for the adjudicator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub failed_verifiers: Option<Vec<FailedVerifier>>,
    /// How many `failed_verifiers`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub count: Option<u64>,
    /// On a coder-phase worktree step: the role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub role: Option<String>,
    /// On a coder-phase worktree step: the base branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub base: Option<String>,
    /// On a coder-phase worktree step: the branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub branch: Option<String>,
    /// On a coder-phase worktree step: the worktree path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub worktree: Option<String>,
    /// On a coder-phase coder step: the dispatch's exit code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub exit_code: Option<i64>,
    /// On a coder-phase verify step: the review verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub verdict: Option<String>,
    /// On a coder-phase verify step: blocker findings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub blockers: Option<u64>,
    /// On a coder-phase verify step: flag findings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub flags: Option<u64>,
    /// On a coder-phase verify step: nit findings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub nits: Option<u64>,
}

impl Attribution for StepResultPayload {
    fn step(&self) -> Option<&str> {
        Some(&self.step_id)
    }
}


impl StepResultPayload {
    /// A result of step `step_id`'s `kind`, with nothing else set.
    pub fn new(step_id: &str, kind: &str) -> Self {
        Self {
            step_id: step_id.to_string(),
            kind: kind.to_string(),
            remote_max_tokens_per_execution: None,
            max_tokens_requested: None,
            max_tokens_sent: None,
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            reasoning_tokens: None,
            cached_tokens: None,
            index: None,
            ok: None,
            remote: None,
            served_model: None,
            wall_ms: None,
            error: None,
            items_in: None,
            items_out: None,
            ok_count: None,
            failed_count: None,
            total_wall_ms: None,
            short_circuit: None,
            failed_verifiers: None,
            count: None,
            role: None,
            base: None,
            branch: None,
            worktree: None,
            exit_code: None,
            verdict: None,
            blockers: None,
            flags: None,
            nits: None,
        }
    }
}

/// A whole-mission teardown of one phase's worktree: the payload of `mission.run.finalize` and
/// `mission.run.abort`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct MissionRunTerminalPayload {
    /// The branch the worktree carried.
    pub branch: Option<String>,
    /// The worktree that was torn down; `null` when there was none.
    pub worktree: Option<String>,
}

impl Attribution for MissionRunTerminalPayload {}

/// Why a `grow` minted nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum GrowReason {
    /// The producing step errored: distinct from a producer that legitimately found zero items.
    ProducerErrored,
    /// The producer found zero items.
    GrewNothing,
}

/// A mission graph grew: the payload of `mission.grow`, the provenance of every task a `grow`
/// template minted from an earlier phase's output. A plan that planned nothing is still a recorded
/// outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct MissionGrowPayload {
    /// The phase the tasks were minted into.
    pub phase: String,
    pub task_template: String,
    /// The producing task.
    pub from: String,
    /// The producing step.
    pub source: String,
    /// How many items the producer's output held.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub items: u64,
    /// The tasks minted.
    pub minted: Vec<String>,
    /// Present when the growth minted nothing; omitted, never `null`, otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub reason: Option<GrowReason>,
    /// On a producer error: the step that errored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub producer_step: Option<String>,
    /// On a producer error: its status, as `NodeStatus` spells it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub producer_status: Option<String>,
}

impl Attribution for MissionGrowPayload {}

/// A run's liveness bookend: the payload of `run.start` (empty), `run.complete` and `run.error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct RunPayload {
    /// How the run ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub result_class: Option<ResultClass>,
    /// Why a run that errored did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub error: Option<String>,
    /// The graph's final status, as it renders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub status: Option<String>,
    /// The gate the run ended at (`coder-phase`), when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub gate: Option<String>,
}

impl Attribution for RunPayload {}


impl RunPayload {
    /// A run that ended in an error: `result_class: error` and why.
    pub fn failed(error: impl Into<String>) -> Self {
        Self { result_class: Some(ResultClass::Error), error: Some(error.into()), ..Self::default() }
    }

    /// A run that ended, classed by whether it did what it was launched to do.
    pub fn ended(ok: bool) -> Self {
        Self { result_class: Some(if ok { ResultClass::Ok } else { ResultClass::Error }), ..Self::default() }
    }
}

/// A code review's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum ReviewVerdict {
    /// At least one blocking finding.
    Blockers,
    /// Flags or nits, nothing blocking.
    #[serde(rename = "flags-only")]
    FlagsOnly,
    /// No findings and the reviewer said so.
    Clean,
    /// No findings and no clean marker: the reviewer may not have engaged with the format, so
    /// inspect manually.
    Indeterminate,
}


impl ReviewVerdict {
    /// The wire word.
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewVerdict::Blockers => "blockers",
            ReviewVerdict::FlagsOnly => "flags-only",
            ReviewVerdict::Clean => "clean",
            ReviewVerdict::Indeterminate => "indeterminate",
        }
    }
}

impl std::fmt::Display for ReviewVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The review's verdict terminal: the payload of `phase.review.verdict`, with the finding counts in
/// the record's `handle`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct PhaseReviewVerdictPayload {
    pub verdict: ReviewVerdict,
}

impl Attribution for PhaseReviewVerdictPayload {}

/// Where a radio request came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum RadioSurface {
    /// The command line.
    Cli,
    /// The editor panel.
    Panel,
}

/// What the radio's routing seat decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum RadioDecision {
    /// A catalog command.
    Route,
    /// The model declined, or its output was not a catalog command.
    Refuse,
    /// The routing call could not run.
    Unavailable,
}

/// One routed invocation of the radio: the payload of `radio.route`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct RadioRoutePayload {
    pub surface: RadioSurface,
    /// The raw request, bounded.
    pub source_text: String,
    pub decision: RadioDecision,
    /// On a route: the catalog command chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub command: Option<String>,
    /// On a route: its arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub args: Option<String>,
    /// On a refusal: why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub reason: Option<String>,
    /// When unavailable: the failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub error: Option<String>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
}

impl Attribution for RadioRoutePayload {
    fn step_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.step_id)
    }
    fn step(&self) -> Option<&str> {
        self.step_id.as_deref()
    }
}

/// A gated command ran: the payload of `gh.verb.executed`, ONE record per executed gated command
/// regardless of outcome, so a blocked or failed attempt is still on the trail. `pr` is a best-
/// effort extraction: the first token of the raw args, verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct GhVerbExecutedPayload {
    pub verb: String,
    /// The first whitespace-delimited token of the args; `null` when there were none.
    pub pr: Option<String>,
    pub worktree: String,
    /// Whether an operator sign-off gate was confirmed during the run; `null` when no gate ran.
    pub confirmed: Option<bool>,
    pub success: bool,
}

impl Attribution for GhVerbExecutedPayload {}

/// What a budget record is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum BudgetScope {
    /// An endpoint's rolling-window budget.
    Endpoint,
    /// A step's per-step cap.
    Step,
}

/// What a budget does on a breach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum BudgetPolicyKind {
    /// Nothing is counted.
    Off,
    /// A breach is surfaced and the work keeps going.
    Warn,
    /// Calls pause until the budget has room again.
    Wait,
}

/// What a breach was measured in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum BudgetMetric {
    /// Tokens.
    Tokens,
    /// Calls.
    Calls,
}

/// How far into a budget the known spend is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub enum BreachLevel {
    /// At or past `warn_at` of the budget, still under it.
    Early,
    /// At or past the budget.
    AtLimit,
}

/// An endpoint budget or a per-step cap acted: the payload of `budget.warn`, `budget.wait`,
/// `budget.resume` and `budget.stop`. One type for the four: `scope` and `message` are always
/// there; the rest belongs to the action and the scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-export", ts(export, export_to = "../../../ui/src/types/generated/"))]
pub struct BudgetPayload {
    pub scope: BudgetScope,
    /// What the operator was told, verbatim.
    pub message: String,
    /// On an endpoint budget: the `endpoints` id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub endpoint_id: Option<String>,
    /// On a per-step cap: the step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub policy: Option<BudgetPolicyKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub level: Option<BreachLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub metric: Option<BudgetMetric>,
    /// The known spend; a floor when some calls reported no complete count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub spent: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub limit: Option<u64>,
    /// Calls that reported no complete token count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub unmetered_calls: Option<u64>,
    /// The window as the operator wrote it (`1d`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub period: Option<String>,
    /// The fraction of the budget at which the early warning fires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub warn_at: Option<f64>,
    /// On a wait: when the window has room, epoch milliseconds, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub resume_at_ms: Option<u64>,
    /// On a wait: how long it will wait, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub wait_ms: Option<u64>,
    /// On a resume or a stop: how long it waited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number", optional))]
    pub waited_ms: Option<u64>,
    /// On a stop: why the run was stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub reason: Option<String>,
    /// On a wait, resume or stop: the waiting process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub pid: Option<u32>,
    /// The graph step the record belongs to, when its session is a step's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub step_id: Option<String>,
    /// The provenance a dispatch caller supplied, carried verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "Record<string, unknown>", optional))]
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Attribution for BudgetPayload {
    fn step_slot(&mut self) -> Option<&mut Option<String>> {
        Some(&mut self.step_id)
    }
    fn step(&self) -> Option<&str> {
        self.step_id.as_deref()
    }
    fn context_slot(&mut self) -> Option<&mut Option<serde_json::Map<String, serde_json::Value>>> {
        Some(&mut self.context)
    }
}
