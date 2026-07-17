//! `StepKind` trait + `StepOutcome` — the step-kind execution contract.

use crate::types::{Step, Task};
use anyhow::Result;
use darkmux_flow::FlowRecord;
use std::collections::BTreeMap;

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
