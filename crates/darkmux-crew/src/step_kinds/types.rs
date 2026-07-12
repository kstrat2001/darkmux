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
}
