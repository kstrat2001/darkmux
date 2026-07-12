//! `StepKind` trait + `StepOutcome` — the step-kind execution contract.

use crate::types::Step;
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
pub trait StepKind: Send + Sync {
    fn id(&self) -> &'static str;
    fn run(&self, step: &Step, input: &BTreeMap<String, String>) -> Result<StepOutcome>;

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
    /// `Placement` from its own config/role.
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
    fn residency(&self, step: &Step) -> Option<darkmux_gestalt::Placement> {
        let _ = step;
        None
    }
}
