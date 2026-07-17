//! `StepKind` trait + `StepOutcome` — the step-kind execution contract.

use crate::types::{Step, Task};
use anyhow::Result;
use darkmux_flow::FlowRecord;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// (#1442) One remote-token bucket metering the per-EXECUTION remote
/// allowance (`DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION`, where one
/// execution = one pipeline stage). Local dispatches never touch it. A
/// `budget` of 0 is exhausted from the FIRST item (`used (0) >= budget
/// (0)`), so a zero allowance refuses every hosted call — the same hard
/// opt-out `admit_remote_execution` gives a single hosted dispatch.
///
/// **The ceiling is SOFT (approximate), by construction (#1451 gate).**
/// Admission is checked BEFORE a call ([`admit`](Self::admit)) and tokens
/// are spent AFTER it ([`spend`](Self::spend)), so a stage can overshoot
/// `budget` by at most ONE granted call — the per-item `max_tokens` is
/// clamped to what the bucket has LEFT ([`remaining`](Self::remaining), gate
/// C6), which bounds that overshoot to whatever the endpoint itself reports
/// ABOVE its granted cap. This is deliberate: a per-execution allowance is a
/// spend GUARDRAIL, not a hard byte gate, and a call's exact cost is
/// unknowable until it runs.
///
/// **Bucket-group semantics (#1442 gate — the highest-stakes carry-forward
/// of the ship-2b rewiring).** Where it once lived private in `builtins`,
/// scoped to a SINGLE `dispatch.map` step, the bucket now lives here so the
/// SCHEDULER can own a `group -> Arc<Mutex<MapRemoteBucket>>` map and hand
/// the SAME bucket to every sibling step that names a `bucket_group` in its
/// config (`run_step_graph`). That is what stops `seats x k` sibling
/// `dispatch.map` probe steps from EACH minting a fresh full per-execution
/// allowance (which would multiply the effective stage ceiling by the step
/// count — the "allowance multiplication" the block's own doc named as the
/// follow-on's obligation). The block stays Tier-1-pure: a `dispatch.map`
/// step that names NO group still creates its own step-scoped bucket from
/// the same budget, so grouped and ungrouped both read the one-execution
/// contract; the caller's `Arc` never becomes a field of the kind itself,
/// it arrives through the scheduler-supplied [`StepRunCtx`].
#[derive(Debug)]
pub struct MapRemoteBucket {
    budget: u64,
    used: u64,
    skipped: u32,
}

impl MapRemoteBucket {
    pub(crate) fn new(budget: u64) -> Self {
        Self { budget, used: 0, skipped: 0 }
    }
    pub(crate) fn exhausted(&self) -> bool {
        self.used >= self.budget
    }
    /// What is left to grant a single call (#1442 gate C6): per-item
    /// `max_tokens` clamps to THIS, not the full budget, so one late item
    /// cannot request more than the bucket has left.
    pub(crate) fn remaining(&self) -> u64 {
        self.budget.saturating_sub(self.used)
    }
    /// (#1442 fan-out) Admit one call and RESERVE its granted `max_tokens`
    /// up front, returning the granted (clamped) cap — or `None` when the
    /// bucket is already exhausted (counted as skipped, for the item's
    /// named-reason result). The reserve-then-[`settle`](Self::settle) shape
    /// replaces the old admit-then-spend-after pair because sibling
    /// `dispatch.map` steps of one `bucket_group` (the probe stage's
    /// `seats x k` fan-out) run CONCURRENTLY on `run_bounded` worker
    /// threads: with spend-after accounting, every in-flight sibling could
    /// admit against the same untouched balance and the stage would
    /// overshoot by one call PER SIBLING. Reserving the granted cap at
    /// admission bounds the whole group's overshoot to what an endpoint
    /// itself reports ABOVE a granted cap — the same soft-ceiling reading as
    /// before, now independent of sibling concurrency. In a sequential
    /// per-item loop the observable behavior is identical to the old pair
    /// (settle always lands before the next admit).
    pub(crate) fn admit_reserve(&mut self, requested: u32) -> Option<u32> {
        if self.exhausted() {
            self.skipped += 1;
            return None;
        }
        let granted = u32::try_from(self.remaining()).unwrap_or(u32::MAX).min(requested);
        self.used = self.used.saturating_add(u64::from(granted));
        Some(granted)
    }
    /// Settle a reserved call against its ACTUAL spend (the reply's reported
    /// usage, or — conservatively — the granted cap when the endpoint omits
    /// usage; see `conservative_hosted_spend`). Replaces the reservation
    /// with the real number; an endpoint that reports above its granted cap
    /// pushes the bucket over (the documented soft-ceiling overshoot), one
    /// that reports under releases the difference back to its siblings.
    pub(crate) fn settle(&mut self, granted: u32, actual: u64) {
        self.used = self.used.saturating_sub(u64::from(granted)).saturating_add(actual);
    }
    /// Count of calls refused because the bucket was exhausted — read by
    /// tests asserting the skip path fired.
    #[cfg(test)]
    pub(crate) fn skipped(&self) -> u32 {
        self.skipped
    }
}

/// (#1442, ship-2b probe/verify retirement) One dispatch a `dispatch.map`
/// item is about to make, surfaced to the scheduler-supplied
/// [`MapDispatchOverride`] test seam. Field-parallel to the union of the
/// LOCAL ([`crate::single_shot::SingleShotRequest`]) and HOSTED
/// ([`crate::single_shot::HostedSingleShotRequest`]) request shapes:
/// `endpoint: Some` marks the HOSTED dialect (where `temperature` is
/// meaningless and carried as `0.0` — the hosted wire request has no such
/// field), `None` the LOCAL one.
pub struct OverrideDispatchCall<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub user: &'a str,
    /// LOCAL dialect only — `0.0` on a hosted call (no wire field).
    pub temperature: f32,
    /// The granted (already bucket-clamped, on the hosted arm) completion
    /// cap this call would send.
    pub max_tokens: u32,
    pub timeout_seconds: u32,
    pub endpoint: Option<&'a darkmux_types::ModelEndpoint>,
}

/// (#1442, ship-2b — the operator-recorded seam decision on PR #1455) An
/// optional dispatch interceptor for `dispatch.map` items, carried on
/// [`StepRunCtx`] so it crosses the `run_bounded` WORKER-THREAD boundary —
/// the same injection discipline as `darkmux-lab`'s
/// `ReviewStepContext::chat_override` (an `Arc<dyn Fn + Send + Sync>`
/// field, `None` at every production call site). A thread-local seam
/// cannot serve here: the scheduler executes steps on spawned scoped
/// threads (`concurrent_dispatch::run_remote_batches` /
/// `run_local_waves`), where a test thread's thread-local is invisible.
/// When present, `dispatch.map` routes every item's call through it INSTEAD
/// of the real `single_shot_chat`/`single_shot_chat_hosted` transport —
/// budget metering, retry semantics, telemetry, and per-item records all
/// still apply exactly as on the real path (the override replaces the
/// TRANSPORT, never the accounting).
pub type MapDispatchOverride =
    Arc<dyn for<'a> Fn(&OverrideDispatchCall<'a>) -> Result<crate::single_shot::SingleShotReply> + Send + Sync>;

/// (#1442) The execution context the SCHEDULER supplies to each step's
/// [`StepKind::run_streaming`] — two seams that must originate OUTSIDE the
/// step (so the step kind holds no caller `Arc` of its own and stays
/// tier-pure):
///
/// 1. **Live emitter (#1442 gate C3, "no blind runs").** A channel back to
///    the scheduler's own emission seam. A step that produces per-item
///    records mid-run sends them through [`StepRunCtx::emit`] so they reach
///    the flow stream LIVE — before the step completes — instead of
///    batching into [`StepOutcome::flow_records`] at wave-drain. The step
///    NEVER touches the global flow sink directly: the scheduler owns the
///    sink (and the lab/fleet boundary that picks WHICH sink), so routing
///    through this channel preserves that boundary.
/// 2. **Scheduler-supplied shared remote bucket (#1442).** When a step
///    names a `bucket_group`, the scheduler resolves the group's shared
///    [`MapRemoteBucket`] and hands it here; sibling steps of the same group
///    meter one allowance BETWEEN them. `None` when the step named no group
///    (the kind falls back to a step-scoped bucket).
pub struct StepRunCtx {
    emitter: Option<std::sync::mpsc::Sender<FlowRecord>>,
    remote_bucket: Option<Arc<Mutex<MapRemoteBucket>>>,
    dispatch_override: Option<MapDispatchOverride>,
}

impl StepRunCtx {
    pub(crate) fn new(
        emitter: Option<std::sync::mpsc::Sender<FlowRecord>>,
        remote_bucket: Option<Arc<Mutex<MapRemoteBucket>>>,
        dispatch_override: Option<MapDispatchOverride>,
    ) -> Self {
        Self { emitter, remote_bucket, dispatch_override }
    }

    /// Emit one flow record LIVE through the scheduler's emission seam
    /// (#1442 gate C3). A `None` emitter (a context with no streaming sink —
    /// e.g. a step kind exercised in a unit test outside the scheduler)
    /// silently drops it; the kind's batched [`StepOutcome::flow_records`]
    /// remains the fallback path.
    pub fn emit(&self, record: FlowRecord) {
        if let Some(tx) = &self.emitter {
            // A closed channel (the scheduler stopped draining, e.g. on an
            // early return) is not a step-level error — the record is best-
            // effort observability, never load-bearing control flow.
            let _ = tx.send(record);
        }
    }

    /// The scheduler-supplied shared remote-token bucket for this step's
    /// `bucket_group`, if the step named one. A grouped `dispatch.map` uses
    /// THIS across its whole collection loop; an ungrouped one gets `None`
    /// here and creates its own step-scoped bucket.
    pub fn remote_bucket(&self) -> Option<&Arc<Mutex<MapRemoteBucket>>> {
        self.remote_bucket.as_ref()
    }

    /// The caller-supplied dispatch interceptor for `dispatch.map` items —
    /// `None` on every production path (see [`MapDispatchOverride`]).
    pub fn dispatch_override(&self) -> Option<&MapDispatchOverride> {
        self.dispatch_override.as_ref()
    }
}

/// One step kind's completed outcome. `output` becomes the Step's
/// persisted `Step.output` on success, and is what downstream Steps
/// see as their own `input` entry keyed by this step's id (see
/// `scheduler::gather_inputs`). `flow_records` are ADDITIONAL records
/// the step kind wants emitted alongside the scheduler's own step-
/// lifecycle bookends (most built-in kinds return an empty `Vec` today —
/// their own dispatch primitives already emit their own records via the
/// ordinary thread-safe `darkmux_flow::record()` free function, which,
/// unlike `darkmux_flow::bookend::BookendGuard`, has no non-`Send`
/// state and is safe to call from inside a `run_bounded` worker thread
/// directly).
#[derive(Debug)]
pub struct StepOutcome {
    pub output: String,
    pub flow_records: Vec<FlowRecord>,
}

/// One registered step-kind implementation. `run` is synchronous and
/// blocking (matches every other dispatch primitive in darkmux — see
/// `workloads::types::WorkloadProvider`'s own doc: "darkmux is a single-
/// task CLI so blocking is fine"). `Send + Sync` so an `Arc<dyn
/// StepKind>` can be cloned into a `run_bounded` worker's `'static`
/// job closure (see the module doc on `StepKindRegistry`).
///
/// `input` is the gathered `output` text of every already-`Complete`
/// dependency, keyed by that dependency's Step id (`scheduler::
/// gather_inputs`) — a step kind decides for itself whether/how to
/// weave prior-step output into its own request (see
/// `DispatchInternalStepKind`/`DispatchSingleShotStepKind` for the
/// convention used).
///
/// `task` is the Step's OWNING `Task` — resolved by `run_step_graph` from
/// `Step.task_id` (falling back to a synthetic empty `Task` if the graph's
/// caller never registered one, e.g. a scheduler-level test exercising
/// pure Step scheduling with no Task-assignment concerns — see
/// `scheduler::run_step_graph`'s doc). A Task is the ASSIGNABLE unit
/// (#1230/#1341) — like a Jira ticket assigned to one crew member,
/// `task.role_id`/`task.profile_name`/`task.workdir`/`task.image` are
/// properties of the whole job, fixed for its duration, not re-declared at
/// every Step; a dispatch-shaped step kind (`DispatchInternalStepKind`)
/// sources its assignment from THESE fields first, falling back to
/// `Step.config` only when the Task leaves a field unset. Purely-procedural
/// step kinds (`procedural.*`) ignore `task` entirely.
pub trait StepKind: Send + Sync {
    fn id(&self) -> &'static str;
    fn run(&self, step: &Step, task: &Task, input: &BTreeMap<String, String>) -> Result<StepOutcome>;

    /// (#1442) The scheduler's ACTUAL entry point — `run` with the
    /// scheduler-supplied [`StepRunCtx`] (live emitter + shared remote
    /// bucket) threaded in. Defaults to ignoring the context and delegating
    /// to [`StepKind::run`], so every existing kind keeps its exact behavior
    /// (records batched into [`StepOutcome::flow_records`], a step-scoped
    /// bucket) with no change. A kind that wants LIVE per-item emission or a
    /// scheduler-shared `bucket_group` (`dispatch.map`) overrides THIS and
    /// leaves `run` as the ctx-free path unit tests still drive directly.
    ///
    /// The context is Arc/channel-backed and `Send` so it crosses the
    /// `run_bounded` worker-thread boundary alongside the job closure.
    fn run_streaming(
        &self,
        step: &Step,
        task: &Task,
        input: &BTreeMap<String, String>,
        ctx: &StepRunCtx,
    ) -> Result<StepOutcome> {
        let _ = ctx;
        self.run(step, task, input)
    }

    /// (#1402) A short, human-facing name for this kind — the graph lens,
    /// the viewer's mission drill-down, and `mission status` all render
    /// THIS instead of the raw registry id (`"dispatch.internal"` reads as
    /// "Dispatch"). Registered once beside each kind's constructor, right
    /// next to `id()`.
    ///
    /// Defaults to `id()` — a kind that hasn't been given a nicer label yet
    /// still renders something legible rather than a hole in the fallback
    /// chain (StepKind display name → kind id → step id → `"unknown"`, see
    /// `darkmux-serve`'s `mission_graph` module doc). Every Tier 1 builtin
    /// and every Tier 3 kind shipped with darkmux overrides this; the
    /// default exists for third-party/future kinds that haven't yet.
    fn display_name(&self) -> &'static str {
        self.id()
    }

    /// (#1230 Packet 3) Which local model, if any, this step needs
    /// resident before it can run — feeds `run_step_graph`'s per-step
    /// `Residency::Local(Placement)` vs `Residency::Remote` classification
    /// (`concurrent_dispatch::run_bounded`'s wave-safety mechanism).
    ///
    /// `None` (the default — every kind's behavior before this hook
    /// existed, and every step kind that isn't a local-model dispatch,
    /// e.g. `procedural.*`) classifies the step `Residency::Remote`:
    /// cap-bounded concurrency only, no RAM-safety wave reasoning. A
    /// dispatch-shaped local kind overrides this to resolve a real
    /// `Placement` from its own config/role/Task assignment.
    ///
    /// **Best-effort, fails open.** This is a SCHEDULING CLASSIFICATION
    /// hint, not the dispatch's own model resolution — the step's `run`
    /// method (and whatever it wraps, e.g. `dispatch::dispatch`'s own
    /// preflight) still does its own full, strict resolution when it
    /// actually executes. If this can't cleanly resolve a placement (no
    /// role, unknown role, quarantined profile, remote endpoint, …) it
    /// returns `None` rather than erroring — worst case the step is
    /// scheduled as `Remote` (today's behavior for every kind), never a
    /// hard failure purely from misclassification.
    ///
    /// `input` is the SAME gathered dependency-output map `run` will
    /// receive (the scheduler computes it once per ready step, before
    /// residency classification — see `scheduler::run_step_graph`). A kind
    /// whose model need is DATA-DEPENDENT can inspect it and return `None`
    /// when the inputs make its dispatch a guaranteed no-op, so the wave
    /// loader never loads a model the step is certain not to use (#1426
    /// ship-2 operator decision — the review verify seat with an empty
    /// confirmed docket is the first consumer). Kinds with static needs
    /// ignore it.
    fn residency(
        &self,
        step: &Step,
        task: &Task,
        input: &std::collections::BTreeMap<String, String>,
    ) -> Option<darkmux_gestalt::Placement> {
        let _ = (step, task, input);
        None
    }
}
